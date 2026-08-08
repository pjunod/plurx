# Phase 3 — Cluster Spike & Decision

Phase 3 is a decision gate, not a feature phase. Two HA risks were spiked with
real experiments; this document records the measurements and the decisions
that Phase 4 builds on. Experiments run 2026-07-19 in the dev sandbox
(ffmpeg 6.1.1, hiqlite 0.14.0, Rust 1.95).

## Decision summary

1. **Replicated Store backend: hiqlite 0.14.** Its `execute` / `query_as_one` /
   `query_map` / `txn` API mirrors the rusqlite code the `Store` trait is
   already built on, it does raft-replicated SQLite with active-active reads
   and writes-from-any-node, it embeds (no external DB — matches "1 or 3+
   nodes"), and it ships a comprehensive 3-node cluster + self-heal test
   suite. openraft + redb + rusqlite remains the documented fallback but is
   not needed. **Verified:** hiqlite compiles cleanly in the workspace, and a
   live node ran a STRICT-table migration, an insert through raft, and a
   `query_as_one` read-back.

2. **Transcode failover: session-based HLS + deterministic-segment property.**
   The Phase 2 single-ffmpeg session stays the primary path (efficient, no
   per-segment artifacts). Failover = a surviving node restarts the session
   seeked to the last-served segment boundary; accurate input-seek guarantees
   any node can produce a valid segment N, so the client keeps its buffered
   segments and continues. An `EXT-X-DISCONTINUITY` is inserted at the
   failover boundary. Per-segment byte-determinism (x264 `threads=1`) is
   available as a caching/dedup optimization but is **not** required for
   correctness.

## Spike 1 — Store backend (hiqlite vs openraft)

### What hiqlite gives us

- API shape (measured against the crate's own tests and a compiled spike):
  - `client.execute(sql, params)` → rows affected — same as `rusqlite::execute`.
  - `client.query_as_one::<T>(sql, params)` (serde) and
    `client.query_map_one::<T>(sql, params)` (`From<&mut Row>`, exactly the
    `_from_row` mappers plurx already has).
  - `client.txn(...)` for atomic multi-statement writes.
  - Placeholders become `$1` (from `?1`); SQL dialect is unchanged SQLite.
- Cluster: `start_node(NodeConfig{ node_id, nodes: [Node{id, addr_raft,
  addr_api}], data_dir, secret_raft, secret_api, … })`. One node
  runs standalone; three form a raft cluster. hiqlite's `execute_query`,
  `self_heal`, and membership tests demonstrate writes on any node replicating
  to all, and recovery after a node drop.
- Embeds fully (no Postgres/etcd), encrypts at rest, ~65 MB RAM for an HA node
  (per upstream) — production-proven as Rauthy's store.

### Migration path (Phase 4)

The `Store` trait is unchanged. M1b begins with `HiqliteAuthStore`, which
implements only settings, users/tokens, and API keys through
`client.execute` / `query_map`. Its deliberately narrow type cannot satisfy
`Arc<dyn Store>`, so plurxd remains on complete SQLite while later M1 slices
port the other traits. Once complete, single-node mode becomes a 1-voter
cluster on the same code path, not a separate "cluster edition" fork.

### Friction found

- The node list belongs in cluster config. Encryption and Raft/API secrets do
  not: M0 keeps them in mode-`0600` files beside `node.id`, and the join-token
  flow transfers them without writing secret material into `plurx.toml`.
- `query_as` uses serde `Deserialize`; `query_map` uses the `From<&mut Row>`
  mappers. plurx will use `query_map` to reuse existing mappers verbatim.

**Verdict:** adopt hiqlite. Keep the `Store` trait boundary; add the raft
backend behind it in Phase 4.

### M0 compatibility proof and one-voter cost

M0 reran the storage decision against the contracts the clustering review
identified as blockers. The semantic proof is
`make hiqlite-spike`; the optimized cost gate is `make hiqlite-baseline`.
Both run from the standalone, non-shipping `spikes/hiqlite-m0` manifest;
ordinary workspace builds do not resolve or compile hiqlite, and `plurxd`
still builds and runs `SqliteStore` in M0.

**Dependency audit boundary:** RustSec gates the production workspace and fuzz
lockfiles. The standalone spike retains its own lockfile, but M1b now resolves
hiqlite 0.14 in the audited root workspace. hiqlite enables cryptr's unused S3
path, whose `s3-simple` 0.8 dependency pinned advisory-affected `quick-xml`
0.39.4. The root workspace vendors that small package with source unchanged
and raises only `quick-xml` to 0.41, the first version patched for
[RUSTSEC-2026-0195](https://rustsec.org/advisories/RUSTSEC-2026-0195.html).
No RustSec ignore was added.

`rkyv` 0.7.46 fixed
[RUSTSEC-2026-0001](https://rustsec.org/advisories/RUSTSEC-2026-0001.html),
and versions below 0.8 were unaffected by
[RUSTSEC-2026-0122](https://rustsec.org/advisories/RUSTSEC-2026-0122.html).
That did not make 0.7 safe indefinitely: the later
[RUSTSEC-2026-0235](https://rustsec.org/advisories/RUSTSEC-2026-0235.html)
affects the 0.7 series and is patched only in rkyv 0.8.17 or newer.

The affected edge is `rust_decimal`'s optional rkyv integration. Hiqlite does
not enable it, but Cargo still records it in the root lockfile. The audited
workspace therefore vendors `rust_decimal` 1.42.1 and removes only that
dormant integration; compiled decimal behavior is unchanged and no RustSec
ignore was added. The standalone M0 lockfile retains the registry package and
remains semantic evidence only, never a release dependency.

| Contract | Observed result |
|---|---|
| FTS5 and triggers | A replicated insert populated the FTS table, and all three voters returned the exact title from a local FTS read. |
| `INSERT … RETURNING` | Returned the generated id through a non-leader node; no connection-local `last_insert_rowid()` is needed. |
| Transaction CAS | Two concurrent epoch-5 writers raced; exactly one wrote epoch 6, while the loser returned `Ok(0)`. Callers must inspect rows affected. |
| Replication unit | hiqlite replays SQL plus bound parameters. `unixepoch()` is rejected on every voter, `CURRENT_TIMESTAMP` is accepted and can diverge, and `DEFAULT (unixepoch())` DDL succeeds before its first implicit insert fails. |
| Transport authentication | TLS listeners started and a client with the wrong API secret could not write. The harness disables certificate verification, so certificate identity remains an M1 multi-process proof. |
| Compaction | With a bounded semantic-test threshold, 96 writes produced both a snapshot and a purged-log position. Restart and snapshot catch-up are not proven in this process. |
| Workspace compatibility | hiqlite 0.14 and rusqlite must share `libsqlite3-sys`; the workspace therefore moves from rusqlite 0.37 to 0.40. The full workspace suite passes on 0.40. |

The semantic probe sizes the M1 port: 70 `unixepoch()` occurrences on 68
lines, including 24 schema defaults, must become leader-computed bound values.
No `CURRENT_TIMESTAMP`, `CURRENT_DATE`, or `CURRENT_TIME` keyword exists in the
store today; M1 adds a CI ban before replicated SQL lands. The current SQLite
backend also has 10 `unchecked_transaction()` sites plus the raw `BEGIN` batch
that applies schema migrations. hiqlite's fixed `txn()` statement list cannot
read, branch, or use `RETURNING`, so seven store transactions and the migration
path need explicit designs rather than mechanical translation; two cache
transactions port as statement batches and one offline renewal branches on
rows affected. M1a keeps every explicit boundary source-pinned in
`store::replicated`. That inventory deliberately excludes untransacted
read-branch-write flows, `RETURNING`, `last_insert_rowid()`, and Rust-driven
backfills; those remain separate port audits.

The recorded M0 cost run was taken 2026-08-07 on an Apple M3 Max MacBook Pro
with 64 GB RAM, macOS 26.6, and the internal APFS data volume. It used the
repo-pinned Rust 1.97.1 toolchain via
`make CARGO='rustup run 1.97.1 cargo' hiqlite-baseline`, release optimization,
one TLS-enabled hiqlite voter, the production 2 MiB WAL and 10,000-entry
snapshot threshold, and 10,000 updates to one watch-state row. Latency is a
duration read followed by an acknowledged upsert with `RETURNING`; neither
side fsyncs each write, so this is an acknowledgment comparison, not a
power-loss durability measurement.

| Measure | SQLite | One-voter hiqlite | Gate | Result |
|---|---:|---:|---:|---|
| p95 progress latency | 0.041458 ms | 0.076834 ms | ≤25 ms and ≤max(2× SQLite, SQLite + 0.5 ms) | Pass |
| Idle RSS after warm-up | — | +7,077,888 bytes (6.75 MiB) | ≤100 MiB additional | Pass |
| Data-directory growth, 10,000 writes | 2,759,224 bytes | 8,309,777 bytes | ≤68,068,864 bytes | Pass |
| Durable watch commits while playing | web: one / 5 s; Apple and Android: one / 10 s | Not yet active | ≤one / 10 s / stream | Known M1d blocker |

The original pure-ratio gate was unstable on sub-millisecond storage because
it treated fixed Raft overhead as proportional overhead. An earlier run
measured a 0.235 ms fixed tax. The revised relative limit is the larger of 2×
SQLite or SQLite + 0.5 ms: the additive branch is approximately twice that
observed tax, while the ratio branch takes over on slower storage. On the
recorded run the relative ceiling was 0.541458 ms and hiqlite used 14.2% of
it; the separate 25 ms user-impact ceiling remains unchanged.

Net hiqlite growth was 830.98 bytes per progress write after production-tuned
snapshotting. At four concurrent streams for six hours per day, today's web
five-second beat extrapolates to 13.69 MiB/day or 4.88 GiB/year; the native
ten-second beat is 6.85 MiB/day or 2.44 GiB/year. The gate itself still permits
39.98 GiB/year at the web rate, so passing it is not evidence that the steady
state is acceptable for a small SD card. M1d must coalesce writes and remeasure
post-compaction growth before the replicated store becomes production.

Crash recovery, snapshot catch-up, and leader failover remain outside this M0
in-process harness. hiqlite 0.14 uses a process-global shutdown handler, so one
embedded voter cannot be stopped and restarted independently; M1 must use
separate processes. The test's deliberate guard failures also surface as
background-thread panics, which is evidence of rejection but not evidence that
the writer remains healthy after replay failure.

The watch-rate row is intentionally not marked green. The handler currently
writes every received beat. Web sends every five seconds; the native clients
already send every ten seconds. M1d owns the server-side coalescer, which must
make the limit true for every client before the replicated store becomes the
production path.

### M1b auth-store promotion and failure proof

`make cluster-check` is the first shipping hiqlite gate. Unlike the M0
semantic test, its voters are separate operating-system processes, so killing
one voter actually removes its raft runtime and listeners.

| Contract | M1b evidence |
|---|---|
| Store surface | All 23 `SettingsStore`, `UserStore`, and `ApiKeyStore` methods are implemented; the lifecycle gate exercises settings, user/admin/password/token, and scoped-key mutation and deletion. |
| Deterministic replay | Every timestamp is computed once by the caller and bound. Generated ids use `RETURNING`. A source guard rejects clock/RNG SQL and out-of-first-appearance `$N` parameters. |
| Replica equality | Writes enter through every embedded client; SHA-256 digests of ordered local `cluster_meta`, settings, users, tokens, and API-key dumps converge across all voters. Only the digest leaves a child process. |
| Follower and leader loss | Fresh three-voter runs kill each role separately; a survivor regains quorum readiness and reads every acknowledged proof row before accepting another write. |
| Compatibility | A remote consistent preflight reads schema and protocol bounds before raft startup. The gate presents an older schema and proves rejection while the live leader remains a compatible voter. |
| Quorum readiness | `HiqliteAuthStore::ping` combines hiqlite health with a bounded consistent read. After a second loss leaves one of three voters, the process remains alive but ping fails. |

This is backend evidence, not a production-store switch. `plurxd` still holds
`Arc<dyn Store>` backed entirely by SQLite until the remaining traits and
import path land; its live `/readyz` route therefore remains SQLite-backed in
this slice.

### M1c catalogue, local-FTS, and stale-root proof

M1c keeps the same separate-process gate and adds libraries, items, files, and
watch state. Authoritative browse reads are quorum-consistent; FTS is derived
on each voter and search reads that local index.

| Contract | M1c evidence |
|---|---|
| Store surface | All library, media, and watch methods are implemented on the Hiqlite type; the backend-neutral SQLite inventory is now 116 methods after adding root identity and atomic reconciliation. |
| Replica equality | Ordered local catalogue/watch dumps join the M1b digest, and all three processes must return the same browse and search ids. |
| Node-local FTS | The controller deletes voter 2's FTS rows directly, proves browse is unchanged and local search is empty, issues the FTS rebuild command against that voter only, and requires three-way search parity again. |
| Root identity | The scanner hashes canonical roots plus their Unix inode identity. A different present root is an error and skips every vanished-file delete. |
| Prune bound | Root comparison, deletion budget, vanished-file deletion, and empty-hierarchy pruning share one transaction. A zero-budget fixture keeps both file and item rows. |
| Loss survival | The existing follower-loss and leader-loss cases now verify every acknowledged library, item, file, watch row, and rebuilt search row before accepting the post-loss write. |

This still is not the daemon switch. M1d owns the remaining durable traits and
the ten-second progress coalescer; M2 owns import and activation. M4 adds the
lease token to the reconciliation boundary M1c made atomic.

## Spike 2 — Deterministic-segment transcode failover

### The question

For active-active transcode failover, can any node produce HLS segment N
independently such that a client mid-stream can fetch subsequent segments from
a *different* node and keep playing? Tested against three sources: constant
frame rate, **sparse keyframes** (only at 0 s and 10 s — the stress case), and
variable frame rate.

Recipe: `ffmpeg -ss <N·d> -i src -t d -fps_mode cfr -r <fps> -c:v libx264 …
-f mpegts seg_N.ts` (each segment an independent ffmpeg — exactly what a
session restarted on another node produces for its first segment).

### Measured results

| Property | Result |
|---|---|
| Same segment from two independent runs | **byte-identical** (x264 `threads=1`) |
| Sparse-keyframe seek (to 4 s / 8 s, no nearby keyframe) | **correct content** — modern input `-ss` is fast *and* accurate |
| VFR source (`-fps_mode cfr -r`) | deterministic, uniform 4.02 s segments |
| Playlist playback of independently-produced seg0/seg1/seg2 | **12.000 s total, all 288 frames** decode |

### Sharp edges (and how Phase 4 handles them)

1. **PTS reset per seeked segment.** Each seeked segment's timestamps restart
   near 0 (even with `-copyts`); a naive `cat` misreports duration. Non-issue
   for HLS — players sequence by the playlist (`EXTINF`), which read the
   correct 12 s. Normal playback (one continuous session) has no resets at
   all; a reset happens only at a *failover* boundary.
2. **Audio DTS discontinuity at a timeline reset.** Occurs once, at the
   failover boundary, not during normal playback. Fix: emit
   `EXT-X-DISCONTINUITY` there so the player remaps its timeline cleanly.
3. **Seeking far from a keyframe decodes from the preceding keyframe** (cost).
   Acceptable for rare failover; byte-determinism lets a cache amortize it.

### Decision

The deterministic-segment *property* — any node produces a valid segment N —
is what makes failover work, and it holds, including for the sparse-keyframe
worst case. We do **not** need per-segment independent ffmpeg as the primary
path (which would pay the decode-from-keyframe cost on every segment and add
per-segment audio seams). Instead:

- **Primary:** the Phase 2 session-based HLS transcode (one ffmpeg, sequential,
  clean).
- **Failover:** restart the session on a surviving node seeked to the
  last-served boundary; insert `EXT-X-DISCONTINUITY`; the client keeps its
  buffer and continues. Cost: a few seconds of rebuffer, once.
- **Optional optimization:** `threads=1` deterministic encode + a shared or
  replicated segment cache so re-served segments are free and byte-identical.

This is the "restart-at-position" fallback the roadmap anticipated — but the
spike shows it is not a fallback at all: it *is* the clean design, and the
harder per-segment model is unnecessary.

## Consequences for Phase 4

- Add `HiqliteStore: Store` behind the existing trait; wire join tokens and
  membership; single node = 1-voter, 3+ = HA.
- Replication classes per ARCHITECTURE §2.2: durable (users/settings/metadata/
  watch state) via raft SQL; ephemeral (playback/transcode session recipes)
  via hiqlite's replicated cache/KV; node-local regenerable (segment/image
  cache) stays on disk.
- Transcode: make the session recipe replicated state; on client failover,
  the new node restarts the session from the recipe + last boundary and emits
  a discontinuity. `threads=1` + segment cache is a follow-on optimization.
- Client node-list + retry (already anticipated in the web player's error
  handling) drives failover; VIP/keepalived and k8s Service are documented
  deployment alternates.
