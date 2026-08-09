# Vendored rust_decimal 1.42.1

This directory is the crates.io `rust_decimal` 1.42.1 package, licensed under
MIT. Plurx removes only the crate's unused optional rkyv 0.7 integration:
the optional dependency, its two public feature flags, and the corresponding
conditional derives on `Decimal`.

Hiqlite 0.14 and openraft's `byte-unit` dependency both resolve
`rust_decimal`, but neither enables its rkyv features. Cargo still records
optional dependency edges in the shipping lockfile, so the unused rkyv 0.7.46 entry trips
[RUSTSEC-2026-0235](https://rustsec.org/advisories/RUSTSEC-2026-0235.html).
Removing the dormant integration keeps Plurx's compiled decimal behavior
unchanged and avoids an advisory exception. Upstream rust_decimal 2.0 has
also removed this integration, but hiqlite 0.14 requires the 1.x API.

Cargo records this package as path-sourced, which means cargo-audit skips it.
The weekly `rust-audit.yml` job uses `scripts/vendor-audit-lock` to restore this
exact release's registry source and checksum before a second advisory scan.
That check must remain until this directory is removed.

Source: <https://crates.io/crates/rust_decimal/1.42.1>

Only `Cargo.toml`, `build.rs`, and `src/` are load-bearing in Plurx's build.
The retained license, README, tests, examples, and benches are upstream
provenance, not an in-place test suite; the workspace excludes this directory
deliberately. Some retained upstream examples/tests still name the removed
rkyv features and are inert. Remove this vendor when every resolving dependency
accepts a rust_decimal release without the affected optional edge.
