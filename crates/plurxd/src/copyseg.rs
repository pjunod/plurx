//! The GOP-aware copy segmenter: plurx cuts the segments, not ffmpeg.
//!
//! ffmpeg writes one continuous fragmented MP4 down a pipe (one fragment per
//! GOP, `crate::transcode` spawns it with [`plurx_core::transcode::copy_pipe_args`]);
//! this task reads that stream, and publishes a segment boundary only in front
//! of a fragment whose first frame is a true random-access point. Everything
//! it writes — `init.mp4`, `segNNNNN.m4s`, `index.m3u8` — has the same names,
//! the same shape and the same tmp-then-rename semantics ffmpeg's HLS muxer
//! gave it, so the serving layer, the segment index, the ahead-window suspend
//! and the GC all carry on unchanged. The playlist is the interface — which
//! is what lets the publish gate hold it back until a cushion of media
//! exists ([`plurx_core::transcode::COPY_PUBLISH_GATE_SECS`]): until the
//! playlist is out, the rest of the daemon agrees nothing has happened.
//!
//! Why bother: every boundary on an open-GOP copy costs exactly one dropped
//! frame, because a player treats a segment's first keyframe as a
//! random-access point and the HEVC spec says to discard the leading pictures
//! there (docs/STUTTER-4K.md §5.3ter). The cutting is the only part of a copy
//! that plurx can change, so the cutting is what changes.
//!
//! The decision logic is all in [`plurx_core::fmp4`] and is pure. What lives
//! here is the I/O and the fallback: if the stream turns out to be one this
//! reader cannot follow, the session kills the pipe and respawns on ffmpeg's
//! own muxer, so the worst case of any surprise is exactly today's behaviour.

use std::path::PathBuf;

use plurx_core::fmp4::{
    self, FragmentReader, Init, Published, SegmentCounts, Segmenter, TrackKind, Unit,
};
use plurx_core::transcode::{
    COPY_FIRST_SEGMENT_SECONDS, COPY_PUBLISH_GATE_SECS, COPY_SEGMENT_MAX_BYTES,
    COPY_SEGMENT_MAX_SECS, COPY_SEGMENT_SECONDS,
};
use tokio::io::{AsyncRead, AsyncReadExt};

/// How much pipe is read at a time. Big enough that a 12 MB/s copy is not a
/// syscall storm, small enough that a SIGSTOPped ffmpeg leaves the reader
/// parked in one `read` rather than holding a large buffer.
const READ_CHUNK: usize = 256 * 1024;

/// A reader holding more than this has stopped making sense: the byte ceiling
/// bounds a published segment, so the only bytes in hand should be one pending
/// segment plus one fragment in flight. Exceeding it is logged once — it does
/// not stop the session, because a stream that is merely unusual is still a
/// stream someone is watching.
///
/// What a healthy session actually costs, since the number should be honest:
/// the pending fragments (up to the floor's worth of media — ~52 MB at the
/// 69 Mb/s reference bitrate) plus, for the instant the merge runs, a second
/// copy of the same bytes in the merged buffer. So peak is roughly 2× a
/// segment, ~105 MB, and this threshold sits above that on purpose: it is
/// meant to catch a policy that has stopped cutting, not to complain about
/// the copy every merge makes.
const MEMORY_WARN_BYTES: usize = 160 * 1024 * 1024;

/// The floor and the two ceilings, as a session sees them.
///
/// A struct rather than three constants read at the point of use, because the
/// tests need to reach a ceiling without a 4K file: the policy is the thing
/// under test, and a 48 MB ceiling is not reachable from a 12-second fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub floor_seconds: u32,
    /// The floor for the first segment only — see
    /// [`plurx_core::fmp4::CutPolicy::first_floor_ticks`].
    pub first_floor_seconds: u32,
    pub max_bytes: usize,
    pub max_seconds: u32,
    /// Hold `index.m3u8` until this many seconds of media exist, so playback
    /// starts behind a cushion instead of at the live edge — the whole story
    /// is on [`plurx_core::transcode::COPY_PUBLISH_GATE_SECS`]. Seconds, not
    /// a segment count: a count multiplies by whatever the opening cuts, and
    /// a quiet opening cuts ceiling-length segments. End of stream overrides
    /// the gate: a film shorter than the cushion publishes whole.
    pub publish_gate_secs: u32,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            floor_seconds: COPY_SEGMENT_SECONDS,
            first_floor_seconds: COPY_FIRST_SEGMENT_SECONDS,
            max_bytes: COPY_SEGMENT_MAX_BYTES,
            max_seconds: COPY_SEGMENT_MAX_SECS,
            publish_gate_secs: COPY_PUBLISH_GATE_SECS,
        }
    }
}

/// How a segmenter session ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The stream was not one this reader could follow, and no player ever
    /// saw output — the playlist was never published, whatever segment files
    /// the publish gate was still holding invisible. The caller respawns on
    /// the legacy muxer — once — clearing the directory first.
    ///
    /// Separate from every other failure on purpose: this is the only ending
    /// where falling back is both possible and correct. Once the playlist is
    /// out, a respawn would rewrite a timeline a player is already holding.
    Unsupported(String),
    /// It ran. The counts are what it published, whether it reached the end of
    /// the film or the session was killed under it.
    ///
    /// Returning stops the reader, which drops the `ChildStdout` the caller
    /// handed over — ffmpeg then takes EPIPE on its next muxer write and
    /// exits. That is how a segmenter session ends its own ffmpeg, and it is
    /// why no path here needs to kill the child itself.
    Ran(SegmentCounts),
}

