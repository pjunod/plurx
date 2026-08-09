# PR #98 review — stage crash-safe clustering import backup

**Reviewed:** 2026-08-09 · head `b11b9d9` (single commit, merge-base =
`origin/main` @ `4243810` — the base IS current main, not stacked) · 6 files,
+402/−9, server-only · **Verdict: APPROVE WITH CHANGES** — the crash-safety
core is real and, unusually for this repo's recent history, discriminatingly
tested; the gaps are all in the *verification* layer the contract line
advertises. Four should-fixes, no blockers.

Companion to the PR#79–#94 review docs in this folder. Everything here ran in
the review sandbox (rustc 1.97.1); no client code is touched, so there is no
GPT device pass this time. The PR body itself was unreachable (the sandbox's
GitHub API proxy still refuses `pjunod/plurx`), so this reviews the commit,
not the prose.

---

## Summary

New `plurx_core::cluster::migration` module implementing CLUSTERING-PLAN §4
steps 1–4 — the source-side half of the M2 SQLite→Hiqlite import:
`prepare_sqlite_import(data_dir)` refuses a future-schema source before
touching anything, discards an abandoned `hiqlite.incoming`, then publishes a
content-addressed, fsynced SQLite backup under `data/migration/` using the
online-backup API (so committed `plurx.db-wal` contents are included), with a
0600 uuid-named temp, `quick_check`, SHA-256 naming
(`plurx-v{schema}-{sha}.db`), and an atomic rename that converges under
concurrent prepares. `SQLITE_SCHEMA_VERSION` (= `MIGRATIONS.len()` = 16) is
extracted and `migrate()` now reads it — equivalent by construction. Root
`rusqlite` gains the `backup` feature (no new crates: sha2/hex/uuid were
already workspace deps). `validation/points.toml` extends the
`database-upgrades` check to also run `cluster::migration` tests and widens
the `persistence.upgrades` contract.

**Nothing calls `prepare_sqlite_import`.** The module ships (compiled into
every plurxd — `cluster` is not feature-gated) but is inert staging for M2's
caller; it uses only rusqlite, no hiqlite types, so `hiqlite-store` stays
optional. That makes the PR behavior-neutral for every install today.

## Baseline (all green at `b11b9d9`)

`cargo build --workspace --all-targets` 5m11s · `cargo test --workspace`
**730 passed / 0 failed** · `cargo test -p plurx-core` 347 + 11 contract ·
the 3 new tests pass in 0.09s · `clippy -p plurx-core --all-targets` 0
warnings · `fmt --check` clean · catalog lint ok (18 points · 21 checks ·
411 audited files) · `history-check` ok · `operations-check` ok. The edited
check command chains with `&&` — fine, `runner.py:494` runs `shell=True`;
executed verbatim it exits 0 running 8 sqlite + 3 migration tests. `plan
--changed-from main --profile ci` correctly selects `database-upgrades`
under `persistence.upgrades` (note it is profiles ci/full/nightly only; the
commit profile still covers the tests via rust-gate).

## Mutation audit — WO-04 protocol, 8 mutations against the new tests

**Caught (5):** replacing the online-backup API with `fs::copy` of the main
db file (both backup tests fail — the flagship WAL-inclusion claim is
genuinely pinned; the test's source store holds ~519 KB of uncheckpointed
WAL) · deleting the abandoned-`hiqlite.incoming` removal · deleting the
abandoned-temp removal · moving the future-schema refusal after the incoming
cleanup (the refusal-changes-nothing test catches the reorder) · publishing
the temp without the rename.

**Survived (3), all in the verification layer:** `quick_check` made vacuous ·
the post-copy schema re-check made vacuous · the existing-backup
sha-mismatch guard made vacuous. Also unpinned by nature: every fsync and
the 0600 mode. The points.toml contract says "publishes a **verified**,
durable SQLite backup" — *verified* is precisely the word the tests don't
back.

## Probes (empirical, sandbox)

- **Crash-state source is readable — a concern raised and REFUTED, don't
  re-raise.** Simulated crash (main + wal copied, no `-shm`, no live
  connection): `prepare_sqlite_import` succeeds and the backup contains the
  WAL-only row. SQLite's read-only WAL recovery handles it.
- **The published backup is `journal_mode=wal`** (header bytes are copied),
  so *every* future open of it — including M2 verification reads — spawns
  `-wal`/`-shm` siblings next to it; and the temp's siblings during
  `quick_check` are named `.plurx.db.<uuid>.incoming-wal`/`-shm`, which the
  abandoned-temp glob (`ends_with(".incoming")`) does NOT match. Clean paths
  drop them, but SIGKILL/power-loss mid-prepare leaves litter no later run
  ever removes.
