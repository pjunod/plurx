//! Default audio/subtitle track selection.
//!
//! Two forces pick the defaults, in order:
//! 1. **Anime dual-audio** (REQ-SUB-2, `prefer_original`): original (usually
//!    Japanese) audio paired with full subtitles in the preferred subtitle
//!    language — never the English dub. Subs are the point here, so the
//!    subtitle mode is ignored on this path.
//! 2. **Server-wide language preferences** (Settings → Playback defaults):
//!    prefer the configured audio language when the file carries it, and
//!    auto-select subtitles per the configured mode — `Auto` shows full subs
//!    only when the chosen audio isn't the preferred subtitle language
//!    (foreign audio → subs on), `Always` always picks one, `Off` selects
//!    none. Forced-only overlay subs remain the floor everywhere.
//!
//! Pure functions, shared by the transcode pipeline (burn-in choice) and the
//! `/decision` endpoint (the clients' default-track flags).

use crate::domain::{AudioStream, SubtitleStream};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrackSelection {
    /// Index among audio streams to play by default.
    pub audio_index: Option<i64>,
    /// Index among subtitle streams to show/burn by default.
    pub subtitle_index: Option<i64>,
}

/// When subtitles auto-select.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SubMode {
    /// Only when the audio playing isn't the preferred subtitle language.
    #[default]
    Auto,
    Always,
    Off,
}

impl SubMode {
    pub fn parse(s: &str) -> SubMode {
        match s {
            "always" => SubMode::Always,
            "off" => SubMode::Off,
            _ => SubMode::Auto,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            SubMode::Auto => "auto",
            SubMode::Always => "always",
            SubMode::Off => "off",
        }
    }
}

/// Server-wide playback language preferences. Defaults to English/English/Auto.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LangPrefs {
    /// ISO 639 code, e.g. "eng".
    pub audio_lang: String,
    pub sub_lang: String,
    pub sub_mode: SubMode,
}

impl Default for LangPrefs {
    fn default() -> Self {
        LangPrefs {
            audio_lang: "eng".into(),
            sub_lang: "eng".into(),
            sub_mode: SubMode::Auto,
        }
    }
}

/// ISO 639-1 / 639-2/B / 639-2/T spellings that mean the same language, so a
/// file tagged "de" matches a preference of "ger". First entry per group is
/// the canonical settings value.
const LANG_ALIASES: &[&[&str]] = &[
    &["eng", "en"],
    &["jpn", "ja", "jp"],
    &["spa", "es"],
    &["fre", "fra", "fr"],
    &["ger", "deu", "de"],
    &["ita", "it"],
    &["por", "pt"],
    &["rus", "ru"],
    &["kor", "ko"],
    &["chi", "zho", "zh"],
    &["hin", "hi"],
    &["ara", "ar"],
    &["nld", "dut", "nl"],
    &["swe", "sv"],
    &["pol", "pl"],
    &["nor", "nob", "no"],
    &["dan", "da"],
    &["fin", "fi"],
    &["tur", "tr"],
    &["tha", "th"],
    &["vie", "vi"],
    &["ukr", "uk"],
    &["ces", "cze", "cs"],
    &["ell", "gre", "el"],
    &["heb", "he"],
    &["hun", "hu"],
    &["ron", "rum", "ro"],
];

/// Does a stream's language tag mean the preferred language?
pub fn lang_matches(code: &Option<String>, pref: &str) -> bool {
    let Some(code) = code else { return false };
    let c = code.to_lowercase();
    let p = pref.to_lowercase();
    if c == p {
        return true;
    }
    LANG_ALIASES
        .iter()
        .any(|group| group.contains(&c.as_str()) && group.contains(&p.as_str()))
}

const JAPANESE: &[&str] = &["jpn", "ja", "jp"];

fn lang_is(code: &Option<String>, targets: &[&str]) -> bool {
    match code {
        Some(c) => {
            let c = c.to_lowercase();
            targets.iter().any(|t| c == *t)
        }
        None => false,
    }
}

