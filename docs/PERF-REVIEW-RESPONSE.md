# Review response — what the review got right, what already shipped, what changes

**Status:** response complete; amended by
[PERF-REVIEW-ASSESSMENT.md](PERF-REVIEW-ASSESSMENT.md) — see §11 ·
**Verdicts:** 10 adopt · 2 adopt with reduced scope · 0 reject · **Written:** 2026-07-28 · **Responds to:**
[PERF-PLAN-REVIEW.md](PERF-PLAN-REVIEW.md) (reviewed at `f400da4`) · **Against
implementation:** `f1a31e2` (Weekend 1 shipped)

Companion to [PERF-PLAN.md](PERF-PLAN.md) and
[PERF-PLAN-REVIEW.md](PERF-PLAN-REVIEW.md). The review evaluated the plan as a
handoff contract; this document is the disposition of each finding — adopted,
adopted with reduced scope, or corrected — plus the answers to the review's
§17 decision list, so the plan can return to `ready to build` with the
choices recorded.

One framing fact the review could not have known: it was written against
`f400da4`, the commit that *added* the plan, but Weekend 1 (M0, §4.1, §4.2,
§4.3, §4.5) shipped in the three commits after it. That matters in both
directions. Three findings (R1, R2, R9) touch code that now exists — and
exactly one of them, R1, catches something built on a wrong premise. Several
of the review's "required revisions" also already exist in the shipped code,
noted per finding below, which is worth recording so the correction pass
fixes what is actually broken and nothing else.

## 1. Disposition summary

| ID | Review severity | Verdict | Status of the affected code | Action |
|---|---|---|---|---|
| R1 | High | **Confirmed — the review is right and the shipped diagnosis was wrong** | Shipped | Correction pass: revert byte raise, fix comment, reclassify B5, add buffer beacons |
| R2 | High | Adopt (production side already media-time; fetched side and byte bound stand) | Shipped, partial | Correction pass: EXTINF accounting, byte cap, GC contract chosen |
| R3 | High | Adopt | Unbuilt (ADAPTIVE Phase 2) | Fold into ADAPTIVE-QUALITY before building Auto |
| R4 | High | Adopt | Unbuilt (§4.6) | Validation runs the full production command |
| R5 | High | Adopt | Unbuilt (M2) | Real-HDR fixture, color assertions, speed floor |
| R6 | High | Adopt with reduced scope | Unbuilt (M2/M3) | v1 = atomic slot + preempt producer via SIGSTOP; full allocator at M4 |
| R7 | High | Adopt | Unbuilt (M3) | Recipe/location split + pipeline digest in v11 from the start |
| R8 | High | Adopt | Unbuilt (M4) | Epoch fencing + explicit prefix policy (client-buffer reliance) |
| R9 | Medium | Adopt (one requirement already satisfied) | Shipped, narrow race | Correction pass: attempt generations, live encoder label |
| R10 | Medium | Adopt with reduced scope | Unbuilt | Additive: playback_id + request_id + DELETE; not a full resource redesign yet |
| R11 | Medium | Adopt | Mixed (chapters shipped; rest not) | Slice 1: availability cache, Trakt move, streamed segments |
| R12 | Low | Adopt | Doc drift | Rewrite ADAPTIVE Phase 1 as the remaining diff |

Calibration, stated plainly because a severity column compresses it away: of
twelve findings, one is "shipped and wrong" (R1), two are "shipped and needs
hardening" (R2, R9), and nine are "unbuilt milestone needs a tighter spec" —
which is exactly what a plan review should produce, and exactly why adopting
those nine costs nothing.

## 2. R1 — confirmed against the vendored source

The review claims the plan's §2.3 buffer diagnosis is backwards. Rather than
weigh the plan's words against the review's words, the claim was checked
against the vendored `hls.min.js` (1.6.16), which contains the actual
forward-target formula:

```js
getMaxBufferLength = function(e){ var t, r = this.config;
  return t = e ? Math.max(8*r.maxBufferSize/e, r.maxBufferLength)
               : r.maxBufferLength,
  Math.min(t, r.maxMaxBufferLength) }
```