/// Everything a session writes, and the playlist it keeps in step.
///
/// The playlist is rewritten whole on every segment rather than appended to,
/// because a reader must never see it half-written: the file is built in
/// memory, written to `index.m3u8.tmp` and renamed over. At 6 s a segment a
/// three-hour film is 1800 lines — rewriting 50 KB per segment costs nothing
/// next to the 50 MB of segment that triggered it.
///
/// The first write is gated: segments land on disk from the start, but
/// `index.m3u8` is withheld until `gate_secs` of media exist, so the first
/// playlist a player loads already holds a cushion and playback starts behind
/// the live edge rather than at it
/// ([`plurx_core::transcode::COPY_PUBLISH_GATE_SECS`]). While the gate is
/// closed nothing downstream knows the segments exist — the segment index,
/// the ahead-window suspend and the GC all read the playlist — and nothing
/// can be served from them, because a client only learns names from the
/// playlist too.
struct SessionDir {
    dir: PathBuf,
    /// The `#EXTINF`/URI pairs, without the header. The header is regenerated
    /// on every write because `TARGETDURATION` grows with the longest segment
    /// published so far.
    entries: String,
    /// Media written so far, summed from every published segment's real
    /// duration — what the publish gate measures.
    published_secs: f64,
    /// Withhold `index.m3u8` until this many seconds of media exist.
    gate_secs: u32,
    /// `ceil` of the longest `EXTINF` so far. Never decreases.
    target_duration: u32,
    /// The playlist is on disk — the moment a player could be holding this
    /// timeline, and so the moment the legacy fallback stops being safe.
    started: bool,
}

impl SessionDir {
    fn new(dir: PathBuf, gate_secs: u32) -> SessionDir {
        SessionDir {
            dir,
            entries: String::new(),
            published_secs: 0.0,
            gate_secs,
            target_duration: 0,
            started: false,
        }
    }

    fn playlist(&self, end: bool) -> String {
        let mut out = fmp4::playlist_header(self.target_duration.max(1));
        out.push_str(&self.entries);
        if end {
            out.push_str("#EXT-X-ENDLIST\n");
        }
        out
    }

    /// tmp + rename, the semantics the whole daemon assumes: a segment or a
    /// playlist is either absent or complete, never partial.
    async fn publish_file(&self, name: &str, bytes: &[u8]) -> std::io::Result<()> {
        let tmp = self.dir.join(format!("{name}.tmp"));
        tokio::fs::write(&tmp, bytes).await?;
        tokio::fs::rename(&tmp, self.dir.join(name)).await
    }

    async fn write_init(&mut self, init: &Init) -> std::io::Result<()> {
        self.publish_file("init.mp4", &init.bytes).await?;
        // No playlist yet: one with no segment in it is a promise the session
        // cannot keep if ffmpeg dies in the next second, and
        // `session_producing` reads exactly this file to decide whether there
        // is real output. It lands when the publish gate opens.
        Ok(())
    }

    async fn write_segment(&mut self, published: &Published) -> std::io::Result<()> {
        let name = published.name();
        self.publish_file(&name, &published.segment.bytes).await?;
        self.entries
            .push_str(&fmp4::playlist_entry(published.seconds, &name));
        self.published_secs += published.seconds;
        // Grows, never shrinks. A client that read a smaller number and then
        // met a longer segment would be entitled to complain; one that read a
        // number far larger than any real segment waits that long between
        // playlist fetches, which is the stall this replaced.
        let need = published.seconds.ceil().max(1.0) as u32;
        self.target_duration = self.target_duration.max(need);
        // The publish gate. The segment is on disk; whether the world learns
        // of it is a separate decision, taken once: until `gate_secs` of
        // media exist there is no playlist at all, and the first one a player
        // loads already lists the whole cushion.
        if !self.started && self.published_secs < f64::from(self.gate_secs) {
            return Ok(());
        }
        self.started = true;
        let text = self.playlist(false);
        self.publish_file("index.m3u8", text.as_bytes()).await
    }

    async fn write_endlist(&mut self) -> std::io::Result<()> {
        if self.entries.is_empty() {
            return Ok(());
        }
        // End of stream opens the gate no matter how few segments exist: a
        // film shorter than the cushion still has to play, and a complete
        // playlist with its ENDLIST is a *better* first read than a live one
        // — the client sees VOD from the start.
        self.started = true;
        let text = self.playlist(true);
        self.publish_file("index.m3u8", text.as_bytes()).await
    }
}

/// Did this write fail because the session was torn down under us?
///
/// Every teardown path removes the session directory (`Session::discard_dir`),
/// and the reader can still be finalizing when it does — a viewer who presses
/// stop, or a player that supersedes its own session, races the last segment
/// out of existence. That is a session ending normally, not a fault, and
/// logging it at ERROR taught the log to cry wolf on the most ordinary event
/// there is. Observed within an hour of the first deploy.
fn session_gone(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::NotFound
}

