//! Default audio/subtitle track selection.
//!
//! Anime dual-audio releases (REQ-SUB-2) want the *original* (usually
//! Japanese) audio with subtitles by default, rather than the English dub.
//! Regular content wants the file's default/first audio and only forced subs.
//! This is a pure function so the rule is testable and shared by the transcode
//! pipeline and (later) the clients' default-track hints.

use crate::domain::{AudioStream, SubtitleStream};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrackSelection {
    /// Index among audio streams to play by default.
    pub audio_index: Option<i64>,
    /// Index among subtitle streams to show/burn by default.
    pub subtitle_index: Option<i64>,
}

fn lang_is(code: &Option<String>, targets: &[&str]) -> bool {
    match code {
        Some(c) => {
            let c = c.to_lowercase();
            targets.iter().any(|t| c == *t)
        }
        None => false,
    }
}

const JAPANESE: &[&str] = &["jpn", "ja", "jp"];
const ENGLISH: &[&str] = &["eng", "en"];

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

/// Choose default tracks. When `prefer_original`, prefer Japanese audio + a
/// full English subtitle; otherwise the default/first audio and only a forced
/// subtitle (for foreign-language segments).
pub fn select_tracks(
    audio: &[AudioStream],
    subs: &[SubtitleStream],
    prefer_original: bool,
) -> TrackSelection {
    if prefer_original {
        // Prefer Japanese audio; if present, pair it with English subs.
        if let Some(jp) = audio.iter().find(|a| lang_is(&a.language, JAPANESE)) {
            let sub = subs
                .iter()
                .find(|s| lang_is(&s.language, ENGLISH) && !s.forced)
                .or_else(|| subs.iter().find(|s| lang_is(&s.language, ENGLISH)))
                .or_else(|| subs.iter().find(|s| s.default))
                .or_else(|| subs.first());
            return TrackSelection {
                audio_index: Some(jp.index),
                subtitle_index: sub.map(|s| s.index),
            };
        }
        // No Japanese track — fall through to the default behavior.
    }

    // Default: the file's default/first audio, and only a forced subtitle.
    TrackSelection {
        audio_index: default_or_first(audio),
        subtitle_index: subs
            .iter()
            .find(|s| s.forced)
            .or_else(|| subs.iter().find(|s| s.default))
            .map(|s| s.index),
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

    #[test]
    fn anime_prefers_japanese_audio_and_english_subs() {
        // Track 0 = English dub (default), Track 1 = Japanese.
        let a = vec![audio(0, "eng", true), audio(1, "jpn", false)];
        let s = vec![sub(0, "eng", false, false), sub(1, "eng", true, false)];
        let sel = select_tracks(&a, &s, true);
        assert_eq!(sel.audio_index, Some(1)); // Japanese, not the default dub
        assert_eq!(sel.subtitle_index, Some(0)); // full English subs, not forced
    }

    #[test]
    fn anime_without_japanese_falls_back() {
        let a = vec![audio(0, "eng", true)];
        let sel = select_tracks(&a, &[], true);
        assert_eq!(sel.audio_index, Some(0));
        assert_eq!(sel.subtitle_index, None);
    }

    #[test]
    fn non_anime_uses_default_audio_and_forced_subs_only() {
        let a = vec![audio(0, "eng", false), audio(1, "fre", true)];
        let s = vec![sub(0, "eng", false, false), sub(1, "eng", true, false)];
        let sel = select_tracks(&a, &s, false);
        assert_eq!(sel.audio_index, Some(1)); // the default track
        assert_eq!(sel.subtitle_index, Some(1)); // only the forced sub
    }

    #[test]
    fn bitmap_detection() {
        assert!(is_bitmap_subtitle("hdmv_pgs_subtitle"));
        assert!(is_bitmap_subtitle("dvd_subtitle"));
        assert!(!is_bitmap_subtitle("subrip"));
        assert!(!is_bitmap_subtitle("ass"));
    }
}
