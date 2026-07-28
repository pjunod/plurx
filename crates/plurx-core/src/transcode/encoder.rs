//! Encoder selection and per-family ffmpeg flags.
//!
//! Detection runs `ffmpeg -encoders` once at startup. Selection prefers a
//! hardware H.264 encoder in a sensible order, falling back to software x264.
//! Each family knows its own decode-side hwaccel flags and encode-side rate
//! control; software and NVENC/VideoToolbox (system-memory frames) are the
//! low-risk paths, VAAPI/QSV follow documented patterns and are validated on
//! real hardware.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoder {
    Software,
    Nvenc,
    Qsv,
    Vaapi,
    VideoToolbox,
}

impl Encoder {
    /// The ffmpeg H.264 encoder name.
    pub fn video_codec(self) -> &'static str {
        match self {
            Encoder::Software => "libx264",
            Encoder::Nvenc => "h264_nvenc",
            Encoder::Qsv => "h264_qsv",
            Encoder::Vaapi => "h264_vaapi",
            Encoder::VideoToolbox => "h264_videotoolbox",
        }
    }

    /// Human label for logs/UI.
    pub fn label(self) -> &'static str {
        match self {
            Encoder::Software => "software (x264)",
            Encoder::Nvenc => "NVIDIA NVENC",
            Encoder::Qsv => "Intel QuickSync",
            Encoder::Vaapi => "VA-API",
            Encoder::VideoToolbox => "Apple VideoToolbox",
        }
    }

    /// Hardware-device init flags, placed before the input (`-i`). VAAPI/QSV
    /// need a device to exist before their upload/encode filters run.
    pub fn init_args(self) -> Vec<String> {
        match self {
            Encoder::Vaapi => vec!["-vaapi_device".into(), vaapi_device()],
            Encoder::Qsv => vec![
                "-init_hw_device".into(),
                "qsv=hw".into(),
                "-filter_hw_device".into(),
                "hw".into(),
            ],
            _ => vec![],
        }
    }

    /// Filter-chain suffix appended after scale/tonemap/subs: uploads
    /// system-memory frames to the GPU for VAAPI/QSV encode. Empty otherwise.
    pub fn filter_suffix(self) -> Option<&'static str> {
        match self {
            Encoder::Vaapi => Some("format=nv12,hwupload"),
            Encoder::Qsv => Some("hwupload=extra_hw_frames=64,format=qsv"),
            _ => None,
        }
    }

    /// Encode-side args (encoder + rate control at `bitrate_kbps`).
    ///
    /// Every family is bounded, not just the two that always were. `-b:v`
    /// alone is a *target*, and a hardware encoder handed only a target will
    /// spend whatever it likes on a hard scene — measured well over 3× the
    /// nominal rate on grain and fast motion. That burst has to cross the same
    /// link as everything else in the house, and on Wi-Fi it is exactly the
    /// spike that empties a viewer's buffer on a stream whose *average* rate
    /// the link handles comfortably. `-maxrate` caps the instantaneous rate and
    /// `-bufsize` says over what window, which together describe a buffering
    /// model rather than a per-segment ceiling: peaks are still allowed, they
    /// just have to be paid back inside the window.
    ///
    /// 1.5× / 2× because a cap tight enough to bind constantly is a quality
    /// setting in disguise — the point is to clip the outliers that hurt
    /// delivery, not to flatten the bitrate curve a VBR encoder exists to
    /// produce (PERF-PLAN §4.6, ADAPTIVE-QUALITY.md Phase 1).
    pub fn encode_args(self, bitrate_kbps: u32) -> Vec<String> {
        let br = format!("{bitrate_kbps}k");
        let maxrate = format!("{}k", bitrate_kbps * 3 / 2);
        let bufsize = format!("{}k", bitrate_kbps * 2);
        let rate_control = |v: Vec<&str>| -> Vec<String> {
            let mut args: Vec<String> = v.into_iter().map(str::to_owned).collect();
            args.extend([
                "-b:v".to_owned(),
                br.clone(),
                "-maxrate".to_owned(),
                maxrate.clone(),
                "-bufsize".to_owned(),
                bufsize.clone(),
            ]);
            args
        };
        match self {
            Encoder::Software => {
                let mut args = rate_control(vec!["-c:v", "libx264", "-preset", "veryfast"]);
                args.extend(["-profile:v".to_owned(), "high".to_owned()]);
                args
            }
            Encoder::Nvenc => rate_control(vec!["-c:v", "h264_nvenc", "-preset", "p4"]),
            Encoder::VideoToolbox => rate_control(vec!["-c:v", "h264_videotoolbox"]),
            // No explicit `-rc_mode`. ffmpeg's VAAPI encoder already selects
            // VBR when maxrate exceeds bitrate, and then falls back to whatever
            // the driver actually implements. Forcing VBR would turn a
            // CBR-only driver from "works, roughly bounded" into "fails
            // validation, no hardware at all" — a worse outcome than the
            // slightly looser bound it was meant to tighten.
            Encoder::Vaapi => rate_control(vec!["-c:v", "h264_vaapi"]),
            Encoder::Qsv => rate_control(vec!["-c:v", "h264_qsv"]),
        }
    }
}

