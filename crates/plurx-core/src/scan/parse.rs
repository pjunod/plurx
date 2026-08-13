//! Filename → identity parsing.
//!
//! Honors the Plex/Jellyfin naming conventions most libraries already use:
//! `Title (Year)/Title (Year).ext` for movies and
//! `Show (Year)/Season NN/Show - S01E02 - Episode.ext` for TV, while also
//! tolerating scene-style dotted names (`Show.Name.S01E02.1080p.WEB.x265.ext`).
//!
//! The library kind decides which parser runs, so a movies library never
//! second-guesses a file as an episode and vice versa.

use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

/// Tokens that mark the end of a real title in a scene-style name. Everything
/// from the first of these onward is release cruft, not part of the title.
const STOP_TOKENS: &[&str] = &[
    "1080p",
    "720p",
    "480p",
    "2160p",
    "4k",
    "uhd",
    "bluray",
    "blu-ray",
    "bdrip",
    "brrip",
    "web",
    "web-dl",
    "webdl",
    "webrip",
    "hdtv",
    "dvdrip",
    "dvd",
    "remux",
    "x264",
    "x265",
    "h264",
    "h265",
    "hevc",
    "avc",
    "av1",
    "xvid",
    "divx",
    "aac",
    "ac3",
    "eac3",
    "dts",
    "dts-hd",
    "truehd",
    "atmos",
    "ddp",
    "dd5",
    "flac",
    "hdr",
    "hdr10",
    "hdr10+",
    "dv",
    "dovi",
    "dolby",
    "vision",
    "sdr",
    "10bit",
    "8bit",
    "hi10p",
    "proper",
    "repack",
    "internal",
    "limited",
    "extended",
    "unrated",
    "remastered",
    "imax",
];

