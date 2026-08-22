//! The ffmpeg binaries and what this particular build can do.
//!
//! Every delivery path shells out to the same two binaries, and two of them
//! (the progressive remux and the HLS sessions) need the same answer to the
//! same question: does this build understand the input-pacing flags? Probing
//! it in one place, once per process, is what keeps the answer consistent —
//! and keeps a stream from failing to start because one path guessed.

use std::time::Duration;

use plurx_core::domain::MediaFile;
use plurx_core::transcode::{
    output_size, EffectiveRateControl, Encoder, OutputGrade, Pacing, Pipeline,
};

/// An override wins only when it names something. An empty `PLURX_FFMPEG=` is
/// what a Compose file produces for an unset variable, and treating that as a
/// binary called "" would fail every spawn with a confusing ENOENT.
fn resolve_bin(override_value: Option<String>, fallback: &str) -> String {
    override_value
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

/// ffmpeg binary, overridable via `PLURX_FFMPEG` (jellyfin-ffmpeg / pinned path).
pub fn ffmpeg_bin() -> String {
    resolve_bin(std::env::var("PLURX_FFMPEG").ok(), "ffmpeg")
}

/// ffprobe binary, overridable via `PLURX_FFPROBE` (jellyfin-ffmpeg / pinned).
pub fn ffprobe_bin() -> String {
    resolve_bin(std::env::var("PLURX_FFPROBE").ok(), "ffprobe")
}

/// The executable and first line it reports from `-version`.
pub async fn ffmpeg_build() -> String {
    let bin = ffmpeg_bin();
    let version = probe_ffmpeg(&["-version"])
        .await
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|line| !line.trim().is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "version unavailable".to_owned());
    format!("{bin} ({version})")
}

/// Which pacing flags this ffmpeg understands. `-readrate` landed in 5.1 and
/// `-readrate_initial_burst` in 6.1; passing either to an older build is a hard
/// exit, not a warning, so probe rather than assume. Probed once per process.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct PacingCaps {
    pub readrate: bool,
    pub initial_burst: bool,
}

static PACING: tokio::sync::OnceCell<PacingCaps> = tokio::sync::OnceCell::const_new();
static DOVI_RPU: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();
static DOVI_RESHAPE: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();
/// One probe result per hardware encoder pairing for the Dolby Vision reshape.
/// Keyed by `Encoder::name()`; a pairing is attempted only after it is proved
/// here, which is the same discipline every other renderer gets.
static DOVI_RESHAPE_HW: std::sync::OnceLock<
    tokio::sync::Mutex<std::collections::HashMap<&'static str, bool>>,
> = std::sync::OnceLock::new();
static DOVI_PASSTHROUGH: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();
static DOVI_PASSTHROUGH_QSV: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();

/// Does this build carry a given bitstream filter? Matched on a whole line of
/// `ffmpeg -bsfs`, which lists exactly one filter per line — a substring
/// search would report `dovi_rpu` for anything merely mentioning it.
fn declares_bsf(list: &str, name: &str) -> bool {
    list.lines().any(|l| l.trim() == name)
}

/// libplacebo help is option-oriented rather than one-name-per-line. Match
/// the declaration token so a prose mention cannot admit a renderer whose
/// installed filter does not actually expose Dolby Vision reshaping.
fn declares_filter_option(help: &str, name: &str) -> bool {
    help.lines().any(|line| {
        line.split_whitespace()
            .next()
            .is_some_and(|token| token == name)
    })
}

/// Help and filter listings go to stdout on modern builds and to stderr on
/// older ones, so every question here is asked of both.
fn merged_output(stdout: &[u8], stderr: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(stderr));
    text
}

/// Ask this ffmpeg one question and hand back what it said.
///
/// The thin shell around the subprocess: everything that *decides* anything
/// from the answer is a pure function below, so a missing binary and a build
/// without a feature are classified by tested code rather than by the spawn.
async fn probe_ffmpeg(args: &[&str]) -> Result<String, String> {
    match tokio::process::Command::new(ffmpeg_bin())
        .args(args)
        .output()
        .await
    {
        Ok(out) => Ok(merged_output(&out.stdout, &out.stderr)),
        Err(e) => Err(e.to_string()),
    }
}