/// The DRI render node for VAAPI, overridable via `PLURX_VAAPI_DEVICE`.
fn vaapi_device() -> String {
    std::env::var("PLURX_VAAPI_DEVICE")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/dev/dri/renderD128".to_owned())
}

/// Which encoders this ffmpeg build exposes.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct EncoderCaps {
    pub nvenc: bool,
    pub qsv: bool,
    pub vaapi: bool,
    pub videotoolbox: bool,
}

impl EncoderCaps {
    /// Pick the best encoder, honoring an explicit preference. `prefer` is a
    /// lowercase family name ("nvenc"|"qsv"|"vaapi"|"videotoolbox"|"software")
    /// or empty for automatic. Automatic order favors the most capable common
    /// hardware, then software.
    pub fn choose(&self, prefer: &str) -> Encoder {
        match prefer {
            "software" => return Encoder::Software,
            "nvenc" if self.nvenc => return Encoder::Nvenc,
            "qsv" if self.qsv => return Encoder::Qsv,
            "vaapi" if self.vaapi => return Encoder::Vaapi,
            "videotoolbox" if self.videotoolbox => return Encoder::VideoToolbox,
            _ => {}
        }
        if self.nvenc {
            Encoder::Nvenc
        } else if self.videotoolbox {
            Encoder::VideoToolbox
        } else if self.qsv {
            Encoder::Qsv
        } else if self.vaapi {
            Encoder::Vaapi
        } else {
            Encoder::Software
        }
    }
}

/// Parse `ffmpeg -encoders` output for which hardware H.264 encoders are
/// *compiled into* this build (not whether the hardware actually works).
pub fn parse_encoder_list(output: &str) -> EncoderCaps {
    EncoderCaps {
        nvenc: output.contains("h264_nvenc"),
        qsv: output.contains("h264_qsv"),
        vaapi: output.contains("h264_vaapi"),
        videotoolbox: output.contains("h264_videotoolbox"),
    }
}

/// The probe's synthetic clip. Every number here was a false negative waiting
/// to happen in the version that used a 64×64 still at 1 fps for a tenth of a
/// second: hardware encoders have minimum dimensions, and one that buffers
/// frames for lookahead emits nothing at all from a one-frame input — which
/// ffmpeg reports as "nothing was written into output file", indistinguishable
/// from a broken device. 720p30 for half a second is fifteen frames: enough to
/// fill any lookahead and cheap enough to run four times at every boot.
const PROBE_SIZE: &str = "1280x720";
const PROBE_FPS: u32 = 30;
const PROBE_SECONDS: f32 = 0.5;
/// The ladder's real 720p rung, so the probe's rate control is production's.
const PROBE_BITRATE_KBPS: u32 = 4_000;