static YEAR_PAREN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\((19\d{2}|20\d{2})\)").expect("valid regex"));
// Word-boundary (not separator-consuming) so consecutive years like
// "Blade.Runner.2049.2017" both match and the LAST wins as the release year.
static YEAR_BARE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(19\d{2}|20\d{2})\b").expect("valid"));
// S01E02 / s1e2 / S01E02E03 (multi), and the 1x02 style.
static SXXEYY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bs(\d{1,3})[\s._-]*e(\d{1,4})(?:[\s._-]*e\d{1,4})*\b").expect("valid")
});
static NX_NN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(\d{1,3})x(\d{1,4})\b").expect("valid"));
static SEASON_DIR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:season|series)[\s._-]*(\d{1,3})$").expect("valid"));
// Anime absolute numbering: "Title - 01", "Title - 12v2", "Title - 100".
static ANIME_EP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s-\s(\d{1,4})(?:v\d+)?(?:\s|\.|\[|\(|$)").expect("valid"));
// The crammed DVD-era form: "Drawn.Together.102" is S01E02. Exactly three
// digits standing alone — the boundaries keep it out of "1080p", "x264" and
// "480p", where the digits touch another word character on one side.
static SEE_CRAMMED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([1-9])(\d{2})\b").expect("valid"));

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMovie {
    pub title: String,
    pub year: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedEpisode {
    pub show_title: String,
    pub show_year: Option<i32>,
    pub season: i32,
    pub episode: i32,
    pub episode_title: Option<String>,
}

/// A book work derived from its shelf path. Text and audio formats use the
/// same identity rule; the scanner keeps their media types separate when it
/// looks the item up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedBook {
    pub title: String,
    pub year: Option<i32>,
    /// Stable scanner identity: the work directory for numbered parts, or the
    /// file itself for a single-file work. It retains any author directory so
    /// two authors' identically titled books never collapse into one item.
    pub identity_path: std::path::PathBuf,
}

/// Parse a text book or audiobook from a path beneath one of `roots`.
///
/// Audiobooks are often one M4B, but just as often a directory of `01.mp3`,
/// `Track 02.flac`, or `Disc 1/Chapter 03.m4a`. In those cases the enclosing
/// work directory is the identity; for a loose descriptive file, the stem is.
/// This keeps a multi-file audiobook as one item without collapsing an
/// author's entire shelf into one title.
pub fn parse_book(path: &Path, roots: &[std::path::PathBuf]) -> ParsedBook {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let relative = roots.iter().find_map(|root| path.strip_prefix(root).ok());
    let dirs: Vec<&str> = relative
        .and_then(Path::parent)
        .map(|parent| {
            parent
                .components()
                .filter_map(|c| c.as_os_str().to_str())
                .collect()
        })
        .unwrap_or_default();

    let parent = dirs.last().copied();
    let (source, identity_path) = match parent {
        // `Title/Disc 1/01.mp3` → Title, not Disc 1.
        Some(p) if track_like(p) && dirs.len() >= 2 => (
            dirs[dirs.len() - 2],
            path.parent()
                .and_then(Path::parent)
                .unwrap_or(path)
                .to_path_buf(),
        ),
        // `Title/01.mp3` and `Author/Title/01.mp3` → the enclosing work.
        Some(p) if track_like(stem) => (p, path.parent().unwrap_or(path).to_path_buf()),
        // A descriptive loose file is already the strongest title signal.
        _ => (stem, path.to_path_buf()),
    };

    match extract_year(source) {
        Some((year, at)) if !title_before_cruft(&source[..at]).is_empty() => ParsedBook {
            title: title_before_cruft(&source[..at]),
            year: Some(year),
            identity_path,
        },
        _ => ParsedBook {
            title: clean_title(source),
            year: None,
            identity_path,
        },
    }
}

fn track_like(raw: &str) -> bool {
    let normalized = raw
        .trim()
        .to_ascii_lowercase()
        .replace(['.', '_', '-'], " ");
    let normalized = normalized.trim();
    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    let short_number = |token: &str| {
        !token.is_empty() && token.len() <= 3 && token.chars().all(|c| c.is_ascii_digit())
    };
    // `01.mp3` and `01 - The Beginning.mp3`, but not the single-file book
    // `1984.m4b`.
    if tokens.first().is_some_and(|token| short_number(token)) {
        return true;
    }
    ["track", "chapter", "part", "disc", "disk", "cd"]
        .iter()
        .any(|prefix| {
            tokens.first().is_some_and(|first| {
                (first == prefix && tokens.get(1).is_some_and(|token| short_number(token)))
                    || first.strip_prefix(prefix).is_some_and(&short_number)
            })
        })
}

/// Normalize separators and collapse whitespace. Dots/underscores become
/// spaces only in scene-style names (no existing spaces), so real titles like
/// "Mr. Robot" survive.
fn clean_title(raw: &str) -> String {
    let mut s = raw.trim().to_owned();
    if !s.contains(' ') && (s.contains('.') || s.contains('_')) {
        s = s.replace(['.', '_'], " ");
    }
    // Dangling separators and bracketed groups.
    s = s.replace('_', " ");
    let s = s.trim().trim_matches(['-', ' ', '.']);
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cut a tokenized title at the first release-cruft token and strip a trailing
/// bracket group. Returns the cleaned title.
fn title_before_cruft(raw: &str) -> String {
    // Drop anything in [] or {} (release groups, quality tags).
    let mut base = String::with_capacity(raw.len());
    let mut depth = 0i32;
    for c in raw.chars() {
        match c {
            '[' | '{' => depth += 1,
            ']' | '}' => depth = (depth - 1).max(0),
            _ if depth == 0 => base.push(c),
            _ => {}
        }
    }

    let normalized = if !base.contains(' ') && (base.contains('.') || base.contains('_')) {
        base.replace(['.', '_'], " ")
    } else {
        base.replace('_', " ")
    };

    let mut kept: Vec<&str> = Vec::new();
    for token in normalized.split_whitespace() {
        let lower = token.to_lowercase();
        let bare = lower.trim_matches(|c: char| !c.is_alphanumeric() && c != '+');
        // Only cut at a release tag once we've kept at least one real title
        // word — titles rarely *start* with a tag word (e.g. "HDR Nights",
        // "Vision", "Extended Family" should keep their leading word).
        if !kept.is_empty() && STOP_TOKENS.contains(&bare) {
            break;
        }
        kept.push(token);
    }
    let joined = kept.join(" ");
    let cleaned = clean_title(&joined);
    // Guard against a title that was entirely tag words → fall back to the
    // whole cleaned string rather than losing the title.
    if cleaned.is_empty() {
        clean_title(&normalized)
    } else {
        cleaned
    }
}

fn extract_year(s: &str) -> Option<(i32, usize)> {
    if let Some(m) = YEAR_PAREN.captures(s) {
        let whole = m.get(0)?;
        let year = m.get(1)?.as_str().parse().ok()?;
        return Some((year, whole.start()));
    }
    // Bare year: take the LAST match, so "2001 A Space Odyssey (1968)" style is
    // handled by the paren branch above, and "Blade.Runner.2049.2017" takes
    // 2017 not 2049.
    let last = YEAR_BARE.captures_iter(s).last()?;
    let year: i32 = last.get(1)?.as_str().parse().ok()?;
    Some((year, last.get(1)?.start()))
}

/// Parse a movie from its path. Always succeeds: worst case the cleaned
/// filename stem becomes the title with no year.
pub fn parse_movie(path: &Path) -> ParsedMovie {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    // A parent dir like "Title (Year)" is usually cleaner than a scene stem.
    let parent = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or_default();

    let source = if YEAR_PAREN.is_match(parent) && !YEAR_PAREN.is_match(stem) {
        parent
    } else {
        stem
    };

    match extract_year(source) {
        Some((year, at)) => ParsedMovie {
            title: title_before_cruft(&source[..at]),
            year: Some(year),
        },
        None => ParsedMovie {
            title: title_before_cruft(source),
            year: None,
        },
    }
}

/// A home-video or photo file's identity: what to call it, and the date its
/// filename gave away (if any).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedHomeMedia {
    pub title: String,
    /// `YYYY-MM-DD` lifted off the front of the filename. Priority 3 in the
    /// recorded-date ladder (docs/HOMEVIDEO-PLAN.md §4.4).
    pub date: Option<String>,
}

// A date at the *start* of the stem: "2019-06-14 - Beach", "20190614_Beach".
// Anchored on purpose — a date in the middle ("IMG_20190614_120000") is
// camera boilerplate, and photos get their dates from EXIF anyway.
static LEADING_DATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(19\d{2}|20\d{2})[-_.]?(\d{2})[-_.]?(\d{2})(?:[\s._-]+(.*))?$").expect("valid")
});

