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

/// Parse a TV episode from its path, or `None` if no S/E marker is present
/// anywhere in the filename.
pub fn parse_episode(path: &Path) -> Option<ParsedEpisode> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();

    // Both the SxxEyy and the NxNN forms yield (season, episode, marker start).
    let captures = SXXEYY.captures(stem).or_else(|| NX_NN.captures(stem))?;
    let whole = captures.get(0)?;
    let (season, episode, marker_start) = (
        captures.get(1)?.as_str().parse().ok()?,
        captures.get(2)?.as_str().parse().ok()?,
        whole.start(),
    );

    // Show title: prefer a clean show folder (grandparent when inside a
    // "Season NN" dir), else the text before the S/E marker in the filename.
    let parent = path.parent();
    let parent_name = parent
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let grandparent_name = parent
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or_default();

    let show_source = if SEASON_DIR.is_match(parent_name) && !grandparent_name.is_empty() {
        grandparent_name
    } else if marker_start > 0 {
        &stem[..marker_start]
    } else if !parent_name.is_empty() && !SEASON_DIR.is_match(parent_name) {
        parent_name
    } else {
        stem
    };

    let (show_title, show_year) = match extract_year(show_source) {
        Some((year, at)) => (title_before_cruft(&show_source[..at]), Some(year)),
        None => (title_before_cruft(show_source), None),
    };

    // Episode title: text after the marker, minus cruft. Empty → None.
    let after = &stem[marker_start..];
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
