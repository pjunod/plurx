//! Build-time version stamping.
//!
//! `CARGO_PKG_VERSION` on its own can't tell a tagged release apart from the
//! forty commits that came after it — every one of them reports the same
//! number to every client and every bug report. So capture the git description
//! at build time and hand it to the crate as `PLURX_BUILD`.
//!
//! Nothing here may fail the build. A source tarball, a Docker context without
//! `.git`, or a machine with no `git` on PATH all fail to find a commit, and
//! CI or a package build can inject `PLURX_BUILD_REF` instead of relying on a
//! checkout being present.
//!
//! **When there is no commit to name, say when instead of saying nothing.**
//! `"unknown"` was the old fallback and it is useless in the one situation it
//! occurs in: somebody has just deployed and wants to know whether their change
//! is running. It cannot answer that. A build timestamp can — it does not name
//! the commit, but it distinguishes this deploy from the last one, which is the
//! actual question. So [`BUILT_AT`] is stamped unconditionally, from the clock
//! at compile time, and the UI falls back to it whenever the commit is unknown.
//!
//! The timestamp does not make the build unreproducible in any way that
//! matters: this script only re-runs when the sources or `.git` change, so the
//! stamp moves exactly when the binary does.

use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    println!("cargo:rerun-if-env-changed=PLURX_BUILD_REF");
    watch_git_head();

    let build = std::env::var("PLURX_BUILD_REF")
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .or_else(git_describe)
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=PLURX_BUILD={build}");
    println!("cargo:rustc-env=PLURX_BUILT_AT={}", built_at());
}

/// Compile time as `YYYY-MM-DDTHH:MM:SSZ`.
///
/// Hand-rolled rather than pulling a date crate into the build graph: this is
/// the only place the daemon needs to format a wall clock at build time, and a
/// dependency that exists to print seven numbers is a dependency that has to be
/// audited, updated and explained forever.
fn built_at() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil-from-days (Howard Hinnant's algorithm), shifted to a March-based
    // year so the leap day lands at the end and needs no special case.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = era * 400 + yoe + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Re-run when HEAD moves (a commit, a checkout, a new tag) so the stamp does
/// not go stale behind a cached build. Emitted only for paths that exist:
/// naming a missing file would make Cargo re-run this script on every single
/// build, which is exactly the case (no `.git`) where there is nothing to
/// re-read anyway.
fn watch_git_head() {
    let Some(root) = repo_root() else { return };
    for rel in ["HEAD", "refs", "packed-refs"] {
        let p = root.join(".git").join(rel);
        if p.exists() {
            println!("cargo:rerun-if-changed={}", p.display());
        }
    }
}

/// Walk up from the crate directory looking for the workspace's `.git`.
fn repo_root() -> Option<PathBuf> {
    let mut dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").ok()?);
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// `v0.1.0`, `v0.1.0-14-gc0ffee`, `v0.1.0-14-gc0ffee-dirty`, or — before the
/// first tag exists — the bare short hash.
fn git_describe() -> Option<String> {
    let out = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty=-dirty"])
        .current_dir(std::env::var("CARGO_MANIFEST_DIR").ok()?)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (!s.is_empty()).then_some(s)
}