- **`source_sha256` is not the source file's hash.** Probe: sha256(`plurx.db`)
  ≠ `source_sha256` whenever the WAL is nonempty — the field holds the hash
  of the *snapshot*. Correct behavior, misleading name.
- **A `user_version=0` empty database is accepted** (publishes
  `plurx-v0-….db`); a missing `plurx.db` errors. M2's caller must own the
  "fresh install, nothing to import" decision.
- **The sha-mismatch guard works when reached:** poisoning the published
  backup at its content-addressed name makes the next prepare fail with
  "does not match its content-addressed name" — which is exactly the test
  the suite is missing (see fix 2).

## Should fix before M2 builds on this (none block merge)

**1. Rename `source_sha256` → `backup_sha256` (or `snapshot_sha256`).**
Probe-proven ≠ hash of the source file when a WAL exists — i.e., in
production always. Plan §4.8's completion marker records a "source checksum";
an M2 implementer comparing this field against `plurx.db` gets a false
mismatch, or worse, "verifies" the wrong thing. One-word API fix, free today,
breaking later.

**2. Pin the verification guards — or stop the contract at "checksummed".**
The cheapest slice is proven to work (probe above), costs ~8 lines, and uses
only the public API: first prepare → overwrite `backup_path` with garbage →
second prepare must err. That kills the surviving sha-guard mutant. The
`quick_check` and version-recheck guards can't fail via the public API
(healthy source ⇒ healthy copy), so either add a small fault seam or accept
and document that they're belt-and-braces; but with 3/8 mutants surviving in
the layer points.toml calls "verified", the word is currently the
existence-not-power pattern WO-04 exists to stop.

**3. Convert the destination to `journal_mode=DELETE` after the copy, before
hash and publish.** One `pragma_update` where the backup completes. Makes the
published snapshot a canonical single-file artifact (the hash then names
something self-contained), stops `-shm`/`-wal` sibling churn for every future
consumer, and shrinks the crash-litter surface. Independently, widen the
abandoned-temp cleanup to `.incoming-wal`/`.incoming-shm` (prefix-match
instead of suffix-match).

**4. Reword the `persistence.upgrades` contract.** "Clustering import
publishes a verified, durable SQLite backup before activation" describes a
clustering import that does not exist in any shipped path — zero callers, by
design. Name the actual object ("the staged import-preparation helper…")
until the M2 caller lands, so the catalog keeps meaning what it says.

## Notes for the M2 caller (file with the milestone, no action here)

- Backup pacing: `run_to_completion(256, 10ms)` is ~1 s of pure sleep per
  100 MB. The pause exists to yield to writers, and §4.2's quiesce means
  there are none; bump the batch or drop the sleep when wiring `plurxd run`,
  or first boots on big libraries eat dead time.
- A second live process writing during backup makes the online backup restart
  indefinitely — plan §4 already assigns "only `plurxd run` may import" to
  M2; keep it.
- Empty/missing-source semantics from the probe above.
- Three call sites still hardcode `"plurx.db"` (`cluster.rs:56`,
  `plurxd/main.rs:131`, `:206`) now that `SQLITE_FILENAME` exists —
  consistency only.
- No CHANGELOG entry — defensible (nothing user-visible; the existing
  Phase-4 `[Unreleased]` bullet still truthfully says "until import … lands")
  and no STATUS.html touch — the WO-09 sweep owns that page. Extend both when
  the import actually activates.

## What holds up (verified — don't re-raise)

Refusal-before-any-mutation is real and mutation-pinned. WAL inclusion is
real and mutation-pinned — a plain file copy would have shipped backups
missing every row still in the WAL, which is what plan §4's literal "copy
`plurx.db`" wording would have produced; the in-module explanation for
deviating is correct and should eventually flow back into the plan doc.
fsync ordering is right (temp `sync_all` before hash/rename; parent dirs
synced after create/remove/rename; the refusal path provably creates
nothing). Concurrent prepares converge via content-addressing plus the
rename-race branch. Cleanup refuses to `rm -r` an unexpected directory and
unlinks rather than follows a symlinked `hiqlite.incoming`. The 0600 temp
matters (the DB carries password hashes, API keys, and — per PR #90 — the
Trakt tokens in cleartext). The `migrate()` refactor is equivalent by
construction, and the module's rusqlite-only dependency footprint keeps
`hiqlite-store` optional exactly as the M1 reviews demanded.

---

*Method: clean clone at `b11b9d9`; full workspace build + tests; targeted
validation-harness runs; 8 hand-built mutants run against
`cargo test -p plurx-core --lib cluster::migration`; 5 filesystem/SQLite
probes via scratch integration tests (deleted after use, tree left clean).*