/// Read `src` to exhaustion, publishing segments into `dir`.
///
/// Generic over the source so the tests can drive a whole session from a byte
/// slice: everything this does between the pipe and the disk is worth testing,
/// and none of it needs a real child process to be worth testing.
pub async fn run<R: AsyncRead + Unpin>(
    mut src: R,
    dir: PathBuf,
    session_id: &str,
    limits: Limits,
) -> Outcome {
    let mut reader = FragmentReader::new();
    let mut out = SessionDir::new(dir, limits.publish_gate_secs);
    // Hold the initialization segment until the first video sample arrives.
    // ffmpeg may put HDR10's static SEIs only in that sample; Apple needs the
    // same records in hvcC before it will accept a PQ HLS variant.
    let mut pending_init: Option<(Init, fmp4::CutPolicy)> = None;
    let mut segmenter: Option<Segmenter> = None;
    let mut buf = vec![0u8; READ_CHUNK];
    let mut warned_memory = false;

    loop {
        let n = match src.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                // A killed child closes the pipe under us. That is a session
                // ending, not a fault to shout about.
                tracing::debug!(session = %session_id, "copy segmenter pipe read: {e}");
                break;
            }
        };
        reader.push(&buf[..n]);

        loop {
            let unit = match reader.next_unit() {
                Ok(Some(unit)) => unit,
                Ok(None) => break,
                Err(e) => {
                    // The fallback door is open until the playlist is out —
                    // not until the first segment. Segments on disk that no
                    // player has ever been told about are not a timeline
                    // anyone holds; the respawn clears the directory and
                    // recuts, and the viewer never learns anything happened.
                    // The publish gate widens this window on purpose.
                    if !out.started {
                        return Outcome::Unsupported(format!("{e}"));
                    }
                    // Past the playlist: the door is shut, because a player
                    // may be holding segments from this timeline. Stop
                    // reading and let the session end like any ffmpeg that
                    // died — the playlist keeps what was real, without an
                    // ENDLIST claiming the film finished here.
                    tracing::error!(
                        session = %session_id,
                        "copy segmenter lost the fragment stream: {e}"
                    );
                    return finish(segmenter, &mut out, session_id, false).await;
                }
            };
            match unit {
                Unit::Init(mut init) => {
                    let Some(video) = init.video() else {
                        return Outcome::Unsupported("the pipe's moov has no video track".into());
                    };
                    if video.codec.is_none() || video.nal_length_size == 0 {
                        return Outcome::Unsupported(
                            "the video track carries no hvcC/avcC to read keyframes with".into(),
                        );
                    }
                    // A copy session maps exactly one video stream and at most
                    // one audio stream, so anything else in the `moov` is
                    // something ffmpeg added on its own — a chapter `text`
                    // track is what it was the first time, and Safari refused
                    // the stream over it while Chrome played on. Falling back
                    // is the right answer to a shape this path did not ask
                    // for: the legacy muxer's output is known to be playable,
                    // and a warning names what appeared.
                    if let Some(odd) = init.tracks.iter().find(|t| t.kind == TrackKind::Other) {
                        return Outcome::Unsupported(format!(
                            "the pipe declared a track this path never asked for \
                             (id {}, timescale {})",
                            odd.id, odd.timescale
                        ));
                    }
                    let video_timescale = video.timescale;
                    sanitize_stale_dolby_brand(&mut init);
                    let policy = fmp4::CutPolicy::new(
                        limits.floor_seconds,
                        limits.first_floor_seconds,
                        limits.max_bytes,
                        limits.max_seconds,
                        video_timescale,
                    );
                    pending_init = Some((init, policy));
                }
                Unit::Fragment(fragment) => {
                    if segmenter.is_none() {
                        let Some((mut init, policy)) = pending_init.take() else {
                            return Outcome::Unsupported(
                                "a fragment arrived before the moov".into(),
                            );
                        };
                        match fmp4::promote_hdr10_static_metadata(&mut init, &fragment) {
                            Ok(true) => tracing::info!(
                                session = %session_id,
                                "promoted HDR10 static metadata into the HLS init segment"
                            ),
                            Ok(false) => {}
                            Err(e) => {
                                return Outcome::Unsupported(format!(
                                    "preparing the HLS init segment: {e}"
                                ));
                            }
                        }
                        if let Err(e) = out.write_init(&init).await {
                            return Outcome::Unsupported(format!("writing init.mp4: {e}"));
                        }
                        segmenter = Some(Segmenter::new(init, policy));
                    }
                    let Some(seg) = segmenter.as_mut() else {
                        unreachable!("the first fragment constructs the segmenter");
                    };
                    match seg.push(fragment) {
                        Ok(Some(published)) => {
                            if let Err(e) = out.write_segment(&published).await {
                                if session_gone(&e) {
                                    tracing::debug!(
                                        session = %session_id,
                                        "session directory went away mid-write; stopping"
                                    );
                                } else {
                                    tracing::error!(
                                        session = %session_id,
                                        "writing {}: {e}", published.name()
                                    );
                                }
                                return Outcome::Ran(seg.counts());
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            // Same door as the reader's: open until the
                            // playlist is out, shut after.
                            if !out.started {
                                return Outcome::Unsupported(format!("{e}"));
                            }
                            tracing::error!(session = %session_id, "merging a segment: {e}");
                            return Outcome::Ran(seg.counts());
                        }
                    }
                    if !warned_memory {
                        let held = seg.pending_bytes() + reader.buffered();
                        if held > MEMORY_WARN_BYTES {
                            warned_memory = true;
                            tracing::warn!(
                                session = %session_id, held_bytes = held,
                                "copy segmenter is holding more than a segment's worth of \
                                 bytes; the byte ceiling should have cut before here"
                            );
                        }
                    }
                }
                // ffmpeg's random-access index, written at EOF. It describes
                // a file nobody will ever open; publishing it would append
                // bytes to a segment that is already complete.
                Unit::Trailer => {}
            }
        }
    }

    // A clean end is one where everything sent was consumed: either ffmpeg
    // wrote its `mfra` trailer, or the pipe closed on a fragment boundary. A
    // truncated fragment left in hand means the child was killed mid-write,
    // and an ENDLIST there would tell the player the film ends early.
    let complete = reader.saw_trailer() || reader.buffered() == 0;
    if segmenter.is_none() && pending_init.is_some() {
        return Outcome::Unsupported("the pipe ended before its first media fragment".into());
    }
    finish(segmenter, &mut out, session_id, complete).await
}

