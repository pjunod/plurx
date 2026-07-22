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

    /// Decode-side hwaccel flags. Kept conservative: hardware decode where the
    /// family supports feeding system-memory frames to software filters.
    pub fn decode_args(self) -> Vec<String> {
        match self {
            // cuda decode; frames downloaded implicitly for sw filters.
            Encoder::Nvenc => vec!["-hwaccel".into(), "cuda".into()],
            // VideoToolbox decode on macOS.
            Encoder::VideoToolbox => vec!["-hwaccel".into(), "videotoolbox".into()],
            // Software decode for VAAPI/QSV in this filter model (sw filters →
            // GPU upload just before the encoder; see filter_suffix).
            Encoder::Software | Encoder::Vaapi | Encoder::Qsv => vec![],
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
    pub fn encode_args(self, bitrate_kbps: u32) -> Vec<String> {
        let br = format!("{bitrate_kbps}k");
        let maxrate = format!("{}k", bitrate_kbps * 3 / 2);
        let bufsize = format!("{}k", bitrate_kbps * 2);
        match self {
            Encoder::Software => vec![
                "-c:v".into(),
                "libx264".into(),
                "-preset".into(),
                "veryfast".into(),
                "-b:v".into(),
                br,
                "-maxrate".into(),
                maxrate,
                "-bufsize".into(),
                bufsize,
                "-profile:v".into(),
                "high".into(),
            ],
            Encoder::Nvenc => vec![
                "-c:v".into(),
                "h264_nvenc".into(),
                "-preset".into(),
                "p4".into(),
                "-b:v".into(),
                br,
                "-maxrate".into(),
                maxrate,
                "-bufsize".into(),
                bufsize,
            ],
            Encoder::VideoToolbox => {
                vec!["-c:v".into(), "h264_videotoolbox".into(), "-b:v".into(), br]
            }
            Encoder::Vaapi => vec!["-c:v".into(), "h264_vaapi".into(), "-b:v".into(), br],
            Encoder::Qsv => vec!["-c:v".into(), "h264_qsv".into(), "-b:v".into(), br],
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

/// A minimal test-encode command for one encoder. Success ⇒ the hardware is
/// actually usable (compiled-in ≠ usable — a GPU-less box has all of NVENC/
/// QSV/VAAPI compiled in but none functional).
fn validation_args(encoder: Encoder) -> Vec<String> {
    let mut args: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];
    args.extend(encoder.init_args());
    args.extend([
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        "testsrc=size=64x64:rate=1:duration=0.1".into(),
    ]);
    if let Some(suffix) = encoder.filter_suffix() {
        args.push("-vf".into());
        args.push(suffix.to_owned());
    }
    args.push("-c:v".into());
    args.push(encoder.video_codec().into());
    args.extend(["-f".into(), "null".into(), "-".into()]);
    args
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
            let why = why.lines().last().map(str::trim).unwrap_or("").trim();
            tracing::warn!(
                encoder = encoder.label(),
                reason = if why.is_empty() { "no error output" } else { why },
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
}
