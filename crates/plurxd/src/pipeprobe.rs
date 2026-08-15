//! Which tone-map pipeline this node is actually allowed to use.
//!
//! A graph that parses is not a graph that works. It can move frames and still
//! produce clipped gray; it can lose the HDR metadata at the decoder and
//! tone-map noise; it can be correct and tagged BT.2020 anyway, which renders
//! wrong on every SDR display; and it can be all of those things right and
//! still be slower than the CPU chain it replaced. Exit status zero proves
//! none of it (PERF-PLAN §5, review R5).
//!
//! So the gate is a real encode of real HDR10, per candidate, per node, checked
//! four ways:
//!
//! 1. it runs at all, with the exact production graph;
//! 2. `ffprobe` says the output is tagged BT.709 — transfer, primaries, matrix;
//! 3. its picture matches the CPU reference's within a deliberately broad
//!    tolerance (encoders differ by a few percent; a gray screen differs by
//!    fifty);
//! 4. it is meaningfully faster than that reference.
//!
//! Point 4 is a *comparison*, not an absolute threshold, which is what makes
//! it honest on hardware nobody has measured. Both runs pay the same process
//! startup on the same fixture, so the overhead cancels and what is left is
//! the pipeline. A graph that merely matches the CPU chain has bought nothing
//! and taken on a driver dependency; it is not selected.
//!
//! The fixture is generated rather than shipped: a 4K HDR10 HEVC clip with
//! real PQ/BT.2020 stream metadata, encoded once and cached in the data dir.
//! Generated means redistribution-safe by construction, and *real compressed
//! HEVC* means the decoder has to preserve the colour metadata across the
//! hardware path — which a raw 10-bit pattern would never have tested.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use plurx_core::transcode::{Encoder, Pipeline, PIPELINE_CANDIDATES};

use crate::ffmpeg::{ffmpeg_bin, ffprobe_bin};

/// How much faster than the CPU chain a GPU graph must be to be worth its
/// driver dependency. §2.9 measured the CPU tone-map costing ~28% of
/// throughput, so a graph that removes it should be clearly ahead; 20% is
/// comfortably under that and comfortably over run-to-run noise.
const MIN_SPEEDUP: f64 = 1.2;

/// Per-channel tolerance against the CPU reference, in 8-bit levels.
///
/// Broad on purpose. Different tone-map operators disagree by a few levels and
/// so do encoders at the same bitrate; the failure this catches is not a
/// slightly different picture but a *wrong* one — a gray screen sits near 128
/// on every channel, a clipped one near 0 or 255, and either is tens of levels
/// away from a real image.
const MAX_CHANNEL_DELTA: f64 = 24.0;

/// Fixture geometry. 4K because the question is about 4K: a 1080p probe would
/// pass on hardware that cannot hold realtime at the resolution that actually
/// stutters. Short because it is generated at first boot.
const FIXTURE_SECONDS: f64 = 1.5;
const FIXTURE_FPS: u32 = 24;
/// Rung the probe encodes to — the Auto rung on hardware, so the probe
/// measures the work a real session does.
const PROBE_HEIGHT: i64 = 1080;

/// What one candidate's run produced.
#[derive(Debug, Clone, Copy)]
struct Sample {
    /// Mean luma and chroma of the output, 8-bit levels.
    y: f64,
    u: f64,
    v: f64,
    elapsed: Duration,
}

impl Sample {
    /// Furthest any channel is from `other`.
    fn max_delta(&self, other: &Sample) -> f64 {
        (self.y - other.y)
            .abs()
            .max((self.u - other.u).abs())
            .max((self.v - other.v).abs())
    }
}

/// The verdict for one candidate, kept whether it passed or not — a node that
/// falls back to the CPU chain should be able to say which graph failed and
/// how, without anyone re-running anything.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PipelineVerdict {
    pub pipeline: String,
    pub label: String,
    pub passed: bool,
    /// Times faster than the CPU reference, when it produced output at all.
    pub speedup: Option<f64>,
    /// Why it was rejected. `None` when it passed.
    pub rejected: Option<String>,
}

/// This node's tone-map capability, as measured.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PipelineReport {
    /// The pipeline sessions will use. Always a real answer — the CPU chain
    /// when nothing else proved itself.
    pub selected: String,
    pub selected_label: String,
    pub verdicts: Vec<PipelineVerdict>,
    /// Absent when the probe could not run at all (no ffmpeg, no fixture),
    /// which is a different thing from every candidate failing.
    pub ran: bool,
}

impl Default for PipelineReport {
    /// An unprobed node still has to name a pipeline, and it has to be the
    /// safe one — a `Default` that meant "no pipeline" would be a hole every
    /// test construction could fall into.
    fn default() -> Self {
        PipelineReport::cpu_only("not probed")
    }
}

impl PipelineReport {
    /// The unprobed default: the chain that always works.
    pub fn cpu_only(reason: &str) -> PipelineReport {
        PipelineReport {
            selected: Pipeline::Cpu.name().to_owned(),
            selected_label: Pipeline::Cpu.label().to_owned(),
            verdicts: vec![PipelineVerdict {
                pipeline: Pipeline::Cpu.name().to_owned(),
                label: Pipeline::Cpu.label().to_owned(),
                passed: true,
                speedup: None,
                rejected: Some(reason.to_owned()),
            }],
            ran: false,
        }
    }

    pub fn selected(&self) -> Pipeline {
        Pipeline::parse(&self.selected).unwrap_or(Pipeline::Cpu)
    }
}

/// Probe every candidate that could pair with `encoder`, and return what this
/// node may use.
///
/// Runs once at startup, after encoder detection — it needs to know which
/// encoder won, because a graph is only worth probing if it can feed it.
pub async fn probe(work_dir: &Path, encoder: Encoder) -> PipelineReport {
    probe_with(&Spawn, work_dir, encoder).await
}

/// The external tools this probe drives.
///
/// A seam rather than a direct spawn, because everything the probe *decides* is
/// decided from these tools' output: which arguments were sent, whether the
/// exit status means failure, whether the colour tags prove an SDR output, and
/// what the frame statistics average to. Behind a trait, all of that is checked
/// against captured ffmpeg and ffprobe output — including the truncated and
/// malformed shapes a probe actually meets in the field — and what is left
/// spawning a process is [`Spawn`], which decides nothing.
trait Tools {
    fn ffmpeg(
        &self,
        args: Vec<String>,
        stdout: Stdout,
    ) -> impl std::future::Future<Output = Result<std::process::Output, String>>;

    fn ffprobe(
        &self,
        args: Vec<String>,
    ) -> impl std::future::Future<Output = Result<std::process::Output, String>>;
}

/// What an ffmpeg run's stdout is for. The encodes write a file and their
/// stdout is noise; a capability query's stdout is the whole answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stdout {
    Discard,
    Inherit,
    /// Read it back. The two encodes write a file and their stdout is noise,
    /// but a capability query's stdout *is* the answer — so the filter probe
    /// asks through the same spawn rather than keeping a second copy of it.
    Capture,
}

/// The production tools: the real binaries, and nothing else.
struct Spawn;

impl Tools for Spawn {
    async fn ffmpeg(
        &self,
        args: Vec<String>,
        stdout: Stdout,
    ) -> Result<std::process::Output, String> {
        tokio::process::Command::new(ffmpeg_bin())
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(match stdout {
                Stdout::Discard => std::process::Stdio::null(),
                Stdout::Inherit => std::process::Stdio::inherit(),
                Stdout::Capture => std::process::Stdio::piped(),
            })
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .map_err(|e| format!("could not run ffmpeg: {e}"))
    }

    async fn ffprobe(&self, args: Vec<String>) -> Result<std::process::Output, String> {
        tokio::process::Command::new(ffprobe_bin())
            .args(&args)
            .output()
            .await
            .map_err(|e| format!("could not run ffprobe: {e}"))
    }
}

/// Which filters a subtitle burn-in needs, and whether this build has them.
///
/// A burn is the most expensive session the server can open, and on a thin
/// ffmpeg it cannot run at all: the graph is rejected at spawn with
/// `No such filter`, the producer exits non-zero seconds later, and the viewer
/// is told the stream failed with no way to learn that the build is simply
/// missing a filter. (Homebrew's ffmpeg 8.1.2 ships without `subtitles`
/// entirely; that is not an exotic configuration.) Asked once per process,
/// like every other question about this binary.
///
/// `sub2video` is deliberately not here. It is ffmpeg's own CLI-level
/// conversion of a subtitle stream into video frames for a filter graph, not a
/// row in `-filters`, so there is nothing to look for — the bitmap composite's
/// two checkable filters are `overlay` and `scale`.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct BurnFilters {
    /// `overlay` — composites the subtitle plane onto the picture.
    pub overlay: bool,
    /// `scale` — puts a PGS canvas (very often 1080p under a 4K video) back
    /// onto the output frame before the composite.
    pub scale: bool,
    /// `subtitles` — libass, which renders text and styled ASS/SSA.
    pub subtitles: bool,
    /// Whether the listing was read at all. A build that could not be asked
    /// is *not* treated as a build that answered "no": refusing a session on
    /// no evidence would be a new outage, so an unreadable listing lets the
    /// burn proceed exactly as it did before this probe existed.
    pub probed: bool,
}