/// Parse a home-video/photo file. **Deliberately not [`parse_movie`]:** that
/// one strips years and codec tokens ("Movie H264 (2024).mp4" → "Movie"),
/// which is right for scene-named movies and destructive here — "Christmas
/// 2019.mp4" must stay "Christmas 2019". So the stem is kept verbatim, with
/// exactly one exception: a leading ISO-ish date moves into the date ladder.
/// Camera junk names (IMG_4021, MVI_0033, DSC01234) stay as they are — they
/// are honest, and the fix for them is the edit UI, not a guess.
pub fn parse_home_media(path: &Path) -> ParsedHomeMedia {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .trim();

    if let Some(caps) = LEADING_DATE.captures(stem) {
        let (year, month, day) = (&caps[1], &caps[2], &caps[3]);
        let (m, d): (u32, u32) = (month.parse().unwrap_or(0), day.parse().unwrap_or(0));
        if (1..=12).contains(&m) && (1..=31).contains(&d) {
            let date = format!("{year}-{month}-{day}");
            let rest = caps.get(4).map(|m| m.as_str().trim()).unwrap_or_default();
            return ParsedHomeMedia {
                // Stripping the date off "2019-06-14.mp4" would leave nothing
                // to click on, so a bare date keeps the whole stem as a title.
                title: if rest.is_empty() {
                    stem.to_owned()
                } else {
                    rest.to_owned()
                },
                date: Some(date),
            };
        }
    }

    ParsedHomeMedia {
        title: stem.to_owned(),
        date: None,
    }
}

/// A season/episode marker found in some name, and where in that name it
/// started (everything before it is the show title, everything after is the
/// episode title).
struct Marker {
    season: i32,
    episode: i32,
    start: usize,
    /// Matched the crammed `102` form, where what follows the digits is the
    /// release group (`-med`), never an episode title.
    crammed: bool,
}

/// Three-digit tokens that are release metadata, never `SEE`. `x264`, `h265`
/// and `480p` can't match the crammed pattern at all (no word boundary inside
/// them), but the same numbers do turn up bare — `Show.H.264.avi` would
/// otherwise become season 2, episode 64.
const NOT_A_CRAMMED_MARKER: &[&str] = &[
    "240", "360", "480", "540", "576", "720", // resolutions
    "264", "265", "266", // codecs
];

/// Find the first `SxxEyy` (or `1x02`) marker in a single name — a filename
/// stem or a directory name.
///
/// `allow_crammed` enables the last-resort `SEE` form (`Drawn.Together.102` →
/// season 1, episode 2), which DVD-era rips use constantly. It is off for anime
/// because absolute numbering owns that shape: `One Piece - 102` is episode
/// 102, not season 1 episode 2, and reading it as `SEE` would silently file
/// three digits of every long-running series under the wrong episode.
fn find_marker(name: &str, allow_crammed: bool) -> Option<Marker> {
    if let Some(captures) = SXXEYY.captures(name).or_else(|| NX_NN.captures(name)) {
        return Some(Marker {
            season: captures.get(1)?.as_str().parse().ok()?,
            episode: captures.get(2)?.as_str().parse().ok()?,
            start: captures.get(0)?.start(),
            crammed: false,
        });
    }
    if !allow_crammed {
        return None;
    }
    // Seasons 1–9 only: a four-digit run is far more often a year or a
    // resolution than season 10, and guessing wrong invents an episode.
    for captures in SEE_CRAMMED.captures_iter(name) {
        let whole = captures.get(0)?;
        if NOT_A_CRAMMED_MARKER.contains(&whole.as_str()) {
            continue;
        }
        let episode: i32 = captures.get(2)?.as_str().parse().ok()?;
        // `100` is a round number in a title far more often than it is
        // episode zero, and specials are written `S01E00` when they are meant.
        if episode == 0 {
            continue;
        }
        return Some(Marker {
            season: captures.get(1)?.as_str().parse().ok()?,
            episode,
            start: whole.start(),
            crammed: true,
        });
    }
    None
}

