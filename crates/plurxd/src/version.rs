//! What this binary is, for everything that has to report it.
//!
//! Two numbers, deliberately kept apart:
//!
//! * [`SEMVER`] is the released version and nothing else. Clients compare it,
//!   so it stays a bare `MAJOR.MINOR.PATCH` that a semver parser accepts.
//! * [`BUILD`] is the git description of the exact commit — the thing that
//!   makes a bug report actionable when the reporter is running `main` rather
//!   than a release.
//!
//! Version policy lives in `docs/RELEASING.md`; the short form is that plurx is
//! 0.x, so the minor number moves on features *and* on breaking changes, and
//! the patch number on fixes.

/// The released version: bare semver, safe to parse and compare.
pub const SEMVER: &str = env!("CARGO_PKG_VERSION");

/// Git description of the commit this binary was built from — `v0.1.0`,
/// `v0.1.0-14-gc0ffee`, `…-dirty`, or `unknown` when built without a checkout.
/// Stamped by `build.rs`; overridable with `PLURX_BUILD_REF` for package builds
/// that have no `.git`.
pub const BUILD: &str = env!("PLURX_BUILD");

/// Both, for `--version` and the startup log: `0.1.0 (v0.1.0-14-gc0ffee)`.
pub const LONG: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("PLURX_BUILD"), ")");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_is_three_numeric_parts() {
        let parts: Vec<&str> = SEMVER.split('.').collect();
        assert_eq!(parts.len(), 3, "SEMVER must be MAJOR.MINOR.PATCH: {SEMVER}");
        for p in parts {
            assert!(
                !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()),
                "non-numeric semver component in {SEMVER}"
            );
        }
    }

    #[test]
    fn build_stamp_is_present() {
        assert!(!BUILD.is_empty());
        assert!(LONG.starts_with(SEMVER));
        assert!(LONG.contains(BUILD));
    }
}
