//! Bounded EPUB parsing and capability-scoped publication resources.
//!
//! An authenticated request opens the container and receives a short-lived,
//! file-revision-bound session. Child resources use only that narrow session
//! capability, so a sandboxed iframe never receives the user's API token.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek};
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use axum::Json;
use bytes::Bytes;
use plurx_core::domain::ItemKind;
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use serde::Serialize;
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;
use zip::ZipArchive;

use super::dto::RevisionDto;
use super::error::ApiError;
use super::extract::AuthUser;
use crate::state::AppState;

pub(crate) const MAX_ENTRIES: usize = 20_000;
pub(crate) const MAX_TOTAL_UNCOMPRESSED: u64 = 1024 * 1024 * 1024;
pub(crate) const MAX_RESOURCE_BYTES: u64 = 128 * 1024 * 1024;
pub(crate) const MAX_MARKUP_BYTES: u64 = 8 * 1024 * 1024;
pub(crate) const MAX_COMPRESSION_RATIO: u64 = 1_000;
pub(crate) const MAX_CONCURRENT_RESOURCE_READS: usize = 8;
pub(crate) const RESOURCE_CHUNK_BYTES: usize = 64 * 1024;
const SESSION_TTL: Duration = Duration::from_secs(2 * 60 * 60);
const MAX_SESSIONS: usize = 64;
const RESOURCE_CHANNEL_DEPTH: usize = 2;
const EPUB_MIMETYPE: &str = "application/epub+zip";
const RESOURCE_CSP: &str = "default-src 'none'; script-src 'none'; connect-src 'none'; \
    img-src 'self' data:; font-src 'self' data:; style-src 'self' 'unsafe-inline'; \
    media-src 'self'; frame-src 'none'; object-src 'none'; form-action 'none'; \
    base-uri 'none'; frame-ancestors 'self'";