/// Does this build carry every named filter?
///
/// Matched on the filter *column* of `ffmpeg -filters`, whose rows read
/// `.S zscale V->V ...`. A substring search over the listing would report
/// `zscale` for any build that merely mentions it in another filter's
/// description, and the graph would then fail at session start. This lived
/// under `#[cfg(test)]`, which is exactly why production never checked.
pub fn declares_filters(listing: &str, names: &[&str]) -> bool {
    names.iter().all(|n| {
        listing
            .lines()
            .any(|l| l.split_whitespace().nth(1) == Some(n))
    })
}

/// Read a `-filters` listing into the burn capabilities.
///
/// A listing that names none of the three is treated as unread rather than as
/// a build without them: every ffmpeg has `scale`, so an answer that lacks it
/// is a parse or a spawn that went wrong, not a capability report.
fn burn_filters_from_listing(listing: &str) -> BurnFilters {
    let caps = BurnFilters {
        overlay: declares_filters(listing, &["overlay"]),
        scale: declares_filters(listing, &["scale"]),
        subtitles: declares_filters(listing, &["subtitles"]),
        probed: true,
    };
    if !caps.overlay && !caps.scale && !caps.subtitles {
        return BurnFilters::default();
    }
    caps
}

impl BurnFilters {
    /// Why this build cannot burn the requested subtitle, or `None` when it
    /// can. The sentence is the one the viewer is shown, so it names the
    /// missing filter and what to do about it.
    pub fn refusal(&self, bitmap: bool) -> Option<String> {
        if !self.probed {
            return None;
        }
        let needed: &[(&str, bool)] = if bitmap {
            &[("overlay", self.overlay), ("scale", self.scale)]
        } else {
            &[("subtitles", self.subtitles)]
        };
        let missing: Vec<&str> = needed
            .iter()
            .filter(|(_, present)| !present)
            .map(|(name, _)| *name)
            .collect();
        if missing.is_empty() {
            return None;
        }
        Some(format!(
            "this server's ffmpeg build has no {} filter, which burning {} \
             subtitles into the picture requires — install a full ffmpeg (the \
             jellyfin-ffmpeg builds carry it) or choose a different subtitle track",
            missing.join(" or "),
            if bitmap { "bitmap" } else { "text" }
        ))
    }
}

static BURN_FILTERS: tokio::sync::OnceCell<BurnFilters> = tokio::sync::OnceCell::const_new();

/// What this build can burn. Probed once per process, through the same spawn
/// seam the tone-map probe uses.
pub async fn burn_filters() -> BurnFilters {
    *BURN_FILTERS
        .get_or_init(|| async { burn_filters_with(&Spawn).await })
        .await
}

async fn burn_filters_with<T: Tools>(tools: &T) -> BurnFilters {
    let args = vec!["-hide_banner".to_owned(), "-filters".to_owned()];
    let Ok(out) = tools.ffmpeg(args, Stdout::Capture).await else {
        tracing::warn!(
            "could not ask ffmpeg for its filter list; subtitle burn-in will be attempted \
             without a preflight, exactly as it was before"
        );
        return BurnFilters::default();
    };
    // Listings go to stdout on modern builds and stderr on older ones.
    let mut listing = String::from_utf8_lossy(&out.stdout).into_owned();
    listing.push_str(&String::from_utf8_lossy(&out.stderr));
    let caps = burn_filters_from_listing(&listing);
    if !caps.probed {
        tracing::warn!("ffmpeg -filters returned nothing recognisable; skipping burn preflight");
    } else if !caps.subtitles || !caps.overlay || !caps.scale {
        tracing::warn!(
            overlay = caps.overlay,
            scale = caps.scale,
            subtitles = caps.subtitles,
            "this ffmpeg is missing a filter subtitle burn-in needs; those sessions will be \
             refused with that reason instead of failing at spawn"
        );
    }
    caps
}

async fn probe_with<T: Tools>(tools: &T, work_dir: &Path, encoder: Encoder) -> PipelineReport {
    // Nothing to keep on the GPU if the encode is on the CPU: the frames would
    // have to come down for it anyway.
    if encoder == Encoder::Software {
        return PipelineReport::cpu_only(
            "software encoder — a GPU graph would download every frame anyway",
        );
    }

    let fixture = match fixture(tools, work_dir).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "could not build the HDR10 probe fixture — staying on the CPU tone-map");
            return PipelineReport::cpu_only(&format!("no probe fixture: {e}"));
        }
    };

    let out = work_dir.join("pipeprobe-out.mp4");
    let report = probe_candidates(encoder, |candidate| {
        run(tools, &fixture, &out, candidate, encoder)
    })
    .await;
    let _ = tokio::fs::remove_file(&out).await;
    report
}

/// Race every candidate that could pair with `encoder` against the CPU chain,
/// and assemble what this node may use.
///
/// Takes the runner rather than performing it, which is what makes the
/// *arbitration* testable without a GPU: the order candidates are tried in,
/// the rule that the first proof wins, what happens when the reference itself
/// cannot run, and the fact that the CPU chain is always in the report are all
/// decisions, and none of them needs a subprocess to check.
async fn probe_candidates<F, Fut>(encoder: Encoder, run_one: F) -> PipelineReport
where
    F: Fn(Pipeline) -> Fut,
    Fut: std::future::Future<Output = Result<Sample, String>>,
{
    // The CPU chain is both the fallback and the yardstick, so it runs first
    // and its result is what everything else is measured against.
    let reference = match run_one(Pipeline::Cpu).await {
        Ok(s) => s,
        Err(e) => {
            // The CPU chain failing means ffmpeg itself is unusable for
            // tone-mapping. Nothing here can be trusted, so nothing is claimed.
            tracing::warn!(error = %e, "the CPU tone-map reference did not run — no pipeline can be validated");
            return PipelineReport::cpu_only(&format!("reference run failed: {e}"));
        }
    };

    let mut verdicts = Vec::new();
    let mut selected = Pipeline::Cpu;
    for candidate in PIPELINE_CANDIDATES.iter().copied() {
        if candidate == Pipeline::Cpu || !candidate.pairs_with(encoder) {
            continue;
        }
        let verdict = judge_sample(candidate, run_one(candidate).await, &reference);
        let passed = verdict.passed;
        verdicts.push(verdict);
        if passed {
            selected = candidate;
            break; // candidates are in preference order; the first proof wins
        }
    }

    verdicts.push(PipelineVerdict {
        pipeline: Pipeline::Cpu.name().to_owned(),
        label: Pipeline::Cpu.label().to_owned(),
        passed: true,
        speedup: Some(1.0),
        rejected: None,
    });

    if selected.on_gpu() {
        tracing::info!(
            pipeline = selected.name(),
            "tone-map runs on the GPU for HDR10 sources"
        );
    } else {
        tracing::info!(
            "no GPU tone-map graph validated on this node — HDR sessions use the CPU chain"
        );
    }
    PipelineReport {
        selected: selected.name().to_owned(),
        selected_label: selected.label().to_owned(),
        verdicts,
        ran: true,
    }
}

/// Does this sample earn the job, measured against the CPU reference it would
/// replace? `Ok(speedup)` when it does.
///
/// Pure, and separated from the running of it, because the *order* is the
/// subtlety: correctness is checked before speed, so a fast wrong answer is
/// reported as wrong rather than as slow. Getting that backwards would send
/// whoever reads the log looking at their GPU's clocks instead of at a gray
/// screen.
fn decide(sample: &Sample, reference: &Sample) -> Result<f64, String> {
    let speedup = reference.elapsed.as_secs_f64() / sample.elapsed.as_secs_f64().max(0.001);
    let delta = sample.max_delta(reference);
    if delta > MAX_CHANNEL_DELTA {
        return Err(format!(
            "output differs from the CPU reference by {delta:.0} levels \
             (Y/U/V {:.0}/{:.0}/{:.0} vs {:.0}/{:.0}/{:.0}) — not the same picture",
            sample.y, sample.u, sample.v, reference.y, reference.u, reference.v
        ));
    }
    if speedup < MIN_SPEEDUP {
        return Err(format!(
            "only {speedup:.2}x the CPU chain — not worth the driver dependency"
        ));
    }
    Ok(speedup)
}

