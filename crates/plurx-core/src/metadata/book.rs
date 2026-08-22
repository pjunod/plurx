//! First-class metadata for Books libraries.
//!
//! There are two authorities and their ordering is part of the contract:
//! an explicit Curator handoff outranks facts embedded in a standalone EPUB.
//! Artwork always lands in Cinema's cache. Neither path writes beside, renames,
//! or otherwise mutates the library file it inspected.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use futures_util::StreamExt;
use quick_xml::events::{BytesStart, BytesText, Event};
use quick_xml::Reader;
use tokio::io::AsyncReadExt;

use crate::domain::{
    ArtworkAttempt, BookMetadataPatch, BookMetadataSource, ItemKind, MediaFile, MetadataPatch,
};
use crate::store::Store;

const EPUB_MIMETYPE: &str = "application/epub+zip";
const MAX_ARCHIVE_ENTRIES: usize = 4_096;
const MAX_MARKUP_BYTES: u64 = 2 * 1024 * 1024;
const MAX_COVER_BYTES: u64 = 12 * 1024 * 1024;
const MAX_TITLE_BYTES: usize = 512;
const MAX_AUTHOR_BYTES: usize = 512;
const MAX_IDENTIFIER_BYTES: usize = 256;
const COVER_HOST: &str = "covers.openlibrary.org";
const COVER_EXTRACT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, thiserror::Error)]
pub enum BookMetadataError {
    #[error("cannot read EPUB: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid EPUB ZIP: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("invalid EPUB metadata: {0}")]
    Invalid(&'static str),
    #[error("EPUB metadata exceeds Cinema's safety limit")]
    Limit,
    #[error("cover URL is not an allowed Open Library HTTPS URL")]
    CoverUrl,
    #[error("cover request failed: {0}")]
    Http(String),
    #[error("cannot extract embedded cover: {0}")]
    EmbeddedCover(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EpubFacts {
    pub title: Option<String>,
    pub author: Option<String>,
    pub identifier: Option<String>,
    pub cover: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct BookEnrichReport {
    pub inspected: usize,
    pub updated: usize,
    pub errors: usize,
}

#[derive(Clone)]
struct ManifestItem {
    href: String,
    media_type: String,
    properties: String,
}

/// Extract only catalogue facts and an optional cover. The publication reader
/// performs its own complete manifest/spine validation; this deliberately
/// does not inflate arbitrary resources just to paint a library card.
pub fn read_epub_facts(path: &Path) -> Result<EpubFacts, BookMetadataError> {
    let file = File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    if archive.is_empty() || archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(BookMetadataError::Limit);
    }
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        normalize_archive_path(entry.name())?;
        if entry.encrypted() {
            return Err(BookMetadataError::Invalid(
                "encrypted content is unsupported",
            ));
        }
    }
    let mimetype = read_entry(&mut archive, "mimetype", 128)?;
    if std::str::from_utf8(&mimetype).ok().map(str::trim) != Some(EPUB_MIMETYPE) {
        return Err(BookMetadataError::Invalid("mimetype is not EPUB"));
    }
    let container = read_entry(&mut archive, "META-INF/container.xml", MAX_MARKUP_BYTES)?;
    let package_path = container_rootfile(&container)?;
    let package = read_entry(&mut archive, &package_path, MAX_MARKUP_BYTES)?;
    let (mut facts, cover_path) = package_facts(&package, &package_path)?;
    if let Some(path) = cover_path {
        facts.cover = Some(read_entry(&mut archive, &path, MAX_COVER_BYTES)?);
    }
    Ok(facts)
}

/// Enrich standalone editions in one Books library: catalogue facts and covers
/// from EPUB packages, and attached cover pictures from audiobook containers.
/// Explicit Curator facts are never parsed as candidates for replacement,
/// which keeps precedence true across later scheduled scans as well as at
/// initial import time; an audiobook cover is an artwork-only patch and cannot
/// change that source rank.
pub async fn enrich_library(
    store: &dyn Store,
    artwork_dir: &Path,
    library_id: i64,
    force: bool,
    only: Option<&[i64]>,
) -> BookEnrichReport {
    let mut report = BookEnrichReport::default();
    let items = match store.book_items(library_id, only).await {
        Ok(items) => items,
        Err(error) => {
            tracing::warn!(library_id, error = %error, "listing books for EPUB metadata");
            report.errors = 1;
            return report;
        }
    };
    if let Err(error) = tokio::fs::create_dir_all(artwork_dir).await {
        tracing::warn!(path = %artwork_dir.display(), error = %error, "creating book artwork cache");
        report.errors = 1;
        return report;
    }
    for item in items {
        if item.kind != ItemKind::Book && item.kind != ItemKind::Audiobook {
            continue;
        }
        if item.kind == ItemKind::Book
            && item.book_metadata_source.as_deref() == Some(BookMetadataSource::Curator.as_str())
        {
            continue;
        }
        if item.kind == ItemKind::Audiobook && !force && item.poster_path.is_some() {
            continue;
        }
        let files = match store.files_for_item(item.id).await {
            Ok(files) => files,
            Err(error) => {
                report.errors += 1;
                tracing::warn!(item = item.id, error = %error, "listing book edition files");
                continue;
            }
        };
        if item.kind == ItemKind::Audiobook {
            enrich_audiobook_cover(store, artwork_dir, item.id, &files, &mut report).await;
            continue;
        }
        let Some(path) = files.into_iter().map(|file| file.path).find(|path| {
            path.extension()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.eq_ignore_ascii_case("epub"))
        }) else {
            continue;
        };
        report.inspected += 1;
        let parsed = tokio::task::spawn_blocking(move || read_epub_facts(&path)).await;
        let facts = match parsed {
            Ok(Ok(facts)) => facts,
            Ok(Err(error)) => {
                report.errors += 1;
                tracing::warn!(item = item.id, error = %error, "reading standalone EPUB metadata");
                continue;
            }
            Err(error) => {
                report.errors += 1;
                tracing::warn!(item = item.id, error = %error, "EPUB metadata worker failed");
                continue;
            }
        };
        let poster_path = if let Some(bytes) = facts
            .cover
            .as_deref()
            .filter(|_| force || item.poster_path.is_none())
        {
            match write_cached_cover(artwork_dir, item.id, bytes).await {
                Ok(path) => Some(path),
                Err(error) => {
                    report.errors += 1;
                    tracing::warn!(item = item.id, error = %error, "caching embedded EPUB cover");
                    None
                }
            }
        } else {
            None
        };
        let patch = BookMetadataPatch {
            title: clean(facts.title, MAX_TITLE_BYTES),
            author: clean(facts.author, MAX_AUTHOR_BYTES),
            work_id: None,
            edition_id: clean(facts.identifier, MAX_IDENTIFIER_BYTES),
            poster_path,
            source: BookMetadataSource::Epub,
        };
        let unchanged = patch
            .title
            .as_deref()
            .is_none_or(|value| value == item.title)
            && patch.author == item.author
            && patch.edition_id == item.book_edition_id
            && patch.poster_path.is_none();
        if unchanged && item.book_metadata_source.as_deref() == Some("epub") {
            continue;
        }
        if let Err(error) = store.apply_book_metadata(item.id, &patch).await {
            report.errors += 1;
            tracing::warn!(item = item.id, error = %error, "storing EPUB metadata");
        } else {
            report.updated += 1;
        }
    }
    report
}

/// Adopt the first attached picture already carried by an audiobook part.
///
/// The scanner deliberately ignores attached pictures as video streams, but
/// keeps ffprobe's raw document. That makes the document a cheap guard before
/// asking ffmpeg to copy the exact image bytes: ordinary audio never spawns a
/// second process, while MP3/M4B/FLAC cover formats stay ffmpeg's problem rather
/// than becoming a pile of container-specific parsers here.
async fn enrich_audiobook_cover(
    store: &dyn Store,
    artwork_dir: &Path,
    item_id: i64,
    files: &[MediaFile],
    report: &mut BookEnrichReport,
) {
    for file in files {
        let probe = match store.get_file_probe_json(file.id).await {
            Ok(Some(probe)) => probe,
            Ok(None) => continue,
            Err(error) => {
                report.errors += 1;
                tracing::warn!(item = item_id, file = file.id, error = %error, "reading audiobook probe for cover art");
                continue;
            }
        };
        let Some(stream_index) = attached_picture_stream(&probe) else {
            continue;
        };
        report.inspected += 1;
        match extract_attached_picture(artwork_dir, item_id, &file.path, stream_index).await {
            Ok(poster_path) => {
                let patch = MetadataPatch {
                    poster_path: Some(poster_path),
                    artwork: Some(ArtworkAttempt::Stored),
                    ..Default::default()
                };
                match store.apply_metadata(item_id, &patch).await {
                    Ok(()) => report.updated += 1,
                    Err(error) => {
                        report.errors += 1;
                        tracing::warn!(item = item_id, error = %error, "storing embedded audiobook cover");
                    }
                }
                return;
            }
            Err(error) => {
                report.errors += 1;
                tracing::warn!(item = item_id, path = %file.path.display(), error = %error, "extracting embedded audiobook cover");
                let patch = MetadataPatch {
                    artwork: Some(ArtworkAttempt::Failed(error.to_string())),
                    ..Default::default()
                };
                if let Err(store_error) = store.apply_metadata(item_id, &patch).await {
                    tracing::warn!(item = item_id, error = %store_error, "recording audiobook cover failure");
                }
            }
        }
    }
}

fn attached_picture_stream(probe: &str) -> Option<i64> {
    let probe: serde_json::Value = serde_json::from_str(probe).ok()?;
    probe
        .get("streams")?
        .as_array()?
        .iter()
        .find(|stream| {
            stream.get("codec_type").and_then(|value| value.as_str()) == Some("video")
                && stream
                    .get("disposition")
                    .and_then(|value| value.get("attached_pic"))
                    .and_then(|value| value.as_i64())
                    .is_some_and(|value| value != 0)
        })?
        .get("index")?
        .as_i64()
        .filter(|index| *index >= 0)
}

fn ffmpeg_bin() -> String {
    std::env::var("PLURX_FFMPEG")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "ffmpeg".to_owned())
}

