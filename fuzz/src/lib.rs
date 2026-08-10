//! Testable controls used only by the standalone PGS fuzz harness.

use std::ffi::OsStr;

/// Enable the disposable hosted crash proof only for the exact opt-in value.
pub fn seeded_crash_enabled(value: Option<&OsStr>) -> bool {
    value == Some(OsStr::new("1"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_explicit_seed_enables_the_crash_proof() {
        assert!(!seeded_crash_enabled(None));
        assert!(!seeded_crash_enabled(Some(OsStr::new(""))));
        assert!(!seeded_crash_enabled(Some(OsStr::new("false"))));
        assert!(seeded_crash_enabled(Some(OsStr::new("1"))));
    }
}
