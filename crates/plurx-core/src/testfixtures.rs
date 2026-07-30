//! Media fixtures for the tests, built by the local ffmpeg on first use.
//!
//! Two crates need the same bytes: `plurx_core::fmp4` proves the reader and
//! the merger against them, and `plurxd::copyseg` drives a whole session
//! through them. Generating them in one place means the two suites cannot
//! drift onto subtly different GOP structures and disagree about what the
//! classifier should have said.
//!
//! Compiled only for tests — `#[cfg(test)]` inside this crate, and behind the
//! `fixtures` feature that `plurxd`'s dev-dependencies turn on. It is not part
//! of the shipped library.
//!
//! Everything lands in `target/fixtures/` and is reused across runs. The
//! sources are small on purpose (12 s of 640x360): CI pays for every x265
//! encode, and none of the properties under test — GOP structure, box layout,
//! sample timing — care how big the picture is.

use std::path::PathBuf;
use std::process::Command;
use std::sync::{LazyLock, Mutex};

pub fn ffmpeg() -> String {
    std::env::var("PLURX_FFMPEG")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "ffmpeg".to_owned())
}

pub fn ffprobe() -> String {
    std::env::var("PLURX_FFPROBE")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "ffprobe".to_owned())
}

/// Fail in terms of the dependency that is missing, rather than in terms of
/// whatever a parser made of an empty file. plurxd shells out to ffmpeg at
/// runtime, so this is a thing to install, not a test to skip: skipping would
/// let CI report green on paths it never ran.
pub fn require_ffmpeg() {
    let bin = ffmpeg();
    let ok = Command::new(&bin)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok();
    assert!(
        ok,
        "these tests need ffmpeg; running `{bin}` failed. Install it \
         (`apt-get install ffmpeg`) or point PLURX_FFMPEG at a build — plurxd \
         requires it at runtime too"
    );
}

/// Run a command, or fail with what it said rather than with its exit code.
pub fn run(cmd: &mut Command) -> Vec<u8> {
    let out = cmd.output().expect("running ffmpeg");
    assert!(
        out.status.success(),
        "{:?} failed: {}",
        cmd,
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

pub fn fixture_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/
    p.pop(); // repo root
    p.push("target");
    p.push("fixtures");
    p
}

/// The GOP shape each fixture exists to have. The classifier's whole job is to
/// tell these apart, so each is spelled out rather than left to the encoder's
/// defaults.
fn x265_params(kind: &str) -> &'static str {
    match kind {
        // Every keyframe a CRA with a RASL leading picture: the disc-remux
        // shape, and the source of the bug.
        "open-gop" => {
            "keyint=42:min-keyint=42:open-gop=1:bframes=4:b-pyramid=2:\
             scenecut=0:repeat-headers=1:log-level=none"
        }
        // Every keyframe an IDR: the control.
        "closed-gop" => {
            "keyint=42:min-keyint=42:open-gop=0:bframes=4:scenecut=0:\
             repeat-headers=1:log-level=none"
        }
        // Open GOP with no B-frames, so every CRA is clean — the case a lazy
        // classifier would call dirty, and the one that gives a session real
        // clean cut points to find.
        "clean-cra" => {
            "keyint=42:min-keyint=42:open-gop=1:bframes=0:scenecut=0:\
             repeat-headers=1:log-level=none"
        }
        other => panic!("no x265 params for fixture {other}"),
    }
}

/// Serializes generation, so two tests racing for the same fixture do not both
/// encode it, and neither reads one the other is still writing.
static FIXTURE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Path to a source fixture, generating it on first use.
///
/// Known kinds: `open-gop`, `closed-gop`, `clean-cra`, `h264`, `vp9`.
pub fn source(kind: &str) -> PathBuf {
    require_ffmpeg();
    let dir = fixture_dir();
    let path = dir.join(format!("{kind}.mkv"));
    let _guard = FIXTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if path.exists() {
        return path;
    }
    std::fs::create_dir_all(&dir).expect("creating target/fixtures");
    let tmp = dir.join(format!("{kind}.mkv.tmp"));
    let mut cmd = Command::new(ffmpeg());
    cmd.args(["-y", "-v", "error"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=24:duration=12",
        ])
        .args([
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=12",
        ]);
    match kind {
        "h264" => {
            cmd.args(["-c:v", "libx264", "-preset", "ultrafast"])
                .args([
                    "-x264-params",
                    "keyint=42:min-keyint=42:open_gop=1:bframes=3",
                ])
                .args(["-c:a", "aac"]);
        }
        // The only codec pair the headless Chromium in this container can
        // actually decode, so the browser end-to-end has to be built on it.
        // VP9 has no leading pictures, so it cannot reproduce the discard —
        // what it proves is the serving contract.
        "vp9" => {
            cmd.args(["-c:v", "libvpx-vp9", "-b:v", "1M", "-deadline", "realtime"])
                .args(["-cpu-used", "8", "-g", "48", "-c:a", "libopus"]);
        }
        _ => {
            cmd.args(["-c:v", "libx265", "-preset", "ultrafast"])
                .args(["-x265-params", x265_params(kind)])
                .args(["-c:a", "aac"]);
        }
    }
    // `-f matroska` because the temp name has no extension ffmpeg knows; the
    // file is renamed into place only once complete, so a killed run never
    // leaves a half-written fixture for the next one to read.
    cmd.args(["-pix_fmt", "yuv420p", "-ac", "2", "-shortest"])
        .args(["-f", "matroska"])
        .arg(&tmp);
    run(&mut cmd);
    std::fs::rename(&tmp, &path).expect("publishing the fixture");
    path
}

/// The production pipe command — the same arguments `copy_pipe_args` builds —
/// run against a fixture, cached beside the source.
pub fn pipe(kind: &str) -> Vec<u8> {
    let src = source(kind);
    let out = pipe_path(kind);
    let _guard = FIXTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(bytes) = std::fs::read(&out) {
        return bytes;
    }
    let mut cmd = Command::new(ffmpeg());
    cmd.args(["-hide_banner", "-loglevel", "error"])
        .arg("-i")
        .arg(&src)
        .args(["-map", "0:v:0", "-map", "0:a:0?", "-sn", "-c:v", "copy"]);
    match kind {
        "h264" | "vp9" => {}
        _ => {
            cmd.args(["-tag:v", "hvc1"])
                .args(["-bsf:v", "filter_units=remove_types=32-34"]);
        }
    }
    // VP9 in fMP4 needs Opus alongside it; everything else re-encodes to AAC
    // exactly as a copy session does when the client cannot take the source.
    if kind == "vp9" {
        cmd.args(["-c:a", "copy"]);
    } else {
        cmd.args(["-c:a", "aac", "-b:a", "256k"]);
    }
    cmd.args(["-avoid_negative_ts", "make_zero"])
        .args([
            "-movflags",
            "frag_keyframe+empty_moov+default_base_moof+delay_moov",
        ])
        .args(["-f", "mp4", "pipe:1"]);
    let bytes = run(&mut cmd);
    let tmp = out.with_extension("tmp");
    std::fs::write(&tmp, &bytes).expect("caching the pipe output");
    std::fs::rename(&tmp, &out).expect("publishing the pipe output");
    bytes
}

/// Where [`pipe`] caches its output, for tests that hand the path to ffprobe.
pub fn pipe_path(kind: &str) -> PathBuf {
    fixture_dir().join(format!("{kind}.pipe.mp4"))
}