async fn extract_attached_picture(
    artwork_dir: &Path,
    item_id: i64,
    media: &Path,
    stream_index: i64,
) -> Result<String, BookMetadataError> {
    tokio::fs::create_dir_all(artwork_dir).await?;
    let mut command = tokio::process::Command::new(ffmpeg_bin());
    command
        .kill_on_drop(true)
        .arg("-nostdin")
        .args(["-v", "error", "-y", "-i"])
        .arg(media)
        .args(["-map", &format!("0:{stream_index}"), "-frames:v", "1"])
        .args(["-c:v", "copy", "-f", "image2pipe", "pipe:1"])
        .stdout(Stdio::piped())
        // The bounded stdout is the diagnostic that matters. Discarding
        // stderr also prevents a malformed input from filling a second pipe
        // while the byte-bound reader is waiting on the first one.
        .stderr(Stdio::null());
    let extract = async move {
        let mut child = command
            .spawn()
            .map_err(|error| BookMetadataError::EmbeddedCover(error.to_string()))?;
        let stdout = child.stdout.take().ok_or_else(|| {
            BookMetadataError::EmbeddedCover("ffmpeg stdout was unavailable".to_owned())
        })?;
        let mut bytes = Vec::new();
        stdout
            .take(MAX_COVER_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
            .await
            .map_err(|error| BookMetadataError::EmbeddedCover(error.to_string()))?;
        if bytes.len() > MAX_COVER_BYTES as usize {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(BookMetadataError::Limit);
        }
        let status = child
            .wait()
            .await
            .map_err(|error| BookMetadataError::EmbeddedCover(error.to_string()))?;
        if !status.success() {
            return Err(BookMetadataError::EmbeddedCover(format!(
                "ffmpeg exited with {status}"
            )));
        }
        Ok(bytes)
    };
    let bytes = tokio::time::timeout(COVER_EXTRACT_TIMEOUT, extract)
        .await
        .map_err(|_| BookMetadataError::EmbeddedCover("ffmpeg timed out".to_owned()))??;
    write_cached_cover(artwork_dir, item_id, &bytes).await
}

/// Fetch one Curator-provided Open Library cover with no redirects, no bearer,
/// a strict host allowlist, and a streaming byte bound.
pub async fn cache_curator_cover(
    artwork_dir: &Path,
    item_id: i64,
    url: &str,
) -> Result<String, BookMetadataError> {
    let parsed = allowed_curator_cover_url(url).ok_or(BookMetadataError::CoverUrl)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .user_agent(concat!("plurx/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| BookMetadataError::Http(error.to_string()))?;
    let response = client
        .get(parsed)
        .send()
        .await
        .map_err(|error| BookMetadataError::Http(error.to_string()))?;
    if !response.status().is_success() {
        return Err(BookMetadataError::Http(format!(
            "status {}",
            response.status()
        )));
    }
    if response
        .content_length()
        .is_some_and(|size| size > MAX_COVER_BYTES)
    {
        return Err(BookMetadataError::Limit);
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| BookMetadataError::Http(error.to_string()))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_COVER_BYTES as usize {
            return Err(BookMetadataError::Limit);
        }
        bytes.extend_from_slice(&chunk);
    }
    tokio::fs::create_dir_all(artwork_dir).await?;
    write_cached_cover(artwork_dir, item_id, &bytes).await
}

/// Parse the sole provider URL Curator currently hands across the pairing.
/// Exposed so the HTTP boundary can reject an unsafe request before queuing
/// work; the downloader calls the same function again at the network edge.
pub fn allowed_curator_cover_url(url: &str) -> Option<reqwest::Url> {
    let parsed = reqwest::Url::parse(url).ok()?;
    (parsed.scheme() == "https"
        && parsed.host_str() == Some(COVER_HOST)
        && parsed.port().is_none()
        && parsed.username().is_empty()
        && parsed.password().is_none())
    .then_some(parsed)
}

async fn write_cached_cover(
    artwork_dir: &Path,
    item_id: i64,
    bytes: &[u8],
) -> Result<String, BookMetadataError> {
    if bytes.is_empty() || bytes.len() > MAX_COVER_BYTES as usize {
        return Err(BookMetadataError::Limit);
    }
    let extension = raster_cover_extension(bytes).ok_or(BookMetadataError::Invalid(
        "cover is not a supported raster image",
    ))?;
    let filename = format!("{item_id}-poster.{extension}");
    let target = artwork_dir.join(&filename);
    let temporary = artwork_dir.join(format!(".{filename}.{}.tmp", uuid::Uuid::new_v4().simple()));
    tokio::fs::write(&temporary, bytes).await?;
    if let Err(error) = tokio::fs::rename(&temporary, &target).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(BookMetadataError::Io(error));
    }
    Ok(filename)
}

fn raster_cover_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("jpg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

fn read_entry<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
    limit: u64,
) -> Result<Vec<u8>, BookMetadataError> {
    let entry = archive.by_name(name)?;
    if entry.encrypted() {
        return Err(BookMetadataError::Invalid(
            "encrypted content is unsupported",
        ));
    }
    if entry.size() > limit {
        return Err(BookMetadataError::Limit);
    }
    let mut bytes = Vec::with_capacity(usize::try_from(entry.size()).unwrap_or(0));
    entry.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit as usize {
        return Err(BookMetadataError::Limit);
    }
    Ok(bytes)
}

fn xml_attribute(
    reader: &Reader<&[u8]>,
    element: &BytesStart<'_>,
    name: &[u8],
) -> Result<Option<String>, BookMetadataError> {
    for attribute in element.attributes() {
        let attribute =
            attribute.map_err(|_| BookMetadataError::Invalid("malformed XML attribute"))?;
        if attribute.key.local_name().as_ref() == name {
            return attribute
                .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, reader.decoder())
                .map(|value| Some(value.into_owned()))
                .map_err(|_| BookMetadataError::Invalid("invalid XML attribute"));
        }
    }
    Ok(None)
}