/// Classify a `-bsfs` listing, including the case where it never arrived.
///
/// A build that could not be asked is treated exactly like one that answered
/// "no": the remux path must not attempt a filter it has no evidence for.
fn dovi_from_probe(probe: Result<String, String>) -> bool {
    let found = match &probe {
        Ok(list) => declares_bsf(list, "dovi_rpu"),
        Err(e) => {
            tracing::warn!(error = %e, "could not probe ffmpeg for bitstream filters");
            false
        }
    };
    if found {
        tracing::info!(
            "ffmpeg has dovi_rpu: Dolby Vision sources remux to their HDR10 base \
             for browsers that cannot decode DV"
        );
    } else {
        tracing::warn!(
            "this ffmpeg has no dovi_rpu bitstream filter (added in 7.1), so a Dolby \
             Vision configuration cannot be removed from a remux — browsers that \
             don't decode DV (Chrome does not; Safari does) will be given a \
             re-encode instead of the source video. Upgrade ffmpeg to 7.1+ to \
             stream those files untouched"
        );
    }
    found
}

/// Can this ffmpeg strip a Dolby Vision configuration (`dovi_rpu`, added in
/// 7.1)?
///
/// **Asked of the binary, not of its version string.** The version heuristic
/// it replaces was a proxy for the capability, and a proxy is exactly what
/// nobody can check from the outside: a Dolby Vision film that played at
/// 1080p in Chrome and untouched in Safari could not be explained without
/// someone shelling into the server, because the one fact that decided it —
/// "does this build have dovi_rpu" — was inferred rather than observed. It is
/// observed now, once, at startup (PERF-PLAN §9: mechanism claims get
/// verified against a live probe).
pub async fn has_dovi_rpu() -> bool {
    *DOVI_RPU
        .get_or_init(|| async { dovi_from_probe(probe_ffmpeg(&["-hide_banner", "-bsfs"]).await) })
        .await
}

/// Can the software-decode/tonemapx route used for non-backward-compatible
/// Dolby Vision start on this build, with the software encoder?
///
/// The option declaration is necessary but not sufficient: the SIMD filter
/// and libx264 must also work together.
/// Probe one synthetic frame through the production graph. The real Profile 5
/// validation remains responsible for proving that RPU side data changes the
/// pixels; this boot probe gates whether the renderer can be attempted at all.
///
/// See [`has_dovi_reshape_with`] for the hardware-encode pairings. Software
/// decode is not negotiable on any of them — the HEVC decoder is what attaches
/// the RPU side data `apply_dovi=1` consumes, and an inherited hardware decode
/// drops it silently — but the *encoder* only has to accept filtered frames,
/// which is what the upload suffix is for.
pub async fn has_dovi_reshape() -> bool {
    *DOVI_RESHAPE
        .get_or_init(|| async {
            let help = probe_ffmpeg(&["-hide_banner", "-h", "filter=tonemapx"]).await;
            let declared = help
                .as_ref()
                .is_ok_and(|text| declares_filter_option(text, "apply_dovi"));
            if !declared {
                tracing::warn!(
                    "ffmpeg tonemapx has no apply_dovi option; non-compatible Dolby Vision transcodes will be refused"
                );
                return false;
            }
            let passed = probe_dovi_reshape_graph(Encoder::Software).await;
            if passed {
                tracing::info!("ffmpeg proved the Dolby Vision tonemapx renderer");
            } else {
                tracing::warn!(
                    "ffmpeg could not run the Dolby Vision tonemapx renderer; non-compatible Dolby Vision transcodes will be refused"
                );
            }
            passed
        })
        .await
}

/// Run one synthetic frame through the production Dolby Vision reshape graph,
/// optionally uploading the filtered frames to a hardware encoder first.
///
/// The encoder's device init, upload suffix and production argument builder
/// are all used here. Omitting the init used to make every QSV probe fail with
/// "A hardware device reference is required", even though the real session
/// supplied that device and the node could run the graph.
async fn probe_dovi_reshape_graph(encoder: Encoder) -> bool {
    let filter = format!(
        "tonemapx=tonemap=bt2390:transfer=bt709:matrix=bt709:primaries=bt709:range=tv:format=yuv420p:apply_dovi=1,scale=64:64,format=yuv420p{}",
        encoder
            .filter_suffix_for(OutputGrade::Sdr)
            .map(|s| format!(",{s}"))
            .unwrap_or_default()
    );
    let mut command = tokio::process::Command::new(ffmpeg_bin());
    command
        .args(["-hide_banner", "-loglevel", "error"])
        .args(encoder.init_args())
        .args(["-f", "lavfi", "-i", "color=size=64x64:rate=1:color=black"])
        .args(["-frames:v", "1", "-vf", &filter])
        .args(encoder.encode_args(1_000, EffectiveRateControl::Vbr, false, None))
        .args(["-f", "null", "-"])
        .kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(20), command.status())
        .await
        .is_ok_and(|result| result.is_ok_and(|status| status.success()))
}