`maxBufferLength` is a **floor**. The byte value can only extend the target
beyond it — `Math.max` — never cap it below. With stock defaults on a
60 Mb/s stream: `max(8·60e6/60e6 = 8 s, 30 s) = 30 s`. The stock player was
already targeting 30 seconds of 4K. The plan's B5 ("60 MB binds before the
30 s target → a 7–11 s ceiling") was backwards, the comment shipped in
`attachHls` states that wrong mechanism, and the `performance.memory`
acceptance was invalid for the reason the review gives — the JS heap does not
see SourceBuffer allocation.

What actually limits a 4K forward buffer on Chrome is the browser's MSE
quota (order of 100–150 MB → roughly 15–27 s at copy bitrates), surfaced as
`QuotaExceededError` → hls.js `BUFFER_FULL_ERROR` → hls.js reduces its own
target. No hls.js configuration value raises that quota.

**What survives from the shipped change, and what reverts:**

- `maxBufferLength: 60` — keeps. It is precisely the "tune seconds first"
  the review prescribes, and it is the operative part of the change.
- `backBufferLength: 30` — keeps, same reasoning (bounded from Infinity).
- `maxBufferSize: 400e6` — reverts to default. By the formula it adds ~10 s
  of *target* only for lower-bitrate copy (at ≥53 Mb/s the 60 s floor
  already exceeds it), and the quota binds first regardless.
- The comment — rewritten to state the real mechanism.
- M0 gains `BUFFER_FULL_ERROR` / append-failure beacons and records the
  configured limits, per the review's revision list.
- PERF-PLAN §2.3/B5 — reclassified from confirmed root cause to hypothesis
  retired by primary-source check.

**Why this doesn't dent the Weekend 1 result:** the reserve now lives on the
*server's* disk. Burst-then-hold plus the ahead window keeps ~180 s of
content produced ahead of the frontier, so a client that can only hold 20 s
refills instantly from local disk instead of waiting on a 1× producer. R1
corrects the smaller lever; the pacing change was the fix, and it stands.

## 3. R2 — adopted; half was already built

The formula the review quotes —
`(max_written − high_segment) × SEGMENT_SECONDS` — is the *plan's*. The
shipped controller computes `produced` from ffmpeg's own `out_time`, which is
real media time and already correct for variable-duration copy segments on
the production side. Three parts of the finding stand against the shipped
code:

1. **The fetched side is index arithmetic** — `(high_segment + 1) ×
   SEGMENT_SECONDS` — and wrong for long-GOP copy sources. A 10 s-GOP source
   whose segment 0 has been fetched reads as "fetched to 4 s" when the truth
   is 10 s. The error is conservative (suspends early, resumes late), which
   is why it survived live verification, but conservative-by-accident is not
   a contract. Fix: track per-segment `start_ms/end_ms/bytes` from the
   playlist's `EXTINF` entries, as the review specifies.
2. **No byte bound.** 180 s of a 100 Mb/s remux is ~2.2 GB — more than the
   tmpfs guidance assumed. A byte ceiling joins the time ceiling; suspend on
   either, resume when both clear their low-water marks.
3. **"Ahead of the playhead" is really "ahead of the download frontier."**
   Correct, and the docs will say so. There is also an interaction the
   review could not have seen: the shipped client change raises the
   frontier up to ~60 s past the true playhead, which consumes most of the
   GC's 60 s keep-behind margin. The forward-only case is safe (those bytes
   are already in MSE), but the review's acceptance — survive a playlist
   reload and a forced retry after GC on Chrome, Safari, and AirPlay — is
   the right test and will be run.