#[derive(Debug)]
enum PublicationError {
    Invalid(&'static str),
    Unsupported(&'static str),
    Limit(&'static str),
    Missing,
    Changed,
    Io(String),
}

impl PublicationError {
    fn api(self) -> ApiError {
        match self {
            Self::Invalid(message) => {
                ApiError::typed(StatusCode::UNPROCESSABLE_ENTITY, "invalid_epub", message)
            }
            Self::Unsupported(message) => ApiError::typed(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_epub",
                message,
            ),
            Self::Limit(message) => {
                ApiError::typed(StatusCode::PAYLOAD_TOO_LARGE, "epub_limit", message)
            }
            Self::Missing => ApiError::NotFound("publication resource"),
            Self::Changed => ApiError::typed(
                StatusCode::CONFLICT,
                "publication_changed",
                "The book file changed. Reopen it before continuing.",
            ),
            Self::Io(message) => ApiError::Internal(message),
        }
    }
}

impl From<std::io::Error> for PublicationError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

impl From<zip::result::ZipError> for PublicationError {
    fn from(error: zip::result::ZipError) -> Self {
        match error {
            zip::result::ZipError::FileNotFound => Self::Missing,
            other => Self::Invalid(match other {
                zip::result::ZipError::UnsupportedArchive(_) => {
                    "The EPUB uses an unsupported ZIP feature."
                }
                _ => "The EPUB ZIP container is malformed.",
            }),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PublicationMetadata {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PublicationLink {
    pub href: String,
    #[serde(rename = "type")]
    pub media_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct TocLink {
    pub href: String,
    pub title: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<TocLink>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PublicationManifest {
    pub metadata: PublicationMetadata,
    #[serde(rename = "readingOrder")]
    pub reading_order: Vec<PublicationLink>,
    pub resources: Vec<PublicationLink>,
    pub toc: Vec<TocLink>,
}

#[derive(Clone, Debug)]
struct Publication {
    manifest: PublicationManifest,
    resource_types: HashMap<String, String>,
}

#[derive(Clone)]
struct PublicationSession {
    user_id: i64,
    path: PathBuf,
    size: i64,
    mtime: i64,
    publication: Arc<Publication>,
    expires_at: Instant,
    touched_at: Instant,
}

pub struct PublicationSessions {
    inner: Mutex<HashMap<String, PublicationSession>>,
    read_slots: Arc<Semaphore>,
}

impl Default for PublicationSessions {
    fn default() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            read_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_RESOURCE_READS)),
        }
    }
}

impl PublicationSessions {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn sessions(&self) -> MutexGuard<'_, HashMap<String, PublicationSession>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn insert(&self, mut session: PublicationSession) -> String {
        let now = Instant::now();
        let mut sessions = self.sessions();
        sessions.retain(|_, existing| existing.expires_at > now);
        if sessions.len() >= MAX_SESSIONS {
            let oldest = sessions
                .iter()
                .min_by_key(|(_, existing)| existing.touched_at)
                .map(|(id, _)| id.clone());
            if let Some(oldest) = oldest {
                sessions.remove(&oldest);
            }
        }
        let id = Uuid::new_v4().to_string();
        session.expires_at = now + SESSION_TTL;
        session.touched_at = now;
        sessions.insert(id.clone(), session);
        id
    }

    fn resolve(&self, id: &str) -> Option<PublicationSession> {
        let now = Instant::now();
        let mut sessions = self.sessions();
        let session = sessions.get_mut(id)?;
        if session.expires_at <= now {
            sessions.remove(id);
            return None;
        }
        session.expires_at = now + SESSION_TTL;
        session.touched_at = now;
        Some(session.clone())
    }

    fn remove_for_user(&self, id: &str, user_id: i64) -> bool {
        let mut sessions = self.sessions();
        if sessions
            .get(id)
            .is_some_and(|session| session.user_id == user_id)
        {
            sessions.remove(id);
            true
        } else {
            false
        }
    }

    async fn read_permit(&self) -> Result<OwnedSemaphorePermit, PublicationError> {
        Arc::clone(&self.read_slots)
            .acquire_owned()
            .await
            .map_err(|_| PublicationError::Io("publication reader closed".to_owned()))
    }
}

#[derive(Serialize)]
pub struct OpenPublicationResponse {
    pub session_id: String,
    pub resource_base: String,
    pub expires_in: u64,
    pub file_id: i64,
    pub revision: RevisionDto,
    pub publication: PublicationManifest,
    pub limits: PublicationLimits,
}

#[derive(Serialize)]
pub struct PublicationLimits {
    pub entries: usize,
    pub total_uncompressed_bytes: u64,
    pub resource_bytes: u64,
    pub markup_bytes: u64,
    pub compression_ratio: u64,
    pub concurrent_resource_reads: usize,
    pub resource_chunk_bytes: usize,
}

/// POST /api/v1/files/:id/publication — parse an EPUB and mint its resource capability.
pub async fn open(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(file_id): Path<i64>,
) -> Result<Json<OpenPublicationResponse>, ApiError> {
    let file = state
        .store
        .get_file(file_id)
        .await?
        .ok_or(ApiError::NotFound("file"))?;
    let item = state
        .store
        .get_item(file.item_id)
        .await?
        .ok_or(ApiError::NotFound("item"))?;
    if item.kind != ItemKind::Book {
        return Err(ApiError::BadRequest(
            "publication sessions are only available for text books".to_owned(),
        ));
    }
    if !file
        .path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("epub"))
    {
        return Err(PublicationError::Unsupported(
            "Cinema's built-in reader currently supports EPUB files only.",
        )
        .api());
    }

    let path = file.path.clone();
    let expected_size = file.size;
    let expected_mtime = file.mtime;
    let publication = tokio::task::spawn_blocking(move || {
        verify_revision(&path, expected_size, expected_mtime)?;
        parse_epub(&path)
    })
    .await
    .map_err(|error| ApiError::Internal(error.to_string()))?
    .map_err(PublicationError::api)?;
    let publication = Arc::new(publication);
    let session_id = state.publications.insert(PublicationSession {
        user_id: user.id,
        path: file.path,
        size: file.size,
        mtime: file.mtime,
        publication: Arc::clone(&publication),
        expires_at: Instant::now(),
        touched_at: Instant::now(),
    });
    Ok(Json(OpenPublicationResponse {
        resource_base: format!("/api/v1/publication/{session_id}/"),
        session_id,
        expires_in: SESSION_TTL.as_secs(),
        file_id,
        revision: RevisionDto {
            size: file.size,
            mtime: file.mtime,
        },
        publication: publication.manifest.clone(),
        limits: PublicationLimits {
            entries: MAX_ENTRIES,
            total_uncompressed_bytes: MAX_TOTAL_UNCOMPRESSED,
            resource_bytes: MAX_RESOURCE_BYTES,
            markup_bytes: MAX_MARKUP_BYTES,
            compression_ratio: MAX_COMPRESSION_RATIO,
            concurrent_resource_reads: MAX_CONCURRENT_RESOURCE_READS,
            resource_chunk_bytes: RESOURCE_CHUNK_BYTES,
        },
    }))
}

/// DELETE /api/v1/publication/:session — close a capability before its TTL.
pub async fn close(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(session): Path<String>,
) -> StatusCode {
    if state.publications.remove_for_user(&session, user.id) {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

/// GET /api/v1/publication/:session/*resource — one bounded EPUB resource.
pub async fn resource(
    State(state): State<AppState>,
    Path((session_id, resource)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let session = state
        .publications
        .resolve(&session_id)
        .ok_or(ApiError::NotFound("publication session"))?;
    let resource = normalize_archive_path(&resource).map_err(PublicationError::api)?;
    let media_type = session
        .publication
        .resource_types
        .get(&resource)
        .cloned()
        .ok_or(ApiError::NotFound("publication resource"))?;
    let permit = state
        .publications
        .read_permit()
        .await
        .map_err(PublicationError::api)?;
    let (content_length, body) =
        stream_entry(session.path, session.size, session.mtime, resource, permit)
            .await
            .map_err(PublicationError::api)?;

    let content_type = HeaderValue::from_str(&media_type)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, content_type);
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&content_length.to_string())
            .map_err(|error| ApiError::Internal(error.to_string()))?,
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(RESOURCE_CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-origin"),
    );
    Ok(response)
}

async fn stream_entry(
    path: PathBuf,
    expected_size: i64,
    expected_mtime: i64,
    requested: String,
    permit: OwnedSemaphorePermit,
) -> Result<(u64, Body), PublicationError> {
    let (ready_tx, ready_rx) = oneshot::channel::<Result<u64, PublicationError>>();
    let (chunk_tx, chunk_rx) = mpsc::channel(RESOURCE_CHANNEL_DEPTH);
    tokio::task::spawn_blocking(move || {
        let mut ready_tx = Some(ready_tx);
        let result = (|| {
            verify_revision(&path, expected_size, expected_mtime)?;
            let mut archive = open_archive(&path)?;
            let mut entry = archive.by_name(&requested)?;
            validate_resource_entry(&entry)?;
            let content_length = entry.size();
            let Some(ready) = ready_tx.take() else {
                return Ok(());
            };
            if ready.send(Ok(content_length)).is_err() {
                return Ok(());
            }

            let mut buffer = vec![0_u8; RESOURCE_CHUNK_BYTES];
            let mut streamed = 0_u64;
            loop {
                let read = entry.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                streamed = streamed
                    .checked_add(read as u64)
                    .ok_or(PublicationError::Limit(
                        "An EPUB resource exceeds Cinema's per-resource safety limit.",
                    ))?;
                if streamed > MAX_RESOURCE_BYTES || streamed > content_length {
                    return Err(PublicationError::Invalid(
                        "An EPUB resource size disagrees with its archive directory.",
                    ));
                }
                if chunk_tx
                    .blocking_send(Ok(Bytes::copy_from_slice(&buffer[..read])))
                    .is_err()
                {
                    return Ok(());
                }
            }
            if streamed != content_length {
                return Err(PublicationError::Invalid(
                    "An EPUB resource size disagrees with its archive directory.",
                ));
            }
            Ok(())
        })();
        if let Err(error) = result {
            if let Some(ready) = ready_tx.take() {
                let _ = ready.send(Err(error));
            } else {
                tracing::warn!(
                    resource = %requested,
                    error = ?error,
                    "EPUB resource stream failed after response headers"
                );
                let _ = chunk_tx
                    .blocking_send(Err(std::io::Error::other("EPUB resource stream failed")));
            }
        }
        drop(permit);
    });

    let content_length = ready_rx
        .await
        .map_err(|_| PublicationError::Io("publication reader stopped".to_owned()))??;
    let stream = futures_util::stream::unfold(chunk_rx, |mut receiver| async move {
        receiver.recv().await.map(|chunk| (chunk, receiver))
    });
    Ok((content_length, Body::from_stream(stream)))
}

fn verify_revision(path: &FsPath, size: i64, mtime: i64) -> Result<(), PublicationError> {
    let metadata = std::fs::metadata(path).map_err(|_| PublicationError::Missing)?;
    let actual_size = i64::try_from(metadata.len()).unwrap_or(i64::MAX);
    let actual_mtime = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX));
    if actual_size != size || actual_mtime.is_some_and(|actual| actual != mtime) {
        return Err(PublicationError::Changed);
    }
    Ok(())
}

fn open_archive(path: &FsPath) -> Result<ZipArchive<File>, PublicationError> {
    Ok(ZipArchive::new(File::open(path)?)?)
}

fn parse_epub(path: &FsPath) -> Result<Publication, PublicationError> {
    let mut archive = open_archive(path)?;
    validate_archive(&mut archive)?;
    let mimetype = read_entry(&mut archive, "mimetype", 128)?;
    if std::str::from_utf8(&mimetype).ok().map(str::trim) != Some(EPUB_MIMETYPE) {
        return Err(PublicationError::Invalid(
            "The ZIP container is not a standards-conforming EPUB.",
        ));
    }
    if archive.by_name("META-INF/encryption.xml").is_ok() {
        let encryption = read_entry(&mut archive, "META-INF/encryption.xml", MAX_MARKUP_BYTES)?;
        if !only_font_obfuscation(&encryption)? {
            return Err(PublicationError::Unsupported(
                "This book is protected or encrypted and Cinema cannot open it.",
            ));
        }
    }
    let container = read_entry(&mut archive, "META-INF/container.xml", MAX_MARKUP_BYTES)?;
    let package_path = container_rootfile(&container)?;
    let package = read_entry(&mut archive, &package_path, MAX_MARKUP_BYTES)?;
    let mut parsed = parse_package(&package, &package_path)?;
    for link in parsed.reading_order.iter().chain(parsed.resources.iter()) {
        if archive.by_name(fragmentless(&link.href)).is_err() {
            return Err(PublicationError::Invalid(
                "The EPUB manifest references a missing archive resource.",
            ));
        }
    }

    let toc = if let Some(nav) = parsed.nav_path.as_deref() {
        let bytes = read_entry(&mut archive, nav, MAX_MARKUP_BYTES)?;
        parse_nav(&bytes, nav)?
    } else if let Some(ncx) = parsed.ncx_path.as_deref() {
        let bytes = read_entry(&mut archive, ncx, MAX_MARKUP_BYTES)?;
        parse_ncx(&bytes, ncx)?
    } else {
        Vec::new()
    };
    let titles = toc_titles(&toc);
    for link in &mut parsed.reading_order {
        link.title = titles.get(fragmentless(&link.href)).cloned();
    }
    let resource_types = parsed
        .resources
        .iter()
        .chain(parsed.reading_order.iter())
        .map(|link| (fragmentless(&link.href).to_owned(), link.media_type.clone()))
        .collect();
    Ok(Publication {
        manifest: PublicationManifest {
            metadata: parsed.metadata,
            reading_order: parsed.reading_order,
            resources: parsed.resources,
            toc,
        },
        resource_types,
    })
}

fn only_font_obfuscation(xml: &[u8]) -> Result<bool, PublicationError> {
    const IDPF_FONT: &str = "http://www.idpf.org/2008/embedding";
    const ADOBE_FONT: &str = "http://ns.adobe.com/pdf/enc#RC";
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut algorithms = Vec::new();
    let mut encrypted_data = 0_usize;
    loop {
        match reader.read_event() {
            Ok(Event::Start(element) | Event::Empty(element))
                if element.local_name().as_ref() == b"EncryptedData" =>
            {
                encrypted_data += 1;
            }
            Ok(Event::Start(element) | Event::Empty(element))
                if element.local_name().as_ref() == b"EncryptionMethod" =>
            {
                if let Some(algorithm) = xml_attribute(&reader, &element, b"Algorithm")? {
                    algorithms.push(algorithm);
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => {
                return Err(PublicationError::Invalid(
                    "The EPUB encryption document is malformed.",
                ));
            }
            _ => {}
        }
    }
    Ok(encrypted_data > 0
        && algorithms.len() == encrypted_data
        && algorithms
            .iter()
            .all(|algorithm| matches!(algorithm.as_str(), IDPF_FONT | ADOBE_FONT)))
}

fn validate_archive<R: Read + Seek>(archive: &mut ZipArchive<R>) -> Result<(), PublicationError> {
    if archive.len() > MAX_ENTRIES {
        return Err(PublicationError::Limit(
            "The EPUB contains too many archive entries.",
        ));
    }
    let mut names = HashSet::with_capacity(archive.len());
    let mut total = 0_u64;
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let raw_name = entry.name().trim_end_matches('/');
        let name = normalize_archive_path(raw_name)?;
        if entry.is_dir() {
            continue;
        }
        if !names.insert(name) {
            return Err(PublicationError::Invalid(
                "The EPUB contains duplicate archive paths.",
            ));
        }
        if entry.encrypted() {
            return Err(PublicationError::Unsupported(
                "This book is protected or encrypted and Cinema cannot open it.",
            ));
        }
        total = total
            .checked_add(entry.size())
            .ok_or(PublicationError::Limit("The EPUB is too large to open."))?;
        if total > MAX_TOTAL_UNCOMPRESSED {
            return Err(PublicationError::Limit(
                "The EPUB expands beyond Cinema's 1 GiB safety limit.",
            ));
        }
        let compressed = entry.compressed_size();
        if entry.size() > 0
            && (compressed == 0 || entry.size() > compressed.saturating_mul(MAX_COMPRESSION_RATIO))
        {
            return Err(PublicationError::Limit(
                "The EPUB contains a suspiciously compressed resource.",
            ));
        }
    }
    Ok(())
}

fn read_entry<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
    limit: u64,
) -> Result<Vec<u8>, PublicationError> {
    let entry = archive.by_name(name)?;
    validate_resource_entry_with_limit(&entry, limit)?;
    let capacity = usize::try_from(entry.size()).unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity);
    entry.take(limit + 1).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err(PublicationError::Limit(
            "An EPUB resource exceeds Cinema's per-resource safety limit.",
        ));
    }
    Ok(bytes)
}