fn xml_text(text: BytesText<'_>) -> Result<String, BookMetadataError> {
    let decoded = text
        .decode()
        .map_err(|_| BookMetadataError::Invalid("invalid XML encoding"))?;
    quick_xml::escape::unescape(&decoded)
        .map(|value| value.into_owned())
        .map_err(|_| BookMetadataError::Invalid("invalid XML entity"))
}

fn container_rootfile(xml: &[u8]) -> Result<String, BookMetadataError> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event() {
            Ok(Event::Start(element) | Event::Empty(element))
                if element.local_name().as_ref() == b"rootfile" =>
            {
                let path = xml_attribute(&reader, &element, b"full-path")?.ok_or(
                    BookMetadataError::Invalid("container has no package document"),
                )?;
                return normalize_archive_path(&path);
            }
            Ok(Event::Eof) => break,
            Err(_) => return Err(BookMetadataError::Invalid("malformed container XML")),
            _ => {}
        }
    }
    Err(BookMetadataError::Invalid(
        "container has no package document",
    ))
}

fn package_facts(
    xml: &[u8],
    package_path: &str,
) -> Result<(EpubFacts, Option<String>), BookMetadataError> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut facts = EpubFacts::default();
    let mut items = HashMap::new();
    let mut epub2_cover_id = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => match element.local_name().as_ref() {
                b"title" if facts.title.is_none() => {
                    let name = element.name();
                    facts.title = clean(
                        Some(xml_text(reader.read_text(name).map_err(|_| {
                            BookMetadataError::Invalid("malformed package XML")
                        })?)?),
                        MAX_TITLE_BYTES,
                    );
                }
                b"creator" if facts.author.is_none() => {
                    let name = element.name();
                    facts.author = clean(
                        Some(xml_text(reader.read_text(name).map_err(|_| {
                            BookMetadataError::Invalid("malformed package XML")
                        })?)?),
                        MAX_AUTHOR_BYTES,
                    );
                }
                b"identifier" if facts.identifier.is_none() => {
                    let name = element.name();
                    facts.identifier = clean(
                        Some(xml_text(reader.read_text(name).map_err(|_| {
                            BookMetadataError::Invalid("malformed package XML")
                        })?)?),
                        MAX_IDENTIFIER_BYTES,
                    );
                }
                b"item" => {
                    let id = xml_attribute(&reader, &element, b"id")?.unwrap_or_default();
                    let href = xml_attribute(&reader, &element, b"href")?.unwrap_or_default();
                    if !id.is_empty() && !href.is_empty() {
                        items.insert(
                            id,
                            ManifestItem {
                                href: resolve_href(package_path, &href)?,
                                media_type: xml_attribute(&reader, &element, b"media-type")?
                                    .unwrap_or_default(),
                                properties: xml_attribute(&reader, &element, b"properties")?
                                    .unwrap_or_default(),
                            },
                        );
                    }
                }
                b"meta"
                    if xml_attribute(&reader, &element, b"name")?.as_deref() == Some("cover") =>
                {
                    epub2_cover_id = xml_attribute(&reader, &element, b"content")?;
                }
                _ => {}
            },
            Ok(Event::Empty(element)) => match element.local_name().as_ref() {
                b"item" => {
                    let id = xml_attribute(&reader, &element, b"id")?.unwrap_or_default();
                    let href = xml_attribute(&reader, &element, b"href")?.unwrap_or_default();
                    if !id.is_empty() && !href.is_empty() {
                        items.insert(
                            id,
                            ManifestItem {
                                href: resolve_href(package_path, &href)?,
                                media_type: xml_attribute(&reader, &element, b"media-type")?
                                    .unwrap_or_default(),
                                properties: xml_attribute(&reader, &element, b"properties")?
                                    .unwrap_or_default(),
                            },
                        );
                    }
                }
                b"meta"
                    if xml_attribute(&reader, &element, b"name")?.as_deref() == Some("cover") =>
                {
                    epub2_cover_id = xml_attribute(&reader, &element, b"content")?;
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => return Err(BookMetadataError::Invalid("malformed package XML")),
            _ => {}
        }
    }
    let cover = items
        .values()
        .find(|item| {
            item.properties
                .split_whitespace()
                .any(|value| value == "cover-image")
        })
        .or_else(|| epub2_cover_id.as_ref().and_then(|id| items.get(id)))
        .filter(|item| item.media_type.starts_with("image/"))
        .map(|item| item.href.clone());
    Ok((facts, cover))
}