/// True for image-based subtitle formats that must be burned in (can't be
/// rendered as text by clients).
pub fn is_bitmap_subtitle(codec: &str) -> bool {
    matches!(
        codec.to_lowercase().as_str(),
        "hdmv_pgs_subtitle" | "pgssub" | "dvd_subtitle" | "dvdsub" | "xsub"
    )
}

fn default_or_first(audio: &[AudioStream]) -> Option<i64> {
    audio
        .iter()
        .find(|a| a.default)
        .or_else(|| audio.first())
        .map(|a| a.index)
}

/// Best full (non-forced) subtitle in a language, falling back to a forced one
/// in that language.
fn sub_in_lang<'a>(subs: &'a [SubtitleStream], lang: &str) -> Option<&'a SubtitleStream> {
    subs.iter()
        .find(|s| lang_matches(&s.language, lang) && !s.forced)
        .or_else(|| subs.iter().find(|s| lang_matches(&s.language, lang)))
}

/// The floor: only forced overlay subs (or the container's flagged default).
fn forced_or_default(subs: &[SubtitleStream]) -> Option<i64> {
    subs.iter()
        .find(|s| s.forced)
        .or_else(|| subs.iter().find(|s| s.default))
        .map(|s| s.index)
}

/// Choose default tracks. See the module docs for the rules.
pub fn select_tracks(
    audio: &[AudioStream],
    subs: &[SubtitleStream],
    prefer_original: bool,
    prefs: &LangPrefs,
) -> TrackSelection {
    if prefer_original {
        // Prefer Japanese audio; if present, pair it with full subs in the
        // preferred subtitle language (mode is ignored — subs are the point).
        if let Some(jp) = audio.iter().find(|a| lang_is(&a.language, JAPANESE)) {
            let sub = sub_in_lang(subs, &prefs.sub_lang)
                .or_else(|| subs.iter().find(|s| s.default))
                .or_else(|| subs.iter().find(|s| !s.forced))
                .or_else(|| subs.first());
            return TrackSelection {
                audio_index: Some(jp.index),
                subtitle_index: sub.map(|s| s.index),
            };
        }
        // No Japanese track — fall through to the preference behavior.
    }

    // Audio: the preferred language when the file has it (a preferred-language
    // track that is also the container default wins over an earlier match),
    // otherwise the container's default/first.
    let audio_index = audio
        .iter()
        .find(|a| lang_matches(&a.language, &prefs.audio_lang) && a.default)
        .or_else(|| {
            audio
                .iter()
                .find(|a| lang_matches(&a.language, &prefs.audio_lang))
        })
        .map(|a| a.index)
        .or_else(|| default_or_first(audio));
    let audio_lang = audio_index
        .and_then(|idx| audio.iter().find(|a| a.index == idx))
        .and_then(|a| a.language.clone());

    let subtitle_index = match prefs.sub_mode {
        SubMode::Off => None,
        SubMode::Always => sub_in_lang(subs, &prefs.sub_lang)
            .map(|s| s.index)
            .or_else(|| subs.iter().find(|s| !s.forced).map(|s| s.index))
            .or_else(|| forced_or_default(subs)),
        SubMode::Auto => {
            if lang_matches(&audio_lang, &prefs.sub_lang) {
                // Audio already speaks the preferred language → overlay only.
                forced_or_default(subs)
            } else {
                // Foreign audio → full subs in the preferred language.
                sub_in_lang(subs, &prefs.sub_lang)
                    .map(|s| s.index)
                    .or_else(|| forced_or_default(subs))
            }
        }
    };

    TrackSelection {
        audio_index,
        subtitle_index,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(index: i64, lang: &str, default: bool) -> AudioStream {
        AudioStream {
            index,
            codec: "aac".into(),
            channels: Some(2),
            language: Some(lang.into()),
            title: None,
            default,
        }
    }
    fn sub(index: i64, lang: &str, forced: bool, default: bool) -> SubtitleStream {
        SubtitleStream {
            index,
            codec: "subrip".into(),
            language: Some(lang.into()),
            title: None,
            default,
            forced,
        }
    }
    fn prefs(audio: &str, sub: &str, mode: SubMode) -> LangPrefs {
        LangPrefs {
            audio_lang: audio.into(),
            sub_lang: sub.into(),
            sub_mode: mode,
        }
    }

    #[test]
    fn anime_prefers_japanese_audio_and_preferred_subs() {
        // Track 0 = English dub (default), Track 1 = Japanese.
        let a = vec![audio(0, "eng", true), audio(1, "jpn", false)];
        let s = vec![sub(0, "eng", false, false), sub(1, "eng", true, false)];
        let sel = select_tracks(&a, &s, true, &LangPrefs::default());
        assert_eq!(sel.audio_index, Some(1)); // Japanese, not the default dub
        assert_eq!(sel.subtitle_index, Some(0)); // full English subs, not forced
    }

    #[test]
    fn anime_without_japanese_falls_back() {
        let a = vec![audio(0, "eng", true)];
        let sel = select_tracks(&a, &[], true, &LangPrefs::default());
        assert_eq!(sel.audio_index, Some(0));
        assert_eq!(sel.subtitle_index, None);
    }

    #[test]
    fn preferred_audio_language_beats_container_default() {
        // French flagged default, but the server prefers English.
        let a = vec![audio(0, "eng", false), audio(1, "fre", true)];
        let s = vec![sub(0, "eng", false, false), sub(1, "eng", true, false)];
        let sel = select_tracks(&a, &s, false, &LangPrefs::default());
        assert_eq!(sel.audio_index, Some(0)); // English wins
        assert_eq!(sel.subtitle_index, Some(1)); // audio = sub lang → forced only
    }

    #[test]
    fn two_letter_tags_match_three_letter_preference() {
        let a = vec![audio(0, "de", true), audio(1, "en", false)];
        let sel = select_tracks(&a, &[], false, &prefs("ger", "eng", SubMode::Auto));
        assert_eq!(sel.audio_index, Some(0));
    }

    #[test]
    fn auto_subs_turn_on_for_foreign_audio() {
        // Only French audio; English subs available → subs auto-select.
        let a = vec![audio(0, "fre", true)];
        let s = vec![sub(0, "fre", false, false), sub(1, "eng", false, false)];
        let sel = select_tracks(&a, &s, false, &LangPrefs::default());
        assert_eq!(sel.audio_index, Some(0));
        assert_eq!(sel.subtitle_index, Some(1)); // full English subs
    }

    #[test]
    fn auto_subs_stay_off_for_preferred_audio() {
        let a = vec![audio(0, "eng", true)];
        let s = vec![sub(0, "eng", false, false)];
        let sel = select_tracks(&a, &s, false, &LangPrefs::default());
        assert_eq!(sel.subtitle_index, None); // no forced subs → nothing
    }

    #[test]
    fn always_and_off_modes() {
        let a = vec![audio(0, "eng", true)];
        let s = vec![sub(0, "eng", false, false), sub(1, "eng", true, false)];
        let always = select_tracks(&a, &s, false, &prefs("eng", "eng", SubMode::Always));
        assert_eq!(always.subtitle_index, Some(0)); // full sub, even on eng audio
        let off = select_tracks(&a, &s, false, &prefs("eng", "eng", SubMode::Off));
        assert_eq!(off.subtitle_index, None); // even the forced one stays off
    }

    #[test]
    fn no_preference_match_falls_back_to_container_default() {
        let a = vec![audio(0, "fre", false), audio(1, "ita", true)];
        let sel = select_tracks(&a, &[], false, &LangPrefs::default());
        assert_eq!(sel.audio_index, Some(1)); // container default
    }

    #[test]
    fn bitmap_detection() {
        assert!(is_bitmap_subtitle("hdmv_pgs_subtitle"));
        assert!(is_bitmap_subtitle("dvd_subtitle"));
        assert!(!is_bitmap_subtitle("subrip"));
        assert!(!is_bitmap_subtitle("ass"));
    }
}
