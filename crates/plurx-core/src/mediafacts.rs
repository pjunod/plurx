//! What one library-list row can say about the media behind it.
//!
//! The item detail response carries a `FileDto` per file and the client works
//! the badges out itself. A list cannot: it would need every file of every
//! item on the page, which is the fan-out this module exists to avoid. So the
//! store aggregates in SQL (one statement for the whole page) and hands each
//! item's answer here to be turned into the terse strings a card or a table
//! row prints.
//!
//! The labels are deliberately the ones `web/index.html` already prints
//! (`codecLabel`, `hdrChip`, `premiumAudio`, `fmtChannels`). A second
//! vocabulary would mean the same file badged "HEVC · DV · TrueHD 7.1" on the
//! detail page and something else on the grid — the kind of drift nobody
//! notices until a user reports the "two different libraries" bug.

use crate::domain::AudioStream;

/// One item's aggregate row: the totals over all its files, plus the columns
/// of the single **best** file (see [`MediaFacts`] for what "best" means and
/// why the descriptive fields all come from one file rather than a union).
#[derive(Debug, Clone, Default)]
pub struct FactsRow {
    pub files: i64,
    pub bytes: i64,
    pub container: Option<String>,
    pub video_codec: Option<String>,
    pub height: Option<i64>,
    /// Coarse HDR type as `detect_hdr` writes it: `dolby_vision` | `hdr10` |
    /// `hlg` | `None`.
    pub hdr: Option<String>,
    /// Rich label as `detect_hdr_format` writes it ("Dolby Vision · Profile 7
    /// (HDR10-compatible)", "HDR10+", "HDR10", "HLG").
    pub hdr_format: Option<String>,
    pub audio: Vec<AudioStream>,
}

/// The aggregated media block for one item.
///
/// **Aggregation rule** (the failure it prevents is a library that reads as
/// worse than it is — a 2160p Dolby Vision remux sitting next to a 720p phone
/// copy must never badge as 720p):
///
/// * `files` / `bytes` describe **all** files of the item: a count and a sum.
///   Sum, not max — the question these answer is "what does this title cost me
///   on disk", and a two-version movie costs both versions.
/// * `video` / `height` / `dr` / `audio` / `container` all describe **one**
///   file: the best one. Best = greatest height (the same rule the existing
///   `resolution` badge uses via `MAX(height)`, so the two can never disagree),
///   then greatest bitrate (a remux beats a re-encode at equal height), then
///   greatest size, then lowest file id — the last two only so the answer is
///   stable across page loads instead of flickering between equal versions.
/// * They come from one file *together*, never a per-field union across files.
///   A union would let the block claim "2160p · DV · TrueHD 7.1" for an item
///   where no single file is all three, and a client that offers to play what
///   the badges promise would then be lying.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaFacts {
    /// How many files this item has.
    pub files: i64,
    /// Total bytes on disk across those files.
    pub bytes: i64,
    /// Best file's video codec, display-cased: "HEVC", "H.264", "AV1".
    pub video: Option<String>,
    /// Best file's height in pixels — equal to the list's `resolution` by
    /// construction, since both are the maximum height over the item's files.
    pub height: Option<i64>,
    /// Best file's coarse HDR type, spelled exactly as `MediaFile.hdr` and
    /// `FileDto.hdr` spell it: `dolby_vision` | `hdr10` | `hlg`, absent for
    /// SDR. A token, not a label — see the module note on why this block
    /// stopped rendering words.
    pub hdr: Option<String>,
    /// Best file's rich HDR label as `detect_hdr_format` wrote it ("HDR10+",
    /// "Dolby Vision · Profile 7 (HDR10-compatible)"), verbatim, absent when
    /// unprobed or SDR. Carried alongside the token for the same reason
    /// `FileDto` carries both: the token is what code compares, the label is
    /// what a client may choose to print.
    pub hdr_format: Option<String>,
    /// Best file's headline audio track: "TrueHD 7.1", "DTS 5.1", "AAC 2.0".
    pub audio: Option<String>,
    /// Best file's container, upper-cased: "MKV", "MP4".
    pub container: Option<String>,
}

impl From<FactsRow> for MediaFacts {
    fn from(row: FactsRow) -> Self {
        MediaFacts {
            files: row.files,
            bytes: row.bytes,
            video: video_codec_label(row.video_codec.as_deref()),
            height: row.height,
            hdr: hdr_token(row.hdr.as_deref(), row.hdr_format.as_deref()),
            hdr_format: row
                .hdr_format
                .as_deref()
                .map(str::trim)
                .filter(|f| !f.is_empty())
                .map(str::to_owned),
            audio: audio_label(&row.audio),
            container: container_label(row.container.as_deref()),
        }
    }
}