/// Turn one candidate's run into the verdict that goes in the report.
///
/// Pure over the run's outcome, so the *reporting* is checkable: a graph that
/// never produced output and a graph that produced the wrong picture are
/// different findings, and a node falling back to the CPU chain has to be able
/// to say which of them happened without anyone re-running anything.
fn judge_sample(
    candidate: Pipeline,
    outcome: Result<Sample, String>,
    reference: &Sample,
) -> PipelineVerdict {
    let reject = |why: String, speedup: Option<f64>| PipelineVerdict {
        pipeline: candidate.name().to_owned(),
        label: candidate.label().to_owned(),
        passed: false,
        speedup,
        rejected: Some(why),
    };

    let sample = match outcome {
        Ok(s) => s,
        Err(e) => {
            tracing::info!(pipeline = candidate.name(), reason = %e, "tone-map graph rejected");
            return reject(e, None);
        }
    };
    let speedup = reference.elapsed.as_secs_f64() / sample.elapsed.as_secs_f64().max(0.001);
    if let Err(why) = decide(&sample, reference) {
        tracing::warn!(pipeline = candidate.name(), reason = %why, "tone-map graph rejected");
        return reject(why, Some(speedup));
    }

    tracing::info!(
        pipeline = candidate.name(),
        speedup = format!("{speedup:.2}x"),
        "tone-map graph validated"
    );
    PipelineVerdict {
        pipeline: candidate.name().to_owned(),
        label: candidate.label().to_owned(),
        passed: true,
        speedup: Some(speedup),
        rejected: None,
    }
}

/// Encode the fixture through `candidate` and measure what came out.
async fn run<T: Tools>(
    tools: &T,
    fixture: &Path,
    out: &Path,
    candidate: Pipeline,
    encoder: Encoder,
) -> Result<Sample, String> {
    let args = probe_args(fixture, out, candidate, encoder);

    let started = Instant::now();
    let output = tools.ffmpeg(args, Stdout::Discard).await?;
    let elapsed = started.elapsed();
    check_exit(&output)?;

    // Tagged BT.709, or a correctly tone-mapped picture still renders wrong on
    // every SDR display — and this is the failure that looks like nothing at
    // all in a log.
    let tags = probe_stream(tools, out, "color_transfer,color_primaries,color_space").await?;
    check_bt709(&tags)?;

    let (y, u, v) = signal_stats(tools, out).await?;
    Ok(Sample { y, u, v, elapsed })
}

/// Did the tool succeed, and if not, what did it say?
///
/// The rejection carried into the report is ffmpeg's own first complaint. A
/// non-zero exit with an empty stderr still has to produce a reason: "rejected,
/// no reason given" is a verdict nobody can act on.
fn check_exit(output: &std::process::Output) -> Result<(), String> {
    if output.status.success() {
        return Ok(());
    }
    Err(first_line(&String::from_utf8_lossy(&output.stderr)))
}

/// The exact ffmpeg command one candidate is measured with.
///
/// Pure, because this is the part that decides whether the measurement means
/// anything: the CPU reference has to be the *same* chain `video_filters`
/// builds for an HDR10 source, or every candidate is compared against a
/// fiction, and the upload suffix has to be attached to exactly the graphs
/// that need it.
fn probe_args(fixture: &Path, out: &Path, candidate: Pipeline, encoder: Encoder) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
    ];
    args.extend(encoder.init_args());
    args.extend(candidate.init_args());
    args.extend(candidate.decode_args());
    args.push("-i".into());
    args.push(fixture.to_string_lossy().into_owned());

    let mut vf = match candidate.filters(None, PROBE_HEIGHT, Some("hdr10")) {
        Some(g) => g,
        // The CPU reference: the exact chain `video_filters` builds for an
        // HDR10 source, spelled here because a probe that measured a
        // *different* CPU chain would be comparing against a fiction.
        None => format!(
            "zscale=tin=smpte2084:min=bt2020nc:pin=bt2020:t=linear:npl=100,format=gbrpf32le,\
             tonemap=tonemap=hable:desat=0,\
             zscale=p=bt709:t=bt709:m=bt709:r=tv,format=yuv420p,\
             scale=-2:'min({PROBE_HEIGHT},ih)'"
        ),
    };
    // The CPU path uploads for a hardware encoder; the vendor graphs already
    // hand over surfaces of the right family. Same rule the real builder uses.
    let vendor_gpu = matches!(candidate, Pipeline::VppQsv | Pipeline::TonemapVaapi);
    if let Some(suffix) = encoder.filter_suffix().filter(|_| !vendor_gpu) {
        vf.push(',');
        vf.push_str(suffix);
    }
    args.push("-vf".into());
    args.push(vf);
    // No forced IDR here: this probe asks whether a tone-map graph produces the
    // right *picture* fast enough, and it writes one short clip rather than a
    // segmented playlist. Where the key frames land does not enter into it.
    args.extend(encoder.encode_args(
        4_000,
        plurx_core::transcode::EffectiveRateControl::Vbr,
        false,
        None,
    ));
    args.push(out.to_string_lossy().into_owned());
    args
}

/// Is the output tagged for an SDR display?
///
/// A correctly tone-mapped picture that is still tagged BT.2020 renders wrong
/// everywhere, and that is the failure that looks like nothing at all in a log
/// — exit status zero, a plausible-looking file, and a viewer with washed-out
/// colour. A missing tag counts as wrong: an untagged output is a claim nobody
/// made, not a passing one.
fn check_bt709(tags: &[(String, String)]) -> Result<(), String> {
    for (key, want) in [
        ("color_transfer", "bt709"),
        ("color_primaries", "bt709"),
        ("color_space", "bt709"),
    ] {
        let got = tags
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        if got != want {
            return Err(format!("output is tagged {key}={got}, not {want}"));
        }
    }
    Ok(())
}

/// Mean Y/U/V of `path`, from ffmpeg's `signalstats` filter.
async fn signal_stats<T: Tools>(tools: &T, path: &Path) -> Result<(f64, f64, f64), String> {
    let out = tools.ffprobe(signal_stats_args(path)).await?;
    parse_signal_stats(&String::from_utf8_lossy(&out.stdout))
}

/// Ask ffprobe for per-frame `signalstats`, as bare CSV.
///
/// The path goes into a *filter argument*, where a colon separates options —
/// so it is escaped rather than quoted. `csv=p=0` is what makes the output the
/// three bare numbers per line that [`parse_signal_stats`] reads; any other
/// `-of` would prefix a section name and every row would parse as nothing.
fn signal_stats_args(path: &Path) -> Vec<String> {
    [
        "-v",
        "quiet",
        "-f",
        "lavfi",
        "-i",
        &format!("movie={},signalstats", escape_lavfi(path)),
        "-show_entries",
        "frame_tags=lavfi.signalstats.YAVG,lavfi.signalstats.UAVG,lavfi.signalstats.VAVG",
        "-of",
        "csv=p=0",
    ]
    .iter()
    .map(|a| (*a).to_owned())
    .collect()
}

/// Mean Y/U/V from `signalstats`' CSV, one row per frame.
///
/// Averaged across frames rather than trusting one: a single frame can be a
/// fade, and the fixture's first frame especially. A row that did not carry
/// three numbers is skipped rather than defaulted — ffprobe truncates its
/// output when it is killed mid-write, and a half-written row read as zeros
/// would drag the mean towards black and report a good picture as clipped.
/// Nothing measurable at all is an error, never a sample of zeros: a gray
/// screen and an empty file must not score alike.
fn parse_signal_stats(text: &str) -> Result<(f64, f64, f64), String> {
    let mut n = 0.0;
    let (mut y, mut u, mut v) = (0.0, 0.0, 0.0);
    for line in text.lines() {
        let f: Vec<f64> = line
            .split(',')
            .filter(|s| !s.trim().is_empty())
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        if f.len() >= 3 {
            y += f[0];
            u += f[1];
            v += f[2];
            n += 1.0;
        }
    }
    if n == 0.0 {
        return Err("no frames to measure — the output is empty".to_owned());
    }
    Ok((y / n, u / n, v / n))
}

/// `key=value` pairs for the first video stream.
async fn probe_stream<T: Tools>(
    tools: &T,
    path: &Path,
    entries: &str,
) -> Result<Vec<(String, String)>, String> {
    let out = tools.ffprobe(stream_args(path, entries)).await?;
    Ok(parse_stream_entries(&String::from_utf8_lossy(&out.stdout)))
}

