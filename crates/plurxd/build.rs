//! Build-time version stamping.
//!
//! `CARGO_PKG_VERSION` on its own can't tell a tagged release apart from the
//! forty commits that came after it — every one of them reports the same
//! number to every client and every bug report. So capture the git description
//! at build time and hand it to the crate as `PLURX_BUILD`.
//!
//! Nothing here may fail the build. A source tarball, a Docker context without
//! `.git`, or a machine with no `git` on PATH all fall back to `"unknown"`, and
//! CI or a package build can inject `PLURX_BUILD_REF` instead of relying on a
//! checkout being present.

use std::path::PathBuf;
use std::process::Command;

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