/// A test-encode command for one encoder, using the arguments a real session
/// would use. Success ⇒ the hardware is actually usable (compiled-in ≠ usable —
/// a GPU-less box has all of NVENC/QSV/VAAPI compiled in but none functional).
///
/// "The arguments a real session would use" is the point, and it is why this
/// calls [`Encoder::encode_args`] rather than naming a codec. The old probe
/// passed `-c:v h264_qsv` and nothing else, so it answered a question nobody
/// asked: it proved the device could encode *something*, then production
/// handed the driver a rate-control set the probe had never tried. A driver
/// that accepts the encoder and rejects `-maxrate` would pass here and fail at
/// play time, on a viewer's first press of play, with the fallback machinery
/// discovering it (PERF-PLAN §4.6, review R4).
fn validation_args(encoder: Encoder) -> Vec<String> {
    let mut args: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];
    args.extend(encoder.init_args());
    args.extend([
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        format!("testsrc=size={PROBE_SIZE}:rate={PROBE_FPS}:duration={PROBE_SECONDS}"),
    ]);
    // Production's filter chain always ends by normalising the pixel format,
    // then uploading to the GPU for the families that encode there. Both
    // matter: a hwupload that the driver refuses is precisely the failure this
    // probe exists to catch, and it cannot be caught without running one.
    let mut vf = "format=yuv420p".to_owned();
    if let Some(suffix) = encoder.filter_suffix() {
        vf.push(',');
        vf.push_str(suffix);
    }
    args.push("-vf".into());
    args.push(vf);
    args.extend(encoder.encode_args(PROBE_BITRATE_KBPS));
    args.extend(["-f".into(), "null".into(), "-".into()]);
    args
}

/// The most useful line of a failed probe's stderr.
///
/// ffmpeg states the actual fault first and summarises afterwards, so the LAST
/// line is reliably its least informative: "Nothing was written into output
/// file, because at least one of its streams received no packets" is true of
/// every failure and points at none of them. The line above it says
/// "Operation not permitted" — a missing device — or "Invalid argument", which
/// is what an operator can act on.
fn first_cause(stderr: &str) -> &str {
    stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.contains("Nothing was written into output file"))
        .unwrap_or("no error output")
}

async fn validate(ffmpeg_bin: &str, encoder: Encoder) -> bool {
    let output = tokio::process::Command::new(ffmpeg_bin)
        .args(validation_args(encoder))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .await;
    match output {
        Ok(out) if out.status.success() => {
            tracing::info!(encoder = encoder.label(), "hardware encoder validated");
            true
        }
        Ok(out) => {
            // Capturing stderr is the whole point: a bare "software x264" tells
            // the operator nothing, but "vaapi failed: Permission denied" points
            // straight at a missing render-group / device passthrough. Shown in
            // the admin log viewer at WARN.
            let why = String::from_utf8_lossy(&out.stderr);
            tracing::warn!(
                encoder = encoder.label(),
                reason = first_cause(&why),
                "hardware encoder present but failed validation — not using it"
            );
            false
        }
        Err(e) => {
            tracing::warn!(encoder = encoder.label(), error = %e, "could not run encoder probe");
            false
        }
    }
}