fn clean(value: Option<String>, max_bytes: usize) -> Option<String> {
    value.map(|value| value.trim().to_owned()).filter(|value| {
        !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
    })
}

fn normalize_archive_path(path: &str) -> Result<String, BookMetadataError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.starts_with('\\')
        || path.contains('\\')
        || path.contains('\0')
        || path.chars().any(char::is_control)
    {
        return Err(BookMetadataError::Invalid("unsafe archive path"));
    }
    let mut normalized = Vec::new();
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(BookMetadataError::Invalid("unsafe archive path"));
        }
        if normalized.is_empty() && segment.len() == 2 && segment.ends_with(':') {
            return Err(BookMetadataError::Invalid("unsafe archive path"));
        }
        normalized.push(segment);
    }
    Ok(normalized.join("/"))
}

fn resolve_href(base_file: &str, href: &str) -> Result<String, BookMetadataError> {
    let href = href.trim().split(['#', '?']).next().unwrap_or_default();
    if href.is_empty() || href.starts_with('/') || href.starts_with('\\') {
        return Err(BookMetadataError::Invalid("unsafe package link"));
    }
    let path = percent_decode_path(href)?;
    if path.contains(':') || path.contains('\\') || path.contains('\0') {
        return Err(BookMetadataError::Invalid("remote or unsafe package link"));
    }
    let mut parts = base_file
        .rsplit_once('/')
        .map(|(parent, _)| parent.split('/').collect::<Vec<_>>())
        .unwrap_or_default();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(BookMetadataError::Invalid("package link escapes archive"));
                }
            }
            value => parts.push(value),
        }
    }
    normalize_archive_path(&parts.join("/"))
}

