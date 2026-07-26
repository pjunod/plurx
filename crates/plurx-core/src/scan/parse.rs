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
}

/// Find the first `SxxEyy` (or `1x02`) marker in a single name — a filename
/// stem or a directory name.
fn find_marker(name: &str) -> Option<Marker> {
    let captures = SXXEYY.captures(name).or_else(|| NX_NN.captures(name))?;
    Some(Marker {
        season: captures.get(1)?.as_str().parse().ok()?,
        episode: captures.get(2)?.as_str().parse().ok()?,
        start: captures.get(0)?.start(),
    })
}

/// Filenames that describe the *file's* role rather than its content. These
/// never inherit their folder's episode marker: a 30-second sample sitting
/// beside the episode would otherwise become a second file on that episode and
/// could out-sort the real one (`files_for_item` ties on height, then path).
const NON_EPISODE_STEMS: &[&str] = &["sample", "trailer", "proof", "screens", "rarbg"];

fn is_non_episode_stem(stem: &str) -> bool {
    stem.to_lowercase()
        .split(['.', '-', '_', ' '])
        .any(|token| NON_EPISODE_STEMS.contains(&token))
}

/// Parse a TV episode from its path, or `None` if no S/E marker is present in
/// the filename or on the folder holding it.
pub fn parse_episode(path: &Path) -> Option<ParsedEpisode> {
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
    let (source, marker, depth) = match find_marker(stem) {
        Some(m) => (stem, m, 0usize),
        None => {
            let dir = *dirs.first()?;
            // A "Season 02" dir carries a season but no episode, and a sample
            // is not the episode — neither may borrow the folder's marker.
            if SEASON_DIR.is_match(dir) || is_non_episode_stem(stem) {
                return None;
            }
            (dir, find_marker(dir)?, 1usize)
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

    // Episode title: text after the marker, minus cruft. Empty → None.
    let after = &source[marker_start..];
    let episode_title = after
        .split_once([' ', '.', '-', '_'])
        .map(|(_, rest)| title_before_cruft(rest))
        .filter(|t| !t.is_empty());

    Some(ParsedEpisode {
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
/// falling back to standard `SxxEyy` when the release uses it. Returns `None`
/// if no episode number is found. Anime episodes map to season 1 with the
/// absolute number (REQ-META-3).
pub fn parse_anime_episode(path: &Path) -> Option<ParsedEpisode> {
    // Some anime use standard S/E — honor it first.
    if let Some(std) = parse_episode(path) {
        return Some(std);
    }
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let cleaned = strip_brackets(stem);

    let caps = ANIME_EP.captures(&cleaned)?;
    let episode: i32 = caps.get(1)?.as_str().parse().ok()?;
    let marker = caps.get(0)?.start();
    let show_title = title_before_cruft(&cleaned[..marker]);
    Some(ParsedEpisode {
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
        parse_episode(&PathBuf::from(p))
    }
    fn home(p: &str) -> ParsedHomeMedia {
        parse_home_media(&PathBuf::from(p))
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

    fn anime(p: &str) -> Option<ParsedEpisode> {
        parse_anime_episode(&PathBuf::from(p))
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
