# Vendored Hiqlite 0.14.0

This directory is the crates.io `hiqlite` 0.14.0 package, licensed under
Apache-2.0. Plurx carries two compatibility patches for clustered deployments:

- `NodeConfig` selects the local node by `Node::id` and rejects duplicate ids.
  Raft ids are durable identities, so a roster such as `1, 3` is valid when an
  unredeemed join token still holds id 2. Padding that roster with a duplicate
  node creates duplicate connection targets and eventually exhausts file
  descriptors.
- The split-brain probe uses Hiqlite's configured HTTP client and API TLS
  verification policy. Auto-generated certificates are intentionally
  self-signed, so the default Reqwest client otherwise reports
  `UnknownIssuer` on every probe.

Remove this vendor when an upstream Hiqlite release contains both fixes and
Plurx has upgraded to it. Until then, the sparse-roster regression in
`crates/plurx-core/src/cluster/migration.rs` keeps the first patch load-bearing.

Cargo records this package as path-sourced, which means cargo-audit skips it.
The weekly `rust-audit.yml` job uses `scripts/vendor-audit-lock` to restore this
exact release's registry source and checksum before a second advisory scan.
That check must remain until this directory is removed.

Source: <https://crates.io/crates/hiqlite/0.14.0>

The retained lockfile, README, tests, and static assets are upstream
provenance, not an in-place test suite; the workspace excludes this directory
deliberately.
