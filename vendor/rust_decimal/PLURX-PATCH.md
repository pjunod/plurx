# Vendored rust_decimal 1.42.1

This directory is the crates.io `rust_decimal` 1.42.1 package, licensed under
MIT. Plurx removes only the crate's unused optional rkyv 0.7 integration:
the optional dependency, its two public feature flags, and the corresponding
conditional derives on `Decimal`.

Hiqlite 0.14 depends on `rust_decimal` for numeric conversion but does not
enable either rkyv feature. Cargo still records optional dependency edges in
the shipping lockfile, so the unused rkyv 0.7.46 entry trips
[RUSTSEC-2026-0235](https://rustsec.org/advisories/RUSTSEC-2026-0235.html).
Removing the dormant integration keeps Plurx's compiled decimal behavior
unchanged and avoids an advisory exception. Upstream rust_decimal 2.0 has
also removed this integration, but hiqlite 0.14 requires the 1.x API.

Source: <https://crates.io/crates/rust_decimal/1.42.1>

The original license, README, tests, examples, build script, and source are
retained beside this note. Remove this vendor when hiqlite no longer resolves
the affected optional edge or a compatible rust_decimal 1.x release drops it.
