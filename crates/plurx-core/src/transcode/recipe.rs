//! What makes two transcodes the same transcode.
//!
//! The pre-transcode cache is content-addressed: a session computes the hash of
//! what it is about to produce, and if that hash is already on disk the work has
//! been done. Everything therefore rests on the hash meaning exactly one thing,
//! and the failure mode of getting it wrong is the worst kind there is — not an
//! error, but the wrong film, or the right film with the wrong subtitles burned
//! into it, served confidently and fast.
//!
//! Two rules follow, and both are load-bearing.
//!
//! **Everything that changes the bytes is in the key.** Not the ones that seem
//! important — all of them. A field added to the recipe and forgotten here is a
//! cache that serves stale output forever, and it will look like a bug in
//! whatever feature added the field.
//!
//! **Invalidation is by mismatch, never by deletion.** The source file's size
//! and mtime are in the key, so a file that changes simply stops matching its
//! old entry; nothing has to notice the change, and nothing can fail to. The
//! orphan ages out through LRU like anything else. A cache that has to be
//! *told* about a change is a cache that will one day not be told.

use sha2::{Digest, Sha256};

use crate::domain::MediaFile;

use super::{Encoder, SubtitleBurn, ToneMap, TranscodeOptions, SEGMENT_SECONDS};

/// Bumped when the meaning of a recipe changes in a way the field list cannot
/// express — a different hash construction, a corrected serialisation, a fixed
/// bug in what the fields *mean*. Every old entry misses; nothing is served
/// wrongly while a deploy rolls out.
const CACHE_FORMAT_VERSION: u32 = 2;

/// Everything about *how this server encodes* that changes the output bytes.
///
/// Separate from the per-session inputs because it is per-node and per-build,
/// and because it is the part reviewers were right to worry about (PERF-PLAN
/// §6.1, review R7): an ffmpeg upgrade that changes the tone-map operator
/// produces a different picture from an identical recipe. Without the build in
/// the key, every viewer keeps getting the old one until somebody notices by
/// eye.
///
/// The encoder family is in here too, and stays until QSV, VA-API, NVENC,
/// VideoToolbox and software are *demonstrated* to satisfy one declared output
/// contract (decision 6). Until then a QSV-produced entry is not a thing a
/// software node may serve, and pretending otherwise trades correctness for
/// a hit rate nobody measured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineDigest {
    /// `ffmpeg -version`'s first line, verbatim — build, not just version. A
    /// distribution's patched 6.1 and a jellyfin 6.1 are different encoders.
    pub ffmpeg_build: String,
    pub encoder: Encoder,
}

impl PipelineDigest {
    fn feed(&self, h: &mut Sha256) {
        field(h, "ffmpeg", self.ffmpeg_build.as_bytes());
        field(h, "encoder", self.encoder.label().as_bytes());
        field(h, "codec", self.encoder.video_codec().as_bytes());
        // The output contract this build produces, spelled out rather than
        // implied by the encoder name: a future change to any of these is a
        // different picture from the same recipe, and has to miss.
        field(h, "pixfmt", b"yuv420p");
        field(h, "colour", b"bt709/bt709/bt709/tv");
        field(h, "muxer", b"hls/mpegts");
        field(h, "segment_policy", SEGMENT_SECONDS.to_string().as_bytes());
    }
}

/// One transcode, as a cache key.
///
/// Built from the same values that build the ffmpeg command, in one place, so
/// the two cannot describe different things.
#[derive(Debug, Clone)]
pub struct Recipe<'a> {
    pub digest: &'a PipelineDigest,
    pub file: &'a MediaFile,
    pub opts: &'a TranscodeOptions,
    /// Whether the audio is copied rather than re-encoded. Not in
    /// [`TranscodeOptions`] because the transcode path always re-encodes;
    /// carried explicitly so the copy path can share this key space.
    pub audio_copied: bool,
}