/// The Plex/Jellyfin local-extras convention: the kind of extra is a hyphen
/// suffix on the stem (`Show.S01E02-trailer.mkv`). Matched on the last
/// hyphen-separated word only, so a release group can't trip it.
const EXTRA_SUFFIXES: &[&str] = &[
    "sample",
    "trailer",
    "proof",
    "behindthescenes",
    "deleted",
    "featurette",
    "short",
    "scene",
    "clip",
    "interview",
    "other",
];

/// Words that mean "this file is not the episode" when they appear in the part
/// of the name that can't be an episode title — before the marker, or anywhere
/// at all when the marker came from the folder. Kept out of the title region
/// because *Show.S01E02.The.Trailer.Park* is an episode, not a trailer.
const EXTRA_TOKENS: &[&str] = &["sample", "trailer", "proof", "screens"];

/// Does this filename declare itself an extra rather than the episode?
///
/// `title_starts_at` is where the episode title begins in `stem` — the marker
/// position when the file carries its own marker, `None` when the marker came
/// off the folder and the whole stem is fair game. This matters because plurx
/// has no notion of extras: an unrecognized sample attaches to the episode as a
/// second version and, tying on height, can sort ahead of the real file
/// (`files_for_item` orders by height, then bitrate, then path).
fn names_itself_an_extra(stem: &str, title_starts_at: Option<usize>) -> bool {
    let lower = stem.to_lowercase();
    if let Some((_, suffix)) = lower.rsplit_once('-') {
        if EXTRA_SUFFIXES.contains(&suffix.trim()) {
            return true;
        }
    }
    let tokens = |s: &str| -> Vec<String> {
        s.split(['.', '-', '_', ' '])
            .map(|t| t.trim().to_owned())
            .collect()
    };
    // "sample" is not a word that appears in real episode titles, so it counts
    // wherever it sits — including `Show.S01E02.sample.avi`.
    if tokens(&lower).iter().any(|t| t == "sample") {
        return true;
    }
    let outside_title = match title_starts_at {
        Some(at) => &lower[..at],
        None => &lower[..],
    };
    tokens(outside_title)
        .iter()
        .any(|t| EXTRA_TOKENS.contains(&t.as_str()))
}

/// Why a file in a Shows library couldn't be read as an episode. The scanner
/// prints the whole path when it reports a skip, so the reason it prints has to
/// be the reason that actually fired — "no marker in the name or the folder"
/// under a path whose folder plainly reads `S01E02` is worse than no message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpisodeSkip {
    /// Neither the filename nor the folder holding it carries a marker.
    NoMarker,
    /// The folder has a marker, but this file says it is not the episode —
    /// a sample, a trailer, a proof clip.
    Extra,
}

/// Parse a TV episode from its path.
pub fn parse_episode(path: &Path) -> Result<ParsedEpisode, EpisodeSkip> {
    parse_episode_inner(path, true)
}

