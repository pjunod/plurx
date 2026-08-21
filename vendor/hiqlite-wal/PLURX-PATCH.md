# Vendored Hiqlite WAL 0.14.0

This directory is the crates.io `hiqlite-wal` 0.14.0 package, licensed under
Apache-2.0. Plurx carries two restart-recovery patches for replicated SQLite:

- Missing `last_purged_log_id` metadata is reconstructed whenever the first
  retained WAL entry is above the initial log range. Snapshot installation can
  leave entries `10000–19999` in WAL file 1, so the retained entry—not a WAL
  rollover—is the evidence that earlier logs were purged. Without this patch,
  OpenRaft requests log 0, the WAL refuses a read below 10000, and the voter
  panics before opening its cluster listener.
- Metadata updates are written and synced to a same-directory staging file,
  then atomically renamed over `meta.hql`. The upstream remove-then-create
  sequence exposes an empty file if the process exits between those operations;
  the next start then refuses `invalid metadata file length` before it can read
  the intact WAL.

Remove this vendor when an upstream Hiqlite release contains the same repair
and Plurx has upgraded to it. Until then,
`single_file_snapshot_tail_restores_its_missing_purge_boundary` and
`interrupted_metadata_replacement_keeps_the_previous_record_readable` keep the
production startup conditions load-bearing.

Cargo records this package as path-sourced, which means cargo-audit skips it.
The weekly `rust-audit.yml` job uses `scripts/vendor-audit-lock` to restore this
exact release's registry source and checksum before a second advisory scan.
That check must remain until this directory is removed.

Source: <https://crates.io/crates/hiqlite-wal/0.14.0>

The retained lockfile, README, and unit tests are upstream provenance. The
workspace excludes this directory deliberately; its focused regression runs
through its own manifest.