fn validate_resource_entry<R: Read>(
    entry: &zip::read::ZipFile<'_, R>,
) -> Result<(), PublicationError> {
    validate_resource_entry_with_limit(entry, MAX_RESOURCE_BYTES)
}

fn validate_resource_entry_with_limit<R: Read>(
    entry: &zip::read::ZipFile<'_, R>,
    limit: u64,
) -> Result<(), PublicationError> {
    if entry.encrypted() {
        return Err(PublicationError::Unsupported(
            "This book is protected or encrypted and Cinema cannot open it.",
        ));
    }
    if entry.size() > limit {
        return Err(PublicationError::Limit(
            "An EPUB resource exceeds Cinema's per-resource safety limit.",
        ));
    }
    Ok(())
}

fn normalize_archive_path(path: &str) -> Result<String, PublicationError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.starts_with('\\')
        || path.contains('\\')
        || path.contains('\0')
        || path.chars().any(char::is_control)
    {
        return Err(PublicationError::Invalid(
            "The EPUB contains an unsafe archive path.",
        ));
    }
    let mut normalized = Vec::new();
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(PublicationError::Invalid(
                "The EPUB contains an unsafe archive path.",
            ));
        }
        if normalized.is_empty() && segment.len() == 2 && segment.ends_with(':') {
            return Err(PublicationError::Invalid(
                "The EPUB contains an unsafe archive path.",
            ));
        }
        normalized.push(segment);
    }
    Ok(normalized.join("/"))
}