/// Remove a Dolby Vision file-type claim after ffmpeg has removed the Dolby
/// configuration and RPUs from the video sample entry.
///
/// ffmpeg 7.1's `dovi_rpu=strip=1` correctly removes `dvcC`/`dvvC` and the RPU
/// NALs, but movenc has already copied `dby1` into `ftyp` by the time the
/// bitstream filter's output parameters reach it. AVPlayer treats that as a
/// contradictory initialization segment and fails the item immediately. The
/// base layer is ordinary `hvc1` HDR10, so replace only the stale file-type
/// brand with the ISO fragmented-MP4 brand already used by this muxer. A real
/// Dolby Vision sample entry keeps its brand untouched.
fn sanitize_stale_dolby_brand(init: &mut Init) -> bool {
    if init.video().is_none_or(|video| video.dolby_vision_config) {
        return false;
    }

    let bytes = &mut init.bytes;
    if bytes.len() < 16 || &bytes[4..8] != b"ftyp" {
        return false;
    }
    let size = u32::from_be_bytes(bytes[0..4].try_into().expect("four-byte ftyp size")) as usize;
    if size < 16 || size > bytes.len() || !(size - 16).is_multiple_of(4) {
        return false;
    }

    let mut changed = false;
    for offset in std::iter::once(8).chain((16..size).step_by(4)) {
        if &bytes[offset..offset + 4] == b"dby1" {
            bytes[offset..offset + 4].copy_from_slice(b"iso6");
            changed = true;
        }
    }
    changed
}

async fn finish(
    segmenter: Option<Segmenter>,
    out: &mut SessionDir,
    session_id: &str,
    complete: bool,
) -> Outcome {
    let Some(mut seg) = segmenter else {
        return Outcome::Unsupported("the pipe ended before its moov arrived".into());
    };
    if complete {
        // `#EXT-X-ENDLIST` says "this is the whole film". Only write it if the
        // last segment actually landed — a playlist terminated one segment
        // short of what was produced tells the player it has everything while
        // the picture stops early, which is worse than a playlist that simply
        // stops growing.
        let final_ok = match seg.finish() {
            Ok(published) => {
                let mut wrote_all = true;
                for segment in published {
                    match out.write_segment(&segment).await {
                        Ok(()) => {}
                        Err(e) if session_gone(&e) => {
                            tracing::debug!(
                                session = %session_id,
                                "session directory went away before the final segment; stopping"
                            );
                            wrote_all = false;
                            break;
                        }
                        Err(e) => {
                            tracing::error!(
                                session = %session_id,
                                "writing a final segment: {e}"
                            );
                            wrote_all = false;
                            break;
                        }
                    }
                }
                wrote_all
            }
            Err(e) => {
                tracing::error!(session = %session_id, "merging the final segment: {e}");
                false
            }
        };
        if final_ok {
            if let Err(e) = out.write_endlist().await {
                if session_gone(&e) {
                    tracing::debug!(session = %session_id, "session gone before the playlist end");
                } else {
                    tracing::error!(session = %session_id, "writing the playlist end: {e}");
                }
            }
        }
    }
    let counts = seg.counts();
    if counts.segments == 0 {
        return Outcome::Unsupported("the pipe ended before a single segment".into());
    }
    Outcome::Ran(counts)
}

/// The end-of-session line `scripts/perf-report` greps.
///
/// One line, one session, every number the cut policy produced — including the
/// ones that are bad news. A ceiling cut still costs the leading picture, and
/// a residual nobody counts is a residual nobody fixes.
pub fn summary(counts: &SegmentCounts) -> String {
    format!(
        "copy segmenter: segments {} · clean cuts {} · ceiling cuts {} · \
         fragments {} · unparseable {} · repaired joins {}",
        counts.segments,
        counts.clean_cuts,
        counts.ceiling_cuts,
        counts.fragments,
        counts.unparseable,
        counts.tfdt_adjustments,
    )
}

