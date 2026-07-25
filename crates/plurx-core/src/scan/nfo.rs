//! Kodi-style `.nfo` sidecar parsing for home libraries.
//!
//! The NFO is a **one-time seed, not a data backend** (docs/HOMEVIDEO-PLAN.md
//! §1): the scanner reads `<basename>.nfo` the first time it ingests a video,
//! builds the item's metadata from it, and then never looks at the file
//! again. plurx never writes one — ARCHITECTURE §8's "plurx never writes to
//! media storage" holds byte for byte.
//!
//! The dialect is Kodi's `<movie>` element, which Jellyfin, Emby and
//! tinyMediaManager all read and write, so existing tooling can author the
//! sidecars plurx consumes. Parsing is deliberately lenient: real-world NFOs
//! are filthy — a bare IMDB URL in a file named `.nfo` is a genre unto itself
//! — and a broken sidecar must never fail a scan.

use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;

/// What a sidecar told us. Every field is optional: an NFO carrying only
/// `<title>` is perfectly normal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Nfo {
    pub title: Option<String>,
    pub overview: Option<String>,
    /// From `<premiered>`, else `<aired>`. Kept verbatim when it looks like a
    /// date (`YYYY-MM-DD` or a full ISO datetime); junk is dropped.
    pub recorded_at: Option<String>,
    /// From `<year>`, else derived from [`Nfo::recorded_at`].
    pub year: Option<i32>,
    /// `<tag>` and `<genre>` folded together — home video has tags, not
    /// genres. De-duplicated, order preserved.
    pub tags: Vec<String>,
}

impl Nfo {
    /// True when the file parsed but said nothing we use. Still counts as
    /// consumed — seeding happens at most once either way.
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.overview.is_none()
            && self.recorded_at.is_none()
            && self.year.is_none()
            && self.tags.is_empty()
    }
}

/// Elements we read. Everything else is ignored without complaint — a Kodi
/// NFO carries dozens of tags we have no use for. Deliberately ignored, with
/// the reason:
/// - `<fileinfo>` / `<streamdetails>`: ffprobe is ground truth for media
///   detail (ARCHITECTURE §7 decision 5). A sidecar's idea of the codec is
///   hearsay.
/// - `<actor>` / `<director>`: there is no people model in v1, and a people
///   model deserves design rather than a JSON column smuggled in here.
/// - ratings, ids, artwork paths: no providers run against home libraries.
fn wanted(tag: &str) -> bool {
    matches!(
        tag,
        "title" | "plot" | "premiered" | "aired" | "year" | "tag" | "genre"
    )
}

/// A date we're willing to store: `YYYY-MM-DD`, optionally with a time. We
/// only ever sort on it, so anything else is noise.
fn looks_like_date(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() < 10 {
        return false;
    }
    let digits_at = |i: usize| bytes[i].is_ascii_digit();
    (0..4).all(digits_at)
        && bytes[4] == b'-'
        && digits_at(5)
        && digits_at(6)
        && bytes[7] == b'-'
        && digits_at(8)
        && digits_at(9)
        && (bytes.len() == 10 || bytes[10] == b'T' || bytes[10] == b' ')
}

