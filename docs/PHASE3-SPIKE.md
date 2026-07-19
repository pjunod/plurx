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
  addr_api}], data_dir, secret_raft, secret_api, enc_keys, … })`. One node
  runs standalone; three form a raft cluster. hiqlite's `execute_query`,
  `self_heal`, and membership tests demonstrate writes on any node replicating
  to all, and recovery after a node drop.
- Embeds fully (no Postgres/etcd), encrypts at rest, ~65 MB RAM for an HA node
  (per upstream) — production-proven as Rauthy's store.

### Migration path (Phase 4)

The `Store` trait is unchanged. A `HiqliteStore` implements it with
`client.execute` / `query_map_one`, reusing the existing row mappers. Because
everything already goes through the trait (ARCHITECTURE §2.1), plurxd, the
scanner, metadata, and the Plex façade need no changes. Single-node mode is a
1-voter cluster — the same code path, no "cluster edition" fork.

### Friction found

- Config requires `enc_keys` (generate with `cryptr::EncKeys::generate()`),
  `secret_raft`/`secret_api`, and the node list — one-time setup, surfaced in
  plurx config and the join-token flow.
- `query_as` uses serde `Deserialize`; `query_map` uses the `From<&mut Row>`
  mappers. plurx will use `query_map` to reuse existing mappers verbatim.

**Verdict:** adopt hiqlite. Keep the `Store` trait boundary; add the raft
backend behind it in Phase 4.

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