impl Recipe<'_> {
    /// The hex SHA-256 that names this transcode's output.
    pub fn hash(&self) -> String {
        let mut h = Sha256::new();
        field(&mut h, "v", CACHE_FORMAT_VERSION.to_string().as_bytes());
        self.digest.feed(&mut h);
        // This occupied the same position with the same literal in the
        // manager-level digest before N1. Moving it here lets a per-title
        // effective quality value participate without changing one byte of a
        // legacy VBR recipe.
        field(
            &mut h,
            "rate_control",
            self.opts.effective_rate_control.recipe_value().as_bytes(),
        );

        // The source. Size and mtime are what make invalidation automatic: a
        // re-encoded or replaced file cannot match its old entry, whether or
        // not anyone tells the cache about it.
        field(&mut h, "file_id", self.file.id.to_string().as_bytes());
        field(&mut h, "size", self.file.size.to_string().as_bytes());
        field(&mut h, "mtime", self.file.mtime.to_string().as_bytes());
        field(
            &mut h,
            "audio_offset",
            self.file.audio_offset_ms.to_string().as_bytes(),
        );

        // What was asked for.
        let o = self.opts;
        field(&mut h, "height", o.target_height.to_string().as_bytes());
        field(
            &mut h,
            "vbitrate",
            o.video_bitrate_kbps.to_string().as_bytes(),
        );
        field(
            &mut h,
            "abitrate",
            o.audio_bitrate_kbps.to_string().as_bytes(),
        );
        field(&mut h, "achannels", o.audio_channels.to_string().as_bytes());
        field(
            &mut h,
            "aindex",
            o.audio_index.unwrap_or(-1).to_string().as_bytes(),
        );
        field(
            &mut h,
            "aaction",
            if self.audio_copied { b"copy" } else { b"aac" },
        );
        field(&mut h, "tonemap", tone_map_name(o.tone_map).as_bytes());
        // The effective per-session renderer, not the node's ordinary HDR10
        // winner. Dolby Vision Profile 5 deliberately runs a different graph;
        // hashing the boot winner would let differently rendered pixels share
        // one cache entry.
        field(&mut h, "pipeline", o.pipeline.name().as_bytes());
        field(
            &mut h,
            "burn",
            burn_key(o.subtitle_burn.as_ref()).as_bytes(),
        );

        // Deliberately NOT in the key: `start_seconds`. A cached asset is the
        // whole title; where a viewer joins it is a seek, not a different
        // encode. Including it would give one film as many entries as it has
        // resume points and never hit any of them twice.
        hex::encode(h.finalize())
    }
}

/// Length-prefixed, name-tagged field feeding.
///
/// Concatenating values would let two different recipes hash identically —
/// `height=108` + `bitrate=0…` and `height=1080` + `bitrate=…` are the same
/// bytes run together. The tag and the length make every field's boundary
/// unambiguous, which is the difference between a hash and a hope.
fn field(h: &mut Sha256, name: &str, value: &[u8]) {
    h.update((name.len() as u32).to_le_bytes());
    h.update(name.as_bytes());
    h.update((value.len() as u32).to_le_bytes());
    h.update(value);
}

fn tone_map_name(t: ToneMap) -> &'static str {
    match t {
        ToneMap::Zscale => "zscale",
        ToneMap::Libplacebo => "libplacebo",
        ToneMap::None => "none",
    }
}

