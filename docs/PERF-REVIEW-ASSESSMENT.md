# Performance review response assessment — what is resolved and what still needs a contract

**Status:** assessment complete · **Result:** correction pass required ·
**Reviewed:** 2026-07-28 · **Repository:** `f1a31e2`

Companion to [PERF-PLAN.md](PERF-PLAN.md) (the performance roadmap),
[ADAPTIVE-QUALITY.md](ADAPTIVE-QUALITY.md) (the adaptive playback design),
and [PERF-PLAN-REVIEW.md](PERF-PLAN-REVIEW.md) (the original review). This
document evaluates the submitted response to that review against the code
committed through `f1a31e2`. It does not replace the response or either
plan. Its job is to separate decisions that are ready to implement from
decisions whose contracts still permit unsafe or incompatible behavior.

The short verdict: the response is technically serious and resolves most
of the original review. It accurately describes the committed Weekend 1
implementation, accepts the important findings, and converts many open
questions into concrete decisions. Before the plans become build
instructions, they still need precise contracts for browser buffering,
playable-media accounting, scratch retention, resource preemption, session
lifecycle, failover overlap, and unsupported bitmap behavior.

## 1. Decision summary — accept the direction, tighten seven contracts

The response should be used as the basis for the plan correction. Its
overall disposition of the review is fair, but “reduced scope” does not
fully resolve R6 or R10, and parts of R1, R2, and R8 need narrower wording.

| Area | Assessment | Required action |
|---|---|---|
| R1 · browser buffer | Correct diagnosis, unproven default | Revert the byte override and make `60 s`/`30 s` a measured experiment |
| R2 · ahead window | Correct direction, incomplete definition | Pace from completed media time and enforce session plus global byte limits |
| R3 · restart handoff | Accepted | Preserve the explicit interruption SLO and test it |
| R4 · encoder validation | Accepted | Validate the complete production command and sliding speed window |
| R5 · HDR validation | Accepted | Require a real HDR fixture, color assertions, and speed assertions |
| R6 · resource arbitration | Partially resolved | Prove suspension releases capacity or terminate and checkpoint background work |
| R7 · cache identity | Accepted | Keep recipe identity separate from node-local locations |
| R8 · cluster failover | Correct direction, ambiguous boundary | Specify sequence, overlap, discontinuity, and fencing invariants |
| R9 · attempt telemetry | Accepted | Add attempt generations, live labels, and recent speed |
| R10 · lifecycle | Partially resolved | Add idempotent `POST` and explicit `DELETE`; retain `GET` only for compatibility |
| R11 · NAS and segment memory | Accepted as an open implementation decision | Cache availability and stream immutable segment bodies |
| R12 · quality menu | Accepted | Correct the plan to describe the menu that already exists |
| Bitmap fallback | Unsafe as written | Reject unsupported burn-in or negotiate a no-burn fallback |
| Software fallback | Output rung is not a capacity proof | Admit only a pipeline demonstrated to remain above realtime |

The plan should say **ready for a contract-correction pass**, not **ready to
build**, until the required actions above have been incorporated. This is
a small change in status but an important one: implementation should not
have to invent lifecycle or failure semantics.

## 2. Verification basis — the committed code supports the response's framing

The response was checked against commit `f1a31e2`, including the player,
transcode manager, HLS endpoints, and FFmpeg progress handling.

### FFmpeg time and fetched-segment index are currently mixed

[`Progress`](../crates/plurxd/src/transcode.rs) records FFmpeg `out_time`,
while the ahead calculation derives fetched time from the highest requested
segment index and a fixed segment duration. The response is therefore right
that the fetched side still assumes uniform durations.

The production side is correct in **units** but not yet in **meaning**.
FFmpeg `out_time` may include media being written to the current temporary
segment. That time is evidence that the encoder is progressing, but it is
not necessarily media the playlist can expose and the client can play.

### Status polling already leaves the idle timer alone

[`session_status`](../crates/plurxd/src/transcode.rs) reads status without
updating session access time. The response is correct that polling does not
currently keep an abandoned session alive. That behavior should remain an
explicit lifecycle invariant and receive a regression test.