fn resolve_href(base_file: &str, href: &str) -> Result<String, PublicationError> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('/') || href.starts_with('\\') {
        return Err(PublicationError::Invalid(
            "The EPUB contains an invalid publication link.",
        ));
    }
    let (without_fragment, fragment) = href
        .split_once('#')
        .map_or((href, None), |(path, fragment)| (path, Some(fragment)));
    let encoded_path = without_fragment.split('?').next().unwrap_or_default();
    let path = percent_decode_path(encoded_path)?;
    if path.contains(':') || path.contains('\\') || path.contains('\0') {
        return Err(PublicationError::Invalid(
            "The EPUB contains a remote or unsafe publication link.",
        ));
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
                    return Err(PublicationError::Invalid(
                        "A publication link escapes the EPUB container.",
                    ));
                }
            }
            value => parts.push(value),
        }
    }
    let resolved = normalize_archive_path(&parts.join("/"))?;
    if let Some(fragment) = fragment.filter(|fragment| !fragment.is_empty()) {
        Ok(format!("{resolved}#{fragment}"))
    } else {
        Ok(resolved)
    }
}

fn local_href(base_file: &str, href: &str) -> Result<Option<String>, PublicationError> {
    let candidate = href.trim().split(['#', '?']).next().unwrap_or_default();
    let first_separator = candidate.find(['/', '\\']).unwrap_or(candidate.len());
    if candidate[..first_separator].contains(':') {
        // EPUB permits declared remote resources. Cinema intentionally omits
        // them from its manifest so neither the server nor hostile markup can
        // turn opening a book into an outbound request.
        Ok(None)
    } else {
        resolve_href(base_file, href).map(Some)
    }
}

fn percent_decode_path(path: &str) -> Result<String, PublicationError> {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = bytes.get(index + 1).and_then(|value| hex_nibble(*value));
            let low = bytes.get(index + 2).and_then(|value| hex_nibble(*value));
            let (Some(high), Some(low)) = (high, low) else {
                return Err(PublicationError::Invalid(
                    "The EPUB contains an invalid percent-encoded link.",
                ));
            };
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded)
        .map_err(|_| PublicationError::Invalid("The EPUB contains a link with invalid UTF-8."))
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn xml_attribute(
    reader: &Reader<&[u8]>,
    element: &BytesStart<'_>,
    name: &[u8],
) -> Result<Option<String>, PublicationError> {
    for attribute in element.attributes().with_checks(false) {
        let attribute =
            attribute.map_err(|_| PublicationError::Invalid("The EPUB XML is malformed."))?;
        if attribute.key.local_name().as_ref() == name {
            return attribute
                .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, reader.decoder())
                .map(|value| Some(value.into_owned()))
                .map_err(|_| PublicationError::Invalid("The EPUB XML is malformed."));
        }
    }
    Ok(None)
}

fn xml_text(text: quick_xml::events::BytesText<'_>) -> Result<String, PublicationError> {
    let decoded = text
        .decode()
        .map_err(|_| PublicationError::Invalid("The EPUB XML encoding is invalid."))?;
    quick_xml::escape::unescape(&decoded)
        .map(|value| value.into_owned())
        .map_err(|_| PublicationError::Invalid("The EPUB XML entities are invalid."))
}