/// Whether the segmenter can be attempted for a source at all.
///
/// The classifier reads HEVC and H.264 keyframes; anything else would be
/// `Unparseable` on every fragment, which works (nothing is ever cut in front
/// of an unparseable fragment) but would spend every session running the byte
/// ceiling. Better to not start.
pub fn supports(video_codec: Option<&str>) -> bool {
    matches!(video_codec, Some("hevc" | "h265" | "h264" | "avc"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use plurx_core::fmp4::{CutReason, Track, VideoCodec};
    use plurx_core::testfixtures::pipe;
    use std::path::Path;

    /// Long enough to reach a ceiling from a 12 s fixture. The floor is 3 s
    /// rather than the shipped 6 s for the same reason: the property under
    /// test is which keyframe a cut lands on, not what the constant is. The
    /// publish gate is 0 — playlist from the first segment, the pre-gate
    /// behaviour — because these tests observe cut placement, not
    /// publication; the gate has its own tests.
    fn brisk() -> Limits {
        Limits {
            floor_seconds: 3,
            first_floor_seconds: 1,
            max_bytes: 48_000_000,
            max_seconds: 15,
            publish_gate_secs: 0,
        }
    }

    async fn session(kind: &str, limits: Limits) -> (tempfile::TempDir, Outcome) {
        let dir = tempfile::tempdir().expect("tempdir");
        let feed = pipe(kind);
        let outcome = run(&feed[..], dir.path().to_path_buf(), "test", limits).await;
        (dir, outcome)
    }

    fn playlist(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("index.m3u8")).unwrap_or_default()
    }

    fn segment_files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("session dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn init_with_dolby_brand(dolby_vision_config: bool) -> Init {
        // The exact `ftyp` shape jellyfin-ffmpeg 7.1 writes after
        // dovi_rpu=strip=1: ordinary ISO brands plus the stale dby1 claim.
        let bytes = Vec::from(&b"\0\0\0\x20ftypiso5\0\0\x02\0iso5iso6dby1mp41"[..]);
        assert_eq!(bytes.len(), 32);
        Init {
            bytes,
            tracks: vec![Track {
                id: 1,
                kind: TrackKind::Video,
                timescale: 24_000,
                codec: Some(VideoCodec::Hevc),
                dolby_vision_config,
                nal_length_size: 4,
                default_sample_duration: 0,
                default_sample_size: 0,
                default_sample_flags: 0,
            }],
        }
    }

    #[test]
    fn stripped_dolby_vision_init_drops_the_stale_dby1_brand() {
        let mut init = init_with_dolby_brand(false);
        assert!(sanitize_stale_dolby_brand(&mut init));
        assert!(!init.bytes.windows(4).any(|fourcc| fourcc == b"dby1"));
        assert_eq!(&init.bytes[24..28], b"iso6");
    }

    #[test]
    fn preserved_dolby_vision_init_keeps_its_dby1_brand() {
        let mut init = init_with_dolby_brand(true);
        assert!(!sanitize_stale_dolby_brand(&mut init));
        assert!(init.bytes.windows(4).any(|fourcc| fourcc == b"dby1"));
    }

    /// The whole point, end to end: given a source that offers clean cut
    /// points, every boundary lands on one.
    #[tokio::test]
    async fn a_session_on_a_source_with_clean_points_publishes_only_clean_cuts() {
        let (dir, outcome) = session("clean-cra", brisk()).await;
        let Outcome::Ran(counts) = outcome else {
            panic!("the segmenter did not run: {outcome:?}");
        };
        assert!(counts.segments >= 2, "{counts:?}");
        assert_eq!(
            counts.ceiling_cuts, 0,
            "a ceiling cut with clean points available"
        );
        assert_eq!(counts.unparseable, 0);
        // Every cut but the last (which ended because the film did) is clean.
        assert_eq!(counts.clean_cuts, counts.segments - 1);

        let text = playlist(dir.path());
        assert!(text.ends_with("#EXT-X-ENDLIST\n"), "{text}");
        assert_eq!(
            text.matches(".m4s\n").count() as u64,
            counts.segments,
            "the playlist and the counts disagree"
        );
        for i in 0..counts.segments {
            assert!(dir.path().join(format!("seg{i:05}.m4s")).exists());
        }
        assert!(dir.path().join("init.mp4").exists());
    }

    /// And the honest half: a source with no clean point still makes progress,
    /// and the counts say what it cost.
    #[tokio::test]
    async fn a_source_with_no_clean_points_cuts_at_the_ceiling_and_says_so() {
        let limits = Limits {
            floor_seconds: 3,
            first_floor_seconds: 1,
            max_bytes: 300_000,
            max_seconds: 15,
            publish_gate_secs: 0,
        };
        let (dir, outcome) = session("open-gop", limits).await;
        let Outcome::Ran(counts) = outcome else {
            panic!("the segmenter did not run: {outcome:?}");
        };
        assert!(counts.ceiling_cuts >= 1, "{counts:?}");
        assert_eq!(counts.clean_cuts, 0, "there was no clean point to find");
        let line = summary(&counts);
        assert!(
            line.contains(&format!("ceiling cuts {}", counts.ceiling_cuts)),
            "the log line hides the cuts that still cost a frame: {line}"
        );
        assert!(playlist(dir.path()).ends_with("#EXT-X-ENDLIST\n"));
    }

    /// The playlist is the interface to everything downstream — the session's
    /// segment index, the ahead-window suspend, the GC, perf-report. Feed it
    /// to the parser the producer uses on real cached parts.
    #[tokio::test]
    async fn the_playlist_matches_the_template_and_produce_can_parse_it() {
        let (dir, outcome) = session("clean-cra", brisk()).await;
        let Outcome::Ran(counts) = outcome else {
            panic!("{outcome:?}");
        };
        let text = playlist(dir.path());

        for tag in [
            "#EXTM3U\n",
            "#EXT-X-VERSION:7\n",
            "#EXT-X-MEDIA-SEQUENCE:0\n",
            "#EXT-X-PLAYLIST-TYPE:EVENT\n",
            "#EXT-X-MAP:URI=\"init.mp4\"\n",
        ] {
            assert!(text.contains(tag), "playlist is missing {tag}: {text}");
        }
        assert!(
            !text.contains("INDEPENDENT-SEGMENTS"),
            "a claim a ceiling cut makes false"
        );

        // TARGETDURATION is the client's playlist-reload interval on a live
        // EVENT playlist (RFC 8216 §6.3.4), so it has to be the real ceiling
        // of what was published — not a constant far above it, which is how a
        // player ends up waiting fifteen seconds to learn a second segment
        // exists and stalls at the end of the first.
        let longest = text
            .lines()
            .filter_map(|l| l.strip_prefix("#EXTINF:"))
            .filter_map(|l| l.split(',').next()?.parse::<f64>().ok())
            .fold(0.0f64, f64::max);
        let declared: u32 = text
            .lines()
            .find_map(|l| l.strip_prefix("#EXT-X-TARGETDURATION:"))
            .and_then(|v| v.trim().parse().ok())
            .expect("a target duration");
        assert_eq!(
            declared,
            longest.ceil().max(1.0) as u32,
            "TARGETDURATION {declared} against a longest segment of {longest:.3}s"
        );

        let part = crate::produce::Part::from_playlist(&text);
        assert_eq!(part.segments.len() as u64, counts.segments);
        assert_eq!(part.durations_ms.len() as u64, counts.segments);
        let total: i64 = part.durations_ms.iter().sum();
        assert!(
            (11_900..=12_100).contains(&total),
            "produce read {total}ms out of a 12 s fixture"
        );
    }

    /// The startup stall, as a test. A player's runway when it loads a live
    /// playlist is the first segment and nothing else; its next chance to see
    /// a second one is one TARGETDURATION away. So the first segment must be
    /// short and the tag must be honest, or the picture stops the moment
    /// segment zero runs out — which is exactly what Safari did, 5631 ms at
    /// 9.2 s, on a server 55 seconds ahead.
    #[tokio::test]
    async fn the_first_segment_is_short_and_the_playlist_says_so() {
        let (dir, outcome) = session("clean-cra", Limits::default()).await;
        let Outcome::Ran(counts) = outcome else {
            panic!("{outcome:?}");
        };
        assert!(counts.segments >= 2, "{counts:?}");
        let text = playlist(dir.path());
        let extinf: Vec<f64> = text
            .lines()
            .filter_map(|l| l.strip_prefix("#EXTINF:"))
            .filter_map(|l| l.split(',').next()?.parse::<f64>().ok())
            .collect();
        let declared: u32 = text
            .lines()
            .find_map(|l| l.strip_prefix("#EXT-X-TARGETDURATION:"))
            .and_then(|v| v.trim().parse().ok())
            .expect("a target duration");

        // Short enough that the first reload arrives before it runs out.
        assert!(
            extinf[0] < declared as f64,
            "the first segment is {}s against a {declared}s reload interval — \
             the player runs dry before it can learn segment 1 exists",
            extinf[0]
        );
        // And short in absolute terms: this is the whole startup buffer.
        assert!(extinf[0] <= 4.0, "first segment {}s", extinf[0]);
        // The shipped floor still governs everything after it.
        for (i, d) in extinf.iter().enumerate().skip(1).take(extinf.len() - 2) {
            assert!(
                *d >= COPY_SEGMENT_SECONDS as f64,
                "segment {i} is {d}s, under the {COPY_SEGMENT_SECONDS}s floor"
            );
        }
    }

    /// A killed session — the pipe stops mid-fragment. What is on disk has to
    /// be exactly what was complete: no `.tmp` residue for the serving layer
    /// to trip over, and no ENDLIST claiming a film that ends early.
    #[tokio::test]
    async fn a_killed_session_leaves_no_tmp_files_and_no_endlist() {
        let dir = tempfile::tempdir().expect("tempdir");
        let feed = pipe("clean-cra");
        // Two thirds of the stream, which lands inside a fragment.
        let cut = feed.len() * 2 / 3;
        let outcome = run(&feed[..cut], dir.path().to_path_buf(), "test", brisk()).await;
        let Outcome::Ran(counts) = outcome else {
            panic!("{outcome:?}");
        };
        assert!(counts.segments >= 1);

        let names = segment_files(dir.path());
        assert!(
            !names.iter().any(|n| n.ends_with(".tmp")),
            "left a partial file behind: {names:?}"
        );
        let text = playlist(dir.path());
        assert!(!text.contains("ENDLIST"), "claimed the film ended: {text}");
        assert_eq!(text.matches(".m4s\n").count() as u64, counts.segments);
    }

    /// The publish gate: segments land on disk from the start, but the
    /// playlist does not exist until the cushion does. A session killed
    /// before the gate fills leaves segment files and NO playlist — the
    /// player was never told anything, which is the property the whole gate
    /// stands on (a client that can't see the live edge can't start at it).
    #[tokio::test]
    async fn the_publish_gate_holds_the_playlist_while_segments_land() {
        let dir = tempfile::tempdir().expect("tempdir");
        let feed = pipe("clean-cra");
        // Mid-fragment, so nothing mistakes this for end of stream — EOF is
        // the one thing allowed to open the gate early.
        let cut = feed.len() * 2 / 3;
        let mut limits = brisk();
        limits.publish_gate_secs = 999;
        let outcome = run(&feed[..cut], dir.path().to_path_buf(), "test", limits).await;
        let Outcome::Ran(counts) = outcome else {
            panic!("{outcome:?}");
        };
        assert!(counts.segments >= 1, "{counts:?}");
        assert!(
            !dir.path().join("index.m3u8").exists(),
            "the gate published a playlist {} segments into a 999 s hold",
            counts.segments
        );
        // The work itself was not held back — only the announcement.
        assert!(dir.path().join("init.mp4").exists());
        for i in 0..counts.segments {
            assert!(dir.path().join(format!("seg{i:05}.m4s")).exists());
        }
    }

    /// And the gate opening mid-stream: once the cushion of media exists, the
    /// first playlist published lists all of it, and every later segment
    /// republishes as before. Same truncated feed as above, a gate the
    /// fixture can actually fill.
    #[tokio::test]
    async fn the_gate_opens_mid_stream_and_the_first_playlist_lists_the_cushion() {
        let dir = tempfile::tempdir().expect("tempdir");
        let feed = pipe("clean-cra");
        let cut = feed.len() * 2 / 3;
        let mut limits = brisk();
        limits.publish_gate_secs = 4;
        let outcome = run(&feed[..cut], dir.path().to_path_buf(), "test", limits).await;
        let Outcome::Ran(counts) = outcome else {
            panic!("{outcome:?}");
        };
        assert!(
            counts.segments >= 2,
            "the fixture no longer cuts two segments by two thirds in — \
             re-derive this test's cut point: {counts:?}"
        );
        let text = playlist(dir.path());
        assert_eq!(
            text.matches(".m4s\n").count() as u64,
            counts.segments,
            "once open, the playlist must keep step with every segment: {text}"
        );
        let listed: f64 = text
            .lines()
            .filter_map(|l| l.strip_prefix("#EXTINF:"))
            .filter_map(|l| l.split(',').next()?.parse::<f64>().ok())
            .sum();
        assert!(
            listed >= 4.0,
            "the gate opened at {listed:.3}s of media against a 4 s line"
        );
        assert!(!text.contains("ENDLIST"), "{text}");
    }

    /// End of stream overrides the gate: a film shorter than the cushion
    /// publishes whole — ENDLIST and all — the moment it finishes. The first
    /// playlist a player loads is simply VOD.
    #[tokio::test]
    async fn a_film_shorter_than_the_gate_still_publishes_whole_at_eof() {
        let mut limits = brisk();
        limits.publish_gate_secs = 999;
        let (dir, outcome) = session("clean-cra", limits).await;
        let Outcome::Ran(counts) = outcome else {
            panic!("{outcome:?}");
        };
        assert!(counts.segments >= 2, "{counts:?}");
        let text = playlist(dir.path());
        assert!(
            text.ends_with("#EXT-X-ENDLIST\n"),
            "a finished film must say so however few segments it has: {text}"
        );
        assert_eq!(text.matches(".m4s\n").count() as u64, counts.segments);
    }

    /// The capability check. A pipe whose `moov` is nonsense must not produce
    /// half a session — it must say so before anything is published, which is
    /// what lets the caller fall back to ffmpeg's own muxer.
    #[tokio::test]
    async fn an_unparseable_moov_falls_back_to_the_legacy_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut feed = pipe("open-gop");
        // Corrupt the moov's first child box size. The box walk then runs past
        // its parent, which is the malformed case, not the truncated one.
        let head = 28 + 8; // past ftyp and the moov header
        feed[head..head + 4].copy_from_slice(&0xffff_ffffu32.to_be_bytes());
        let outcome = run(&feed[..], dir.path().to_path_buf(), "test", brisk()).await;
        assert!(
            matches!(outcome, Outcome::Unsupported(_)),
            "a broken moov did not ask for the fallback: {outcome:?}"
        );
        assert!(!dir.path().join("index.m3u8").exists());
    }

    /// A chapter track in the init is the shape that broke Safari. Even with
    /// `-map_chapters -1` shutting it off at the source, a stream that somehow
    /// arrives carrying one must go to the legacy muxer rather than to a
    /// player: that output is known to be playable, and this one is known not
    /// to be.
    #[tokio::test]
    async fn a_track_this_path_never_asked_for_takes_the_fallback() {
        let src = plurx_core::testfixtures::source_with_chapters();
        // The pipe command WITHOUT the chapter strip — what shipped, and what
        // Safari refused.
        let out = std::process::Command::new(plurx_core::testfixtures::ffmpeg())
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(&src)
            .args(["-map", "0:v:0", "-map", "0:a:0?", "-sn", "-c:v", "copy"])
            .args([
                "-tag:v",
                "hvc1",
                "-bsf:v",
                "filter_units=remove_types=32-34",
            ])
            .args([
                "-c:a",
                "aac",
                "-b:a",
                "256k",
                "-avoid_negative_ts",
                "make_zero",
            ])
            .args([
                "-movflags",
                "frag_keyframe+empty_moov+default_base_moof+delay_moov",
            ])
            .args(["-f", "mp4", "pipe:1"])
            .output()
            .expect("ffmpeg");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = run(&out.stdout[..], dir.path().to_path_buf(), "test", brisk()).await;
        match outcome {
            Outcome::Unsupported(reason) => {
                assert!(reason.contains("never asked for"), "{reason}");
            }
            other => panic!("a chapter track was served to a player: {other:?}"),
        }
        assert!(!dir.path().join("index.m3u8").exists());
    }

    /// Nothing at all down the pipe — ffmpeg refused the source outright.
    #[tokio::test]
    async fn an_empty_pipe_asks_for_the_fallback() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = run(&[][..], dir.path().to_path_buf(), "test", brisk()).await;
        assert!(matches!(outcome, Outcome::Unsupported(_)), "{outcome:?}");
    }

    /// The pipe arrives in whatever pieces the kernel felt like, and a
    /// SIGSTOPped ffmpeg can leave minutes between them. Feeding the same
    /// bytes through a source that hands over a trickle at a time has to
    /// produce the identical session.
    #[tokio::test]
    async fn a_trickling_pipe_produces_the_same_session() {
        let (whole, whole_outcome) = session("clean-cra", brisk()).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let feed = pipe("clean-cra");
        let trickle = tokio::io::BufReader::with_capacity(1, &feed[..]);
        let outcome = run(trickle, dir.path().to_path_buf(), "test", brisk()).await;
        assert_eq!(outcome, whole_outcome);
        assert_eq!(playlist(dir.path()), playlist(whole.path()));
        assert_eq!(segment_files(dir.path()), segment_files(whole.path()));
    }

    /// The whole path, once, against a live ffmpeg: the real
    /// `copy_pipe_args`, a real child process, its real stdout, this reader,
    /// and real files on disk — and what comes out decodes frame for frame
    /// like the source.
    ///
    /// Every other test here feeds `run()` a byte slice, which is right for
    /// the disk semantics but proves nothing about the arguments or about
    /// reading from a pipe that arrives at ffmpeg's pace. This is the one that
    /// would notice if `copy_pipe_args` stopped producing a stream this reader
    /// can follow — the exact failure that would silently put every copy
    /// session on the legacy fallback with only a log line to say so.
    #[tokio::test]
    async fn a_live_ffmpeg_pipe_produces_a_session_that_decodes_like_the_source() {
        use plurx_core::domain::MediaFile;
        use plurx_core::transcode::{copy_pipe_args, Pacing};

        let src = plurx_core::testfixtures::source("clean-cra");
        let file = MediaFile {
            id: 1,
            item_id: 1,
            path: src.clone(),
            size: 1,
            mtime: 1,
            duration_ms: Some(12_000),
            container: Some("mkv".into()),
            video_codec: Some("hevc".into()),
            video_profile: Some("Main".into()),
            width: Some(640),
            height: Some(360),
            bit_depth: Some(8),
            hdr: None,
            hdr_format: None,
            bitrate: Some(1_000_000),
            audio_streams: vec![],
            subtitle_streams: vec![],
            scanned_at: 1,
            audio_offset_ms: 0,
            probed: true,
        };
        // Unpaced: the pacing flags are the daemon's business and a 12 s
        // fixture read at 2× would just make the test slow.
        let args = copy_pipe_args(&file, 0.0, None, true, Pacing::unpaced(), false);
        let mut child = tokio::process::Command::new(plurx_core::testfixtures::ffmpeg())
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawning ffmpeg");
        let stdout = child.stdout.take().expect("stdout pipe");

        let dir = tempfile::tempdir().expect("tempdir");
        let limits = Limits {
            floor_seconds: 3,
            first_floor_seconds: 1,
            max_bytes: 48_000_000,
            max_seconds: 15,
            publish_gate_secs: 0,
        };
        let outcome = run(stdout, dir.path().to_path_buf(), "live", limits).await;
        let _ = child.wait().await;

        let Outcome::Ran(counts) = outcome else {
            panic!("a live ffmpeg pipe was not readable: {outcome:?}");
        };
        assert!(counts.segments >= 2, "{counts:?}");
        assert_eq!(counts.unparseable, 0, "{counts:?}");
        assert_eq!(
            counts.ceiling_cuts, 0,
            "a clean source cut dirty: {counts:?}"
        );
        let text = playlist(dir.path());
        assert!(text.ends_with("#EXT-X-ENDLIST\n"), "{text}");

        // init + every segment, concatenated, against the source. framemd5
        // hashes each frame WITH its pts and duration, so a reordered,
        // retimed or dropped frame all fail here.
        let mut cat = std::fs::read(dir.path().join("init.mp4")).expect("init.mp4");
        for i in 0..counts.segments {
            let seg = dir.path().join(format!("seg{i:05}.m4s"));
            cat.extend_from_slice(&std::fs::read(&seg).expect("segment"));
        }
        let candidate = dir.path().join("candidate.mp4");
        std::fs::write(&candidate, &cat).expect("candidate");

        let framemd5 = |path: &Path, stream: &str| {
            let out = plurx_core::testfixtures::run(
                std::process::Command::new(plurx_core::testfixtures::ffmpeg())
                    .args(["-v", "error", "-i"])
                    .arg(path)
                    .args(["-map", stream, "-f", "framemd5", "-"]),
            );
            String::from_utf8_lossy(&out)
                .lines()
                .filter(|l| !l.starts_with('#'))
                .collect::<Vec<_>>()
                .join("\n")
        };
        // Materialize the reference through the cached path, then compare.
        let _ = plurx_core::testfixtures::pipe("clean-cra");
        let reference = plurx_core::testfixtures::pipe_path("clean-cra");
        for stream in ["0:v:0", "0:a:0"] {
            assert_eq!(
                framemd5(&candidate, stream),
                framemd5(&reference, stream),
                "{stream} decodes differently after a live session"
            );
        }
    }

    #[test]
    fn only_codecs_whose_keyframes_can_be_read_are_attempted() {
        assert!(supports(Some("hevc")));
        assert!(supports(Some("h265")));
        assert!(supports(Some("h264")));
        assert!(!supports(Some("vp9")));
        assert!(!supports(Some("av1")));
        assert!(!supports(None));
    }

    /// The reason string reaches the log, so it has to say something.
    #[test]
    fn the_summary_names_every_count() {
        let counts = SegmentCounts {
            fragments: 40,
            segments: 7,
            clean_cuts: 5,
            ceiling_cuts: 1,
            unparseable: 0,
            tfdt_adjustments: 0,
        };
        let s = summary(&counts);
        for want in [
            "segments 7",
            "clean cuts 5",
            "ceiling cuts 1",
            "fragments 40",
            "unparseable 0",
        ] {
            assert!(s.contains(want), "{s}");
        }
    }

    /// `CutReason` is what the counts are made of; a rename that silently
    /// changed a label would change what perf-report reads.
    #[test]
    fn cut_reasons_keep_their_labels() {
        assert_eq!(CutReason::Clean.label(), "clean");
        assert_eq!(CutReason::ByteCeiling.label(), "byte-ceiling");
        assert_eq!(CutReason::TimeCeiling.label(), "time-ceiling");
        assert_eq!(CutReason::EndOfStream.label(), "eof");
    }
}