/// Can the Dolby Vision reshape run with this encoder on this build?
///
/// Why this exists: the reshape was pinned to software *encode* as well as
/// software decode, and only the decode half was ever load-bearing. The RPU
/// side data `apply_dovi=1` consumes is attached by the software HEVC decoder
/// and does not survive an inherited hardware decode — that constraint is
/// real and unchanged. The encoder merely has to accept the filter's output,
/// which is what `hwupload` is for. Pinning it to libx264 capped every
/// non-DV-capable client at the software Auto rung (720p) for a 4K Dolby
/// Vision source, even on a node with a perfectly good hardware encoder that
/// was doing nothing.
///
/// Nothing is assumed: each pairing is probed once, exactly like the software
/// route, and an unproved pairing falls back to software rather than failing a
/// session in front of a viewer.
pub async fn has_dovi_reshape_with(encoder: plurx_core::transcode::Encoder) -> bool {
    if encoder == Encoder::Software {
        return has_dovi_reshape().await;
    }
    // The filter chain is the same one the software route already proved; if
    // that failed, no upload target can rescue it.
    if !has_dovi_reshape().await {
        return false;
    }
    let key = encoder.label();
    let cell =
        DOVI_RESHAPE_HW.get_or_init(|| tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let mut memo = cell.lock().await;
    if let Some(known) = memo.get(key) {
        return *known;
    }
    let passed = probe_dovi_reshape_graph(encoder).await;
    if passed {
        tracing::info!(
            encoder = key,
            "ffmpeg proved the Dolby Vision reshape with a hardware encoder"
        );
    } else {
        tracing::info!(
            encoder = key,
            "the Dolby Vision reshape cannot use this hardware encoder on this build; \
             falling back to the software route"
        );
    }
    memo.insert(key, passed);
    passed
}

/// The message `tonemapx` prints when its HDR passthrough mode is asked for
/// and the input is not Dolby Vision. It is not a failure of the build.
const PASSTHROUGH_NEEDS_DOVI: &str = "passthrough only works for Dolby Vision inputs";

/// Can the HDR10 rung — Profile 5 decode → `tonemapx` HDR passthrough →
/// libx265 Main10 — start on this build?
///
/// **This is deliberately not [`has_dovi_reshape`] with the transfer swapped,
/// and the difference is the whole probe.** Two things make the obvious
/// version wrong:
///
/// 1. **The output format has to move with the transfer.** `tonemapx` with
///    `transfer=smpte2084` and an 8-bit output format does not return an
///    error — it hits `Assertion 0 failed at libavfilter/vf_tonemapx.c:1475`
///    and calls `abort()` (SIGABRT, exit 134, measured). A boot probe that
///    kept `format=yuv420p` would kill the daemon's own capability check.
///    The graph therefore comes from `Pipeline::DoviPassthrough`, whose
///    transfer and pixel format are one `OutputGrade` and cannot be
///    separated.
/// 2. **No synthetic source has an RPU.** Passthrough is Dolby-Vision-input
///    only, so the filter is entitled to refuse a `lavfi` frame with
///    [`PASSTHROUGH_NEEDS_DOVI`]. That refusal says the *input* was wrong,
///    not that the build cannot do this, so it counts as a pass — the
///    per-source proof (`dovi_reshape_changes_pixels`) is what establishes a
///    real RPU, and it runs against the actual file.
///
/// What this probe does establish: the filter takes these options, the graph
/// builds at 10-bit without aborting, and `libx265` exists in this build and
/// accepts the production `-x265-params`. A death by signal is reported as a
/// failure and logged loudly, because that is finding (1) happening for real.
pub async fn has_dovi_passthrough() -> bool {
    *DOVI_PASSTHROUGH
        .get_or_init(|| async {
            let help = probe_ffmpeg(&["-hide_banner", "-h", "filter=tonemapx"]).await;
            if !help
                .as_ref()
                .is_ok_and(|text| declares_filter_option(text, "apply_dovi"))
            {
                tracing::warn!(
                    "ffmpeg tonemapx has no apply_dovi option; the Dolby Vision HDR10 rung will be refused"
                );
                return false;
            }
            let Some(filter) = Pipeline::DoviPassthrough.filters(Some(64), 64, Some("dolby_vision"))
            else {
                return false;
            };
            let mut command = tokio::process::Command::new(ffmpeg_bin());
            command
                .kill_on_drop(true)
                .args(["-hide_banner", "-loglevel", "error"])
                .args(["-f", "lavfi", "-i", "color=size=64x64:rate=1:color=black"])
                .args(["-frames:v", "1", "-vf"])
                .arg(&filter)
                .args(["-c:v", "libx265"])
                .args(["-x265-params", "hdr10=1:repeat-headers=1"])
                .args(["-f", "null", "-"]);
            let output = tokio::time::timeout(Duration::from_secs(20), command.output()).await;
            let Ok(Ok(output)) = output else {
                tracing::warn!(
                    "ffmpeg could not run the Dolby Vision HDR10 passthrough renderer; the HDR10 rung will be refused"
                );
                return false;
            };
            let stderr = String::from_utf8_lossy(&output.stderr);
            if output.status.success() {
                tracing::info!("ffmpeg proved the Dolby Vision HDR10 passthrough renderer");
                return true;
            }
            // `code()` is None only when a signal killed the process. That is
            // the abort() case, and it must never read as a soft refusal.
            if output.status.code().is_some() && stderr.contains(PASSTHROUGH_NEEDS_DOVI) {
                tracing::info!(
                    "ffmpeg proved the Dolby Vision HDR10 passthrough renderer (it declined the \
                     synthetic non-Dolby input, which is the documented behaviour)"
                );
                return true;
            }
            tracing::warn!(
                status = ?output.status,
                "ffmpeg could not run the Dolby Vision HDR10 passthrough renderer; the HDR10 rung \
                 will be refused: {}",
                stderr.trim()
            );
            false
        })
        .await
}

/// Can this node accept the software Dolby renderer's 10-bit frames and run
/// the measured QSV Main10 encode graph?
///
/// The synthetic frame cannot carry a Dolby RPU, so the passthrough filter's
/// documented refusal is proved separately by [`has_dovi_passthrough`]. This
/// probe establishes the other half that refusal cannot reach: QSV device
/// init, P010 conversion/upload, Main10 encode, bounded VBR and PQ/BT.2020
/// output flags. A real source's RPU mutation is still proved per file.
pub async fn has_dovi_passthrough_with(encoder: Encoder) -> bool {
    if encoder == Encoder::Software {
        return has_dovi_passthrough().await;
    }
    if encoder != Encoder::Qsv || !has_dovi_passthrough().await {
        return false;
    }
    *DOVI_PASSTHROUGH_QSV
        .get_or_init(|| async {
            let Some(upload) = encoder.filter_suffix_for(OutputGrade::Hdr10) else {
                return false;
            };
            let filter = upload.to_owned();
            let mut command = tokio::process::Command::new(ffmpeg_bin());
            command
                .kill_on_drop(true)
                .args(["-hide_banner", "-loglevel", "error"])
                .args(encoder.init_args())
                .args(["-f", "lavfi", "-i", "color=size=64x64:rate=1:color=black"])
                .args(["-frames:v", "1", "-vf", &filter])
                .args(encoder.encode_args_for(
                    OutputGrade::Hdr10,
                    20_000,
                    EffectiveRateControl::Vbr,
                    false,
                    None,
                ))
                .args(["-f", "null", "-"]);
            let passed = tokio::time::timeout(Duration::from_secs(20), command.status())
                .await
                .is_ok_and(|result| result.is_ok_and(|status| status.success()));
            if passed {
                tracing::info!(
                    encoder = encoder.label(),
                    "ffmpeg proved the Dolby Vision HDR10 renderer's hardware encode half"
                );
            } else {
                tracing::warn!(
                    encoder = encoder.label(),
                    "ffmpeg could not run the Dolby Vision HDR10 hardware encode graph; using the measured software rung"
                );
            }
            passed
        })
        .await
}

async fn dovi_probe_output(file: &MediaFile, apply: bool) -> Result<Vec<String>, String> {
    let seek = file
        .duration_ms
        .map(|duration| (duration / 5).saturating_sub(1_000) as f64 / 1_000.0)
        .unwrap_or(0.0)
        .max(0.0);
    let (width, height) = output_size(file, 720)
        .ok_or_else(|| "Dolby Vision pixel probe has no valid output size".to_owned())?;
    let filter = Pipeline::DoviTonemapx
        .filters(Some(width), height, Some("dolby_vision"))
        .ok_or_else(|| "Dolby Vision renderer produced no filter graph".to_owned())?
        .replace("apply_dovi=1", &format!("apply_dovi={}", u8::from(apply)));
    let mut command = tokio::process::Command::new(ffmpeg_bin());
    command
        .kill_on_drop(true)
        .args(["-hide_banner", "-loglevel", "error"])
        .args(["-ss", &format!("{seek:.3}"), "-i"])
        .arg(&file.path)
        .args(["-map", "0:v:0", "-frames:v", "3", "-an", "-vf"])
        .arg(filter)
        .args(["-f", "framemd5", "-"]);
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .map_err(|_| "Dolby Vision pixel probe timed out".to_owned())?
        .map_err(|error| format!("starting Dolby Vision pixel probe: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Dolby Vision pixel probe exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let frames: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(str::to_owned)
        .collect();
    (!frames.is_empty())
        .then_some(frames)
        .ok_or_else(|| "Dolby Vision pixel probe produced no frame hashes".to_owned())
}

/// Prove, on the requested Profile 5 source, that the decoder exports RPU side
/// data through the production graph and that tonemapx changes pixels when
/// Dolby Vision application is enabled. A mere option probe cannot make that
/// claim because a frame with no DOVI metadata makes the option a no-op.
pub async fn dovi_reshape_changes_pixels(file: &MediaFile) -> bool {
    let enabled = dovi_probe_output(file, true).await;
    let disabled = dovi_probe_output(file, false).await;
    match (enabled, disabled) {
        (Ok(enabled), Ok(disabled)) if enabled != disabled => true,
        (Ok(_), Ok(_)) => {
            tracing::warn!(file = %file.path.display(), "Dolby Vision RPU mutation did not change sampled pixels");
            false
        }
        (enabled, disabled) => {
            tracing::warn!(file = %file.path.display(), ?enabled, ?disabled, "could not prove Dolby Vision RPU pixel reshaping");
            false
        }
    }
}

/// Scan `ffmpeg -h full` for the pacing options.
///
/// Matches the *declaration* — an indented line whose first token is the option
/// — not any mention of the name. A plain substring search reports `-readrate`
/// on an ffmpeg 4.x that has no such option, because `-re`'s own help line reads
/// "…equivalent to -readrate 1". Getting that wrong is not cosmetic: an
/// unrecognised option makes ffmpeg exit rather than warn, so every stream on
/// that build would fail to start.
fn parse_pacing_caps(help: &str) -> PacingCaps {
    let declared = |name: &str| {
        help.lines().any(|l| {
            // split_whitespace already skips the leading indent.
            l.split_whitespace().next().is_some_and(|tok| tok == name)
        })
    };
    PacingCaps {
        readrate: declared("-readrate"),
        initial_burst: declared("-readrate_initial_burst"),
    }
}

/// Classify a `-h full` listing, including the case where it never arrived.
fn pacing_from_probe(probe: Result<String, String>) -> PacingCaps {
    let caps = match &probe {
        Ok(help) => parse_pacing_caps(help),
        Err(e) => {
            tracing::warn!(error = %e, "could not probe ffmpeg for pacing support");
            PacingCaps::default()
        }
    };
    if !caps.readrate {
        tracing::warn!(
            "this ffmpeg has no -readrate; remux streams will run unpaced and can \
             saturate a client's link, and HLS sessions fall back to realtime pacing \
             (which cannot build a playback buffer). ffmpeg 6.1+ is recommended."
        );
    } else if !caps.initial_burst {
        // WARN, not info, since the publish gate raised the stakes: a
        // copy session's first playlist waits for COPY_PUBLISH_GATE_SECS
        // of media, and without a burst that cushion is produced at the
        // flat paced rate instead of at I/O speed — at the default 2x,
        // that alone is ~6+ seconds of every time-to-first-frame, and
        // it is invisible unless something names it.
        tracing::warn!(
            "this ffmpeg has -readrate but not -readrate_initial_burst (needs 6.1+; \
             jellyfin-ffmpeg7 has it): sessions are paced flat from the first byte, \
             so the copy path's publish gate fills at the paced rate instead of at \
             I/O speed and every play starts seconds slower than it needs to"
        );
    }
    caps
}

pub async fn pacing_caps() -> PacingCaps {
    *PACING
        .get_or_init(|| async {
            let help = probe_ffmpeg(&["-hide_banner", "-h", "full"]).await;
            let mut caps = pacing_from_probe(help);
            // Some builds (notably ffmpeg 8.0.1) declare the option but
            // silently ignore it. Check behaviourally when the declaration
            // looks promising.
            if caps.initial_burst {
                let measured = classify_burst(caps, probe_burst().await);
                if caps.initial_burst && !measured.initial_burst {
                    let ffmpeg_build = ffmpeg_build().await;
                    tracing::warn!(
                        "this ffmpeg build ({ffmpeg_build}) declares -readrate_initial_burst but does not honour it: \
                         sessions are paced flat from the first byte, so the copy path's publish gate \
                         fills at the paced rate instead of at I/O speed and every play starts seconds \
                         slower than it needs to"
                    );
                }
                caps = measured;
            }
            caps
        })
        .await
}

impl PacingCaps {
    /// Turn admin settings into flags this build actually has.
    ///
    /// `legacy_realtime_ok` decides what a pre-5.1 build gets. True for the
    /// copy path: it was paced with a bare `-re` before `-readrate` existed
    /// here, and an *unpaced* copy floods the session directory with a whole
    /// 4K film — realtime is the lesser evil. False for transcode, which has
    /// never been paced at all: capping an encoder at 1x on an old build
    /// would be a new regression dressed as a fallback, and a transcode that
    /// outruns realtime is bounded by the ahead-window suspend anyway.
    pub fn resolve(&self, rate: f64, burst: f64, legacy_realtime_ok: bool) -> Pacing {
        if rate <= 0.0 {
            return Pacing::unpaced();
        }
        if !self.readrate {
            return Pacing {
                legacy_re: legacy_realtime_ok,
                ..Pacing::unpaced()
            };
        }
        Pacing {
            readrate: Some(rate),
            initial_burst: self.initial_burst.then_some(burst).filter(|b| *b > 0.0),
            legacy_re: false,
        }
    }
}

/// Check whether this ffmpeg build actually honours `-readrate_initial_burst`.
///
/// Some builds (notably ffmpeg 8.0.1-3ubuntu2) declare the option in their
/// help text but silently ignore it at runtime, so a purely textual probe of
/// the declaration is insufficient. This runs a short behavioural test.
///
/// The test creates a 2-second `testsrc` fixture and runs ffmpeg with a 300 s
/// burst at 2x readrate. An honoured burst consumes the fixture at I/O speed
/// (sub-200 ms). An inert burst paces at the readrate and takes ~1 second of
/// wall time. A 600 ms threshold cleanly separates the two cases.
fn classify_burst(mut caps: PacingCaps, probe: Result<Duration, String>) -> PacingCaps {
    caps.initial_burst &= probe.is_ok_and(|elapsed| elapsed < Duration::from_millis(600));
    caps
}

fn burst_probe_args() -> [&'static str; 14] {
    [
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-readrate_initial_burst",
        "300",
        "-readrate",
        "2",
        "-i",
        "testsrc=duration=2:size=2x2:rate=1",
        "-f",
        "null",
        "-",
    ]
}

async fn probe_burst() -> Result<Duration, String> {
    let args = burst_probe_args();
    let start = std::time::Instant::now();
    match tokio::process::Command::new(ffmpeg_bin())
        .args(args)
        .output()
        .await
    {
        Ok(out) if out.status.success() => Ok(start.elapsed()),
        Ok(out) => Err(format!("exited with {}", out.status)),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Compose file with an unset variable hands the process `PLURX_FFMPEG=`,
    /// and spawning a binary named "" fails with an ENOENT that names nothing.
    #[test]
    fn an_empty_override_is_not_a_binary_name() {
        assert_eq!(resolve_bin(None, "ffmpeg"), "ffmpeg");
        assert_eq!(resolve_bin(Some(String::new()), "ffprobe"), "ffprobe");
        assert_eq!(
            resolve_bin(Some("/opt/jellyfin-ffmpeg/ffmpeg".to_owned()), "ffmpeg"),
            "/opt/jellyfin-ffmpeg/ffmpeg"
        );
    }

    /// Older builds print help and listings to stderr, so a probe that read
    /// stdout alone would report every capability as absent on them.
    #[test]
    fn a_probe_reads_both_streams() {
        assert_eq!(merged_output(b"out\n", b"err\n"), "out\nerr\n");
        // Invalid UTF-8 is replaced rather than dropped: a lossy byte must not
        // take the rest of the listing with it.
        assert!(merged_output(&[0xff, b'\n'], b"dovi_rpu\n").ends_with("dovi_rpu\n"));
    }

    /// `-bsfs` lists exactly one filter per line. A substring search would
    /// claim `dovi_rpu` on any build whose listing merely mentions it — and
    /// then every Dolby Vision remux would fail at session start.
    #[test]
    fn a_bitstream_filter_is_matched_on_a_whole_line() {
        let listing = "Bitstream filters:\n  h264_mp4toannexb\n  dovi_rpu\n  hevc_metadata\n";
        assert!(declares_bsf(listing, "dovi_rpu"));
        assert!(!declares_bsf(listing, "av1_metadata"));
        // The failure the whole-line rule exists for.
        assert!(!declares_bsf(
            "  hevc_metadata (see also dovi_rpu)\n",
            "dovi_rpu"
        ));
    }

    /// A build that could not be asked must be classified exactly like one
    /// that answered "no". Reporting the capability on a failed probe would
    /// send every DV remux at a bitstream filter that is not there.
    #[test]
    fn an_unprobeable_ffmpeg_has_no_dovi_filter() {
        assert!(dovi_from_probe(Ok("  dovi_rpu\n".to_owned())));
        assert!(!dovi_from_probe(Ok("  hevc_metadata\n".to_owned())));
        assert!(!dovi_from_probe(Err(
            "No such file or directory (os error 2)".to_owned()
        )));
        // Truncated output — the spawn succeeded but the listing was cut off
        // mid-name. Half a filter name is not a filter.
        assert!(!dovi_from_probe(Ok(
            "Bitstream filters:\n  dovi_r".to_owned()
        )));
    }

    #[test]
    fn a_dolby_vision_renderer_option_is_matched_as_a_declaration() {
        assert!(declares_filter_option(
            "   apply_dovi <boolean> ..FV....... Apply Dolby Vision metadata if possible (default true)\n",
            "apply_dovi"
        ));
        assert!(!declares_filter_option(
            "   tonemap <int> ..FV....... used with apply_dovi\n",
            "apply_dovi"
        ));
        assert!(!declares_filter_option(
            "   apply_dovi_legacy <boolean> ..FV.......\n",
            "apply_dovi"
        ));
    }

    /// Same rule for pacing: an ffmpeg that could not be probed gets the
    /// pre-5.1 answer, because passing `-readrate` to a build without it is a
    /// hard exit rather than a warning.
    #[test]
    fn an_unprobeable_ffmpeg_gets_the_conservative_pacing_answer() {
        let modern =
            pacing_from_probe(Ok("  -readrate x\n  -readrate_initial_burst y\n".to_owned()));
        assert!(modern.readrate && modern.initial_burst);

        let failed = pacing_from_probe(Err("Permission denied (os error 13)".to_owned()));
        assert!(!failed.readrate, "a failed probe must not claim -readrate");
        assert!(!failed.initial_burst);
        // And the conservative answer must survive into the flags: nothing is
        // emitted for a transcode, `-re` for the copy path.
        assert!(failed.resolve(2.0, 90.0, false).args().is_empty());
    }

    /// The 5.1–6.0 middle build, classified through the probe rather than the
    /// parser: it keeps `-readrate` and loses only the burst clause. Worth
    /// pinning separately because this is the build where the publish gate
    /// fills at the paced rate — the flags must still carry the rate, since
    /// degrading the whole thing to `-re` would unpace every session on it.
    #[test]
    fn a_build_with_readrate_but_no_burst_keeps_its_rate() {
        let caps = pacing_from_probe(Ok(
            "  -readrate speed     read input at specified rate\n".to_owned()
        ));
        assert!(caps.readrate, "5.1+ declares -readrate");
        assert!(!caps.initial_burst, "the burst clause arrives in 6.1");
        assert_eq!(
            caps.resolve(2.0, 90.0, true).args(),
            vec!["-readrate", "2.00"],
            "the rate survives; only the burst is dropped"
        );
    }

    #[test]
    fn pacing_caps_come_from_the_help_text() {
        // ffmpeg 6.1+: both flags.
        let modern = "  -re                 read input at native frame rate\n  \
                      -readrate speed     read input at specified rate\n  \
                      -readrate_initial_burst seconds  initial burst\n";
        let caps = parse_pacing_caps(modern);
        assert!(caps.readrate);
        assert!(caps.initial_burst);

        // ffmpeg 5.1–6.0: rate limiting but no burst.
        let caps = parse_pacing_caps("  -readrate speed     read input at specified rate\n");
        assert!(caps.readrate);
        assert!(!caps.initial_burst);

        // Older: neither. Must not be fooled by the substring in -re's help.
        let caps = parse_pacing_caps(
            "  -re                 read input at native frame rate; equivalent to -readrate 1\n",
        );
        assert!(!caps.readrate);
        assert!(!caps.initial_burst);
    }

    #[test]
    fn burst_probe_classifies_honoured_inert_and_failed_measurements() {
        let declared = PacingCaps {
            readrate: true,
            initial_burst: true,
        };
        let honoured = classify_burst(declared, Ok(Duration::from_millis(599)));
        assert!(honoured.initial_burst);
        assert_eq!(
            honoured.resolve(2.0, 90.0, true).args(),
            vec!["-readrate_initial_burst", "90.0", "-readrate", "2.00"]
        );
        for probe in [
            Ok(Duration::from_millis(600)),
            Ok(Duration::from_secs(1)),
            Err("probe failed".to_owned()),
        ] {
            let corrected = classify_burst(declared, probe);
            assert!(!corrected.initial_burst);
            assert_eq!(
                corrected.resolve(2.0, 90.0, true).args(),
                vec!["-readrate", "2.00"]
            );
        }
    }

    #[test]
    fn burst_probe_applies_pacing_to_the_input() {
        let args = burst_probe_args();
        let input = args
            .iter()
            .position(|arg| *arg == "-i")
            .expect("the probe must declare its synthetic input");
        for option in ["-readrate_initial_burst", "-readrate"] {
            let option_index = args
                .iter()
                .position(|arg| *arg == option)
                .unwrap_or_else(|| panic!("the probe must pass {option}"));
            assert!(
                option_index < input,
                "{option} is an input option and must precede -i: {args:?}"
            );
        }
    }

    #[test]
    fn resolve_matches_the_build_and_the_caller() {
        let modern = PacingCaps {
            readrate: true,
            initial_burst: true,
        };
        assert_eq!(
            modern.resolve(2.0, 90.0, true).args(),
            vec!["-readrate_initial_burst", "90.0", "-readrate", "2.00"]
        );
        // Rate 0 means "unpaced" — emit nothing, whatever the build supports.
        assert!(modern.resolve(0.0, 90.0, true).args().is_empty());

        // 5.1–6.0: the rate lands, the burst clause is dropped.
        let rate_only = PacingCaps {
            readrate: true,
            initial_burst: false,
        };
        assert_eq!(
            rate_only.resolve(2.5, 90.0, true).args(),
            vec!["-readrate", "2.50"]
        );

        // Pre-5.1 splits by caller: copy degrades to realtime, transcode to
        // nothing (it was never paced, so `-re` would be a new cap).
        let ancient = PacingCaps::default();
        assert_eq!(ancient.resolve(2.0, 90.0, true).args(), vec!["-re"]);
        assert!(ancient.resolve(2.0, 90.0, false).args().is_empty());
    }

    #[tokio::test]
    #[ignore = "nightly runner capability contract"]
    async fn nightly_runner_has_ffmpeg_readrate() {
        let caps = pacing_caps().await;
        eprintln!(
            "nightly ffmpeg capability: binary={} readrate={} initial_burst={}",
            ffmpeg_bin(),
            caps.readrate,
            caps.initial_burst,
        );
        assert!(
            caps.readrate,
            "nightly arbitration coverage requires ffmpeg 5.1+ (-readrate)"
        );
    }
}