fn container_rootfile(xml: &[u8]) -> Result<String, PublicationError> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event() {
            Ok(Event::Start(element) | Event::Empty(element))
                if element.local_name().as_ref() == b"rootfile" =>
            {
                let path = xml_attribute(&reader, &element, b"full-path")?.ok_or(
                    PublicationError::Invalid("The EPUB container has no package document."),
                )?;
                return normalize_archive_path(&path);
            }
            Ok(Event::Eof) => break,
            Err(_) => {
                return Err(PublicationError::Invalid(
                    "The EPUB container document is malformed.",
                ));
            }
            _ => {}
        }
    }
    Err(PublicationError::Invalid(
        "The EPUB container has no package document.",
    ))
}

#[derive(Clone)]
struct ManifestItem {
    href: Option<String>,
    media_type: String,
    properties: String,
}

struct ParsedPackage {
    metadata: PublicationMetadata,
    reading_order: Vec<PublicationLink>,
    resources: Vec<PublicationLink>,
    nav_path: Option<String>,
    ncx_path: Option<String>,
}

fn parse_package(xml: &[u8], package_path: &str) -> Result<ParsedPackage, PublicationError> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut items: HashMap<String, ManifestItem> = HashMap::new();
    let mut spine = Vec::new();
    let mut spine_toc = None;
    let mut capture = None;
    let mut title = String::new();
    let mut identifier = None;
    let mut language = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(element) | Event::Empty(element)) => {
                match element.local_name().as_ref() {
                    b"item" => {
                        let id = xml_attribute(&reader, &element, b"id")?.unwrap_or_default();
                        let href = xml_attribute(&reader, &element, b"href")?.unwrap_or_default();
                        let media_type =
                            xml_attribute(&reader, &element, b"media-type")?.unwrap_or_default();
                        if id.is_empty() || href.is_empty() || media_type.is_empty() {
                            return Err(PublicationError::Invalid(
                                "The EPUB package manifest has an incomplete item.",
                            ));
                        }
                        let href = local_href(package_path, &href)?;
                        if items
                            .insert(
                                id,
                                ManifestItem {
                                    href,
                                    media_type,
                                    properties: xml_attribute(&reader, &element, b"properties")?
                                        .unwrap_or_default(),
                                },
                            )
                            .is_some()
                        {
                            return Err(PublicationError::Invalid(
                                "The EPUB package manifest contains a duplicate id.",
                            ));
                        }
                    }
                    b"itemref" => {
                        if let Some(idref) = xml_attribute(&reader, &element, b"idref")? {
                            if xml_attribute(&reader, &element, b"linear")?.as_deref() != Some("no")
                            {
                                spine.push(idref);
                            }
                        }
                    }
                    b"spine" => spine_toc = xml_attribute(&reader, &element, b"toc")?,
                    b"title" | b"identifier" | b"language" => {
                        capture = Some(element.local_name().as_ref().to_vec())
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(text)) => {
                let value = xml_text(text)?.trim().to_owned();
                match capture.as_deref() {
                    Some(b"title") if !value.is_empty() => title.push_str(&value),
                    Some(b"identifier") if !value.is_empty() && identifier.is_none() => {
                        identifier = Some(value)
                    }
                    Some(b"language") if !value.is_empty() && language.is_none() => {
                        language = Some(value)
                    }
                    _ => {}
                }
            }
            Ok(Event::End(element))
                if capture.as_deref() == Some(element.local_name().as_ref()) =>
            {
                capture = None;
            }
            Ok(Event::Eof) => break,
            Err(_) => {
                return Err(PublicationError::Invalid(
                    "The EPUB package document is malformed.",
                ));
            }
            _ => {}
        }
    }
    if title.trim().is_empty() || spine.is_empty() {
        return Err(PublicationError::Invalid(
            "The EPUB package needs a title and a non-empty reading order.",
        ));
    }

    let mut reading_order = Vec::with_capacity(spine.len());
    let mut reading_ids = HashSet::new();
    for id in spine {
        let item = items.get(&id).ok_or(PublicationError::Invalid(
            "The EPUB spine references a missing manifest item.",
        ))?;
        let href = item.href.as_ref().ok_or(PublicationError::Unsupported(
            "Cinema cannot open a book whose reading order is remote.",
        ))?;
        reading_ids.insert(id);
        reading_order.push(PublicationLink {
            href: href.clone(),
            media_type: item.media_type.clone(),
            title: None,
        });
    }
    let nav_path = items
        .values()
        .find(|item| {
            item.properties
                .split_whitespace()
                .any(|value| value == "nav")
        })
        .and_then(|item| item.href.as_deref())
        .map(|href| fragmentless(href).to_owned());
    let ncx_path = spine_toc
        .as_ref()
        .and_then(|id| items.get(id))
        .or_else(|| {
            items
                .values()
                .find(|item| item.media_type == "application/x-dtbncx+xml")
        })
        .and_then(|item| item.href.as_deref())
        .map(|href| fragmentless(href).to_owned());
    let resources = items
        .into_iter()
        .filter(|(id, _)| !reading_ids.contains(id))
        .filter_map(|(_, item)| {
            item.href.map(|href| PublicationLink {
                href,
                media_type: item.media_type,
                title: None,
            })
        })
        .collect();
    Ok(ParsedPackage {
        metadata: PublicationMetadata {
            title: title.trim().to_owned(),
            identifier,
            language,
        },
        reading_order,
        resources,
        nav_path,
        ncx_path,
    })
}

#[derive(Default)]
struct TocBuilder {
    href: Option<String>,
    title: String,
    children: Vec<TocLink>,
}

impl TocBuilder {
    fn finish(self) -> Option<TocLink> {
        let href = self.href?;
        let title = self.title.split_whitespace().collect::<Vec<_>>().join(" ");
        (!title.is_empty()).then_some(TocLink {
            href,
            title,
            children: self.children,
        })
    }
}

