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
    // Nothing to keep on the GPU if the encode is on the CPU: the frames would
    // have to come down for it anyway.
    if encoder == Encoder::Software {
        return PipelineReport::cpu_only(
            "software encoder — a GPU graph would download every frame anyway",
        );
    }

    let fixture = match fixture(work_dir).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "could not build the HDR10 probe fixture — staying on the CPU tone-map");
            return PipelineReport::cpu_only(&format!("no probe fixture: {e}"));
        }
    };

    // The CPU chain is both the fallback and the yardstick, so it runs first
    // and its result is what everything else is measured against.
    let out = work_dir.join("pipeprobe-out.mp4");
    let reference = match run(&fixture, &out, Pipeline::Cpu, encoder).await {
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
        let verdict = judge(&fixture, &out, candidate, encoder, &reference).await;
        let passed = verdict.passed;
        verdicts.push(verdict);
        if passed {
            selected = candidate;
            break; // candidates are in preference order; the first proof wins
        }
    }
    let _ = tokio::fs::remove_file(&out).await;

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

/// Run one candidate and decide whether it earned the job.
async fn judge(
    fixture: &Path,
    out: &Path,
    candidate: Pipeline,
    encoder: Encoder,
    reference: &Sample,
) -> PipelineVerdict {
    let reject = |why: String, speedup: Option<f64>| PipelineVerdict {
        pipeline: candidate.name().to_owned(),
        label: candidate.label().to_owned(),
        passed: false,
        speedup,
        rejected: Some(why),
    };

    let sample = match run(fixture, out, candidate, encoder).await {
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
async fn run(
    fixture: &Path,
    out: &Path,
    candidate: Pipeline,
    encoder: Encoder,
) -> Result<Sample, String> {
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
    args.extend(encoder.encode_args(4_000, false, None));
    args.push(out.to_string_lossy().into_owned());

    let started = Instant::now();
    let output = tokio::process::Command::new(ffmpeg_bin())
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("could not run ffmpeg: {e}"))?;
    let elapsed = started.elapsed();
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(first_line(&err));
    }

    // Tagged BT.709, or a correctly tone-mapped picture still renders wrong on
    // every SDR display — and this is the failure that looks like nothing at
    // all in a log.
    let tags = probe_stream(out, "color_transfer,color_primaries,color_space").await?;
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

    let (y, u, v) = signal_stats(out).await?;
    Ok(Sample { y, u, v, elapsed })
}

/// Mean Y/U/V of `path`, from ffmpeg's `signalstats` filter.
async fn signal_stats(path: &Path) -> Result<(f64, f64, f64), String> {
    let out = tokio::process::Command::new(ffprobe_bin())
        .args([
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
        ])
        .output()
        .await
        .map_err(|e| format!("could not run ffprobe: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    // Average across frames rather than trusting one: a single frame can be a
    // fade, and the fixture's first frame especially.
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
async fn probe_stream(path: &Path, entries: &str) -> Result<Vec<(String, String)>, String> {
    let out = tokio::process::Command::new(ffprobe_bin())
        .args([
            "-v",
            "quiet",
            "-select_streams",
            "v:0",
            "-show_entries",
            &format!("stream={entries}"),
            "-of",
            "default=nw=1",
            &path.to_string_lossy(),
        ])
        .output()
        .await
        .map_err(|e| format!("could not run ffprobe: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect())
}

/// Build (or reuse) the HDR10 fixture.
///
/// Cached by name in the transcode work dir, which is recreated at boot — so
/// this costs a few seconds once per start, not once per probe, and never
/// accumulates. Regenerating on every boot is deliberate over persisting it:
/// a stale fixture from an older ffmpeg is a probe measuring the wrong thing.
async fn fixture(work_dir: &Path) -> Result<PathBuf, String> {
    let path = work_dir.join("hdr10-probe.mkv");
    if tokio::fs::metadata(&path).await.is_ok() {
        return Ok(path);
    }
    tokio::fs::create_dir_all(work_dir)
        .await
        .map_err(|e| format!("{e}"))?;
    let out = tokio::process::Command::new(ffmpeg_bin())
        .args([
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
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("could not run ffmpeg: {e}"))?;
    if !out.status.success() {
        return Err(first_line(&String::from_utf8_lossy(&out.stderr)));
    }
    Ok(path)
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
    /// than a thin ffmpeg.
    async fn has_filters(names: &[&str]) -> bool {
        let Ok(out) = tokio::process::Command::new(ffmpeg_bin())
            .args(["-hide_banner", "-filters"])
            .output()
            .await
        else {
            return false;
        };
        let listed = String::from_utf8_lossy(&out.stdout);
        names.iter().all(|n| {
            listed
                .lines()
                .any(|l| l.split_whitespace().nth(1) == Some(n))
        })
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

        let clip = fixture(dir.path()).await.expect("fixture");
        // Really HDR10, in the stream, not just a 10-bit pixel format — the
        // decoder has to carry this through for a GPU graph to be testable.
        let tags = probe_stream(&clip, "color_transfer,color_primaries,color_space")
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
        assert_eq!(fixture(dir.path()).await.expect("cached"), clip);

        // The reference run: the CPU chain, checked the same ways every
        // candidate is. It asserts BT.709 on its output internally, so
        // reaching a Sample at all is that assertion passing.
        let out = dir.path().join("out.mp4");
        let sample = run(&clip, &out, Pipeline::Cpu, Encoder::Software)
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

    #[test]
    fn a_lavfi_path_is_escaped() {
        let p = PathBuf::from("/data/odd:name/probe.mkv");
        assert_eq!(escape_lavfi(&p), "/data/odd\\:name/probe.mkv");
    }
}