/// A burn's identity: which track, whether it is a picture, and whether it
/// actually happened.
///
/// `applied` is always true today — every requested burn is performed since
/// bitmap subtitles gained an overlay graph. The field stays because a future
/// path that *cannot* burn must not be able to hash as though it did: an entry
/// that claims subtitles it does not have is served to somebody who asked for
/// them and gets none, with no error anywhere.
fn burn_key(burn: Option<&SubtitleBurn>) -> String {
    match burn {
        None => "none".to_owned(),
        Some(b) => format!(
            "{}:{}:applied",
            b.subtitle_index,
            if b.bitmap { "bitmap" } else { "text" }
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcode::EffectiveRateControl;
    use std::path::PathBuf;

    fn digest() -> PipelineDigest {
        PipelineDigest {
            ffmpeg_build: "ffmpeg version 6.1.1-3ubuntu5".to_owned(),
            encoder: Encoder::Software,
        }
    }

    fn media() -> MediaFile {
        MediaFile {
            id: 42,
            item_id: 7,
            path: PathBuf::from("/media/Heat.mkv"),
            size: 12_345_678,
            mtime: 1_700_000_000,
            duration_ms: Some(9_000_000),
            container: Some("mkv".into()),
            video_codec: Some("hevc".into()),
            video_profile: None,
            width: Some(3840),
            height: Some(2160),
            bit_depth: Some(10),
            hdr: Some("hdr10".into()),
            hdr_format: None,
            bitrate: Some(60_000_000),
            audio_streams: vec![],
            subtitle_streams: vec![],
            scanned_at: 0,
            audio_offset_ms: 0,
            probed: true,
        }
    }

    fn hash_of(d: &PipelineDigest, f: &MediaFile, o: &TranscodeOptions, copied: bool) -> String {
        Recipe {
            digest: d,
            file: f,
            opts: o,
            audio_copied: copied,
        }
        .hash()
    }

    /// The property the whole cache rests on: anything that changes the output
    /// bytes changes the name of those bytes.
    ///
    /// Written as a table because the failure it guards against is *omission* —
    /// a field added to the recipe and not to the hash — and an omission is
    /// invisible in code that only checks the fields somebody remembered. Each
    /// case here is a byte of output that would otherwise be served from the
    /// wrong entry.
    #[test]
    fn every_input_that_changes_the_output_changes_the_hash() {
        let base_d = digest();
        let base_f = media();
        let base_o = TranscodeOptions::default();
        let base = hash_of(&base_d, &base_f, &base_o, false);

        type Mutate = (
            &'static str,
            fn(&mut PipelineDigest, &mut MediaFile, &mut TranscodeOptions, &mut bool),
        );
        let cases: &[Mutate] = &[
            // The build and the pipeline: an ffmpeg upgrade or a different
            // tone-map graph is a different picture from the same recipe.
            ("ffmpeg build", |d, _, _, _| {
                d.ffmpeg_build = "ffmpeg version 7.1".into()
            }),
            ("encoder family", |d, _, _, _| d.encoder = Encoder::Qsv),
            ("pipeline", |_, _, o, _| {
                o.pipeline = crate::transcode::Pipeline::VppQsv
            }),
            // The source. Size and mtime are the invalidation mechanism.
            ("file id", |_, f, _, _| f.id = 43),
            ("size", |_, f, _, _| f.size += 1),
            ("mtime", |_, f, _, _| f.mtime += 1),
            ("audio offset", |_, f, _, _| f.audio_offset_ms = 250),
            // What was asked for.
            ("height", |_, _, o, _| o.target_height = 720),
            ("video bitrate", |_, _, o, _| o.video_bitrate_kbps += 1),
            ("effective rate control", |_, _, o, _| {
                o.effective_rate_control = EffectiveRateControl::Qvbr { quality: 23 }
            }),
            ("audio bitrate", |_, _, o, _| o.audio_bitrate_kbps += 1),
            ("audio channels", |_, _, o, _| o.audio_channels = 6),
            ("audio index", |_, _, o, _| o.audio_index = Some(1)),
            ("audio action", |_, _, _, c| *c = true),
            ("tone map", |_, _, o, _| o.tone_map = ToneMap::None),
            ("subtitle burn", |_, _, o, _| {
                o.subtitle_burn = Some(SubtitleBurn {
                    subtitle_index: 0,
                    bitmap: true,
                })
            }),
        ];

        let mut seen = std::collections::HashMap::new();
        seen.insert(base.clone(), "unchanged");
        for (name, mutate) in cases {
            let (mut d, mut f, mut o, mut c) =
                (base_d.clone(), base_f.clone(), base_o.clone(), false);
            mutate(&mut d, &mut f, &mut o, &mut c);
            let got = hash_of(&d, &f, &o, c);
            assert_ne!(got, base, "changing the {name} did not change the hash");
            if let Some(other) = seen.insert(got, name) {
                panic!("{name} and {other} hash the same — one of them is not in the key");
            }
        }
    }

    /// Two burns of different tracks are different pictures, and a text burn
    /// is not a bitmap burn of the same index.
    #[test]
    fn a_burn_is_identified_by_which_track_and_what_kind() {
        let (d, f) = (digest(), media());
        let with = |idx, bitmap| {
            let o = TranscodeOptions {
                subtitle_burn: Some(SubtitleBurn {
                    subtitle_index: idx,
                    bitmap,
                }),
                ..Default::default()
            };
            hash_of(&d, &f, &o, false)
        };
        assert_ne!(with(0, true), with(1, true), "different track");
        assert_ne!(with(0, true), with(0, false), "different kind");
        assert_ne!(
            with(0, false),
            hash_of(&d, &f, &TranscodeOptions::default(), false),
            "burning something is not the same as burning nothing"
        );
    }

    /// Where a viewer joins a title is not a property of the title. Including
    /// the start offset would give one film as many cache entries as it has
    /// resume points, and hit none of them twice.
    #[test]
    fn resume_position_is_not_part_of_the_identity() {
        let (d, f) = (digest(), media());
        let at = |secs| {
            let o = TranscodeOptions {
                start_seconds: secs,
                ..Default::default()
            };
            hash_of(&d, &f, &o, false)
        };
        assert_eq!(at(0.0), at(1234.5));
    }

    /// Fields are length-prefixed so their boundaries cannot blur. Without
    /// that, two recipes whose values run together into the same bytes hash
    /// identically — and the one that gets served is whichever was cached
    /// first.
    #[test]
    fn adjacent_fields_cannot_be_confused_for_one_another() {
        let (d, f) = (digest(), media());
        let opts = |h: i64, b: u32| TranscodeOptions {
            target_height: h,
            video_bitrate_kbps: b,
            ..Default::default()
        };
        // "108" + "01000" vs "1080" + "1000": identical concatenated.
        assert_ne!(
            hash_of(&d, &f, &opts(108, 1000), false),
            hash_of(&d, &f, &opts(1080, 1000), false)
        );
        let mut a = f.clone();
        let mut b = f.clone();
        a.size = 1;
        a.mtime = 23;
        b.size = 12;
        b.mtime = 3;
        assert_ne!(
            hash_of(&d, &a, &opts(1080, 8000), false),
            hash_of(&d, &b, &opts(1080, 8000), false)
        );
    }

    /// Same inputs, same name — the whole point. Stable across runs and
    /// processes, since it addresses bytes on disk that outlive both.
    #[test]
    fn the_same_recipe_always_has_the_same_name() {
        let (d, f, o) = (digest(), media(), TranscodeOptions::default());
        let first = hash_of(&d, &f, &o, false);
        assert_eq!(first, hash_of(&d, &f, &o, false));
        assert_eq!(first.len(), 64, "hex sha-256");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// Version 2 intentionally invalidates version 1: the effective session
    /// renderer now replaces the node's generic boot pipeline in the key, so
    /// Profile 5 output made by the old relabel route can never be reused.
    /// Pin the new bytes so any later fleet-wide invalidation stays explicit.
    #[test]
    fn legacy_vbr_recipe_hash_is_a_golden_fixture() {
        let (d, f, o) = (digest(), media(), TranscodeOptions::default());
        assert_eq!(
            hash_of(&d, &f, &o, false),
            "b9dae1c44d396f94c760a627b7a3b709ec591db227c162eb5588d91420f767c6"
        );
    }

    #[test]
    fn every_effective_quality_value_has_a_distinct_identity() {
        let (d, f) = (digest(), media());
        let hash = |quality| {
            let o = TranscodeOptions {
                effective_rate_control: EffectiveRateControl::Qvbr { quality },
                ..Default::default()
            };
            hash_of(&d, &f, &o, false)
        };
        let vbr = hash_of(&d, &f, &TranscodeOptions::default(), false);
        assert_ne!(hash(22), vbr);
        assert_ne!(hash(23), vbr);
        assert_ne!(hash(22), hash(23));
    }
}