fn parse_nav(xml: &[u8], nav_path: &str) -> Result<Vec<TocLink>, PublicationError> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut in_toc = false;
    let mut nav_depth = 0_usize;
    let mut stack: Vec<TocBuilder> = Vec::new();
    let mut roots = Vec::new();
    let mut in_anchor = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => match element.local_name().as_ref() {
                b"nav" if !in_toc => {
                    let kind = xml_attribute(&reader, &element, b"type")?.unwrap_or_default();
                    if kind.split_whitespace().any(|value| value == "toc") {
                        in_toc = true;
                        nav_depth = 1;
                    }
                }
                b"nav" if in_toc => nav_depth += 1,
                b"li" if in_toc => stack.push(TocBuilder::default()),
                b"a" if in_toc && !stack.is_empty() => {
                    if let Some(href) = xml_attribute(&reader, &element, b"href")? {
                        if let Some(entry) = stack.last_mut() {
                            entry.href = local_href(nav_path, &href)?;
                        }
                    }
                    in_anchor = true;
                }
                _ => {}
            },
            Ok(Event::Text(text)) if in_toc && in_anchor => {
                if let Some(entry) = stack.last_mut() {
                    entry.title.push_str(&xml_text(text)?);
                    entry.title.push(' ');
                }
            }
            Ok(Event::End(element)) => match element.local_name().as_ref() {
                b"a" if in_toc => in_anchor = false,
                b"li" if in_toc => {
                    if let Some(link) = stack.pop().and_then(TocBuilder::finish) {
                        if let Some(parent) = stack.last_mut() {
                            parent.children.push(link);
                        } else {
                            roots.push(link);
                        }
                    }
                }
                b"nav" if in_toc => {
                    nav_depth = nav_depth.saturating_sub(1);
                    if nav_depth == 0 {
                        break;
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => {
                return Err(PublicationError::Invalid(
                    "The EPUB navigation document is malformed.",
                ));
            }
            _ => {}
        }
    }
    Ok(roots)
}