### The browser configuration still contains the assumption R1 corrects

The HLS initialization in
[`web/index.html`](../crates/plurxd/src/web/index.html) sets
`maxBufferLength: 60`, `backBufferLength: 30`, and elevated
`maxBufferSize` values. Its accompanying comment describes the byte limit
as the binding limit. The response correctly retracts that explanation and
proposes returning the byte limit to hls.js defaults.

### Session creation remains a state-changing `GET`

[`http/hls.rs`](../crates/plurxd/src/http/hls.rs) still creates or retrieves
a transcode session from a `GET` route. Playback and request identifiers
would reduce duplicate work, but they would not change the HTTP semantics:
a retry, speculative fetch, crawler, or intermediary could still trigger a
state change.

### Segment delivery still allocates the whole body

The segment path in [`transcode.rs`](../crates/plurxd/src/transcode.rs)
reads each segment into a `Vec<u8>` before returning it. The response is
right to leave streaming immutable segment files as an implementation
decision rather than claiming the memory cost has already been removed.

## 3. R1 — browser limits vary, so the buffer target must be measured

The response makes the important correction: hls.js does not treat
`maxBufferSize` as a simple hard limit that overrides
`maxBufferLength`. The
[hls.js API contract](https://github.com/video-dev/hls.js/blob/master/docs/API.md)
describes `maxBufferLength` as a guaranteed minimum and permits buffering
beyond the byte target to reach it.

The remaining problem is the replacement assumption that Chrome generally
offers `100–150 MB`. Chromium has several video memory limits, including
`150 MB`, `80 MB`, `30 MB`, and lower Android-specific limits in its
[demuxer memory configuration](https://chromium.googlesource.com/chromium/src/%2B/refs/tags/133.0.6878.1/media/base/demuxer_memory_limit.h).
Runtime limits can also change with platform capability and memory
pressure. A desktop observation is not a portable player contract.

### Required contract

1. **Remove the explicit large `maxBufferSize`.** Let hls.js begin from its
   maintained default because the present override encodes an unsupported
   model of browser capacity.
2. **Keep `maxBufferLength: 60` and `backBufferLength: 30` provisional.**
   M0 must compare them with at least one smaller configuration across high
   and low bitrate sources.
3. **Record buffer-full recovery.** Telemetry must distinguish quota
   pressure, append failure, media error, and network starvation.
4. **Segment results by client class.** At minimum, report browser, device
   memory class when available, codec, bitrate, and copy versus transcode.

**Acceptance:** the selected defaults have no repeated buffer-full recovery
loop in the device matrix, and the forward buffer reaches the intended
runway without a material TTFF or memory-regression penalty.

## 4. R2 — one clock must mean completed, playable media

Pacing, status, garbage collection, and failover should use a common media
timeline. Segment count cannot supply that timeline because copy-HLS
segments follow source keyframes and therefore vary in duration.

### Separate encoder progress from playable progress

Use two explicit production measurements:

- `encoder_out_time_seconds` — the latest FFmpeg progress time, used for
  watchdogs, recent encoder speed, and detecting a stuck process.
- `produced_playable_end_seconds` — the end time of the last complete,
  published playlist segment, used for ahead-window pacing and client
  runway.

Then define:

```text
ahead_playable_seconds =
    produced_playable_end_seconds - fetched_end_seconds
```

`fetched_end_seconds` must come from the requested segment's actual
`EXTINF` boundary, not `(index + 1) × target_duration`. If a status field
uses the fetched frontier rather than the playhead, its name and
documentation must say so.

### Retention must include the forward-fetch distance

The fetched frontier can be well ahead of the actual playhead. Deleting
everything more than the desired back buffer behind that frontier can
therefore remove media that the client is playing or may retry.

Without a trusted playback heartbeat, the minimum retention behind the
fetched frontier is:

```text
retention_seconds =
    maximum_forward_fetch_seconds
    + desired_back_buffer_seconds
    + retry_allowance_seconds
```

For a `60 s` forward buffer and `30 s` back buffer, retention must already
exceed `90 s` before adding retry allowance. The exact allowance should
cover the longest client retry and playlist-reload path observed in M0.

### A per-session byte cap needs a global parent budget

A `2 GB` session cap prevents one runaway session but does not protect a
shared scratch filesystem from several healthy sessions. The controller
must enforce:

- a global scratch budget;
- a per-session budget derived from active concurrency and reserved headroom;
- accounting for complete segments, temporary segments, and retained
  takeover material;
- deterministic behavior when either limit is reached.

The safe response to a limit is to pause production or evict media known to
be outside the retention contract. It is not safe to delete a segment that
may still be referenced by a published playlist.

**Acceptance:** a variable-GOP fixture never exceeds the time or byte
window, never deletes media within the retention contract, and does not
stall when production resumes at the low-water mark. A multi-session test
must also demonstrate that total scratch use remains below the global
budget.

## 5. R6 — stopping compute is not the same as releasing capacity

The response's reduced-scope arbiter is useful: a slot decision under the
session lock closes the most obvious admission race. The proposed
preemption mechanism is not yet sufficient.

`SIGSTOP` halts the FFmpeg process, but the stopped process may retain its
GPU context, hardware codec session, device descriptors, mapped buffers,
and output files. If the scarce resource is a hardware session rather than
GPU execution time, the live request may remain blocked.

### Required contract

Choose one behavior and make it hardware-testable:

1. **Checkpoint and terminate background work.** Record the last published
   boundary, stop the process, release its admission permit, and resume
   later from that boundary. This gives live work a real capacity
   guarantee at the cost of restart work.
2. **Suspend only after proving capacity release.** A target-specific test
   must show that a stopped producer no longer consumes a slot needed by
   the live pipeline. If any supported target fails that test, it uses
   checkpoint and terminate instead.

The arbiter should own a permit from admission until process exit. A live
session has priority over cache work; cache work cannot reacquire capacity
while a live waiter exists.

**Acceptance:** start the maximum supported number of background hardware
pipelines, request live playback, and demonstrate that live encoding begins
within the admission SLO on every supported hardware family.

## 6. R10 — identifiers help retries, but `POST` fixes the lifecycle

`playback_id`, `request_id`, and explicit stop behavior are all worth
adopting. They solve multi-tab identity, duplicate request detection, and
abandoned-session cleanup. They do not make a state-changing `GET` safe or
idempotent.

### Minimal compatible lifecycle

The correction does not require a large resource-model redesign:

```text
POST   /api/hls/sessions       create or idempotently recover a session
GET    /api/hls/sessions/{id}  read session state
DELETE /api/hls/sessions/{id}  release the client's ownership
```

The creation request should carry an idempotency key or stable request ID.
Repeating it with the same normalized recipe returns the same playback
session; reusing it with a different recipe returns a conflict. `DELETE`
must be idempotent and suitable for browser keepalive delivery.

The old `GET` start route may remain temporarily for compatibility, but it
should be documented as deprecated and implemented through the same
idempotent creation path.

**Acceptance:** repeated creation requests cannot start duplicate encoders;
status requests cannot extend idle lifetime; repeated deletion succeeds;
and a compatibility `GET` cannot bypass admission, identity, or cleanup
rules.

## 7. R8 — failover needs sequence-level invariants

Owner epochs and renewable leases are the right fencing mechanism. The
response also correctly chooses a bounded initial recovery target instead
of claiming immediate seamless failover. The remaining ambiguity is the
phrase “restart one boundary before the last trusted boundary.”

Restarting before the last trusted boundary implies overlap regeneration.
Relying only on the client's existing buffer implies that the replacement
owner does not promise those prefix bytes. Both can be valid, but they
produce different playlists and failure behavior.

### Required contract

For each takeover, define:

1. **Trusted prefix:** the final segment identity and end time whose bytes
   were fenced and published by the prior owner.
2. **Resume boundary:** the input timestamp and segment sequence at which
   the replacement encoder starts.
3. **Publication boundary:** whether an overlap segment is exposed or
   discarded before the new owner publishes.
4. **Playlist sequence:** the exact `MEDIA-SEQUENCE` of the first takeover
   playlist.
5. **Discontinuity:** whether the first new segment carries
   `EXT-X-DISCONTINUITY` and how `DISCONTINUITY-SEQUENCE` advances.
6. **Fence:** the owner epoch required for every playlist, segment, and
   cache-location publication.

The playlist must never reference bytes that neither the old nor the new
owner has durably published. A stale owner must be unable to publish after
lease loss even if its FFmpeg process continues briefly.

**Acceptance:** failure injection at every point in a segment lifecycle
produces either continued playback or a bounded, classified interruption.
It never produces a playlist reference to missing bytes and never accepts
a stale-owner write.

## 8. Fallback semantics — degradation must be explicit and measurable

Two response decisions need stronger semantics even though they are not
new review IDs.

### Requested bitmap burn-in cannot silently disappear

If the client requests bitmap subtitle burn-in and the selected pipeline
cannot perform it, serving a burn-omitted result changes the requested
media. Labeling the result accurately improves observability but does not
make the substitution correct.

The server must either:

- reject the recipe as unsupported; or
- return an explicit fallback offer that the client accepts before session
  creation.

Only an accepted no-burn recipe may use the no-burn cache identity. The UI
must tell the viewer that the requested subtitles are unavailable.

### A `720p` output is not automatically software-safe

Capacity depends on the whole pipeline: input decode · HDR tone-map · scale
· subtitle composition · output encode. A 4K HDR source can remain
sub-realtime even when the output is `720p`.

Software fallback should therefore require a measured representative speed
margin, not only a resolution ceiling. The initial threshold should be
conservative—for example, recent stable speed above `1.2×`—and M0 should
validate or revise it. If no measured recipe can sustain the margin, return
an explicit capacity error rather than starting a session expected to
stall.

**Acceptance:** every admitted fallback sustains the chosen speed margin
over the validation window, and every semantic downgrade requires explicit
client acceptance.

## 9. Corrections to carry into the plans

The next editing pass should make these changes in
[PERF-PLAN.md](PERF-PLAN.md) and
[ADAPTIVE-QUALITY.md](ADAPTIVE-QUALITY.md):

1. **Mark browser buffer values as provisional.** Explain that hls.js
   duration and browser quota interact, and make M0 responsible for the
   default.
2. **Name the three media frontiers.** Encoder progress, published playable
   progress, and fetched progress must not share one ambiguous “ahead”
   field.
3. **Add retention and global scratch formulas.** Include temporary bytes
   and multi-session concurrency.
4. **Require real capacity release from cache preemption.** Allow
   `SIGSTOP` only on targets where a test proves it releases the resource
   live work needs.
5. **Add the compatible `POST` lifecycle.** Keep the old route only as a
   deprecation bridge.
6. **Specify failover at the sequence level.** Include trusted prefix,
   publication boundary, discontinuity, and epoch fencing.
7. **Reject or negotiate semantic fallbacks.** Bitmap omission is not an
   implementation detail.
8. **Gate software fallback on measured pipeline speed.** Resolution alone
   is not an admission rule.
9. **Fix document links.** The submitted response names
   `PERFPLANREVIEW.md`; the repository file is
   [PERF-PLAN-REVIEW.md](PERF-PLAN-REVIEW.md).

After this pass, the plans can move to **ready to build** because the
remaining uncertainty will be measurement uncertainty owned by M0, not
undefined behavior left to the implementer.

## 10. Non-goals — what this assessment does not reopen

- **It does not reject burst-then-bound pacing.** That remains the correct
  architecture; this assessment only tightens its clocks and limits.
- **It does not require a simultaneous ABR ladder.** One active JIT encode
  remains appropriate for the target hardware.
- **It does not require seamless cluster failover in the first milestone.**
  A measured interruption is acceptable if the sequence and fencing
  contracts are correct.
- **It does not require removing the compatibility `GET` immediately.**
  It requires a safe canonical `POST` path and a deprecation boundary.
- **It does not select final thresholds without data.** Buffer duration,
  retry allowance, restart SLO, lease timing, and fallback speed margins
  remain hypotheses until M0 measures them.
- **It does not make implementation changes.** This document is an
  assessment and handoff correction, not a patch to the player or
  transcoder.