/// ffprobe codec name to the badge text, mirroring `codecLabel` in
/// `web/index.html`. Unknown codecs are upper-cased rather than dropped: a
/// name we have not met is still more use on a card than a blank.
fn video_codec_label(codec: Option<&str>) -> Option<String> {
    let codec = codec.map(str::trim).filter(|c| !c.is_empty())?;
    Some(
        match codec.to_lowercase().as_str() {
            "hevc" | "h265" => "HEVC",
            "h264" | "avc1" => "H.264",
            "av1" => "AV1",
            "vp9" => "VP9",
            "vc1" => "VC-1",
            "mpeg2video" => "MPEG-2",
            _ => return Some(codec.to_uppercase()),
        }
        .to_owned(),
    )
}

/// The dynamic range **the file was mastered in**, as the coarse token
/// `detect_hdr` writes and every other DTO in this codebase carries.
///
/// This is a source fact, not a delivery one. plurx routinely delivers a
/// Dolby Vision source as its HDR10 base layer (an ffmpeg without `dovi_rpu`
/// cannot even strip the configuration — see `plurxd/src/ffmpeg.rs`), and a
/// client with no HDR display gets tone-mapped SDR. A list badge cannot know
/// any of that: it is drawn before a client, a display or a decision exists.
/// So this says what is on disk and the play path keeps owning what is sent.
/// `delivered_dynamic_range` (MEDIA-BADGES-PLAN.md §3.2) is the other half of
/// that pair, and it lives on `/decision` and `/hls/sessions` where a decision
/// actually exists — never here.
///
/// **This used to return a display label** ("DV", "HDR10+") because it was
/// ported from the web app's `hdrChip`. That was wrong, and subtly: it moved
/// labelling from the clients to the server, and the clients do not agree
/// about wording. Apple's badge builder prints "HDR" where `hdrChip` prints
/// "HDR10+", so an Apple grid card fed by this block would have contradicted
/// the Apple detail screen beneath it — the two-vocabularies drift this
/// module exists to prevent, introduced by the module preventing it. Emitting
/// the token plus the raw label instead means web can call `hdrChip` on this
/// block unmodified, Apple and Android keep their own wording, and a
/// source-vs-delivered comparison is string equality, as §3.2 intends.
///
/// **Multiple files**: there is no aggregate token here and this function does
/// not invent one. An item with a DV version and an HDR10 version reports the
/// dynamic range of its best file, because the whole block describes that one
/// file (see [`MediaFacts`]). Naming the union — "DV+HDR10", "mixed" — would
/// be a value outside the closed set §3.2 defines.
fn hdr_token(hdr: Option<&str>, hdr_format: Option<&str>) -> Option<String> {
    let coarse = hdr.map(str::trim).filter(|h| !h.is_empty());
    let rich = hdr_format.map(str::trim).filter(|f| !f.is_empty());
    // Dolby Vision from either column: the coarse one is what the decision
    // engine keys on, the rich one is what a v4-era backfill wrote.
    if coarse == Some("dolby_vision")
        || rich
            .map(|f| f.to_lowercase().contains("dolby"))
            .unwrap_or(false)
    {
        return Some("dolby_vision".to_owned());
    }
    if let Some(c) = coarse {
        return Some(c.to_lowercase());
    }
    // Coarse column missing — a file probed before the v4 migration backfilled
    // it. Recover the token from the rich label rather than dropping the badge;
    // detect_hdr_format's non-DV range is exactly these two.
    let r = rich?.to_lowercase();
    if r.starts_with("hdr10") {
        Some("hdr10".to_owned())
    } else if r.starts_with("hlg") {
        Some("hlg".to_owned())
    } else {
        None
    }
}

/// The one audio track worth naming on a card, as "codec channels".
///
/// Ranked by codec first and channel count second, which is `premiumAudio`'s
/// order (TrueHD beats DTS beats DD+) with the tie broken by channels. Codec
/// first is the deliberate half: a TrueHD 5.1 track is the reason someone
/// keeps that file, and ranking by channels first would hide it behind a
/// commentary-grade EAC3 7.1.
fn audio_label(streams: &[AudioStream]) -> Option<String> {
    let best = streams.iter().max_by_key(|s| {
        (
            audio_codec_rank(&s.codec),
            s.channels.unwrap_or(0),
            // Last resort: the track the muxer marked default, then the first
            // one. Stability again — never a coin flip between equal tracks.
            i64::from(s.default),
            -s.index,
        )
    })?;
    let codec = audio_codec_label(&best.codec)?;
    Some(match channel_label(best.channels) {
        Some(ch) => format!("{codec} {ch}"),
        None => codec,
    })
}

