//! The GOP-aware copy segmenter: plurx cuts the segments, not ffmpeg.
//!
//! ffmpeg writes one continuous fragmented MP4 down a pipe (one fragment per
//! GOP, `crate::transcode` spawns it with [`plurx_core::transcode::copy_pipe_args`]);
//! this task reads that stream, and publishes a segment boundary only in front
//! of a fragment whose first frame is a true random-access point. Everything
//! it writes — `init.mp4`, `segNNNNN.m4s`, `index.m3u8` — has the same names,
//! the same shape and the same tmp-then-rename semantics ffmpeg's HLS muxer
//! gave it, so the serving layer, the segment index, the ahead-window suspend
//! and the GC all carry on unchanged. The playlist is the interface.
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

use plurx_core::fmp4::{self, FragmentReader, Init, Published, SegmentCounts, Segmenter, Unit};
use plurx_core::transcode::{COPY_SEGMENT_MAX_BYTES, COPY_SEGMENT_MAX_SECS, COPY_SEGMENT_SECONDS};
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
    pub max_bytes: usize,
    pub max_seconds: u32,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            floor_seconds: COPY_SEGMENT_SECONDS,
            max_bytes: COPY_SEGMENT_MAX_BYTES,
            max_seconds: COPY_SEGMENT_MAX_SECS,
        }
    }
}

/// How a segmenter session ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The stream was never one this reader could follow, and nothing was
    /// published. The caller respawns on the legacy muxer — once.
    ///
    /// Separate from every other failure on purpose: this is the only ending
    /// where falling back is both possible and correct. Once a segment exists,
    /// a respawn would rewrite a timeline a player is already holding.
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
struct SessionDir {
    dir: PathBuf,
    playlist: String,
}

impl SessionDir {
    fn new(dir: PathBuf) -> SessionDir {
        SessionDir {
            dir,
            playlist: String::new(),
        }
    }

    /// tmp + rename, the semantics the whole daemon assumes: a segment or a
    /// playlist is either absent or complete, never partial.
    async fn publish_file(&self, name: &str, bytes: &[u8]) -> std::io::Result<()> {
        let tmp = self.dir.join(format!("{name}.tmp"));
        tokio::fs::write(&tmp, bytes).await?;
        tokio::fs::rename(&tmp, self.dir.join(name)).await
    }

    async fn write_init(&mut self, init: &Init, target_duration: u32) -> std::io::Result<()> {
        self.publish_file("init.mp4", &init.bytes).await?;
        self.playlist = fmp4::playlist_header(target_duration);
        // The header is not written yet: a playlist with no segment in it is
        // a promise the session cannot keep if ffmpeg dies in the next second,
        // and `session_producing` reads exactly this file to decide whether
        // there is real output. It lands with the first segment.
        Ok(())
    }

    async fn write_segment(&mut self, published: &Published) -> std::io::Result<()> {
        let name = published.name();
        self.publish_file(&name, &published.segment.bytes).await?;
        self.playlist
            .push_str(&fmp4::playlist_entry(published.seconds, &name));
        self.publish_file("index.m3u8", self.playlist.as_bytes())
            .await
    }

    async fn write_endlist(&mut self) -> std::io::Result<()> {
        if self.playlist.is_empty() {
            return Ok(());
        }
        self.playlist.push_str("#EXT-X-ENDLIST\n");
        self.publish_file("index.m3u8", self.playlist.as_bytes())
            .await
    }
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
    let mut out = SessionDir::new(dir);
    let mut segmenter: Option<Segmenter> = None;
    let mut buf = vec![0u8; READ_CHUNK];
    let mut warned_memory = false;
    let mut published_any = false;

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
                    if !published_any {
                        return Outcome::Unsupported(format!("{e}"));
                    }
                    // Mid-stream: the fallback door is shut, because a player
                    // is holding segments from this timeline. Stop reading and
                    // let the session end like any ffmpeg that died — the
                    // playlist keeps what was real, without an ENDLIST
                    // claiming the film finished here.
                    tracing::error!(
                        session = %session_id,
                        "copy segmenter lost the fragment stream: {e}"
                    );
                    return finish(segmenter, &mut out, session_id, false).await;
                }
            };
            match unit {
                Unit::Init(init) => {
                    let Some(video) = init.video() else {
                        return Outcome::Unsupported("the pipe's moov has no video track".into());
                    };
                    if video.codec.is_none() || video.nal_length_size == 0 {
                        return Outcome::Unsupported(
                            "the video track carries no hvcC/avcC to read keyframes with".into(),
                        );
                    }
                    let policy = fmp4::CutPolicy::new(
                        limits.floor_seconds,
                        limits.max_bytes,
                        limits.max_seconds,
                        video.timescale,
                    );
                    if let Err(e) = out.write_init(&init, limits.max_seconds).await {
                        return Outcome::Unsupported(format!("writing init.mp4: {e}"));
                    }
                    segmenter = Some(Segmenter::new(init, policy));
                }
                Unit::Fragment(fragment) => {
                    let Some(seg) = segmenter.as_mut() else {
                        return Outcome::Unsupported("a fragment arrived before the moov".into());
                    };
                    match seg.push(fragment) {
                        Ok(Some(published)) => {
                            if let Err(e) = out.write_segment(&published).await {
                                tracing::error!(
                                    session = %session_id,
                                    "writing {}: {e}", published.name()
                                );
                                return Outcome::Ran(seg.counts());
                            }
                            published_any = true;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            if !published_any {
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
    finish(segmenter, &mut out, session_id, complete).await
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
            Ok(Some(published)) => match out.write_segment(&published).await {
                Ok(()) => true,
                Err(e) => {
                    tracing::error!(session = %session_id, "writing the final segment: {e}");
                    false
                }
            },
            Ok(None) => true,
            Err(e) => {
                tracing::error!(session = %session_id, "merging the final segment: {e}");
                false
            }
        };
        if final_ok {
            if let Err(e) = out.write_endlist().await {
                tracing::error!(session = %session_id, "writing the playlist end: {e}");
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
    use plurx_core::fmp4::CutReason;
    use plurx_core::testfixtures::pipe;
    use std::path::Path;

    /// Long enough to reach a ceiling from a 12 s fixture. The floor is 3 s
    /// rather than the shipped 6 s for the same reason: the property under
    /// test is which keyframe a cut lands on, not what the constant is.
    fn brisk() -> Limits {
        Limits {
            floor_seconds: 3,
            max_bytes: 48_000_000,
            max_seconds: 15,
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
            max_bytes: 300_000,
            max_seconds: 15,
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
            "#EXT-X-TARGETDURATION:15\n",
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

        let part = crate::produce::Part::from_playlist(&text);
        assert_eq!(part.segments.len() as u64, counts.segments);
        assert_eq!(part.durations_ms.len() as u64, counts.segments);
        let total: i64 = part.durations_ms.iter().sum();
        assert!(
            (11_900..=12_100).contains(&total),
            "produce read {total}ms out of a 12 s fixture"
        );
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