fn parse_episode_inner(path: &Path, allow_crammed: bool) -> Result<ParsedEpisode, EpisodeSkip> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();

    // Directory names, nearest first: [parent, grandparent, ...].
    let dirs: Vec<&str> = path
        .ancestors()
        .skip(1)
        .filter_map(|p| p.file_name())
        .filter_map(|s| s.to_str())
        .collect();

    // The marker normally lives in the filename. Plenty of releases put it on
    // the *folder* instead and name the file after a hash
    // (`Show.S01E06.480p.DVD.x265/956a4a82….mkv`), so fall back to the
    // immediate parent — the same directory the show title already comes from.
    // `depth` is how far up the name we ended up using lives, so the show-title
    // search below starts above it rather than re-reading it.
    let (source, marker, depth) = match find_marker(stem, allow_crammed) {
        // The file carries its own marker — but a sample carries one too, and
        // plurx would file it as a second version of the episode.
        Some(m) if names_itself_an_extra(stem, Some(m.start)) => {
            return Err(EpisodeSkip::Extra);
        }
        Some(m) => (stem, m, 0usize),
        None => {
            let dir = *dirs.first().ok_or(EpisodeSkip::NoMarker)?;
            // A "Season 02" dir carries a season but no episode, so there is
            // nothing to inherit from it either way.
            if SEASON_DIR.is_match(dir) {
                return Err(EpisodeSkip::NoMarker);
            }
            let Some(m) = find_marker(dir, allow_crammed) else {
                return Err(EpisodeSkip::NoMarker);
            };
            // The folder does say which episode it is — but this file says it
            // isn't the episode, and that distinction is what gets reported.
            // Nothing in the stem is a title here, so all of it is checked.
            if names_itself_an_extra(stem, None) {
                return Err(EpisodeSkip::Extra);
            }
            (dir, m, 1usize)
        }
    };
    let (season, episode, marker_start) = (marker.season, marker.episode, marker.start);

    // Show title: prefer a clean show folder (the one above a "Season NN"
    // dir), else the text before the S/E marker in the name that carried it.
    let parent_name = dirs.get(depth).copied().unwrap_or_default();
    let grandparent_name = dirs.get(depth + 1).copied().unwrap_or_default();

    let show_source = if SEASON_DIR.is_match(parent_name) && !grandparent_name.is_empty() {
        grandparent_name
    } else if marker_start > 0 {
        &source[..marker_start]
    } else if !parent_name.is_empty() && !SEASON_DIR.is_match(parent_name) {
        parent_name
    } else {
        source
    };

    let (show_title, show_year) = match extract_year(show_source) {
        Some((year, at)) => (title_before_cruft(&show_source[..at]), Some(year)),
        None => (title_before_cruft(show_source), None),
    };

    // Episode title: text after the marker, minus cruft. Empty → None. The
    // crammed form gets none at all — `102-med` is followed by the release
    // group, and "med" as an episode title is worse than "Episode 2".
    let after = &source[marker_start..];
    let episode_title = (!marker.crammed)
        .then(|| {
            after
                .split_once([' ', '.', '-', '_'])
                .map(|(_, rest)| title_before_cruft(rest))
        })
        .flatten()
        .filter(|t| !t.is_empty());

    Ok(ParsedEpisode {
        show_title: if show_title.is_empty() {
            "Unknown".to_owned()
        } else {
            show_title
        },
        show_year,
        season,
        episode,
        episode_title,
    })
}