/// Lossless first, then object/lossy tiers. Only the order matters, never the
/// numbers; an unknown codec sorts last so a real one always wins.
fn audio_codec_rank(codec: &str) -> u8 {
    let c = codec.to_lowercase();
    if c.contains("truehd") || c.contains("mlp") {
        6
    } else if c.contains("dts") {
        5
    } else if c.contains("flac") || c.starts_with("pcm") {
        4
    } else if c.contains("eac3") {
        3
    } else if c.contains("ac3") {
        2
    } else if c.is_empty() {
        0
    } else {
        1
    }
}

fn audio_codec_label(codec: &str) -> Option<String> {
    let codec = codec.trim();
    if codec.is_empty() {
        return None;
    }
    let c = codec.to_lowercase();
    Some(
        if c.contains("truehd") {
            "TrueHD"
        } else if c.contains("dts") {
            "DTS"
        } else if c.contains("eac3") {
            "DD+"
        } else if c == "ac3" {
            "DD"
        } else if c.contains("flac") {
            "FLAC"
        } else if c.starts_with("pcm") {
            "PCM"
        } else if c == "opus" {
            "Opus"
        } else {
            return Some(codec.to_uppercase());
        }
        .to_owned(),
    )
}

/// Channel count to the spoken form, mirroring `fmtChannels`.
fn channel_label(channels: Option<i64>) -> Option<String> {
    match channels? {
        n if n <= 0 => None,
        1 => Some("Mono".to_owned()),
        2 => Some("2.0".to_owned()),
        6 => Some("5.1".to_owned()),
        7 => Some("6.1".to_owned()),
        8 => Some("7.1".to_owned()),
        n => Some(format!("{n}ch")),
    }
}