/// Detect *usable* encoders: parse the build's encoder list, then test-encode
/// each candidate so we never pick a compiled-but-nonfunctional GPU path.
pub async fn detect_encoders(ffmpeg_bin: &str) -> EncoderCaps {
    let output = tokio::process::Command::new(ffmpeg_bin)
        .args(["-hide_banner", "-encoders"])
        .output()
        .await;
    let compiled = match output {
        Ok(out) => parse_encoder_list(&String::from_utf8_lossy(&out.stdout)),
        Err(e) => {
            tracing::warn!(error = %e, "could not run ffmpeg -encoders; software only");
            return EncoderCaps::default();
        }
    };

    // Validate each compiled-in hardware encoder against real hardware.
    let caps = EncoderCaps {
        nvenc: compiled.nvenc && validate(ffmpeg_bin, Encoder::Nvenc).await,
        qsv: compiled.qsv && validate(ffmpeg_bin, Encoder::Qsv).await,
        vaapi: compiled.vaapi && validate(ffmpeg_bin, Encoder::Vaapi).await,
        videotoolbox: compiled.videotoolbox && validate(ffmpeg_bin, Encoder::VideoToolbox).await,
    };
    tracing::info!(
        nvenc = caps.nvenc,
        qsv = caps.qsv,
        vaapi = caps.vaapi,
        videotoolbox = caps.videotoolbox,
        "usable hardware encoders (validated); software x264 always available"
    );
    caps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_encoder_list() {
        let sample = " V....D h264_nvenc  NVIDIA\n V..... h264_qsv Intel\n V....D libx264 x264\n";
        let caps = parse_encoder_list(sample);
        assert!(caps.nvenc);
        assert!(caps.qsv);
        assert!(!caps.vaapi);
        assert!(!caps.videotoolbox);
    }

    #[test]
    fn choose_honors_preference_then_falls_back() {
        let caps = EncoderCaps {
            nvenc: true,
            vaapi: true,
            ..Default::default()
        };
        assert_eq!(caps.choose("vaapi"), Encoder::Vaapi);
        assert_eq!(caps.choose("software"), Encoder::Software);
        // Unavailable preference → automatic (nvenc wins here).
        assert_eq!(caps.choose("qsv"), Encoder::Nvenc);
        // No preference → automatic.
        assert_eq!(caps.choose(""), Encoder::Nvenc);
        // Nothing available → software.
        assert_eq!(EncoderCaps::default().choose(""), Encoder::Software);
    }

    #[test]
    fn encoder_names() {
        assert_eq!(Encoder::Software.video_codec(), "libx264");
        assert_eq!(Encoder::Vaapi.video_codec(), "h264_vaapi");
    }

    const ALL: [Encoder; 5] = [
        Encoder::Software,
        Encoder::Nvenc,
        Encoder::Qsv,
        Encoder::Vaapi,
        Encoder::VideoToolbox,
    ];

    fn has(v: &[String], needle: &str) -> bool {
        v.iter().any(|s| s == needle)
    }

    #[test]
    fn codecs_and_labels_for_every_variant() {
        // Every arm returns a distinct, non-empty codec + label.
        let codecs: Vec<_> = ALL.iter().map(|e| e.video_codec()).collect();
        assert_eq!(
            codecs,
            [
                "libx264",
                "h264_nvenc",
                "h264_qsv",
                "h264_vaapi",
                "h264_videotoolbox"
            ]
        );
        for e in ALL {
            assert!(!e.label().is_empty());
        }
        assert_eq!(Encoder::Nvenc.label(), "NVIDIA NVENC");
        assert_eq!(Encoder::VideoToolbox.label(), "Apple VideoToolbox");
    }

    #[test]
    fn init_and_filter_flags_per_family() {
        // VAAPI needs a device before the input; QSV a named hw device.
        let vaapi = Encoder::Vaapi.init_args();
        assert_eq!(vaapi.len(), 2);
        assert_eq!(vaapi[0], "-vaapi_device");
        assert_eq!(
            Encoder::Qsv.init_args(),
            vec!["-init_hw_device", "qsv=hw", "-filter_hw_device", "hw"]
        );
        // System-memory encoders need no device init.
        for e in [Encoder::Software, Encoder::Nvenc, Encoder::VideoToolbox] {
            assert!(e.init_args().is_empty(), "{e:?} should not init a device");
        }

        // Only VAAPI/QSV upload frames to the GPU via a filter suffix.
        assert_eq!(Encoder::Vaapi.filter_suffix(), Some("format=nv12,hwupload"));
        assert_eq!(
            Encoder::Qsv.filter_suffix(),
            Some("hwupload=extra_hw_frames=64,format=qsv")
        );
        for e in [Encoder::Software, Encoder::Nvenc, Encoder::VideoToolbox] {
            assert_eq!(e.filter_suffix(), None);
        }
    }

    /// EVERY family is bounded, hardware included. An encoder handed only a
    /// target bitrate treats it as a suggestion, and the burst it takes on a
    /// hard scene crosses the viewer's link as a spike — the failure this
    /// exists to prevent is a stream whose average rate the link carries
    /// easily and whose peaks empty the buffer anyway.
    #[test]
    fn every_encoder_bounds_its_bitrate() {
        for encoder in [
            Encoder::Software,
            Encoder::Nvenc,
            Encoder::VideoToolbox,
            Encoder::Vaapi,
            Encoder::Qsv,
        ] {
            let args = encoder.encode_args(4000);
            assert!(has(&args, encoder.video_codec()), "{encoder:?} codec");
            // 4000k target → 1.5x cap over a 2x window.
            assert!(
                has(&args, "-b:v") && has(&args, "4000k"),
                "{encoder:?} target"
            );
            assert!(
                has(&args, "-maxrate") && has(&args, "6000k"),
                "{encoder:?} cap"
            );
            assert!(
                has(&args, "-bufsize") && has(&args, "8000k"),
                "{encoder:?} window"
            );
        }
        // Per-family extras survive the shared rate-control block.
        let sw = Encoder::Software.encode_args(4000);
        assert!(has(&sw, "veryfast") && has(&sw, "high"));
        assert!(has(&Encoder::Nvenc.encode_args(4000), "p4"));
    }

    /// The probe must run what production runs. Proving it by *comparison*
    /// rather than by restating the flags is the whole point: a rate-control
    /// argument added to a session and not to the probe is exactly the drift
    /// that let a driver validate and then refuse the real thing (review R4).
    #[test]
    fn the_probe_encodes_with_production_arguments() {
        for encoder in [
            Encoder::Software,
            Encoder::Nvenc,
            Encoder::VideoToolbox,
            Encoder::Vaapi,
            Encoder::Qsv,
        ] {
            let probe = validation_args(encoder);
            let production = encoder.encode_args(PROBE_BITRATE_KBPS);
            let at = probe
                .windows(production.len())
                .position(|w| w == production.as_slice());
            assert!(
                at.is_some(),
                "{encoder:?}: probe does not run the production encode args\n  probe: {probe:?}\n  want:  {production:?}"
            );
            // And the device/filter setup around them, since a refused
            // hwupload is a failure only a real upload can find.
            for arg in encoder.init_args() {
                assert!(has(&probe, &arg), "{encoder:?}: probe skips {arg}");
            }
            if let Some(suffix) = encoder.filter_suffix() {
                assert!(
                    probe.iter().any(|a| a.ends_with(suffix)),
                    "{encoder:?}: probe skips the hwupload filter"
                );
            }
            assert_eq!(&probe[probe.len() - 3..], &["-f", "null", "-"]);
        }
    }

    /// The operator reads this line and nothing else. ffmpeg buries the cause
    /// above its own summary, so taking the last line — which is what this used
    /// to do — reported the one sentence that is true of every failure.
    #[test]
    fn a_failed_probe_reports_the_cause_not_the_summary() {
        let nvenc_without_a_gpu = "Error while filtering: Operation not permitted\n\
             [out#0/null @ 0x1] Nothing was written into output file, because at least one of its streams received no packets.\n";
        assert_eq!(
            first_cause(nvenc_without_a_gpu),
            "Error while filtering: Operation not permitted"
        );
        assert_eq!(
            first_cause("Error parsing global options: Invalid argument\n"),
            "Error parsing global options: Invalid argument"
        );
        // A probe that says nothing at all still has to say something.
        assert_eq!(first_cause(""), "no error output");
        assert_eq!(
            first_cause("[out#0/null @ 0x1] Nothing was written into output file, because...\n"),
            "no error output",
            "the summary alone is not a cause"
        );
    }

    /// A one-frame clip is not a test of an encoder that buffers frames: it
    /// emits nothing, and "nothing was written into output file" is
    /// indistinguishable from a dead device. This probe has to be long enough
    /// to fill a lookahead and large enough to clear hardware minimums.
    #[test]
    fn the_probe_clip_is_long_enough_to_produce_packets() {
        let args = validation_args(Encoder::Nvenc);
        let src = args
            .iter()
            .find(|a| a.starts_with("testsrc="))
            .expect("a source");
        assert!(src.contains(PROBE_SIZE), "not a real frame size: {src}");
        let frames = (PROBE_SECONDS * PROBE_FPS as f32) as u32;
        assert!(
            frames >= 10,
            "{frames} frames is not enough for a lookahead"
        );
    }

    #[test]
    fn vaapi_device_defaults_and_honors_env() {
        // Clean env → documented default render node.
        std::env::remove_var("PLURX_VAAPI_DEVICE");
        assert_eq!(vaapi_device(), "/dev/dri/renderD128");
        // Explicit override wins; empty is ignored (falls back to default).
        std::env::set_var("PLURX_VAAPI_DEVICE", "/dev/dri/renderD129");
        assert_eq!(vaapi_device(), "/dev/dri/renderD129");
        std::env::set_var("PLURX_VAAPI_DEVICE", "");
        assert_eq!(vaapi_device(), "/dev/dri/renderD128");
        std::env::remove_var("PLURX_VAAPI_DEVICE");
    }
}
