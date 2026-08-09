# Performance II review response — all eight findings accepted

**Status:** response complete · 2026-08-09 · answers
[PERF2-PLAN-REVIEW.md](PERF2-PLAN-REVIEW.md) (review of plan `dd903f59`) ·
plan revised in place to v2 in the same branch commit series.

The review's verdict stands: v1's seam contracts were not
implementation-ready, and its central N3 recipe was wrong. Every finding was
independently re-verified against the tree before being accepted — the
claims that were new to the review (the request fingerprint carrying the
resolved height via `t{height}`, the hardcoded `avc1.640034,mp4a.40.2` at
`transcode.rs:2496,3739`, `set_offline_package_recipe` refusing a changed
hash mid-preparation at `store/sqlite/offline.rs:423-438`, `scripts/bench`'s
no-pixels line, and PLAYBACK-TESTING's shaping exclusion) all check out.
Nothing in the review is refuted. The probe transcript in review §6 is the
kind of evidence PERF-PLAN §9 asks for, and it settled in one run what the
v1 text had assumed.

## Dispositions

### R1 — accepted. The seam is now a protocol with one owner (§6.1 v2)

The "hydration, not splicing" framing is deleted, along with "no playlist
splicing," "no new client semantics," and the misattributed "same restart
contract Phase 3 proved." v2 §6.1 specifies what the probe demanded:

- The **server owns the served playlist** for hydrated sessions —
  `Manager::playlist` composes prefix-manifest entries +
  `EXT-X-DISCONTINUITY` + writer entries with server-managed
  `MEDIA-SEQUENCE` and `EXT-X-DISCONTINUITY-SEQUENCE` (Apple 8.13/8.17).
  The raw writer playlist is never served for a hydrated session; the
  existing `served_live_playlist` rewrite is the named precedent. FFmpeg
  `append_list` was considered and not chosen: a server-owned merge keeps
  one owner for pruning, discontinuity accounting, and the atomic-publish
  rule below, where `append_list` would leave the writer authoritative.
- **Timestamps:** `-output_ts_offset covered_ms` paired with
  `-start_number K` (the ADAPTIVE-QUALITY Phase 3 pairing; the review is
  right that v1 used the filename knob and expected the timestamp
  result). The discontinuity marker stays regardless.
- **Atomic publish:** hydrate → validate against the manifest → start the
  continuation encoder → publish one playlist generation; the gate opens
  only on the published generation, closing the two-histories race the
  probe exposed.
- The prefix-hydrated session is now framed as what it is: a planned,
  same-node instance of the M4 §7.3 takeover contract.
- A dedicated **N3.0 seam spike** now precedes any schema or producer work
  (also R8), and §13 sequences it explicitly.

### R2 — accepted. Identity proven, artifact kind explicit, all-or-nothing (§6.1 v2)

- Fresh `stat` of the source must equal the recipe's (size, mtime) before
  any hydrated playlist publishes, and the continuation encoder is spawned
  against that verified identity; mismatch discards the prefix and falls
  back to a plain live session.
- Location rows gain `kind = full | prefix` — never inferred from
  `covered_ms` at use time — plus a boundary manifest (segment list, exact
  per-segment EXTINF and sizes, summed covered duration, container
  generation). The manifest is authoritative; the artifact's VOD-shaped
  playlist (ENDLIST and all) is never served, which answers the
  VOD-to-EVENT question directly.
- Hydration semantics specified: hard-link same-filesystem else copy;
  `CacheReadGuard` held lookup-to-session-end exactly as full hits hold
  it; all-or-nothing with no prefix URI ever advertised on any failure.
- The acceptance failure-matrix now distinguishes source-replaced,
  cache-corrupted, and LRU-pressure cases instead of one "kill the cache"
  line.

### R3 — accepted. Packet truth before player truth (new §6.4)

A new §6.4 defines the media-layer gates: per-stream monotonic DTS/PTS
under the offset contract; bounded audio gap/overlap (starting bound one
AAC frame, 1024 samples) and video gap/overlap (± one frame duration);
EXTINF-vs-packet agreement; decoded-frame/audio comparison against a
single-encoder reference; the fixture matrix (24000/1001, 30000/1001,
integer, VFR, audio-leading, 44.1 kHz, 5.1-to-stereo); run per enabled
encoder family. Player-level stall/error runs are demoted to a separate,
later gate. N3's acceptance now requires the packet gate on the session's
actual output, not beacons alone.

### R4 — accepted. One effective identity, resolved before hashing (§4.3 v2)

- `EffectiveRateControl` is resolved through validation/fallback *before*
  anything is hashed; the requested value never reaches a recipe. Runtime
  mode flips re-validate and publish atomically.
- One `effective_recipe()` builder replaces the four independent
  constructors; `rate_control` moves from the manager-level
  `PipelineDigest` constant to per-recipe fields (N2's per-title quality
  makes manager-level placement wrong, as the review says).
- Legacy VBR digest bytes preserved exactly, pinned by golden-hash
  fixtures; inequality fixtures per effective QVBR value; cross-path
  agreement fixtures.
- Offline packages persist their effective recipe inputs at creation and
  resume from that snapshot — the `set_offline_package_recipe` pinning
  the review traced is exactly why.
- N1 relabeled **medium** effort (§4, §13).

### R5 — accepted. Reopen bound, normalized once, cause typed (§7.3 v2)

- The stall reopen carries `previous_session_id` + typed `reopen_reason`;
  the server validates ownership, reads the previous *resolved* rung
  once, and persists the resolved target under `request_id` **before**
  superseding — extending `claim_request` per the review's own
  clean-list, so a transport replay returns the same session/target and
  can neither double-step nor 409.
- Floor, manual-height, and budget-scope semantics are stated: floor =
  lowest rung then one same-rung retry then the existing terminal
  surface; manual picks are never auto-stepped; the Apple recovery budget
  is per source-session, reset by 5 s of established playback — today's
  mechanism, now named as the definition.
- The Apple reopen queue gains a typed cause with merge precedence
  (user-initiated actions clear a pending stall cause; the
  HDR-compatibility lane is untouched).
- The review's five test cases are adopted verbatim in §7.3.

### R6 — accepted. Caps on create, typed OutputCodec, fMP4 decided (§8.1 v2)

- The create body gains the same `Caps` object the decision request
  carries (additive; absent = H.264); `OutputCodec` is typed separately
  from encoder family and rides fingerprint, effective recipe, `Session`,
  and response.
- **HEVC JIT output is fMP4** — Apple authoring item 1.5 settles v1's
  open question, and the plan now says so. `produce::assemble` gains
  `EXT-X-MAP`/init ownership; resume paths prove init compatibility or
  publish a discontinuity + new map.
- `CODECS` is derived from the produced init segment (the copy path's
  `hvc1` derivation becomes the contract), fail-closed — the two
  hardcoded `avc1.640034` sites the review found are named in the plan
  as the bug this prevents.
- Predictive production without a requesting client is specified: H.264
  by default; HEVC entries only for `playback_id`s whose recent
  decisions declared HEVC decode.
- N5.1 relabeled **large** effort; device-matrix + validator gates
  required before the toggle leaves default-off.

### R7 — accepted. Harnesses precede the features they judge

Every criticized acceptance section now embeds an exact command shape
with a nonzero-failure contract and a stable artifact: `scripts/bench
rate-control` (N1), `prefix_hydration_seam` + `scripts/perf2-seam-probe`
(N3/§6.4), `scripts/playback-lab run --suite stall-recovery
--network-profile 8mbps-to-1.5mbps` (N4), `scripts/perf2-hevc-validate`
(N5.1). §13 sequences each harness slice before its milestone.
"Byte-identical with controller off" is now "normalized trace (UUIDs,
ports, wall-clock times, temp paths scrubbed) identical."

### R8 — accepted. Order, labels, and migration versions corrected (§13, §11.4)

The §13 table now runs: N0 → N1 harness → N1 → N4 harness/priors → N4 →
N2 → N3.0 spike → N3 → N5.1 (container abstraction included) → background
lanes, with the container work explicitly before any HEVC/AV1 prefix.
Effort labels updated per the review's table (N1 medium, N3 largest, N4
medium-large, N5.1 large). Migration versions are unique: v16 telemetry
(N0), v17 cache artifact kind/manifest (N3).

## Retained from the review's clean list

The items the review verified and endorsed are load-bearing in v2 and
will not be re-litigated: cache-before-admission, reader-guard pinning,
the existing recipe field inventory, extending `claim_request` rather
than adding a retry store, the distinct Apple stall predicates, the copy
path's init/`.m4s`/`hvc1` machinery as the HEVC building block, SDR
labeling for 8-bit BT.709 HEVC, N1/N2-before-prefix-fill, and the
default-off / no-external-service guardrails.

## What happens next

v2 is ready for re-review or for §14 ratification. The §14 ledger is
unchanged by this response — no review finding altered a D-item — and the
first buildable slice remains N0, which no finding touched.