fn percent_decode_path(path: &str) -> Result<String, BookMetadataError> {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = bytes.get(index + 1).and_then(|value| hex_nibble(*value));
            let low = bytes.get(index + 2).and_then(|value| hex_nibble(*value));
            let (Some(high), Some(low)) = (high, low) else {
                return Err(BookMetadataError::Invalid("invalid percent-encoded link"));
            };
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| BookMetadataError::Invalid("link is not UTF-8"))
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn fixture(package: &str, cover: Option<&[u8]>) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().expect("temp EPUB");
        let mut writer = zip::ZipWriter::new(file.reopen().expect("reopen EPUB"));
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let deflated = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        writer.start_file("mimetype", stored).expect("mimetype");
        writer
            .write_all(EPUB_MIMETYPE.as_bytes())
            .expect("write mimetype");
        writer
            .start_file("META-INF/container.xml", deflated)
            .expect("container");
        writer.write_all(br#"<container><rootfiles><rootfile full-path="OEBPS/book.opf"/></rootfiles></container>"#).expect("write container");
        writer
            .start_file("OEBPS/book.opf", deflated)
            .expect("package");
        writer.write_all(package.as_bytes()).expect("write package");
        if let Some(cover) = cover {
            writer
                .start_file("OEBPS/Images/cover.jpg", deflated)
                .expect("cover");
            writer.write_all(cover).expect("write cover");
        }
        writer.finish().expect("finish EPUB");
        file
    }

    #[test]
    fn extracts_author_identifier_and_epub3_cover_without_expanding_the_book() {
        let file = fixture(
            r#"<package><metadata><title>Proof &amp; Practice</title><creator>A. &amp; B. Reader</creator><identifier>urn:isbn:9780000000001</identifier></metadata><manifest><item id="cover" href="Images/cover.jpg" media-type="image/jpeg" properties="cover-image"/></manifest></package>"#,
            Some(b"cover-bytes"),
        );
        let facts = read_epub_facts(file.path()).expect("EPUB facts");
        assert_eq!(facts.title.as_deref(), Some("Proof & Practice"));
        assert_eq!(facts.author.as_deref(), Some("A. & B. Reader"));
        assert_eq!(facts.identifier.as_deref(), Some("urn:isbn:9780000000001"));
        assert_eq!(facts.cover.as_deref(), Some(b"cover-bytes".as_slice()));
    }

    #[test]
    fn refuses_traversing_cover_href() {
        let file = fixture(
            r#"<package><metadata><title>Bad</title></metadata><manifest><item id="cover" href="../../secret.jpg" media-type="image/jpeg" properties="cover-image"/></manifest></package>"#,
            None,
        );
        assert!(read_epub_facts(file.path()).is_err());
    }

    #[test]
    fn package_facts_are_individually_bounded_and_control_free() {
        let long_title = "x".repeat(MAX_TITLE_BYTES + 1);
        let package = format!(
            "<package><metadata><title>{long_title}</title><creator>A&#x9;Reader</creator><identifier>{}</identifier></metadata></package>",
            "i".repeat(MAX_IDENTIFIER_BYTES + 1)
        );
        let file = fixture(&package, None);
        let facts = read_epub_facts(file.path()).expect("bounded EPUB facts");
        assert_eq!(facts.title, None);
        assert_eq!(facts.author, None);
        assert_eq!(facts.identifier, None);
    }

    #[test]
    fn cover_cache_accepts_only_bounded_raster_formats() {
        assert_eq!(
            raster_cover_extension(&[0xff, 0xd8, 0xff, 0x00]),
            Some("jpg")
        );
        assert_eq!(
            raster_cover_extension(b"\x89PNG\r\n\x1a\nrest"),
            Some("png")
        );
        assert_eq!(raster_cover_extension(b"<svg><script/></svg>"), None);
    }

    #[test]
    fn curator_cover_allowlist_is_exact_https_without_credentials_or_ports() {
        assert!(
            allowed_curator_cover_url("https://covers.openlibrary.org/b/olid/OL1M-L.jpg").is_some()
        );
        for url in [
            "http://covers.openlibrary.org/b/olid/OL1M-L.jpg",
            "https://covers.openlibrary.org:444/b/olid/OL1M-L.jpg",
            "https://user@covers.openlibrary.org/b/olid/OL1M-L.jpg",
            "https://covers.openlibrary.org.example.com/b/olid/OL1M-L.jpg",
        ] {
            assert!(allowed_curator_cover_url(url).is_none(), "accepted {url}");
        }
    }

    #[test]
    fn finds_only_a_real_attached_picture_stream() {
        assert_eq!(
            attached_picture_stream(
                r#"{"streams":[
                    {"index":0,"codec_type":"audio"},
                    {"index":1,"codec_type":"video","disposition":{"attached_pic":1}}
                ]}"#
            ),
            Some(1)
        );
        for probe in [
            r#"{"streams":[{"index":1,"codec_type":"video","disposition":{"attached_pic":0}}]}"#,
            r#"{"streams":[{"index":1,"codec_type":"audio","disposition":{"attached_pic":1}}]}"#,
            "not json",
        ] {
            assert_eq!(attached_picture_stream(probe), None, "accepted {probe}");
        }
    }
}