/// Parse NFO text. `None` means "not a Kodi `<movie>` sidecar" — not XML at
/// all, empty, or a different root element. The caller records one scan
/// problem and marks the item seeded anyway (§4.3): a broken sidecar should
/// complain once, not on every scan.
pub fn parse(text: &str) -> Option<Nfo> {
    let mut reader = Reader::from_str(text);
    let config = reader.config_mut();
    config.trim_text(true);
    config.check_end_names = false;

    let mut nfo = Nfo::default();
    let mut depth = 0usize;
    let mut in_movie = false;
    // Which of the wanted elements we're inside, if any. Only direct children
    // of <movie> count — a <title> nested in <actor> is not the movie's title.
    let mut field: Option<String> = None;
    let mut seen_root = false;
    let mut premiered: Option<String> = None;
    let mut aired: Option<String> = None;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).to_lowercase();
                depth += 1;
                if depth == 1 {
                    seen_root = true;
                    in_movie = name == "movie";
                    if !in_movie {
                        return None;
                    }
                } else if depth == 2 && in_movie && wanted(&name) {
                    field = Some(name);
                }
            }
            Ok(Event::End(_)) => {
                if depth == 2 {
                    field = None;
                }
                depth = depth.saturating_sub(1);
            }
            Ok(Event::Empty(e)) => {
                // <movie/> is a valid, if useless, sidecar.
                if depth == 0 {
                    seen_root = true;
                    if String::from_utf8_lossy(e.local_name().as_ref()).to_lowercase() != "movie" {
                        return None;
                    }
                    in_movie = true;
                }
            }
            Ok(Event::Text(_)) | Ok(Event::CData(_)) if field.is_none() => {}
            Ok(event @ (Event::Text(_) | Event::CData(_))) => {
                let Some(name) = field.as_deref() else {
                    continue;
                };
                let raw = match &event {
                    // Text is entity-decoded; CDATA is verbatim by definition.
                    Event::Text(t) => t.decode().map(|s| s.into_owned()).unwrap_or_default(),
                    Event::CData(c) => String::from_utf8_lossy(c.as_ref()).into_owned(),
                    _ => String::new(),
                };
                let value = raw.trim().to_owned();
                if value.is_empty() {
                    continue;
                }
                match name {
                    "title" => nfo.title = Some(value),
                    "plot" => nfo.overview = Some(value),
                    "premiered" if looks_like_date(&value) => premiered = Some(value),
                    "aired" if looks_like_date(&value) => aired = Some(value),
                    "year" => nfo.year = value.parse().ok(),
                    "tag" | "genre" if !nfo.tags.iter().any(|t| t.eq_ignore_ascii_case(&value)) => {
                        nfo.tags.push(value);
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            // Malformed markup: keep whatever we understood so far rather
            // than throwing away a mostly-good file.
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    if !seen_root || !in_movie {
        return None;
    }
    // <premiered> wins; <aired> is the fallback.
    nfo.recorded_at = premiered.or(aired);
    if nfo.year.is_none() {
        nfo.year = nfo
            .recorded_at
            .as_deref()
            .and_then(|d| d.get(..4))
            .and_then(|y| y.parse().ok());
    }
    Some(nfo)
}

/// Read and parse a sidecar. `None` covers both "unreadable" and "not a
/// usable `<movie>` NFO" — the caller treats them the same. Encoding junk is
/// tolerated (lossy UTF-8), because plenty of NFOs in the wild are Latin-1
/// with a BOM and a shrug.
pub fn read(path: &Path) -> Option<Nfo> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    parse(text.trim_start_matches('\u{feff}'))
}

/// The sidecar path for a media file: same directory, same stem, `.nfo`.
/// Returns the path only if it exists — the extension match is
/// case-insensitive, as Windows-authored libraries are full of `.NFO`.
pub fn sidecar_for(media: &Path) -> Option<std::path::PathBuf> {
    let stem = media.file_stem()?;
    let dir = media.parent()?;
    for ext in ["nfo", "NFO", "Nfo"] {
        let candidate = dir.join(stem).with_extension(ext);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_nfo_maps_every_supported_field() {
        let nfo = parse(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
            <movie>
                <title>Beach day</title>
                <plot>The kids discovered crabs.</plot>
                <premiered>2019-06-14</premiered>
                <year>2019</year>
                <tag>beach</tag>
                <tag>kids</tag>
                <genre>Family</genre>
                <runtime>12</runtime>
                <fileinfo><streamdetails><video><codec>h264</codec></video></streamdetails></fileinfo>
                <actor><name>Someone</name><role>Self</role></actor>
            </movie>"#,
        )
        .expect("parses");
        assert_eq!(nfo.title.as_deref(), Some("Beach day"));
        assert_eq!(nfo.overview.as_deref(), Some("The kids discovered crabs."));
        assert_eq!(nfo.recorded_at.as_deref(), Some("2019-06-14"));
        assert_eq!(nfo.year, Some(2019));
        assert_eq!(nfo.tags, vec!["beach", "kids", "Family"]);
    }

    #[test]
    fn minimal_nfo_is_just_a_title() {
        let nfo = parse("<movie><title>Christmas 2019</title></movie>").expect("parses");
        assert_eq!(nfo.title.as_deref(), Some("Christmas 2019"));
        assert_eq!(nfo.recorded_at, None);
        assert_eq!(nfo.year, None);
        assert!(nfo.tags.is_empty());
    }

    #[test]
    fn aired_is_the_fallback_and_seeds_the_year() {
        let nfo = parse("<movie><aired>2004-08-02</aired></movie>").expect("parses");
        assert_eq!(nfo.recorded_at.as_deref(), Some("2004-08-02"));
        assert_eq!(nfo.year, Some(2004), "year derives from the date");

        // <premiered> wins when both are present, whatever the order.
        let nfo =
            parse("<movie><aired>2004-08-02</aired><premiered>2004-07-01</premiered></movie>")
                .expect("parses");
        assert_eq!(nfo.recorded_at.as_deref(), Some("2004-07-01"));
    }

    #[test]
    fn tags_and_genres_fold_together_without_duplicates() {
        let nfo = parse(
            "<movie><tag>Beach</tag><genre>beach</genre><genre>holiday</genre>
             <tag>holiday</tag></movie>",
        )
        .expect("parses");
        assert_eq!(nfo.tags, vec!["Beach", "holiday"]);
    }

    #[test]
    fn a_bare_url_file_is_not_an_nfo() {
        // The classic: scrapers drop the IMDB link in a .nfo and call it a day.
        assert_eq!(parse("https://www.imdb.com/title/tt0133093/"), None);
    }

    #[test]
    fn malformed_xml_keeps_what_it_understood() {
        // Truncated mid-file (a copy that died): the parser stops, it does not
        // panic and does not discard the fields it already read.
        let nfo = parse("<movie><title>Half a file</title><plot>Cut off").expect("parses");
        assert_eq!(nfo.title.as_deref(), Some("Half a file"));
    }

    #[test]
    fn empty_and_wrong_root_files_are_none() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("   \n  "), None);
        assert_eq!(parse("<tvshow><title>Nope</title></tvshow>"), None);
    }

    #[test]
    fn unknown_tag_soup_parses_what_it_knows() {
        let nfo = parse(
            "<movie><uniqueid type=\"imdb\">tt1</uniqueid><ratings><rating><value>7</value>
             </rating></ratings><title>Known</title><thumb>poster.jpg</thumb>
             <premiered>not a date</premiered></movie>",
        )
        .expect("parses");
        assert_eq!(nfo.title.as_deref(), Some("Known"));
        assert_eq!(nfo.recorded_at, None, "a junk date is dropped, not stored");
        assert!(nfo.tags.is_empty());
    }

    #[test]
    fn nested_title_does_not_become_the_movie_title() {
        let nfo = parse(
            "<movie><actor><name>Someone</name><title>Not the movie</title></actor>
             <title>The movie</title></movie>",
        )
        .expect("parses");
        assert_eq!(nfo.title.as_deref(), Some("The movie"));
    }

    #[test]
    fn empty_movie_element_parses_to_nothing() {
        let nfo = parse("<movie/>").expect("parses");
        assert!(nfo.is_empty());
    }

    #[test]
    fn reads_a_file_with_a_bom_and_finds_its_sidecar() {
        let dir = tempfile::tempdir().expect("tempdir");
        let video = dir.path().join("Beach.mp4");
        std::fs::write(&video, b"x").expect("write video");
        assert_eq!(sidecar_for(&video), None);

        let sidecar = dir.path().join("Beach.nfo");
        std::fs::write(
            &sidecar,
            "\u{feff}<movie><title>Beach</title></movie>".as_bytes(),
        )
        .expect("write nfo");
        assert_eq!(sidecar_for(&video).as_deref(), Some(sidecar.as_path()));
        let nfo = read(&sidecar).expect("reads");
        assert_eq!(nfo.title.as_deref(), Some("Beach"));

        assert_eq!(read(&dir.path().join("missing.nfo")), None);
    }
}