/// Remove `[...]` and `{...}` bracket groups (release group, hashes).
fn strip_brackets(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '[' | '{' => depth += 1,
            ']' | '}' => depth = (depth - 1).max(0),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parse an anime episode using absolute numbering (`[Group] Title - NN`),
/// falling back to standard `SxxEyy` when the release uses it. Anime episodes
/// map to season 1 with the absolute number (REQ-META-3).
pub fn parse_anime_episode(path: &Path) -> Result<ParsedEpisode, EpisodeSkip> {
    // Some anime use standard S/E — honor it first, but never the crammed
    // `SEE` form: in anime, three digits after a dash are the absolute episode.
    match parse_episode_inner(path, false) {
        Ok(std) => return Ok(std),
        Err(EpisodeSkip::Extra) => return Err(EpisodeSkip::Extra),
        Err(EpisodeSkip::NoMarker) => {}
    }
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    // Only the unambiguous half of the rule here: an anime episode title in the
    // filename could contain "trailer", but never "sample", and the `-trailer`
    // suffix convention means what it says.
    if names_itself_an_extra(stem, Some(0)) {
        return Err(EpisodeSkip::Extra);
    }
    let cleaned = strip_brackets(stem);

    let caps = ANIME_EP.captures(&cleaned).ok_or(EpisodeSkip::NoMarker)?;
    let episode: i32 = caps
        .get(1)
        .and_then(|m| m.as_str().parse().ok())
        .ok_or(EpisodeSkip::NoMarker)?;
    let marker = caps.get(0).ok_or(EpisodeSkip::NoMarker)?.start();
    let show_title = title_before_cruft(&cleaned[..marker]);
    Ok(ParsedEpisode {
        show_title: if show_title.is_empty() {
            "Unknown".to_owned()
        } else {
            show_title
        },
        show_year: None,
        season: 1,
        episode,
        episode_title: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn movie(p: &str) -> ParsedMovie {
        parse_movie(&PathBuf::from(p))
    }
    fn ep(p: &str) -> Option<ParsedEpisode> {
        parse_episode(&PathBuf::from(p)).ok()
    }
    fn skip(p: &str) -> EpisodeSkip {
        parse_episode(&PathBuf::from(p)).expect_err("expected a skip")
    }
    fn home(p: &str) -> ParsedHomeMedia {
        parse_home_media(&PathBuf::from(p))
    }

    fn book(p: &str) -> ParsedBook {
        parse_book(&PathBuf::from(p), &[PathBuf::from("/books")])
    }

    #[test]
    fn books_keep_text_and_single_file_audio_titles() {
        assert_eq!(
            book("/books/Ursula K. Le Guin/The Dispossessed (1974).epub"),
            ParsedBook {
                title: "The Dispossessed".into(),
                year: Some(1974),
                identity_path: PathBuf::from(
                    "/books/Ursula K. Le Guin/The Dispossessed (1974).epub"
                ),
            }
        );
        assert_eq!(
            book("/books/Project Hail Mary.m4b"),
            ParsedBook {
                title: "Project Hail Mary".into(),
                year: None,
                identity_path: PathBuf::from("/books/Project Hail Mary.m4b"),
            }
        );
    }

    #[test]
    fn audiobook_tracks_collapse_to_the_work_directory() {
        assert_eq!(
            book("/books/Andy Weir/Project Hail Mary/01.mp3").title,
            "Project Hail Mary"
        );
        assert_eq!(
            book("/books/Andy Weir/Project Hail Mary/Disc 1/Chapter 03.m4a").title,
            "Project Hail Mary"
        );
        assert_eq!(
            book("/books/Andy Weir/Project Hail Mary/01 - A Question.mp3").title,
            "Project Hail Mary"
        );
        assert_eq!(
            book("/books/Andy Weir/Project Hail Mary/Chapter 04 Grace.mp3").title,
            "Project Hail Mary"
        );
        // A descriptive file under an author directory stays the title; the
        // author must never become one giant audiobook.
        assert_eq!(
            book("/books/Andy Weir/The Martian.m4b").title,
            "The Martian"
        );
        assert_eq!(book("/books/George Orwell/1984.m4b").title, "1984");
    }

    #[test]
    fn home_titles_are_the_filename_verbatim() {
        // The movie parser would make these "Christmas", "Beach" and "Dad".
        assert_eq!(
            home("/h/2019/Christmas 2019.mp4"),
            ParsedHomeMedia {
                title: "Christmas 2019".into(),
                date: None
            }
        );
        assert_eq!(
            home("/h/Beach 4k HDR.mov"),
            ParsedHomeMedia {
                title: "Beach 4k HDR".into(),
                date: None
            }
        );
        // Camera junk stays honest rather than becoming a guess.
        for junk in ["IMG_4021", "MVI_0033", "DSC01234"] {
            assert_eq!(home(&format!("/h/{junk}.mp4")).title, junk);
        }
    }

    #[test]
    fn a_leading_date_moves_into_the_date_field() {
        assert_eq!(
            home("/h/2019-06-14 - Beach.mp4"),
            ParsedHomeMedia {
                title: "Beach".into(),
                date: Some("2019-06-14".into())
            }
        );
        assert_eq!(
            home("/h/20190614_Beach day.mp4"),
            ParsedHomeMedia {
                title: "Beach day".into(),
                date: Some("2019-06-14".into())
            }
        );
        // Nothing left after stripping → keep the stem, still take the date.
        assert_eq!(
            home("/h/2019-06-14.mp4"),
            ParsedHomeMedia {
                title: "2019-06-14".into(),
                date: Some("2019-06-14".into())
            }
        );
    }

    #[test]
    fn only_a_real_leading_date_counts() {
        // Impossible month/day: not a date, so the name stays whole.
        assert_eq!(home("/h/2019-13-40 party.mp4").date, None);
        assert_eq!(home("/h/2019-13-40 party.mp4").title, "2019-13-40 party");
        // Mid-name dates are camera boilerplate (and photos use EXIF anyway).
        assert_eq!(home("/h/IMG_20190614_120000.jpg").date, None);
        assert_eq!(
            home("/h/IMG_20190614_120000.jpg").title,
            "IMG_20190614_120000"
        );
        // A number that isn't a date at all.
        assert_eq!(home("/h/12345678.mp4").date, None);
    }

    #[test]
    fn movies_plex_style() {
        assert_eq!(
            movie("/m/The Matrix (1999)/The Matrix (1999).mkv"),
            ParsedMovie {
                title: "The Matrix".into(),
                year: Some(1999)
            }
        );
        assert_eq!(
            movie("/m/Dune Part Two (2024) [2160p].mkv"),
            ParsedMovie {
                title: "Dune Part Two".into(),
                year: Some(2024)
            }
        );
    }

    #[test]
    fn movies_scene_style() {
        assert_eq!(
            movie("/m/Blade.Runner.2049.2017.1080p.BluRay.x265-GROUP.mkv"),
            ParsedMovie {
                title: "Blade Runner 2049".into(),
                year: Some(2017)
            }
        );
        assert_eq!(
            movie("/m/Heat.1995.REMUX.1080p.mkv"),
            ParsedMovie {
                title: "Heat".into(),
                year: Some(1995)
            }
        );
    }

    #[test]
    fn title_leading_with_a_tag_word_is_kept() {
        // "HDR" is a release-tag stop word, but here it leads a real title.
        assert_eq!(
            movie("/m/HDR Nights (2024)/HDR Nights (2024).mkv"),
            ParsedMovie {
                title: "HDR Nights".into(),
                year: Some(2024)
            }
        );
        // Still strips trailing cruft after real words.
        assert_eq!(
            movie("/m/Vision.2020.1080p.BluRay.x264.mkv"),
            ParsedMovie {
                title: "Vision".into(),
                year: Some(2020)
            }
        );
    }

    #[test]
    fn movie_without_year() {
        assert_eq!(
            movie("/m/Some Home Video.mp4"),
            ParsedMovie {
                title: "Some Home Video".into(),
                year: None
            }
        );
    }

    #[test]
    fn episodes_folder_style() {
        assert_eq!(
            ep("/tv/Severance (2022)/Season 01/Severance - S01E03 - In Perpetuity.mkv"),
            Some(ParsedEpisode {
                show_title: "Severance".into(),
                show_year: Some(2022),
                season: 1,
                episode: 3,
                episode_title: Some("In Perpetuity".into()),
            })
        );
    }

    #[test]
    fn episodes_scene_style() {
        let e = ep("/tv/The.Bear.S02E05.1080p.WEB.h264-GROUP.mkv").expect("parsed");
        assert_eq!(e.show_title, "The Bear");
        assert_eq!((e.season, e.episode), (2, 5));
    }

    #[test]
    fn episodes_1x02_style() {
        let e = ep("/tv/Firefly/Firefly 1x02.mkv").expect("parsed");
        assert_eq!(e.show_title, "Firefly");
        assert_eq!((e.season, e.episode), (1, 2));
    }

    #[test]
    fn multi_episode_takes_first() {
        let e = ep("/tv/Show/Season 1/Show S01E01E02.mkv").expect("parsed");
        assert_eq!((e.season, e.episode), (1, 1));
    }

    #[test]
    fn non_episode_returns_none() {
        assert!(ep("/tv/Show/Season 1/poster.jpg").is_none());
        assert!(ep("/tv/random movie (2020).mkv").is_none());
    }

    /// Torrent packaging: the release *folder* carries the marker and the file
    /// inside is an MD5 hash. The old parser only read the file stem, so these
    /// were skipped with a message that printed a path containing `S01E06`
    /// while claiming there was no marker in it.
    #[test]
    fn hash_named_file_inherits_its_release_folder() {
        let e = ep(
            "/8tb/tv/Drawn Together/Season 1/Drawn.Together.2004.S01E06.Dirty.Pranking.Number.2.\
             480p.DVD.x265.Panda/956a4a82d3e71a92e95bc3658e6978d7.mkv",
        )
        .expect("parsed from the folder name");
        // "Season 1"'s parent is the clean show folder, so it wins the title.
        assert_eq!(e.show_title, "Drawn Together");
        assert_eq!((e.season, e.episode), (1, 6));
        assert_eq!(e.episode_title.as_deref(), Some("Dirty Pranking Number 2"));
    }

    #[test]
    fn release_folder_supplies_title_and_year_with_no_show_dir() {
        let e = ep("/tv/The.Bear.2022.S02E05.1080p.WEB.h264-GROUP/a1b2c3d4.mkv").expect("parsed");
        assert_eq!(e.show_title, "The Bear");
        assert_eq!(e.show_year, Some(2022));
        assert_eq!((e.season, e.episode), (2, 5));
    }

    #[test]
    fn folder_fallback_only_applies_when_the_file_has_no_marker() {
        // The file's own marker still wins over a mismatched folder.
        let e = ep("/tv/Show.S01E06.1080p/Show.S01E07.mkv").expect("parsed");
        assert_eq!((e.season, e.episode), (1, 7));
    }

    #[test]
    fn folder_fallback_skips_samples_and_season_dirs() {
        // A sample beside the episode must not become a second file on it.
        assert!(ep("/tv/Show.S01E06.1080p.WEB/sample.mkv").is_none());
        assert!(ep("/tv/Show.S01E06.1080p.WEB/show-sample.mkv").is_none());
        // A season dir has a season but no episode — nothing to inherit.
        assert!(ep("/tv/Show/Season 1/1x.mkv").is_none());
        assert!(ep("/tv/Show/Season 01/00000000000000000000000000000000.mkv").is_none());
        // And a plain show folder still yields nothing.
        assert!(ep("/tv/Drawn Together/956a4a82d3e71a92e95bc3658e6978d7.mkv").is_none());
    }

    /// The skip reason is what the scan report prints next to the full path, so
    /// "no marker anywhere" must not be said about a path whose folder has one.
    #[test]
    fn a_skipped_file_reports_the_reason_that_fired() {
        assert_eq!(
            skip(
                "/8tb/tv/Drawn Together/Season 1/Drawn.Together.S01E02.DVDRip.XviD-MEDiEVAL/\
                 sample.drawn.together.102-med.avi"
            ),
            EpisodeSkip::Extra,
        );
        // No marker on the folder either → the extras rule never came up.
        assert_eq!(
            skip("/tv/Drawn Together/Season 1/sample.avi"),
            EpisodeSkip::NoMarker,
        );
        assert_eq!(skip("/tv/Show/Season 1/poster.jpg"), EpisodeSkip::NoMarker);
    }

    #[test]
    fn crammed_see_numbering() {
        let e = ep("/tv/Drawn Together/Season 1/drawn.together.102-med.avi").expect("parsed");
        assert_eq!(e.show_title, "Drawn Together");
        assert_eq!((e.season, e.episode), (1, 2));
        // "-med" is the release group; no episode title is better than that one.
        assert_eq!(e.episode_title, None);

        // Also usable as the folder fallback, and past season 9's little
        // brother: 9x99 is the ceiling of the form.
        let e = ep("/tv/Show.512.DVDRip.XviD-GRP/abc123.avi").expect("parsed");
        assert_eq!((e.season, e.episode), (5, 12));
        assert_eq!(e.show_title, "Show");
        assert_eq!(ep("/tv/Show/Season 9/show.999.avi").expect("p").episode, 99);
    }

    #[test]
    fn crammed_numbering_ignores_release_cruft() {
        // Codec and resolution tokens are the whole reason for the deny-list:
        // "264" would otherwise be season 2, episode 64.
        assert!(ep("/tv/Show/Season 1/show.H.264.avi").is_none());
        assert!(ep("/tv/Show/Season 1/show.720.avi").is_none());
        // Digits touching a word character on either side aren't the form.
        assert!(ep("/tv/Show/Season 1/show.1080p.x264.avi").is_none());
        assert!(ep("/tv/Show/Season 1/show.2004.remux.avi").is_none());
        // Episode 0 is written S01E00 when it is meant; "100" in a title isn't.
        assert!(ep("/tv/Show/Season 1/The 100 pilot.avi").is_none());
    }

    #[test]
    fn anime_absolute_numbering_beats_the_crammed_form() {
        // The regression the `allow_crammed` flag exists to prevent: this is
        // episode 102, not season 1 episode 2.
        let e = anime("/a/[Group] One Piece - 102 [1080p].mkv").expect("parsed");
        assert_eq!((e.season, e.episode), (1, 102));
    }

    fn anime(p: &str) -> Option<ParsedEpisode> {
        parse_anime_episode(&PathBuf::from(p)).ok()
    }

    #[test]
    fn anime_absolute_numbering() {
        let e = anime("/a/[SubsPlease] Sousou no Frieren - 01 (1080p) [A1B2C3].mkv").expect("p");
        assert_eq!(e.show_title, "Sousou no Frieren");
        assert_eq!((e.season, e.episode), (1, 1));

        // Version suffix and 3-digit numbers.
        let e = anime("/a/[Group] One Piece - 1042v2 [720p].mkv").expect("p");
        assert_eq!(e.show_title, "One Piece");
        assert_eq!(e.episode, 1042);

        // Plain "Title - NN.ext".
        let e = anime("/a/Bocchi the Rock - 05.mkv").expect("p");
        assert_eq!(e.show_title, "Bocchi the Rock");
        assert_eq!(e.episode, 5);
    }

    #[test]
    fn anime_honors_standard_se() {
        // Anime that uses S/E still parses via the standard path.
        let e = anime("/a/Attack on Titan/Season 4/Attack on Titan - S04E01.mkv").expect("p");
        assert_eq!((e.season, e.episode), (4, 1));
    }

    #[test]
    fn anime_without_number_is_none() {
        assert!(anime("/a/[Group] Some Movie (2020) [1080p].mkv").is_none());
    }

    #[test]
    fn dotted_title_with_dot_in_name() {
        // Spaces present → dots are NOT separators ("Mr. Robot" preserved).
        let e = ep("/tv/Mr. Robot (2015)/Season 01/Mr. Robot - S01E01.mkv").expect("parsed");
        assert_eq!(e.show_title, "Mr. Robot");
    }
}