/// Ask ffprobe for named stream properties, one `key=value` per line.
///
/// `v:0` and not every stream: a file with an attached cover image has two
/// video streams, and the second one's tags would answer for the picture.
/// `default=nw=1` is the format [`parse_stream_entries`] reads.
fn stream_args(path: &Path, entries: &str) -> Vec<String> {
    [
        "-v",
        "quiet",
        "-select_streams",
        "v:0",
        "-show_entries",
        &format!("stream={entries}"),
        "-of",
        "default=nw=1",
        &path.to_string_lossy(),
    ]
    .iter()
    .map(|a| (*a).to_owned())
    .collect()
}

/// `key=value` lines from `-of default=nw=1`.
///
/// Lines without a `=` are dropped rather than guessed at: ffprobe emits
/// warnings and section markers on the same stream, and a truncated run ends
/// mid-line. A tag that did not arrive must be absent from the result, because
/// `check_bt709` reads absence as "not proven" — inventing an empty value for
/// it would turn an unreadable output into a passing one.
fn parse_stream_entries(stdout: &str) -> Vec<(String, String)> {
    stdout
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect()
}

/// Build (or reuse) the HDR10 fixture.
///
/// Cached by name in the transcode work dir, which is recreated at boot — so
/// this costs a few seconds once per start, not once per probe, and never
/// accumulates. Regenerating on every boot is deliberate over persisting it:
/// a stale fixture from an older ffmpeg is a probe measuring the wrong thing.
async fn fixture<T: Tools>(tools: &T, work_dir: &Path) -> Result<PathBuf, String> {
    let path = work_dir.join("hdr10-probe.mkv");
    if tokio::fs::metadata(&path).await.is_ok() {
        return Ok(path);
    }
    tokio::fs::create_dir_all(work_dir)
        .await
        .map_err(|e| format!("{e}"))?;
    let out = tools.ffmpeg(fixture_args(&path), Stdout::Inherit).await?;
    check_exit(&out)?;
    Ok(path)
}

/// The command that generates the HDR10 fixture.
///
/// Pure, because the fixture is the measurement's foundation: without real
/// PQ/BT.2020 signalling *in the stream* a hardware decoder has nothing to
/// carry through to the filter, and a probe run on such a clip would pass every
/// candidate — which is worse than having no probe at all.
fn fixture_args(path: &Path) -> Vec<String> {
    [
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
        &format!(
            "testsrc2=size=3840x2160:rate={FIXTURE_FPS}:duration={FIXTURE_SECONDS},format=yuv420p10le"
        ),
        "-c:v",
        "libx265",
        "-preset",
        "ultrafast",
        // Real HDR10 signalling in the stream, not just a pixel format.
        // This is the part a hardware decoder has to carry through to the
        // filter — losing it is what turns a PQ signal into flat gray, and
        // a fixture without it could never have caught that.
        "-x265-params",
        "colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:\
         master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1):\
         max-cll=1000,400",
        "-pix_fmt",
        "yuv420p10le",
        &path.to_string_lossy(),
    ]
    .iter()
    .map(|a| (*a).to_owned())
    .collect()
}

fn first_line(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no error output")
        .to_owned()
}