fn container_label(container: Option<&str>) -> Option<String> {
    container
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_uppercase)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(index: i64, codec: &str, channels: i64, default: bool) -> AudioStream {
        AudioStream {
            index,
            codec: codec.to_owned(),
            channels: Some(channels),
            language: Some("eng".into()),
            title: None,
            default,
        }
    }

    #[test]
    fn video_labels_match_the_clients_vocabulary() {
        assert_eq!(video_codec_label(Some("hevc")).as_deref(), Some("HEVC"));
        assert_eq!(video_codec_label(Some("h264")).as_deref(), Some("H.264"));
        assert_eq!(video_codec_label(Some("avc1")).as_deref(), Some("H.264"));
        assert_eq!(
            video_codec_label(Some("mpeg2video")).as_deref(),
            Some("MPEG-2")
        );
        // Unknown: upper-cased, not dropped.
        assert_eq!(video_codec_label(Some("prores")).as_deref(), Some("PRORES"));
        assert_eq!(video_codec_label(Some("  ")), None);
        assert_eq!(video_codec_label(None), None);
    }

    /// One vocabulary, and it is the wire's, not any client's. A DV file is
    /// `dolby_vision` whichever column says so; HDR10+ collapses to the same
    /// `hdr10` token `detect_hdr` writes, and the "+" survives in the label
    /// beside it rather than in a token nothing else in the codebase speaks.
    #[test]
    fn hdr_token_speaks_the_wire_vocabulary_not_a_badge_one() {
        assert_eq!(
            hdr_token(
                Some("dolby_vision"),
                Some("Dolby Vision · Profile 7 (HDR10-compatible)")
            )
            .as_deref(),
            Some("dolby_vision")
        );
        // Coarse column missing (a file probed before v4's backfill ran).
        assert_eq!(
            hdr_token(None, Some("Dolby Vision")).as_deref(),
            Some("dolby_vision")
        );
        // Rich column missing (a file probed before v4 existed at all).
        assert_eq!(
            hdr_token(Some("dolby_vision"), None).as_deref(),
            Some("dolby_vision")
        );
        // HDR10+ is not a token. detect_hdr never writes one, so neither do we.
        assert_eq!(
            hdr_token(Some("hdr10"), Some("HDR10+")).as_deref(),
            Some("hdr10")
        );
        assert_eq!(
            hdr_token(Some("hdr10"), Some("HDR10")).as_deref(),
            Some("hdr10")
        );
        assert_eq!(hdr_token(Some("hlg"), None).as_deref(), Some("hlg"));
        // Token recovered from the label alone, both non-DV shapes.
        assert_eq!(hdr_token(None, Some("HDR10+")).as_deref(), Some("hdr10"));
        assert_eq!(hdr_token(None, Some("HLG")).as_deref(), Some("hlg"));
        assert_eq!(hdr_token(None, None), None);
        assert_eq!(hdr_token(Some(""), Some("")), None);
    }

    /// The pairing is the point: the token is what code compares against
    /// `delivered_dynamic_range`, the label is what a client prints. Dropping
    /// either would push one platform into inventing the other.
    #[test]
    fn the_label_rides_along_verbatim_beside_the_token() {
        let facts = MediaFacts::from(FactsRow {
            files: 1,
            bytes: 1,
            container: None,
            video_codec: None,
            height: None,
            hdr: Some("hdr10".into()),
            hdr_format: Some("HDR10+".into()),
            audio: vec![],
        });
        assert_eq!(facts.hdr.as_deref(), Some("hdr10"));
        assert_eq!(facts.hdr_format.as_deref(), Some("HDR10+"));
    }

    #[test]
    fn audio_picks_the_headline_track_not_the_widest() {
        // TrueHD 5.1 over an EAC3 7.1 commentary: codec rank first.
        assert_eq!(
            audio_label(&[
                stream(0, "eac3", 8, true),
                stream(1, "truehd", 6, false),
                stream(2, "aac", 2, false),
            ])
            .as_deref(),
            Some("TrueHD 5.1")
        );
        // Same codec: channels break the tie.
        assert_eq!(
            audio_label(&[stream(0, "dts", 2, true), stream(1, "dts", 8, false)]).as_deref(),
            Some("DTS 7.1")
        );
        assert_eq!(
            audio_label(&[stream(0, "aac", 2, true)]).as_deref(),
            Some("AAC 2.0")
        );
        assert_eq!(
            audio_label(&[stream(0, "flac", 1, true)]).as_deref(),
            Some("FLAC Mono")
        );
        assert_eq!(
            audio_label(&[stream(0, "opus", 4, true)]).as_deref(),
            Some("Opus 4ch")
        );
        assert_eq!(audio_label(&[]), None);
        // A track with no codec name is not a label.
        assert_eq!(audio_label(&[stream(0, "", 6, true)]), None);
    }

    /// Two tracks that rank identically must not swap when the row order
    /// does: the badge would flicker between renders of the same page for no
    /// reason a user could explain.
    #[test]
    fn audio_choice_does_not_depend_on_stream_order() {
        let a = stream(0, "dts", 6, true);
        let b = stream(1, "dts", 6, true);
        assert_eq!(
            audio_label(&[a.clone(), b.clone()]),
            audio_label(&[b, a]),
            "the same tracks in a different order chose a different label"
        );
    }

    #[test]
    fn a_row_becomes_the_block_a_card_prints() {
        let facts = MediaFacts::from(FactsRow {
            files: 2,
            bytes: 75_800_000_000,
            container: Some("mkv".into()),
            video_codec: Some("hevc".into()),
            height: Some(2160),
            hdr: Some("dolby_vision".into()),
            hdr_format: Some("Dolby Vision · Profile 7 (HDR10-compatible)".into()),
            audio: vec![stream(0, "truehd", 8, true)],
        });
        assert_eq!(
            facts,
            MediaFacts {
                files: 2,
                bytes: 75_800_000_000,
                video: Some("HEVC".into()),
                height: Some(2160),
                hdr: Some("dolby_vision".into()),
                hdr_format: Some("Dolby Vision · Profile 7 (HDR10-compatible)".into()),
                audio: Some("TrueHD 7.1".into()),
                container: Some("MKV".into()),
            }
        );
    }

    /// A file that never probed still exists and still costs disk. It reports
    /// its size and nothing else, rather than being dropped (which would make
    /// an item's `bytes` quietly understate what is on the volume).
    #[test]
    fn an_unprobed_file_reports_size_and_no_labels() {
        let facts = MediaFacts::from(FactsRow {
            files: 1,
            bytes: 4_200_000_000,
            ..Default::default()
        });
        assert_eq!(facts.files, 1);
        assert_eq!(facts.bytes, 4_200_000_000);
        assert_eq!(facts.video, None);
        assert_eq!(facts.hdr, None);
        assert_eq!(facts.hdr_format, None);
        assert_eq!(facts.audio, None);
        assert_eq!(facts.container, None);
    }
}