fn parse_ncx(xml: &[u8], ncx_path: &str) -> Result<Vec<TocLink>, PublicationError> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut stack: Vec<TocBuilder> = Vec::new();
    let mut roots = Vec::new();
    let mut in_label = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => match element.local_name().as_ref() {
                b"navPoint" => stack.push(TocBuilder::default()),
                b"text" if !stack.is_empty() => in_label = true,
                b"content" if !stack.is_empty() => {
                    if let Some(src) = xml_attribute(&reader, &element, b"src")? {
                        if let Some(entry) = stack.last_mut() {
                            entry.href = local_href(ncx_path, &src)?;
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::Empty(element)) if element.local_name().as_ref() == b"content" => {
                if let (Some(entry), Some(src)) =
                    (stack.last_mut(), xml_attribute(&reader, &element, b"src")?)
                {
                    entry.href = local_href(ncx_path, &src)?;
                }
            }
            Ok(Event::Text(text)) if in_label => {
                if let Some(entry) = stack.last_mut() {
                    entry.title.push_str(&xml_text(text)?);
                    entry.title.push(' ');
                }
            }
            Ok(Event::End(element)) => match element.local_name().as_ref() {
                b"text" => in_label = false,
                b"navPoint" => {
                    if let Some(link) = stack.pop().and_then(TocBuilder::finish) {
                        if let Some(parent) = stack.last_mut() {
                            parent.children.push(link);
                        } else {
                            roots.push(link);
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => {
                return Err(PublicationError::Invalid(
                    "The EPUB NCX document is malformed.",
                ));
            }
            _ => {}
        }
    }
    Ok(roots)
}

fn fragmentless(href: &str) -> &str {
    href.split('#').next().unwrap_or(href)
}

fn toc_titles(toc: &[TocLink]) -> HashMap<&str, String> {
    fn visit<'a>(links: &'a [TocLink], titles: &mut HashMap<&'a str, String>) {
        for link in links {
            titles
                .entry(fragmentless(&link.href))
                .or_insert_with(|| link.title.clone());
            visit(&link.children, titles);
        }
    }
    let mut titles = HashMap::new();
    visit(toc, &mut titles);
    titles
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use http_body_util::BodyExt;
    use tempfile::NamedTempFile;
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    use super::*;

    fn fixture(entries: &[(&str, &str)]) -> NamedTempFile {
        let file = NamedTempFile::new().expect("fixture file");
        let mut writer = ZipWriter::new(file.reopen().expect("fixture writer"));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, body) in entries {
            writer.start_file(*name, options).expect("fixture entry");
            writer.write_all(body.as_bytes()).expect("fixture bytes");
        }
        writer.finish().expect("finish fixture");
        file
    }

    fn base_entries() -> Vec<(&'static str, &'static str)> {
        vec![
            ("mimetype", EPUB_MIMETYPE),
            (
                "META-INF/container.xml",
                r#"<?xml version="1.0"?><container><rootfiles><rootfile full-path="OEBPS/book.opf"/></rootfiles></container>"#,
            ),
            (
                "OEBPS/book.opf",
                r#"<?xml version="1.0"?><package><metadata><title>Proof Book</title><identifier>urn:proof</identifier><language>en</language></metadata><manifest><item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/><item id="one" href="Text/one.xhtml" media-type="application/xhtml+xml"/><item id="two" href="Text/two.xhtml" media-type="application/xhtml+xml"/><item id="img" href="Images/cover.jpg" media-type="image/jpeg"/></manifest><spine><itemref idref="one"/><itemref idref="two"/></spine></package>"#,
            ),
            (
                "OEBPS/nav.xhtml",
                r#"<html xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="Text/one.xhtml#start">One</a><ol><li><a href="Text/two.xhtml">Two nested</a></li></ol></li></ol></nav></body></html>"#,
            ),
            ("OEBPS/Text/one.xhtml", "<html><body>One</body></html>"),
            ("OEBPS/Text/two.xhtml", "<html><body>Two</body></html>"),
            ("OEBPS/Images/cover.jpg", "not-a-real-jpeg"),
        ]
    }

    #[test]
    fn epub3_nav_produces_spine_and_authored_toc() {
        let file = fixture(&base_entries());
        let publication = parse_epub(file.path()).expect("valid EPUB 3");
        assert_eq!(publication.manifest.metadata.title, "Proof Book");
        assert_eq!(publication.manifest.reading_order.len(), 2);
        assert_eq!(
            publication.manifest.reading_order[0].href,
            "OEBPS/Text/one.xhtml"
        );
        assert_eq!(publication.manifest.toc[0].title, "One");
        assert_eq!(publication.manifest.toc[0].children[0].title, "Two nested");
    }

    #[test]
    fn declared_remote_resources_are_omitted_not_fetched() {
        let mut entries = base_entries();
        entries[2] = (
            "OEBPS/book.opf",
            r#"<package><metadata><title>Remote Proof</title></metadata><manifest><item id="one" href="Text/one.xhtml" media-type="application/xhtml+xml"/><item id="tracker" href="https://example.com/tracker.png" media-type="image/png"/></manifest><spine><itemref idref="one"/></spine></package>"#,
        );
        let file = fixture(&entries);
        let publication = parse_epub(file.path()).expect("remote resource is optional");
        assert!(publication
            .manifest
            .resources
            .iter()
            .all(|link| !link.href.starts_with("http")));
    }

    #[test]
    fn epub2_ncx_produces_authored_toc() {
        let mut entries = base_entries();
        entries[2] = (
            "OEBPS/book.opf",
            r#"<package><metadata><title>EPUB 2</title></metadata><manifest><item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/><item id="one" href="Text/one.xhtml" media-type="application/xhtml+xml"/></manifest><spine toc="ncx"><itemref idref="one"/></spine></package>"#,
        );
        entries[3] = (
            "OEBPS/toc.ncx",
            r#"<ncx><navMap><navPoint><navLabel><text>Chapter One</text></navLabel><content src="Text/one.xhtml#top"/></navPoint></navMap></ncx>"#,
        );
        let file = fixture(&entries);
        let publication = parse_epub(file.path()).expect("valid EPUB 2");
        assert_eq!(publication.manifest.toc[0].title, "Chapter One");
        assert_eq!(publication.manifest.toc[0].href, "OEBPS/Text/one.xhtml#top");
    }

    #[test]
    fn unsafe_archive_paths_fail_closed() {
        for name in ["../outside", "/absolute", "C:/windows", "a\\b"] {
            assert!(normalize_archive_path(name).is_err(), "accepted {name}");
        }
        let mut entries = base_entries();
        entries.push(("../outside", "secret"));
        let file = fixture(&entries);
        assert!(matches!(
            parse_epub(file.path()),
            Err(PublicationError::Invalid(_))
        ));
    }

    #[test]
    fn suspiciously_compressed_entries_fail_closed() {
        let file = NamedTempFile::new().expect("compression fixture");
        let mut writer = ZipWriter::new(file.reopen().expect("compression writer"));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, body) in base_entries() {
            writer.start_file(name, options).expect("base entry");
            writer.write_all(body.as_bytes()).expect("base bytes");
        }
        writer
            .start_file("OEBPS/bomb.bin", options)
            .expect("compressed entry");
        std::io::copy(&mut std::io::repeat(0).take(4 * 1024 * 1024), &mut writer)
            .expect("compressed bytes");
        writer.finish().expect("finish compression fixture");
        assert!(matches!(
            parse_epub(file.path()),
            Err(PublicationError::Limit(_))
        ));
    }

    #[test]
    fn a_missing_declared_resource_fails_before_session_issue() {
        let mut entries = base_entries();
        entries.retain(|(name, _)| *name != "OEBPS/Text/two.xhtml");
        let file = fixture(&entries);
        assert!(matches!(
            parse_epub(file.path()),
            Err(PublicationError::Invalid(_))
        ));
    }

    #[test]
    fn publication_links_may_climb_within_but_not_outside_container() {
        assert_eq!(
            resolve_href("OEBPS/Text/chapter.xhtml", "../Images/map.png#large")
                .expect("contained parent"),
            "OEBPS/Images/map.png#large"
        );
        assert!(resolve_href("book.opf", "../secret").is_err());
        assert!(resolve_href("OEBPS/book.opf", "https://example.com/leak").is_err());
        assert_eq!(
            resolve_href("OEBPS/book.opf", "Text/chapter%201.xhtml")
                .expect("percent-encoded space"),
            "OEBPS/Text/chapter 1.xhtml"
        );
        assert!(resolve_href("OEBPS/book.opf", "%2e%2e/%2e%2e/secret").is_err());
    }

    #[test]
    fn protected_publication_is_not_mislabeled_as_corrupt() {
        let mut entries = base_entries();
        entries.push(("META-INF/encryption.xml", "<encryption/>"));
        let file = fixture(&entries);
        assert!(matches!(
            parse_epub(file.path()),
            Err(PublicationError::Unsupported(_))
        ));
    }

    #[test]
    fn standard_font_obfuscation_does_not_mislabel_a_drm_free_book() {
        let mut entries = base_entries();
        entries.push((
            "META-INF/encryption.xml",
            r#"<encryption><EncryptedData><EncryptionMethod Algorithm="http://www.idpf.org/2008/embedding"/></EncryptedData></encryption>"#,
        ));
        let file = fixture(&entries);
        parse_epub(file.path()).expect("font obfuscation is not DRM");
    }

    #[test]
    fn every_encrypted_payload_needs_its_own_allowed_font_method() {
        let mut entries = base_entries();
        entries.push((
            "META-INF/encryption.xml",
            r#"<encryption><EncryptedData><EncryptionMethod Algorithm="http://www.idpf.org/2008/embedding"/></EncryptedData><EncryptedData/></encryption>"#,
        ));
        let file = fixture(&entries);
        assert!(matches!(
            parse_epub(file.path()),
            Err(PublicationError::Unsupported(_))
        ));
    }

    #[test]
    fn publication_sessions_cannot_grow_the_registry_without_bound() {
        let sessions = PublicationSessions::default();
        let publication = Arc::new(Publication {
            manifest: PublicationManifest {
                metadata: PublicationMetadata {
                    title: "Bounded".to_owned(),
                    identifier: None,
                    language: None,
                },
                reading_order: Vec::new(),
                resources: Vec::new(),
                toc: Vec::new(),
            },
            resource_types: HashMap::new(),
        });
        for index in 0..(MAX_SESSIONS + 10) {
            sessions.insert(PublicationSession {
                user_id: 1,
                path: PathBuf::from(format!("/book-{index}.epub")),
                size: 1,
                mtime: 1,
                publication: Arc::clone(&publication),
                expires_at: Instant::now(),
                touched_at: Instant::now(),
            });
        }
        assert_eq!(sessions.sessions().len(), MAX_SESSIONS);
    }

    #[test]
    fn publication_resource_readers_cannot_exceed_the_node_cap() {
        let sessions = PublicationSessions::default();
        let mut permits = Vec::new();
        for _ in 0..MAX_CONCURRENT_RESOURCE_READS {
            permits.push(
                Arc::clone(&sessions.read_slots)
                    .try_acquire_owned()
                    .expect("resource read slot"),
            );
        }
        assert!(Arc::clone(&sessions.read_slots)
            .try_acquire_owned()
            .is_err());
        permits.pop();
        assert!(Arc::clone(&sessions.read_slots).try_acquire_owned().is_ok());
    }

    /// Nightly/manual proof for the scale boundary. The 500 MiB entry is
    /// stored rather than compressed so the archive is honest. Opening reads
    /// only the central directory/package/navigation documents; a separate
    /// 120 MiB child is then drained through bounded HTTP chunks, while a
    /// direct read of the 500 MiB child is refused by the 128 MiB cap.
    #[tokio::test]
    #[ignore = "writes a 620 MiB temporary EPUB for the memory-bound proof"]
    async fn epub_manifest_does_not_expand_large_resource() {
        const LARGE_BYTES: u64 = 500 * 1024 * 1024;
        const STREAMED_BYTES: u64 = 120 * 1024 * 1024;
        const RSS_CEILING: u64 = 256 * 1024 * 1024;
        let file = NamedTempFile::new().expect("large fixture file");
        let mut writer = ZipWriter::new(file.reopen().expect("large fixture writer"));
        let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, body) in [
            ("mimetype", EPUB_MIMETYPE),
            (
                "META-INF/container.xml",
                r#"<container><rootfiles><rootfile full-path="OEBPS/book.opf"/></rootfiles></container>"#,
            ),
            (
                "OEBPS/book.opf",
                r#"<package><metadata><title>Large Proof</title></metadata><manifest><item id="chapter" href="chapter.xhtml" media-type="application/xhtml+xml"/><item id="sample" href="sample.bin" media-type="application/octet-stream"/><item id="plate" href="plate.bin" media-type="application/octet-stream"/></manifest><spine><itemref idref="chapter"/></spine></package>"#,
            ),
            ("OEBPS/chapter.xhtml", "<html><body>Proof</body></html>"),
        ] {
            writer.start_file(name, deflated).expect("metadata entry");
            writer.write_all(body.as_bytes()).expect("metadata bytes");
        }
        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        writer
            .start_file("OEBPS/sample.bin", stored)
            .expect("streamed entry");
        std::io::copy(&mut std::io::repeat(1).take(STREAMED_BYTES), &mut writer)
            .expect("streamed stored bytes");
        writer
            .start_file("OEBPS/plate.bin", stored)
            .expect("large entry");
        std::io::copy(&mut std::io::repeat(0).take(LARGE_BYTES), &mut writer)
            .expect("large stored bytes");
        writer.finish().expect("finish large EPUB");

        let publication = parse_epub(file.path()).expect("open large EPUB manifest");
        assert_eq!(publication.manifest.metadata.title, "Large Proof");
        let metadata = file.as_file().metadata().expect("large EPUB metadata");
        let size = i64::try_from(metadata.len()).expect("large EPUB size");
        let mtime = metadata
            .modified()
            .expect("large EPUB mtime")
            .duration_since(std::time::UNIX_EPOCH)
            .expect("large EPUB after epoch")
            .as_secs() as i64;
        let sessions = PublicationSessions::default();
        let permit = sessions.read_permit().await.expect("resource read permit");
        let (content_length, mut body) = stream_entry(
            file.path().to_path_buf(),
            size,
            mtime,
            "OEBPS/sample.bin".to_owned(),
            permit,
        )
        .await
        .expect("stream bounded child");
        assert_eq!(content_length, STREAMED_BYTES);
        let mut streamed = 0_u64;
        while let Some(frame) = body.frame().await {
            let frame = frame.expect("streamed resource frame");
            if let Ok(bytes) = frame.into_data() {
                assert!(bytes.len() <= RESOURCE_CHUNK_BYTES);
                streamed += u64::try_from(bytes.len()).expect("chunk size");
            }
        }
        assert_eq!(streamed, STREAMED_BYTES);
        let peak_rss = peak_rss_bytes();
        eprintln!("620 MiB EPUB + 120 MiB streamed child peak RSS: {peak_rss} bytes");
        assert!(
            peak_rss < RSS_CEILING,
            "opening a 500 MiB EPUB used {peak_rss} bytes of RSS"
        );
        let mut archive = open_archive(file.path()).expect("reopen large EPUB");
        assert!(matches!(
            read_entry(&mut archive, "OEBPS/plate.bin", MAX_RESOURCE_BYTES),
            Err(PublicationError::Limit(_))
        ));
    }

    #[cfg(unix)]
    fn peak_rss_bytes() -> u64 {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        // SAFETY: getrusage initializes the pointed-to rusage when it returns
        // zero; the pointer is valid and uniquely borrowed for the call.
        let status = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
        assert_eq!(status, 0, "getrusage failed");
        // SAFETY: the successful getrusage call above initialized every field.
        let usage = unsafe { usage.assume_init() };
        let raw = u64::try_from(usage.ru_maxrss).unwrap_or(u64::MAX);
        if cfg!(target_os = "macos") {
            raw
        } else {
            raw.saturating_mul(1024)
        }
    }

    #[cfg(not(unix))]
    fn peak_rss_bytes() -> u64 {
        0
    }
}