/// `movie=` takes a filter-argument path: colons and backslashes are special.
fn escape_lavfi(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace(':', "\\:")
        .replace('\'', "\\'")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// True when this ffmpeg build actually has every named filter.
    ///
    /// `require_ffmpeg` only proves the binary runs. The CPU tone-map chain
    /// needs `zscale` (libzimg) and `tonemap`, and a stock Homebrew ffmpeg
    /// ships without zscale — so the guard passed and the graph then failed
    /// with `No such filter: 'zscale'`, which reads as a broken repo rather
    /// than a thin ffmpeg. Now asked through the production
    /// [`declares_filters`] rather than a private copy of it, so the parse a
    /// burn preflight refuses a session on is the parse this guard proves.
    async fn has_filters(names: &[&str]) -> bool {
        let args = vec!["-hide_banner".to_owned(), "-filters".to_owned()];
        let Ok(out) = Spawn.ffmpeg(args, Stdout::Capture).await else {
            return false;
        };
        declares_filters(&String::from_utf8_lossy(&out.stdout), names)
    }

    #[test]
    fn a_gray_screen_is_further_from_the_reference_than_any_tone_map_operator() {
        let reference = Sample {
            y: 84.0,
            u: 136.0,
            v: 130.0,
            elapsed: Duration::from_secs(4),
        };
        // A different-but-sane operator: a few levels apart, which is what
        // hable and bt.2390 actually differ by. Must pass.
        let other_operator = Sample {
            y: 91.0,
            u: 132.0,
            v: 134.0,
            ..reference
        };
        assert!(other_operator.max_delta(&reference) < MAX_CHANNEL_DELTA);

        // The failure this exists for: everything at mid-gray.
        let gray = Sample {
            y: 128.0,
            u: 128.0,
            v: 128.0,
            ..reference
        };
        assert!(
            gray.max_delta(&reference) > MAX_CHANNEL_DELTA,
            "gray must fail"
        );

        // And its opposite, a clipped black frame.
        let clipped = Sample {
            y: 16.0,
            u: 128.0,
            v: 128.0,
            ..reference
        };
        assert!(
            clipped.max_delta(&reference) > MAX_CHANNEL_DELTA,
            "clipping must fail"
        );
    }

    /// A graph that ties the node to a driver has to pay for it. §2.9 measured
    /// the CPU tone-map costing ~28% of throughput, so a graph that removes it
    /// and comes back level was not doing what it claimed.
    #[test]
    fn matching_the_cpu_chain_is_not_a_win() {
        let reference = Sample {
            y: 84.0,
            u: 136.0,
            v: 130.0,
            elapsed: Duration::from_secs(10),
        };
        let same_speed = Sample {
            elapsed: Duration::from_secs(10),
            ..reference
        };
        assert!(
            decide(&same_speed, &reference).is_err(),
            "a graph that draws level has bought only a driver dependency"
        );
        let twice_as_fast = Sample {
            elapsed: Duration::from_secs(5),
            ..reference
        };
        assert_eq!(decide(&twice_as_fast, &reference), Ok(2.0));
    }

    /// Correctness is judged before speed, so a fast wrong answer reads as
    /// wrong. Reported the other way round, it would send whoever is reading
    /// to look at GPU clocks instead of at a gray screen.
    #[test]
    fn a_fast_wrong_answer_is_reported_as_wrong() {
        let reference = Sample {
            y: 84.0,
            u: 136.0,
            v: 130.0,
            elapsed: Duration::from_secs(10),
        };
        let fast_and_gray = Sample {
            y: 128.0,
            u: 128.0,
            v: 128.0,
            elapsed: Duration::from_secs(1),
        };
        let why = decide(&fast_and_gray, &reference).expect_err("must be rejected");
        assert!(
            why.contains("not the same picture"),
            "a gray screen must not be reported as a speed problem: {why}"
        );
    }

    /// Falling back is a real answer, not an absence of one: a node that never
    /// probed still has to name a pipeline, and it has to be the safe one.
    #[test]
    fn an_unprobed_node_still_answers() {
        let report = PipelineReport::cpu_only("no ffmpeg");
        assert_eq!(report.selected(), Pipeline::Cpu);
        assert!(!report.ran);
        assert!(!report.verdicts.is_empty(), "it must say why");
    }

    /// The probe's own machinery, end to end, against a real HDR10 encode.
    ///
    /// It cannot test a GPU graph in CI — there is no GPU — but everything
    /// underneath one is testable, and each of those is a failure that would
    /// make the probe *pass everything*, which is worse than not having one:
    /// a fixture that isn't really HDR10 (the decoder has nothing to
    /// preserve), a reference chain that doesn't produce BT.709 (every
    /// candidate is then compared against a wrong answer), or a measurement
    /// that reads no frames (a gray screen scores identically to a good one).
    #[tokio::test]
    async fn the_reference_run_produces_real_bt709_from_real_hdr10() {
        crate::transcode::require_ffmpeg();
        // A box whose ffmpeg cannot tone-map at all has nothing for this probe
        // to measure, so skip rather than fail — the same way the metadata
        // tests degrade to "not run here" when ffmpeg is absent. CI runs a
        // build that has these, so the coverage is not lost. Note this is also
        // a real deployment fact, not just a test one: plurxd on such a build
        // cannot run the CPU tone-map chain either, which is why OPERATIONS
        // recommends jellyfin-ffmpeg.
        if !has_filters(&["zscale", "tonemap"]).await {
            eprintln!(
                "skipping the_reference_run_produces_real_bt709_from_real_hdr10: \
                 `{}` has no zscale/tonemap — install jellyfin-ffmpeg or point \
                 PLURX_FFMPEG at a full build",
                ffmpeg_bin()
            );
            return;
        }
        let dir = tempfile::tempdir().expect("workdir");

        let clip = fixture(&Spawn, dir.path()).await.expect("fixture");
        // Really HDR10, in the stream, not just a 10-bit pixel format — the
        // decoder has to carry this through for a GPU graph to be testable.
        let tags = probe_stream(&Spawn, &clip, "color_transfer,color_primaries,color_space")
            .await
            .expect("probe");
        let get = |k: &str| {
            tags.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(get("color_transfer"), "smpte2084", "fixture is not PQ");
        assert_eq!(get("color_primaries"), "bt2020", "fixture is not BT.2020");

        // Generating it twice reuses the first: the probe costs a few seconds
        // once per boot, not once per candidate.
        assert_eq!(fixture(&Spawn, dir.path()).await.expect("cached"), clip);

        // The reference run: the CPU chain, checked the same ways every
        // candidate is. It asserts BT.709 on its output internally, so
        // reaching a Sample at all is that assertion passing.
        let out = dir.path().join("out.mp4");
        let sample = run(&Spawn, &clip, &out, Pipeline::Cpu, Encoder::Software)
            .await
            .expect("the CPU chain must work — it is the fallback for everything");
        assert!(
            sample.y > 1.0 && sample.y < 254.0,
            "a black or blown-out reference means the measurement is broken, not the picture: {sample:?}"
        );

        // And it is its own yardstick: compared against itself it is the same
        // picture at the same speed, so it fails on speed alone.
        let why = decide(&sample, &sample).expect_err("nothing beats itself");
        assert!(why.contains("the CPU chain"), "wrong rejection: {why}");
    }

    use std::os::unix::process::ExitStatusExt;

    /// ffmpeg and ffprobe replaced by output captured from real runs, so every
    /// decision made *about* that output is exercised — including the
    /// truncated and malformed shapes a probe meets in the field.
    #[derive(Default)]
    struct Recorded {
        ffmpeg: Vec<Result<std::process::Output, String>>,
        ffprobe: Vec<Result<std::process::Output, String>>,
        ffmpeg_args: std::cell::RefCell<Vec<Vec<String>>>,
        ffprobe_args: std::cell::RefCell<Vec<Vec<String>>>,
        next_ffmpeg: std::cell::Cell<usize>,
        next_ffprobe: std::cell::Cell<usize>,
    }

    fn ok_output(stdout: &str) -> Result<std::process::Output, String> {
        Ok(std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        })
    }

    fn failed_output(stderr: &str) -> Result<std::process::Output, String> {
        Ok(std::process::Output {
            // Exit code 1, encoded the way `wait()` reports it.
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        })
    }

    /// Output captured from a real `ffprobe -show_entries stream=... -of
    /// default=nw=1` over a tone-mapped clip.
    const BT709_TAGS: &str = "color_space=bt709\ncolor_transfer=bt709\ncolor_primaries=bt709\n";
    /// Real `signalstats` CSV: YAVG,UAVG,VAVG per frame.
    const SIGNALSTATS: &str = "84.1,136.0,130.2\n83.7,135.6,130.9\n84.4,136.4,129.8\n";

    impl Tools for Recorded {
        async fn ffmpeg(
            &self,
            args: Vec<String>,
            _stdout: Stdout,
        ) -> Result<std::process::Output, String> {
            self.ffmpeg_args.borrow_mut().push(args);
            let i = self.next_ffmpeg.get();
            self.next_ffmpeg.set(i + 1);
            self.ffmpeg
                .get(i)
                .cloned()
                .unwrap_or_else(|| Err("no recorded ffmpeg run".to_owned()))
        }

        async fn ffprobe(&self, args: Vec<String>) -> Result<std::process::Output, String> {
            self.ffprobe_args.borrow_mut().push(args);
            let i = self.next_ffprobe.get();
            self.next_ffprobe.set(i + 1);
            self.ffprobe
                .get(i)
                .cloned()
                .unwrap_or_else(|| Err("no recorded ffprobe run".to_owned()))
        }
    }

    fn sample(y: f64, secs: f64) -> Sample {
        Sample {
            y,
            u: 136.0,
            v: 130.0,
            elapsed: Duration::from_secs_f64(secs),
        }
    }

    /// Records which candidates were actually asked to run, so the
    /// arbitration's *order* is checkable and not just its answer.
    struct Recorder {
        outcomes: Vec<(Pipeline, Result<Sample, String>)>,
        asked: std::cell::RefCell<Vec<Pipeline>>,
    }

    impl Recorder {
        fn new(outcomes: Vec<(Pipeline, Result<Sample, String>)>) -> Recorder {
            Recorder {
                outcomes,
                asked: std::cell::RefCell::new(Vec::new()),
            }
        }

        fn run(&self, candidate: Pipeline) -> Result<Sample, String> {
            self.asked.borrow_mut().push(candidate);
            self.outcomes
                .iter()
                .find(|(p, _)| *p == candidate)
                .map(|(_, r)| r.clone())
                .unwrap_or_else(|| Err("not configured".to_owned()))
        }
    }

    /// A software encode has nothing to keep on the GPU — the frames would come
    /// down for the encoder anyway — so no candidate is even run.
    /// A stock Homebrew ffmpeg ships without zscale (no libzimg), so the guard
    /// that reads this listing decides whether the CPU tone-map chain can run
    /// at all — and a substring match would say yes on a build that cannot.
    #[test]
    fn a_filter_is_matched_on_its_own_column() {
        let listing = " ... zscale           V->V       Scale.\n\
                        .S tonemap           V->V       Conversion to/from HDR.\n";
        assert!(declares_filters(listing, &["zscale", "tonemap"]));
        assert!(!declares_filters(listing, &["zscale", "tonemap_opencl"]));
        // The failure the column rule exists for: a build without zscale that
        // names it in another filter's description.
        assert!(!declares_filters(
            " .S tonemap  V->V  Conversion (see also zscale).\n",
            &["zscale"]
        ));
        assert!(!declares_filters("", &["zscale"]));
    }

    /// Real rows from `ffmpeg -filters`, in the two shapes that matter: a
    /// build that can burn, and Homebrew's ffmpeg 8.1.2, which has `overlay`
    /// and `scale` but no `subtitles` at all and fails a text burn at spawn
    /// with `No such filter: 'subtitles'`.
    const FULL_FILTERS: &str = " TS overlay           VV->V      Overlay a video source on top of the input.\n \
                                 .. scale             V->V       Scale the input video size.\n \
                                 ..C subtitles        V->V       Render text subtitles onto input video.\n";
    const NO_LIBASS_FILTERS: &str = " TS overlay           VV->V      Overlay a video source on top of the input.\n \
                                      .. scale             V->V       Scale the input video size.\n \
                                      .S tonemap           V->V       Conversion to/from different dynamic ranges.\n";

    /// A build that has the filters refuses nothing; one that is missing a
    /// filter refuses that burn *by name*, before an ffmpeg is spawned to die
    /// of it. This check used to exist only under `#[cfg(test)]`, so
    /// production never asked and the viewer got an anonymous failure a
    /// minute later.
    #[test]
    fn a_burn_is_refused_by_name_only_on_a_build_that_cannot_do_it() {
        let full = burn_filters_from_listing(FULL_FILTERS);
        assert!(full.probed && full.overlay && full.scale && full.subtitles);
        assert_eq!(full.refusal(true), None, "a full build burns bitmaps");
        assert_eq!(full.refusal(false), None, "and text");

        let thin = burn_filters_from_listing(NO_LIBASS_FILTERS);
        assert!(thin.probed && thin.overlay && thin.scale && !thin.subtitles);
        // A bitmap burn needs overlay + scale, which this build has. Refusing
        // it would break the web player's only burn path for no reason.
        assert_eq!(
            thin.refusal(true),
            None,
            "a missing libass says nothing about the bitmap composite"
        );
        let refusal = thin.refusal(false).expect("a text burn cannot run here");
        assert!(
            refusal.contains("subtitles"),
            "the refusal must name the missing filter: {refusal}"
        );
        assert!(
            refusal.contains("ffmpeg"),
            "and what to do about it: {refusal}"
        );

        // And the bitmap side, on a build with no overlay.
        let no_overlay = burn_filters_from_listing(
            " .. scale             V->V       Scale the input video size.\n\
              ..C subtitles        V->V       Render text subtitles onto input video.\n",
        );
        let refusal = no_overlay
            .refusal(true)
            .expect("no overlay, no bitmap composite");
        assert!(refusal.contains("overlay"), "{refusal}");
        assert_eq!(no_overlay.refusal(false), None);
    }

    /// A listing that could not be read is not a build that answered "no".
    /// Refusing every burn because one capability query failed would be a new
    /// outage; the burn proceeds exactly as it did before this probe existed.
    #[test]
    fn an_unreadable_filter_listing_refuses_nothing() {
        for listing in ["", "ffmpeg: command not found", "\n\n"] {
            let caps = burn_filters_from_listing(listing);
            assert!(!caps.probed, "{listing:?} is not a capability report");
            assert_eq!(caps.refusal(true), None, "{listing:?}");
            assert_eq!(caps.refusal(false), None, "{listing:?}");
        }
        // Including the case where the spawn itself failed.
        assert_eq!(BurnFilters::default().refusal(true), None);
    }

    /// Through the same spawn seam the tone-map probe uses, so the preflight
    /// reads a real `-filters` invocation rather than a second copy of one.
    #[tokio::test]
    async fn the_burn_preflight_asks_ffmpeg_for_its_filter_list() {
        let tools = Recorded {
            ffmpeg: vec![ok_output(NO_LIBASS_FILTERS)],
            ..Recorded::default()
        };
        let caps = burn_filters_with(&tools).await;
        assert_eq!(
            tools.ffmpeg_args.borrow()[0],
            vec!["-hide_banner".to_owned(), "-filters".to_owned()]
        );
        assert!(caps.probed && !caps.subtitles);

        // A spawn that never happened answers "unprobed", not "unsupported".
        let broken = Recorded {
            ffmpeg: vec![Err("could not run ffmpeg: ENOENT".to_owned())],
            ..Recorded::default()
        };
        assert!(!burn_filters_with(&broken).await.probed);
    }

    #[tokio::test]
    async fn a_software_encoder_skips_the_probe_entirely() {
        let dir = tempfile::tempdir().expect("workdir");
        // Through the real entry point: a software encode must not reach the
        // tools at all, so this cannot spawn anything.
        assert_eq!(
            probe(dir.path(), Encoder::Software).await.selected(),
            Pipeline::Cpu
        );
        let report = probe_with(&Recorded::default(), dir.path(), Encoder::Software).await;
        assert_eq!(report.selected(), Pipeline::Cpu);
        assert!(
            !report.ran,
            "nothing was measured, so nothing may be claimed"
        );
        assert!(report.verdicts[0]
            .rejected
            .as_deref()
            .unwrap_or_default()
            .contains("software encoder"));
        // And it did not leave a fixture behind on the way out.
        assert!(!dir.path().join("hdr10-probe.mkv").exists());
    }

    /// No fixture means no measurement, and no measurement means the CPU chain
    /// — stated with the reason, rather than a silent GPU claim.
    #[tokio::test]
    async fn a_node_that_cannot_build_a_fixture_stays_on_the_cpu_chain() {
        let dir = tempfile::tempdir().expect("workdir");
        // A work dir that cannot exist: its parent is a regular file.
        let blocked = dir.path().join("not-a-dir");
        std::fs::write(&blocked, b"file").expect("write");
        let report = probe_with(&Recorded::default(), &blocked.join("work"), Encoder::Nvenc).await;

        assert_eq!(report.selected(), Pipeline::Cpu);
        assert!(!report.ran);
        assert!(
            report.verdicts[0]
                .rejected
                .as_deref()
                .unwrap_or_default()
                .contains("no probe fixture"),
            "{:?}",
            report.verdicts[0].rejected
        );
    }

    /// The fixture costs a few seconds once per boot, not once per candidate.
    #[tokio::test]
    async fn an_existing_fixture_is_reused_rather_than_regenerated() {
        let dir = tempfile::tempdir().expect("workdir");
        let path = dir.path().join("hdr10-probe.mkv");
        std::fs::write(&path, b"pretend this is HDR10").expect("write");
        // No recorded run is configured, so reaching ffmpeg at all would fail.
        let tools = Recorded::default();
        assert_eq!(fixture(&tools, dir.path()).await.expect("cached"), path);
        assert!(
            tools.ffmpeg_args.borrow().is_empty(),
            "a cached fixture must not be regenerated"
        );
        assert_eq!(
            std::fs::read(&path).expect("read"),
            b"pretend this is HDR10"
        );
    }

    /// A generator that fails leaves no fixture behind and reports ffmpeg's own
    /// complaint — an empty file at that path would be worse than none, because
    /// the next boot would reuse it.
    #[tokio::test]
    async fn a_fixture_that_fails_to_generate_reports_the_encoders_complaint() {
        let dir = tempfile::tempdir().expect("workdir");
        let tools = Recorded {
            ffmpeg: vec![failed_output("Unknown encoder 'libx265'\n")],
            ..Recorded::default()
        };
        let why = fixture(&tools, dir.path())
            .await
            .expect_err("no libx265, no fixture");
        assert_eq!(why, "Unknown encoder 'libx265'");
        assert_eq!(
            tools.ffmpeg_args.borrow()[0],
            fixture_args(&dir.path().join("hdr10-probe.mkv")),
            "the generator must be run with the HDR10 signalling arguments"
        );

        // A build with no ffmpeg at all reports that instead.
        let tools = Recorded {
            ffmpeg: vec![Err("could not run ffmpeg: No such file".to_owned())],
            ..Recorded::default()
        };
        assert!(fixture(&tools, dir.path())
            .await
            .expect_err("no ffmpeg")
            .contains("could not run ffmpeg"));
    }

    /// The measurement path end to end over captured tool output: the encode
    /// runs, the output is confirmed SDR-tagged, and the picture is measured.
    #[tokio::test]
    async fn a_successful_run_measures_the_picture_it_confirmed_was_bt709() {
        let dir = tempfile::tempdir().expect("workdir");
        let clip = dir.path().join("hdr10-probe.mkv");
        let out = dir.path().join("out.mp4");
        let tools = Recorded {
            ffmpeg: vec![ok_output("")],
            ffprobe: vec![ok_output(BT709_TAGS), ok_output(SIGNALSTATS)],
            ..Recorded::default()
        };

        let sample = run(&tools, &clip, &out, Pipeline::Cpu, Encoder::Software)
            .await
            .expect("a measured sample");
        assert!((sample.y - 84.066_666).abs() < 1e-4, "{sample:?}");
        assert!((sample.u - 136.0).abs() < 1e-4, "{sample:?}");
        assert!((sample.v - 130.3).abs() < 1e-4, "{sample:?}");

        // The tags were asked of the output file, not of the fixture: probing
        // the input would confirm the source is HDR10 and prove nothing.
        let probed = tools.ffprobe_args.borrow();
        let tagged_path = probed[0].last().expect("path").clone();
        assert_eq!(tagged_path, out.to_string_lossy().into_owned());
        assert!(probed[1].iter().any(|a| a.contains("signalstats")));
    }

    /// Each of the three ways a run can fail produces its own reason, because
    /// they send whoever reads the log to three different places.
    #[tokio::test]
    async fn each_way_a_run_can_fail_reports_its_own_reason() {
        let dir = tempfile::tempdir().expect("workdir");
        let clip = dir.path().join("hdr10-probe.mkv");
        let out = dir.path().join("out.mp4");
        let go = |tools: Recorded| {
            let (clip, out) = (clip.clone(), out.clone());
            async move {
                run(&tools, &clip, &out, Pipeline::Libplacebo, Encoder::Nvenc)
                    .await
                    .expect_err("must fail")
            }
        };

        // 1. The graph did not run.
        assert_eq!(
            go(Recorded {
                ffmpeg: vec![failed_output("[vf#0:0] Impossible to convert\nExiting\n")],
                ..Recorded::default()
            })
            .await,
            "[vf#0:0] Impossible to convert"
        );

        // 2. It ran, and produced a picture still tagged for HDR — which
        // renders wrong on every SDR display and looks like nothing in a log.
        let why = go(Recorded {
            ffmpeg: vec![ok_output("")],
            ffprobe: vec![ok_output(
                "color_space=bt2020nc\ncolor_transfer=smpte2084\ncolor_primaries=bt2020\n",
            )],
            ..Recorded::default()
        })
        .await;
        assert!(why.contains("not bt709"), "{why}");

        // 3. It ran, was tagged right, and produced no frames to measure.
        let why = go(Recorded {
            ffmpeg: vec![ok_output("")],
            ffprobe: vec![ok_output(BT709_TAGS), ok_output("")],
            ..Recorded::default()
        })
        .await;
        assert!(why.contains("output is empty"), "{why}");
    }

    /// The whole probe over captured output: a fixture is generated, the CPU
    /// reference is measured, and a candidate that is the same picture three
    /// times faster takes the job.
    #[tokio::test]
    async fn a_candidate_that_is_the_same_picture_and_faster_wins_the_probe() {
        let dir = tempfile::tempdir().expect("workdir");
        let tools = Recorded {
            // fixture, reference encode, candidate encode
            ffmpeg: vec![ok_output(""), ok_output(""), ok_output("")],
            ffprobe: vec![
                ok_output(BT709_TAGS),
                ok_output(SIGNALSTATS),
                ok_output(BT709_TAGS),
                ok_output(SIGNALSTATS),
            ],
            ..Recorded::default()
        };

        let report = probe_with(&tools, dir.path(), Encoder::Nvenc).await;
        assert!(report.ran);
        // Both encodes measured the same picture; only the clock separates
        // them, and a recorded run has no clock — so the candidate is rejected
        // on speed rather than on picture, which is the honest outcome.
        assert_eq!(report.selected(), Pipeline::Cpu);
        let candidate = report
            .verdicts
            .iter()
            .find(|v| v.pipeline == Pipeline::Libplacebo.name())
            .expect("libplacebo was tried");
        assert!(
            candidate
                .rejected
                .as_deref()
                .unwrap_or_default()
                .contains("the CPU chain"),
            "{:?}",
            candidate.rejected
        );
        // The scratch output is removed on the way out — the probe must not
        // leave a stray mp4 in the session directory.
        assert!(!dir.path().join("pipeprobe-out.mp4").exists());
    }

    /// A node whose ffmpeg cannot even generate the fixture stays on the CPU
    /// chain and says so, rather than claiming an unprobed GPU graph.
    #[tokio::test]
    async fn a_probe_with_no_working_ffmpeg_claims_nothing() {
        let dir = tempfile::tempdir().expect("workdir");
        let report = probe_with(
            &Recorded {
                ffmpeg: vec![Err("could not run ffmpeg: No such file".to_owned())],
                ..Recorded::default()
            },
            dir.path(),
            Encoder::Vaapi,
        )
        .await;
        assert!(!report.ran);
        assert_eq!(report.selected(), Pipeline::Cpu);
        assert!(report.verdicts[0]
            .rejected
            .as_deref()
            .unwrap_or_default()
            .contains("no probe fixture"));
    }

    /// A non-zero exit still has to produce a reason: "rejected, no reason
    /// given" is a verdict nobody can act on.
    #[test]
    fn a_silent_failure_still_reports_something() {
        assert_eq!(check_exit(&ok_output("").expect("output")), Ok(()));
        assert_eq!(
            check_exit(&failed_output("").expect("output")),
            Err("no error output".to_owned())
        );
        assert_eq!(
            check_exit(&failed_output("Invalid argument\n").expect("output")),
            Err("Invalid argument".to_owned())
        );
    }

    /// The path goes into a filter *argument*, where a colon separates
    /// options — a data dir with a colon in it would otherwise silently become
    /// two filter options and measure nothing.
    #[test]
    fn the_signalstats_query_escapes_its_path_and_asks_for_bare_csv() {
        let args = signal_stats_args(Path::new("/data/odd:name/out.mp4"));
        let input = args
            .iter()
            .position(|a| a == "-i")
            .map(|i| args[i + 1].clone())
            .expect("an input");
        assert_eq!(input, "movie=/data/odd\\:name/out.mp4,signalstats");
        // `csv=p=0` is what produces the three bare numbers per line the
        // parser reads; any other format prefixes a section name and every
        // row parses as nothing.
        assert!(args.contains(&"csv=p=0".to_owned()), "{args:?}");
        assert!(args.iter().any(|a| a.contains("YAVG")), "{args:?}");
    }

    /// A file with an attached cover image has two video streams. Asking about
    /// all of them would let the poster's tags answer for the picture.
    #[test]
    fn the_stream_query_asks_only_the_first_video_stream() {
        let args = stream_args(Path::new("/tmp/out.mp4"), "color_transfer,color_space");
        assert!(args.windows(2).any(|w| w == ["-select_streams", "v:0"]));
        assert!(args.contains(&"stream=color_transfer,color_space".to_owned()));
        assert!(args.contains(&"default=nw=1".to_owned()));
        assert_eq!(args.last().map(String::as_str), Some("/tmp/out.mp4"));
    }

    /// A reference that did not run means nothing on this node can be
    /// validated — including, especially, a candidate that *did* run. Claiming
    /// a GPU graph off an unmeasured baseline is the failure this guards.
    #[tokio::test]
    async fn a_reference_that_cannot_run_validates_nothing() {
        let rec = Recorder::new(vec![(
            Pipeline::Cpu,
            Err("no such filter: zscale".to_owned()),
        )]);
        let r = &rec;
        let report = probe_candidates(Encoder::Nvenc, |p| async move { r.run(p) }).await;

        assert_eq!(report.selected(), Pipeline::Cpu);
        assert!(!report.ran, "an unvalidated node must not claim it probed");
        assert!(report.verdicts[0]
            .rejected
            .as_deref()
            .unwrap_or_default()
            .contains("reference run failed"));
        assert_eq!(
            *rec.asked.borrow(),
            vec![Pipeline::Cpu],
            "no candidate may be run against a reference that failed"
        );
    }

    /// Candidates are in preference order and the first proof wins — the
    /// cheaper answer is not re-litigated against the ones behind it.
    #[tokio::test]
    async fn the_first_candidate_to_prove_itself_stops_the_race() {
        let rec = Recorder::new(vec![
            (Pipeline::Cpu, Ok(sample(84.0, 10.0))),
            // Same picture, three times the speed.
            (Pipeline::Libplacebo, Ok(sample(86.0, 3.0))),
            (Pipeline::TonemapOpencl, Ok(sample(84.0, 1.0))),
        ]);
        let r = &rec;
        let report = probe_candidates(Encoder::Nvenc, |p| async move { r.run(p) }).await;

        assert_eq!(report.selected(), Pipeline::Libplacebo);
        assert!(report.ran);
        assert_eq!(
            *rec.asked.borrow(),
            vec![Pipeline::Cpu, Pipeline::Libplacebo],
            "a faster candidate behind a winner must not be run"
        );
        // The CPU chain is always in the report, as the yardstick at 1.0x.
        let cpu = report
            .verdicts
            .iter()
            .find(|v| v.pipeline == "cpu")
            .expect("the CPU chain is always reported");
        assert_eq!(cpu.speedup, Some(1.0));
        assert!(cpu.passed);
    }

    /// A vendor graph is only offered its own device's encoder. Running
    /// `vpp_qsv` against NVENC would fail in a way that reads as a broken GPU
    /// rather than a pairing that was never valid.
    #[tokio::test]
    async fn only_candidates_that_pair_with_the_encoder_are_run() {
        let rec = Recorder::new(vec![
            (Pipeline::Cpu, Ok(sample(84.0, 10.0))),
            (Pipeline::Libplacebo, Err("no vulkan device".to_owned())),
            (Pipeline::TonemapOpencl, Err("no opencl device".to_owned())),
        ]);
        let r = &rec;
        let report = probe_candidates(Encoder::Nvenc, |p| async move { r.run(p) }).await;

        assert_eq!(
            *rec.asked.borrow(),
            vec![Pipeline::Cpu, Pipeline::Libplacebo, Pipeline::TonemapOpencl],
            "the QSV and VA-API graphs do not pair with NVENC"
        );
        // Everything failed, so the node falls back — and says which graph
        // failed and how, without anyone re-running anything.
        assert_eq!(report.selected(), Pipeline::Cpu);
        assert!(report.ran, "it did measure; every candidate simply lost");
        let why: Vec<&str> = report
            .verdicts
            .iter()
            .filter_map(|v| v.rejected.as_deref())
            .collect();
        assert!(
            why.iter().any(|w| w.contains("no vulkan device")),
            "{why:?}"
        );
        assert!(
            why.iter().any(|w| w.contains("no opencl device")),
            "{why:?}"
        );
    }

    /// A candidate that ran and lost is a different finding from one that never
    /// ran at all, and the report has to keep them apart.
    #[test]
    fn a_graph_that_never_produced_output_reports_no_speedup() {
        let reference = sample(84.0, 10.0);

        let never_ran = judge_sample(
            Pipeline::Libplacebo,
            Err("Device creation failed".to_owned()),
            &reference,
        );
        assert!(!never_ran.passed);
        assert_eq!(
            never_ran.speedup, None,
            "a graph that produced nothing has no speed to report"
        );
        assert_eq!(
            never_ran.rejected.as_deref(),
            Some("Device creation failed")
        );

        // Ran, fast, and wrong: the speed is known and reported, but the
        // rejection names the picture — otherwise whoever reads the log goes
        // looking at GPU clocks instead of at a gray screen.
        let gray = judge_sample(
            Pipeline::Libplacebo,
            Ok(Sample {
                y: 128.0,
                u: 128.0,
                v: 128.0,
                elapsed: Duration::from_secs(2),
            }),
            &reference,
        );
        assert!(!gray.passed);
        assert_eq!(gray.speedup, Some(5.0), "it did run, and it was fast");
        assert!(gray
            .rejected
            .as_deref()
            .unwrap_or_default()
            .contains("not the same picture"));

        let won = judge_sample(Pipeline::Libplacebo, Ok(sample(86.0, 2.0)), &reference);
        assert!(won.passed);
        assert_eq!(won.speedup, Some(5.0));
        assert_eq!(won.rejected, None);
        assert_eq!(won.label, Pipeline::Libplacebo.label());
    }

    /// The reference must be the exact chain `video_filters` builds for an
    /// HDR10 source — a probe measuring a different CPU chain compares every
    /// candidate against a fiction.
    #[test]
    fn the_reference_command_is_the_production_cpu_chain() {
        let args = probe_args(
            Path::new("/tmp/hdr10-probe.mkv"),
            Path::new("/tmp/out.mp4"),
            Pipeline::Cpu,
            Encoder::Software,
        );
        let vf = args
            .iter()
            .position(|a| a == "-vf")
            .map(|i| args[i + 1].clone())
            .expect("a filter graph");
        assert!(vf.contains("tonemap=tonemap=hable"), "{vf}");
        assert!(vf.contains("zscale=p=bt709:t=bt709:m=bt709"), "{vf}");
        assert!(vf.contains("tin=smpte2084"), "{vf}");
        assert!(
            vf.ends_with(&format!("scale=-2:'min({PROBE_HEIGHT},ih)'")),
            "a software encode needs no upload suffix: {vf}"
        );
        assert_eq!(args.last().map(String::as_str), Some("/tmp/out.mp4"));
        assert!(args.contains(&"/tmp/hdr10-probe.mkv".to_owned()));
    }

    /// The CPU chain uploads for a hardware encoder; the vendor graphs already
    /// hand over surfaces of the right family and must not upload twice.
    #[test]
    fn only_the_graphs_that_need_an_upload_get_one() {
        let cpu_into_vaapi = probe_args(
            Path::new("/f.mkv"),
            Path::new("/o.mp4"),
            Pipeline::Cpu,
            Encoder::Vaapi,
        );
        let vf = |args: &[String]| {
            args.iter()
                .position(|a| a == "-vf")
                .map(|i| args[i + 1].clone())
                .expect("a filter graph")
        };
        let uploaded = vf(&cpu_into_vaapi);
        assert!(uploaded.ends_with("format=nv12,hwupload"), "{uploaded}");

        let vendor = probe_args(
            Path::new("/f.mkv"),
            Path::new("/o.mp4"),
            Pipeline::TonemapVaapi,
            Encoder::Vaapi,
        );
        let vendor_graph = vf(&vendor);
        assert!(
            !vendor_graph.contains("hwupload"),
            "the VA-API graph already hands over VA-API surfaces: {vendor_graph}"
        );
    }

    /// The fixture is the foundation: without PQ/BT.2020 signalling in the
    /// stream a hardware decoder has nothing to carry through, and the probe
    /// would pass every candidate — worse than having no probe at all.
    #[test]
    fn the_fixture_command_asks_for_real_hdr10_signalling() {
        let args = fixture_args(Path::new("/data/transcode/hdr10-probe.mkv"));
        let joined = args.join(" ");
        assert!(joined.contains("transfer=smpte2084"), "{joined}");
        assert!(joined.contains("colorprim=bt2020"), "{joined}");
        assert!(joined.contains("colormatrix=bt2020nc"), "{joined}");
        assert!(joined.contains("master-display="), "{joined}");
        assert!(
            joined.contains("yuv420p10le"),
            "10-bit is not HDR on its own"
        );
        // 4K, because a 1080p probe passes on hardware that cannot hold
        // realtime at the resolution that actually stutters.
        assert!(joined.contains("size=3840x2160"), "{joined}");
        assert_eq!(
            args.last().map(String::as_str),
            Some("/data/transcode/hdr10-probe.mkv")
        );
    }

    /// An untagged output is a claim nobody made, not a passing one: BT.2020
    /// tags on a tone-mapped picture render wrong on every SDR display, and
    /// exit status zero says nothing about it.
    #[test]
    fn a_missing_or_wrong_colour_tag_fails_the_output() {
        let tagged = |t: &str, p: &str, s: &str| {
            vec![
                ("color_transfer".to_owned(), t.to_owned()),
                ("color_primaries".to_owned(), p.to_owned()),
                ("color_space".to_owned(), s.to_owned()),
            ]
        };
        assert_eq!(check_bt709(&tagged("bt709", "bt709", "bt709")), Ok(()));

        let why = check_bt709(&tagged("smpte2084", "bt2020", "bt2020nc"))
            .expect_err("PQ output must be rejected");
        assert!(why.contains("color_transfer=smpte2084"), "{why}");

        // Absent is not "fine": nothing proved the output is SDR-tagged.
        let why = check_bt709(&[("color_transfer".to_owned(), "bt709".to_owned())])
            .expect_err("a missing tag must be rejected");
        assert!(why.contains("color_primaries=,"), "{why}");
        assert!(check_bt709(&[]).is_err());
    }

    /// ffprobe writes warnings and section markers on the same stream, and a
    /// killed run ends mid-line. A tag that did not arrive must be *absent*,
    /// because `check_bt709` reads absence as "not proven".
    #[test]
    fn stream_entries_survive_noise_and_truncation() {
        let entries = parse_stream_entries(
            "color_transfer=bt709\n\
             [STREAM]\n\
             color_primaries = bt709 \n\
             color_spa",
        );
        assert_eq!(
            entries,
            vec![
                ("color_transfer".to_owned(), "bt709".to_owned()),
                ("color_primaries".to_owned(), "bt709".to_owned()),
            ]
        );
        // And the truncated run therefore fails rather than passing on two
        // tags out of three.
        assert!(check_bt709(&entries).is_err());
        assert!(parse_stream_entries("").is_empty());
    }

    /// A gray screen and an empty file must not score alike: an unmeasurable
    /// output is an error, never a sample of zeros.
    #[test]
    fn signalstats_averages_frames_and_refuses_an_empty_output() {
        let (y, u, v) = parse_signal_stats("100,130,120\n200,140,140\n").expect("two frames");
        assert!((y - 150.0).abs() < 1e-9, "{y}");
        assert!((u - 135.0).abs() < 1e-9, "{u}");
        assert!((v - 130.0).abs() < 1e-9, "{v}");

        // A truncated final row carries fewer than three numbers and is
        // skipped — read as zeros it would drag the mean towards black and
        // report a good picture as clipped.
        let (y, _, _) = parse_signal_stats("100,130,120\n200,140,140\n90,13").expect("two frames");
        assert!(
            (y - 150.0).abs() < 1e-9,
            "the partial row must not count: {y}"
        );

        for empty in ["", "\n\n", "N/A,N/A,N/A\n"] {
            let why = parse_signal_stats(empty).expect_err("nothing to measure");
            assert!(why.contains("output is empty"), "{empty:?}: {why}");
        }
    }

    /// The rejection sent to the report is ffmpeg's own first complaint, not
    /// the last line of its banner.
    #[test]
    fn a_failure_is_reported_as_ffmpegs_first_complaint() {
        assert_eq!(
            first_line("\n  \n[vf#0:0] No such filter: 'zscale'\nError opening filters\n"),
            "[vf#0:0] No such filter: 'zscale'"
        );
        assert_eq!(first_line(""), "no error output");
        assert_eq!(first_line("   \n\t\n"), "no error output");
    }

    #[test]
    fn a_lavfi_path_is_escaped() {
        let p = PathBuf::from("/data/odd:name/probe.mkv");
        assert_eq!(escape_lavfi(&p), "/data/odd\\:name/probe.mkv");
    }
}