**GC/playlist contract, chosen (review's option 1):** keep EVENT; retain
segment files for at least the client back-buffer window plus a retry
allowance; document that older URIs eventually 404; size `KEEP_BEHIND` from
that documented window rather than a bare constant.

The review's note that the 15 s reaper is too coarse to be the primary flow
controller is accepted in principle; the shipped hysteresis (resume at half
the window at this review's cut) made the coarseness mostly harmless. The
implemented request path now re-evaluates on playlist/index refresh and client
frontier advance. No producer-side segment-completion callback exists; the 15 s
reaper remains the repair path that bounds that gap, and current documentation
states that limitation explicitly.

## 4. R9 — adopted; the race is real and narrow

Shipped sequence on hardware→software fallback: kill old child → clear dir →
`progress.reset()` → spawn replacement sharing the same `Progress`. The old
child's stdout reader task can still drain buffered lines *after* the reset
and write stale values into the shared telemetry. The window is small — it
survived repeated live runs — but it is exactly the shape of bug that stays
invisible until it kills a healthy session once a month. Per-attempt
generation counters (updates apply only when the generation matches the
active attempt) close it for ~20 lines.

Also adopted from R9:

- `encoder_label` goes stale after fallback — the activity page keeps saying
  "Intel QuickSync" while software runs. Pre-existing, now fixed with the
  label moving into per-attempt state.
- Recent-speed (windowed delta over `out_time`, short EWMA) alongside the
  cumulative value — the cumulative number is misleading around suspends and
  useless for placement.
- Client beacons gain a playback-attempt id and a reason
  (cold-start · seek · audio-switch · quality · failover), and pause/seek
  do not count as stalls.

**Already satisfied, for the record:** the review requires that status
polling not touch the idle-reaper clock. The shipped `session_status`
deliberately does not call `touch` — asking how a stream is doing is not
fetching from it — so leaving the stats overlay open cannot keep an
abandoned encoder alive.

## 5. R4, R5, R7, R8 — adopted wholesale; nothing built touches them

**R4.** Correct: `validation_args` never calls `Encoder::encode_args`, so a
driver could validate and then reject the real session's rate-control flags.
The startup probe will run the complete production argument set, and the
acceptance language changes from "every segment ≤1.5×" to a sliding-window
bound with a stated overshoot — `maxrate`/`bufsize` describe a buffering
model, not a per-segment promise.

**R5.** Correct and important: a synthetic 10-bit pattern proves a graph
parses, not that it emits correct SDR at useful speed. Adopted in full: a
small redistribution-safe HDR10 fixture decoded through the candidate
hardware path, production graph, short encode, `ffprobe` color-tag
assertions, sampled-range comparison against the CPU reference with broad
tolerance, and a minimum recent speed. Runtime fallback stays regardless —
a startup probe cannot cover every profile. The subtitle correction is also
accepted as stated: bitmap burns are *skipped* today, "burn forces the CPU
path" is true of text subtitles only, and no cache recipe may claim a bitmap
burn that was omitted.

**R7.** The review caught the plan contradicting itself: M4 says placement
prefers a node holding a cache entry; M3's single-row schema cannot express
"held by more than one node." The recipe/location split ships in migration
v11 from the start — semantic identity in one table, physical copies with
`node_id · storage_class · relative_dir` in another — along with the
pipeline digest (ffmpeg build fingerprint, output codec/color contract,
GOP/rate policy) in the recipe hash. Encoder family stays *in* the hash
until output-contract equivalence across families is demonstrated, exactly
as the review argues. Lease-fenced publication (prove the lease before the
rename; only rename flips `complete`) and generating VOD playlists as VOD
rather than string-appending `ENDLIST` are both adopted.

**R8.** The strongest section of the review. The plan's "stitched playlist
with served-prefix entries" hand-waved where prefix *bytes* live when their
owner is dead — they were node-local, so nowhere. Adopted: the
`SessionOwner` state with owner epochs and lease fencing; conservative
restart from one boundary before the last trusted point; and an explicit
prefix policy — **client-buffer reliance** (the review's option 1): the
replacement playlist begins at the takeover sequence with a discontinuity,
and old bytes are not promised. The ≤5 s resume target is downgraded to a
budgeted, measured figure (see decision 9) — the review is right that a
lease alone can eat the whole budget.

## 6. R6 and R10 — adopted with reduced scope

**R6.** The findings are right: an idle-only gate checked at start lets a
long cache encode collide with a viewer who arrives later; the session-count
check as planned was racy; and "cap full → software 4K HDR" is not a safe
fallback. The reduction is in the machinery, not the guarantees. For
single-node M3: slot acquisition happens atomically under the existing
sessions lock, and the cache producer is suspended (the SIGSTOP machinery
now exists) or killed the moment a live session registers — live-first with
a bounded yield interval, which is the property the review's acceptance
actually tests. The five-priority RAII allocator with per-device capacities
is the right end state and arrives with M4, when failover recovery and
cross-node placement give the extra priority classes something to mean.
Over-capacity behavior changes now, per decision 5.

**R10.** Adopted in substance: a client-generated `playback_id` scopes
supersession to the player instance (fixing two-devices-one-account, which
automatic restarts would have made routine), a `request_id` makes creation
idempotent, and an explicit `DELETE` with `keepalive` on player close
removes the 60–75 s zombie window — a real GPU-time win. The reduction:
these land as additive parameters and one new route first, rather than a
full POST-resource redesign of session lifecycle in one step. Same contract,
smaller diff, and the capability-URL model that AirPlay depends on is
untouched either way. If the cluster milestone later wants the full resource
shape, the additive version converts mechanically.

## 7. R3, R11, R12 — adopted

**R3.** All future work (ADAPTIVE Phase 2), all correct: teardown-first
restart machinery makes the "buffer covers the switch" claim false as
written; one-rung-plus-cooldown is too slow on a cliff (severe pressure
jumps directly to the highest safe rung; cooldown governs upgrades, never
emergency downgrades); `bandwidthEstimate` is seeded across instances; and
the controller consumes server encode speed from the status endpoint that
now exists — a producer below 1× with a shrinking runway is actionable
before the client ever stalls. Phase 2 ships the review's Option A: the
visible restart, measured, with an interruption SLO (decision 4).

**R11.** Adopted into the cheap-wins slice: availability cached with a short
age (the open stays authoritative), Trakt "watching now" moves from
`/decision` to session creation — the review is right that a decision
endpoint should be pure — and completed segments stream from disk instead of
buffering ~35 MB `Vec`s per request, with immutable cache headers, since
after `temp_file`'s rename a segment never changes. Merging decision and
start into one round trip is deferred until the R10 contract settles.

**R12.** Correct; the quality menu shipped 2026-07-23 and ADAPTIVE Phase 1
was never reconciled. It will be rewritten as the remaining diff: the
server-owned ladder response, the 360p rung, height snapping, bounded
hardware rate control (R4), and the menu consuming the ladder instead of
hardcoding it.

## 8. The §17 decisions, answered

The review is right that these ten choices are what stands between "strong
direction" and "implementation contract." Proposed answers — eight decided,
two flagged as the operator's call:

1. **Client buffer policy:** seconds only — `maxBufferLength 60`,
   `backBufferLength 30`, `maxBufferSize` at default. Changing the byte
   value requires M0 evidence that byte targeting, not browser quota, is the
   binding limit (recorded runway plateau without `BUFFER_FULL_ERROR`).
2. **Server ahead policy:** referenced to the *fetched frontier*, named as
   such. Media-time window 180 s high / 150 s low from `EXTINF` sums; byte
   ceiling settings-backed (default 2 GB/session), suspend on either bound,
   resume when every active bound is below its low-water. Per-session and
   global byte limits release at half; status names which reason is binding.
3. **Playlist/GC contract:** EVENT with documented expiring URIs; retained
   window sized from client back-buffer plus retry allowance; acceptance is
   surviving playlist reload and forced retry after GC on all three client
   classes.
4. **ABR handoff:** Option A — visible restart, "Adjusting quality…"
   overlay, p95 interruption measured and published. *Operator's call:* the
   SLO number itself (proposed: p95 ≤ 2.5 s on LAN before Phase 3's gate
   reopens).
5. **Over-capacity:** short queue (≤5 s) → software only at a
   software-safe rung (720p SDR-class work; never software 4K HDR) → clear
   capacity error. On cluster, "another node" inserts before the queue.
6. **Cache identity:** encoder family and pipeline digest included in the
   hash initially; family-agnostic sharing only after per-family output
   contracts are proven equivalent. *Operator's call:* whether that
   relaxation is ever worth the validation burden on a homogeneous-enough
   fleet.
7. **Background preemption:** suspend, don't cancel, when live work
   arrives; cancel and discard (temp dir never published) only when
   suspended past 30 minutes or evicted by budget.
8. **Failover prefix:** client-buffer reliance. The takeover playlist
   starts at the new sequence behind a discontinuity; prefix bytes are not
   promised.
9. **Owner fencing:** 6 s lease renewed every 2 s; epoch on every owner
   mutation and segment route; end-to-end resume target restated as a
   *budget* — detect + claim + open + first segment + client retry — with
   ≤10 s as the initial honest target, tightened only by measurement.
10. **Bitmap subtitles:** remain unsupported; a bitmap-burn request caches
    (and serves) as burn-omitted, labeled honestly; the overlay graph stays
    a fast-follow outside this plan.

## 9. Order of work from here

The review's revised slice order (§15) is adopted for everything not yet
built, with one amendment: the shipped Weekend 1 work is not unshipped to
satisfy the ordering — its violations of that order are exactly the three
findings above, and they are corrected in place. Concretely, next:

| Step | Contents | Source |
|---|---|---|
| Correction pass | R1 config/comment/plan reclassification · R2 EXTINF accounting + byte bound + GC contract · R9 generations + live label | this doc §§2–4 |
| Slice 0/1 remainder | playback_id + request_id + DELETE · availability cache · Trakt move · streamed segments · attempt-tagged beacons | R10, R11, R9 |
| Then as reviewed | cadence experiment + R4 validation → real-HDR GPU spike (R5) → ABR with SLO (R3) → arbiter v1 + cache with R7 schema → cluster with R8 fencing | review §15 |

PERF-PLAN.md returns to `ready to build` when the correction pass lands and
its §17 answers are folded in; until then its status line says so.

## 10. What the review teaches about process

The review was commissioned after the first slice shipped, and the one
finding that hurt (R1) is precisely the kind that review-before-build
catches: a mechanism claim that shaped code, checkable in five minutes
against a primary source, wrong. The two-line lesson for this repo: claims
about *external* contracts (hls.js, FFmpeg, browser quotas) get verified
against the vendored source or a live probe before they enter a plan as
root causes — the plan's own file:line citations had this property for
plurx code and lacked it for third-party behavior, and that asymmetry is
where the error lived.

## 11. Addendum — amended by the round-2 assessment (2026-07-28)

[PERF-REVIEW-ASSESSMENT.md](PERF-REVIEW-ASSESSMENT.md) reviewed this
response against `f1a31e2` and tightened seven contracts. This document
stands as the record of round 1; where the two disagree, the assessment —
and the final contracts folded into [PERF-PLAN.md](PERF-PLAN.md) — win.
What changed:

- **§8 decision 5 (over-capacity):** "720p software" is not an admission
  rule — a 4K HDR source is sub-realtime in software regardless of output
  height. Admission now requires measured pipeline speed (~1.2× recent)
  for the recipe class; otherwise a capacity error. PERF-PLAN §5, §8.6-5.
- **§8 decision 8 (failover prefix):** pure client-buffer reliance
  contradicted the "restart one boundary early" clause this response also
  carried. Final: hybrid — one regenerated overlap segment covers the
  served-but-unacknowledged gap; client buffer covers everything earlier;
  six-field takeover contract in PERF-PLAN §7.3.
- **§6 (R6 scope):** SIGSTOP is not capacity release — a stopped ffmpeg
  retains its hardware codec session. Producer preemption is
  checkpoint-and-terminate (PERF-PLAN §6.2); the live-session suspend is
  a disk mechanism only.
- **§6 (R10 scope):** the additive-params position is withdrawn in favor
  of the assessment's minimal `POST`/`DELETE` lifecycle with the old GET
  as a deprecated bridge (PERF-PLAN §8.5).
- **§2 (R2):** ahead accounting additionally separates encoder progress
  from playable progress (ffmpeg `out_time` counts the in-progress
  segment), retention derives from the client's forward-fetch distance,
  and a global scratch budget joins the per-session cap (PERF-PLAN §4.2).
- **§8 decision 10 (bitmap):** held, with sharper wording — disclosure is
  the negotiation (the server picks the burn track today; there is no
  client request to reject), and cache identity records `applied=false`.
  Recorded as a deliberate deviation from the assessment's
  reject-or-negotiate framing.
