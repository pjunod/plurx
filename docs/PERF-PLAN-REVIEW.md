# Performance plan review — what to correct before implementation

**Status:** review complete · **Reviewed:** 2026-07-28 ·
**Repository:** `f400da4` · **Scope:** design and implementation readiness

Companion to [PERF-PLAN.md](PERF-PLAN.md) (the proposed performance
milestones) and [ADAPTIVE-QUALITY.md](ADAPTIVE-QUALITY.md) (the proposed
bandwidth controller). This document evaluates those plans against the
current player, transcode manager, FFmpeg builders, and public FFmpeg and
hls.js contracts. It does not replace either plan, and it makes no code
changes. Its job is to identify what should be corrected before an
implementing agent treats the plans as a handoff contract.

The short verdict: the direction is right, but the plans are not yet ready
to build exactly as written. Instrument-first · burst-then-bound pacing ·
shorter startup segments · GPU tone-mapping · one active JIT rendition are
the right decisions. The buffer model, ahead-window accounting, ABR
handoff, resource arbitration, cache schema, and cluster failover protocol
need revision first.

## 1. Decision summary — keep the architecture, revise the contracts

The highest-value parts of the plan should survive intact:

1. **Instrument before tuning.** Encode progress, TTFF, buffer runway, and
   stall beacons turn playback complaints into measurable failure classes.
2. **Remove realtime pacing from copy-HLS.** A producer held at exactly
   `1×` cannot build reserve. Burst followed by a bounded pace is the right
   model.
3. **Move chapter probing off the play path.** Immutable media metadata
   belongs at scan time, not between a click and the first frame.
4. **Shorten the startup segment.** Segment completion is a hard startup
   gate; reducing it is a direct TTFF improvement when the source GOP
   permits it.
5. **Move HDR tone-mapping onto the GPU.** The CPU float RGB chain is the
   structural 4K bottleneck. Client buffering can conceal jitter, but it
   cannot make a persistently sub-realtime producer healthy.
6. **Keep one active JIT encode.** A full simultaneous ABR ladder is the
   wrong default for small iGPUs. Adaptation should change the one active
   recipe unless later measurements justify brief overlap.

The corrections are ranked below.

| ID | Severity | Finding | Required revision |
|---|---|---|---|
| R1 | High | hls.js's 60 MB default is not a 7–11 s hard cap | Reclassify B5 as a hypothesis and tune from M0 evidence |
| R2 | High | Ahead-window math assumes fixed copy-segment durations | Account in media time and bytes, not segment count |
| R3 | High | Existing restart machinery destroys the old HLS instance | Specify a handoff or accept and measure the interruption |
| R4 | High | Encoder validation does not exercise new rate-control flags | Validate the complete representative encode command |
| R5 | High | A synthetic HDR probe cannot prove correct tone-mapping | Validate a real HDR fixture, output color, and speed |
| R6 | High | Cache work is gated only when it starts | Add a race-free, preemptible resource arbiter |
| R7 | High | The cache schema cannot represent multiple node-local copies | Split recipe identity from cache locations |
| R8 | High | Failover does not define unavailable prefix bytes or ownership fencing | Specify owner epochs, overlap, and playlist takeover |
| R9 | Medium | Progress and encoder state can race across fallback attempts | Add attempt generations and recent-speed telemetry |
| R10 | Medium | Session creation is a side-effecting, non-idempotent GET | Introduce playback IDs, idempotent POST, and explicit stop |
| R11 | Medium | `/decision` still touches the NAS and segments buffer in RAM | Cache availability and stream immutable segment bodies |
| R12 | Low | The adaptive plan says a manual menu still needs adding | Reconcile the plan with the menu already in the player |

## 2. Review basis — what was and was not verified

**Inspected plans:** [PERF-PLAN.md](PERF-PLAN.md) and
[ADAPTIVE-QUALITY.md](ADAPTIVE-QUALITY.md), including their referenced
milestone contracts and acceptance checks.

**Inspected implementation:** the HLS argument builders in
[`plurx-core::transcode`](../crates/plurx-core/src/transcode/mod.rs), encoder
selection and validation in
[`encoder.rs`](../crates/plurx-core/src/transcode/encoder.rs), session
management in
[`plurxd/src/transcode.rs`](../crates/plurxd/src/transcode.rs), the HLS and
decision endpoints in [`http/hls.rs`](../crates/plurxd/src/http/hls.rs) and
[`http/stream.rs`](../crates/plurxd/src/http/stream.rs), and the player
lifecycle in [`web/index.html`](../crates/plurxd/src/web/index.html).

**External contracts checked:** the
[hls.js API](https://github.com/video-dev/hls.js/blob/master/docs/API.md),
the [FFmpeg HLS muxer](https://ffmpeg.org/ffmpeg-formats.html), the
[FFmpeg input pacing options](https://ffmpeg.org/ffmpeg.html), and the
[FFmpeg filter catalogue](https://ffmpeg.org/ffmpeg-filters.html).

**Not verified:** no Intel or AMD deployment GPU was available during this
review. The QSV, VA-API, OpenCL, Vulkan, and jellyfin-ffmpeg graph findings
are therefore requirements for a hardware spike, not claims that a specific
graph works on `nynuc`.

## 3. R1 — the hls.js buffer diagnosis is backwards

[PERF-PLAN.md](PERF-PLAN.md) §2.3 says `maxBufferSize: 60 MB` binds before
the default `maxBufferLength: 30`, limiting a 45–70 Mb/s stream to roughly
7–11 seconds. That is not how the vendored hls.js configuration is defined.

The official hls.js API describes `maxBufferLength` as the guaranteed amount
the player will try to buffer even when it exceeds `maxBufferSize`.
`maxBufferSize` is a byte-based input to the target calculation, described
as a "minimum maximum," not a hard browser quota. The browser can still
evict MSE data or reject appends when its own quota is full, but raising the
hls.js byte value does not raise that browser quota.

The current player constructs hls.js with defaults in `attachHls`:

```javascript
const hls = new Hls({
  enableWorker: false,
  xhrSetup: x => {
    if (TOKEN) x.setRequestHeader("authorization", "Bearer " + TOKEN);
  },
});
```

### Consequence

B5 is not established by the configuration values alone. A real 7–11
second ceiling could still exist because of MSE quota, browser eviction,
segment authoring, or measurement timing, but M0 must demonstrate it.
Section 4.3's proposed `maxBufferSize: 400e6` may do nothing useful and may
increase the frequency or cost of `BUFFER_FULL_ERROR`.

The acceptance check using `performance.memory` is also incomplete.
JavaScript heap measurements do not reliably represent the media engine's
SourceBuffer allocation.

### Required revision

1. Remove B5 from the confirmed root-cause table until M0 records actual
   forward buffer over time.
2. Add client events for `BUFFER_FULL_ERROR`, append failure, browser
   eviction if observable, and the configured hls.js limits.
3. Tune seconds first: `maxBufferLength` · `maxMaxBufferLength` ·
   `backBufferLength`.
4. Leave `maxBufferSize` at its default for the baseline. Raise it only when
   the evidence shows byte targeting, rather than browser quota, is the
   limiter.
5. Measure browser-process memory or media internals in the long-run test;
   do not use JavaScript heap alone.

**Acceptance:** on the same 4K copy-HLS fixture, the baseline records
buffer runway, MSE errors, and process memory for at least 60 minutes. A
configuration change ships only if it increases p10 runway without raising
buffer-full errors or unbounded process memory.

## 4. R2 — ahead must be measured in media time and bytes

The proposed controller computes:

```text
ahead = (max_written - max(high_segment, 0)) × SEGMENT_SECONDS
```

This has three separate problems.

### Copy segments are not fixed-duration

The transcode path forces keyframes on the segment grid. The copy path
cannot. FFmpeg cuts an HLS segment at the next keyframe after `hls_time`, so
a 2-second target can produce a 2-, 5-, 10-, or longer segment depending on
the source GOP. The plan acknowledges this in §4.4, but §4.2 still uses
index arithmetic.

### `high_segment` is a request frontier, not a playhead

The server updates `high_segment` after a segment has been fetched. An eager
client may have fetched 30–60 seconds beyond `video.currentTime`.
Consequently:

- producer minus `high_segment` is ahead of the download frontier;
- it is not ahead of actual playback;
- GC behind `high_segment` can delete media near the real playhead.

Using the request frontier is still useful for bounding unconsumed server
output, but the field and acceptance checks must name it honestly.

### A seconds-only window does not bound disk

The same 180 seconds may be hundreds of megabytes at a transcode rung or
more than a gigabyte for high-bitrate 4K copy. A malformed or unexpectedly
high source bitrate makes a time-only limit an unsafe disk contract.

### Required revision

Track completed segments as immutable metadata:

```text
SegmentMeta {
    index,
    start_ms,
    end_ms,
    bytes,
}

produced_end_ms = end of the newest completed segment
fetched_end_ms  = max end of any successfully served segment
ahead_ms        = produced_end_ms - fetched_end_ms
ahead_bytes     = bytes of completed segments after fetched_end_ms
```

Derive `start_ms` and `end_ms` from playlist `EXTINF` entries or muxer
progress associated with completed files. Do not multiply indexes by the
target duration.

Suspend when either the time window or the byte window is exceeded. Resume
with hysteresis when both fall below their low-water marks. Make the byte
limit explicit and settings-backed, or derive it from a configured hard
scratch budget divided by the maximum number of sessions.

Run the controller when a segment completes and when the request frontier
advances. The 15-second reaper can remain the repair loop, but it is too
coarse to be the primary flow controller.

### EVENT playlist and GC contract

FFmpeg's EVENT type forces an append-only playlist containing all entries.
The current server independently deletes old segment files. A normal
forward-playing client may never ask for them again, but a playlist reload,
retry, decoder reset, or native player may.

The plan must choose and test one of these contracts:

1. Keep EVENT, retain every URI for at least the documented maximum retry
   and back-buffer window, and accept that older entries eventually 404.
2. Use a sliding media playlist with `MEDIA-SEQUENCE` and let FFmpeg's HLS
   deletion semantics own the window.
3. Rewrite the served playlist to remove entries only after every supported
   client no longer needs them.

**Acceptance:** variable-GOP copy fixtures prove the calculated media
window matches cumulative `EXTINF` durations. A high-bitrate fixture cannot
exceed the configured byte budget beyond one in-progress segment. Chrome,
Safari, and AirPlay survive playlist reload and a forced segment retry after
GC has begun.

## 5. R3 — restart-based ABR needs a handoff contract

[ADAPTIVE-QUALITY.md](ADAPTIVE-QUALITY.md) says a quality restart costs
roughly 1–3 seconds and that the existing buffer usually covers it. The
existing restart machinery does not preserve that guarantee:

1. `teardownHls()` destroys the incumbent hls.js instance.
2. Audio and seek restarts tear it down before awaiting the new session.
3. `attachHls()` always tears down again before attaching the replacement.
4. The new session starts at the old current position, so preparation time
   can also create a backward jump unless the timeline is reconciled.

The current behavior is acceptable for a user-initiated audio switch. An
automatic controller may do it several times during one title, so the
interruption becomes part of normal playback and needs its own SLO.

### One-rung downgrade plus cooldown is too slow

The proposed controller drops one rung and then applies a 20-second
cooldown to every switch. On an 8.2 Mb/s to 1.5 Mb/s bandwidth cliff:

```text
1080p ──stall──▶ 720p ──20 s──▶ 480p ──20 s──▶ 360p
```

The player can remain above the sustainable rate for 40 seconds. A cooldown
prevents oscillation, but it must not prevent emergency recovery.

### Required revision

Use asymmetric selection, not only asymmetric thresholds:

- **Severe pressure:** a stall, near-empty runway, or estimate far below the
  current rung selects the highest rung safely below the estimate in one
  move.
- **Mild pressure:** two consecutive low estimates may step down one rung.
- **Further downgrade:** remains available during cooldown if the buffer is
  still draining.
- **Upgrade:** requires the long healthy window and owns the cooldown.

Preserve the last bandwidth estimate outside the Hls instance and seed the
replacement instance. hls.js exposes `bandwidthEstimate` as a setter for
this purpose. Also incorporate recent server encode speed when the status
endpoint exists: a producer below `1×` with shrinking runway is actionable
before it causes a client stall.

### Handoff options

**Option A — accept the blip:** destroy and restart as today, show the
"Adjusting quality…" overlay, and make interruption duration an explicit
product property. This is the smallest design.

**Option B — prepare then attach:** start the replacement session while the
old SourceBuffer continues to play, wait for the first replacement segment,
then switch on a calculated future boundary. This requires timeline and
supersession changes because the server currently kills the old session
before starting the new one.

**Option C — multivariant HLS:** defer the problem to Phase 3, accepting the
extra server session model and brief dual-encode overlap. This should remain
behind the existing decision gate.

For Phase 2, Option A is honest and likely sufficient. The plan should not
claim the buffer conceals the switch unless Option B is actually built.

**Acceptance:** a scripted 8 Mb/s to 1.5 Mb/s cliff reaches a sustainable
rung with no more than one automatic restart. Quality-switch interruption
p95 is measured and documented. A bandwidth recovery does not cause more
than one upgrade in 60 seconds, and recreating hls.js does not reset the
controller to its 500 kb/s default estimate.

## 6. R4 — rate-control validation must exercise rate control

Section 4.6 proposes adding `-maxrate`, `-bufsize`, and possibly VA-API
`-rc_mode VBR`, then relying on the current encoder validation to catch a
driver that rejects them.

The current `validation_args` function does not call
`Encoder::encode_args`. It initializes the device, applies the upload
suffix, specifies the encoder name, and writes to null. A driver can pass
startup validation and reject the real session's rate-control combination.

### Required revision

Run two different validations:

1. **Startup capability probe:** a short encode with the complete production
   encoder arguments, including rate-control mode, bitrate, maxrate,
   bufsize, pixel format, and representative frame rate.
2. **Corpus measurement:** a grain-heavy clip that records actual output
   bitrate over the measurement window the network controller cares about.

`maxrate` plus `bufsize` describes a buffering model, not a promise that
every individual 2-second segment remains below exactly `1.5×`. Replace the
current acceptance statement with:

- the exact window over which peak bitrate is calculated;
- the permitted overshoot and why;
- a no-flag-rejection smoke test for every available encoder family;
- the actual HLS `BANDWIDTH` value advertised to clients.

**Acceptance:** every validated family accepts the exact production
rate-control arguments. A grain-heavy fixture stays inside the documented
sliding-window bound, and the ladder reports a peak value that covers the
measured stream.

## 7. R5 — GPU tone-map validation needs real HDR media

M2 correctly makes GPU tone-mapping the structural 4K fix and correctly
proposes probing capabilities rather than branching on versions. The probe
as described is too weak.

A 10-bit test pattern labelled as HDR proves that a filter graph parses and
can move frames. It does not prove:

- the decoder preserves the source HDR metadata needed by the filter;
- the filter emits correct SDR rather than clipped or washed output;
- the output is tagged BT.709 with the expected matrix, transfer, and
  primaries;
- the graph is faster than the CPU reference on the real source shape;
- HLG and Dolby Vision follow the intended fallback paths.

The example QSV and VA-API graphs should remain hypotheses until checked
against the exact jellyfin-ffmpeg build, driver, kernel, and device. Public
upstream filter documentation is useful for syntax, but it is not a
substitute for probing a patched deployment build.

### Required revision

Bundle a tiny redistribution-safe HDR10 fixture and validate each candidate
pipeline end to end:

1. Decode the real compressed source through the candidate hardware path.
2. Tone-map and scale using the production graph.
3. Encode a short H.264 output.
4. Assert SDR color tags with `ffprobe`.
5. Compare sampled luma/chroma ranges or a perceptual metric against the CPU
   reference, with a deliberately broad tolerance for encoder differences.
6. Require a minimum recent speed, not merely exit status zero.

Keep runtime fallback. A startup probe cannot cover every codec profile or
driver state. If the chosen graph stops progressing, restart once on the
known CPU graph and record the pipeline downgrade.

### Subtitle scope must be corrected

The plan says subtitle burn-in forces the CPU path. That is true for the
current text `subtitles` filter. Bitmap subtitles are marked as a
fast-follow and are explicitly skipped by `video_filters`; they are not
currently burned. M2 and the cache recipe must say:

- text burn-in selects the compatible CPU/hybrid graph;
- bitmap burn-in remains unsupported until an overlay graph is implemented;
- no cache entry may claim bitmap subtitles were burned when they were
  omitted.

**Acceptance:** the deployment fixture produces a visually sane BT.709 SDR
output at or above the target speed. HDR10 selects the proven GPU graph;
HLG and Dolby Vision select only their explicitly validated candidates.
Logs name the candidate, fallback reason, and actual encoder attempt.

## 8. R6 — live and background work need one resource arbiter

The pre-transcode producer runs only when `active_sessions == 0`. A movie
cache encode may run for minutes. A viewer arriving after it starts still
contends with it, which is the exact workload the gate intends to prevent.

The hardware-session cap has a related problem. When the cap is full, the
plan starts a new session directly on software. For 4K HDR, software is the
known sub-realtime path and may consume the CPU needed to serve the two
healthy hardware sessions.

Session counts are also racy: two starts can both observe a free slot before
either registers its session.

### Required revision

Put all GPU-consuming work behind one allocator with explicit priority and
RAII-style permits:

| Priority | Work | When capacity is unavailable |
|---|---|---|
| 1 | Live playback | Preempt background work; then place, downgrade, or queue |
| 2 | Failover recovery | Reserve or preempt; it restores an interrupted viewer |
| 3 | Interactive seek/quality restart | Reuse the playback's permit or replace it atomically |
| 4 | Pre-transcode cache | Suspend or cancel immediately when live work arrives |
| 5 | Maintenance benchmark | Run only under an explicit operator action |

The allocator should be per hardware device or encoder family, not only a
single global count. Capacity defaults may start at two, but measured
hardware capability should be allowed to override them.

When hardware is unavailable, the policy should choose among another node,
a lower software-safe rung, a short queue, or a capacity response. "Always
start 1080p software" is not a safe universal fallback.

**Acceptance:** start a background cache encode, then request live
playback. The background worker yields within a bounded interval and live
TTFF remains inside its SLO. Concurrent start tests cannot acquire more
hardware permits than configured. Releasing, fallback, suspension, and
process death cannot leak a permit.

## 9. R7 — recipe identity and physical cache location are different data

The M3 schema stores one unique `recipe_hash` and one `dir`. M4 later says a
recipe may exist in multiple node-local caches and placement should prefer a
node holding it. One row cannot represent both claims:

- different nodes may mount local cache roots at different paths;
- the same recipe may exist on several nodes;
- one node can evict its copy without invalidating another;
- a shared NAS copy is global, while a local copy is not.

### Required revision

Separate semantic output identity from physical copies:

```sql
CREATE TABLE transcode_cache_recipes (
    recipe_hash   TEXT PRIMARY KEY,
    file_id       INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    recipe_version INTEGER NOT NULL,
    created_at    INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

CREATE TABLE transcode_cache_locations (
    recipe_hash   TEXT NOT NULL REFERENCES transcode_cache_recipes(recipe_hash)
                               ON DELETE CASCADE,
    node_id       TEXT NOT NULL,
    storage_class TEXT NOT NULL, -- local | shared
    relative_dir  TEXT NOT NULL,
    bytes         INTEGER NOT NULL,
    complete      INTEGER NOT NULL DEFAULT 0,
    last_used_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    last_seen_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (recipe_hash, node_id, storage_class)
) STRICT;
```

Store a relative directory under the configured cache root, not an absolute
path replicated to every node.

### The recipe needs an output contract

`cache_format_version` is necessary but easy to forget. The recipe should
identify, directly or through a single pipeline-version digest:

- FFmpeg/jellyfin-ffmpeg build fingerprint;
- output codec · profile · level · pixel format;
- color transfer · primaries · matrix · range;
- muxer and segment format;
- GOP/segment policy;
- rate-control policy;
- selected audio and subtitle semantics;
- every filter that changes visible or audible output.

Excluding encoder family is reasonable only after QSV, VA-API, NVENC,
VideoToolbox, and software all satisfy the same declared output contract.
Until then, include the family or a normalized pipeline identity.

### Publication needs fencing

Distributed producers must write to unique temporary directories under the
same filesystem as the final path. Before rename, the worker must prove it
still owns the job lease. This prevents an expired worker from publishing
over a newer result. Only after rename succeeds should the location become
`complete=1`.

Generate VOD playlists as VOD when possible. Post-processing an EVENT
playlist must replace its playlist type and validate `ENDLIST`; appending
tags by string manipulation is an avoidable corruption path.

**Acceptance:** two nodes can hold the same recipe locally and evict either
copy independently. A shared copy is visible from another mount root. A
worker that loses its lease cannot publish. A build or pipeline-version
change always misses the old recipe.

## 10. R8 — failover must define ownership and unavailable bytes

The Phase 3 spike establishes an important property: a surviving node can
restart an encode at a segment boundary and emit valid media after a
discontinuity. It does not by itself define a complete distributed serving
protocol.

M4 says the survivor serves a stitched playlist containing served-prefix
entries. The primary design keeps live segment bytes node-local. When the
owner is dead, those prefix URIs are not available on the survivor unless
the client already has them or the survivor regenerates them.

"Last served" is also not "last received." The server knows that it opened
and wrote an HTTP response; it does not know the client fully received and
appended it before the owner failed.

### Required revision

Specify these state fields:

```text
SessionOwner {
    session_id,
    owner_node,
    owner_epoch,
    lease_expires_at,
    recipe,
    produced_through,
    requested_through,
    client_acknowledged_through?,
}
```

Every owner mutation and segment route must carry the epoch. A node whose
lease expires may no longer publish or claim ownership, which fences a
partitioned former owner.

On takeover, restart conservatively from one segment before the last
trusted boundary unless a client acknowledgement proves a later boundary
is safe. Publish a new media sequence and an `EXT-X-DISCONTINUITY` at the
takeover point.

Choose one prefix policy explicitly:

1. **Client-buffer reliance:** the replacement playlist begins at the
   takeover sequence; old bytes are not promised.
2. **Overlap regeneration:** the survivor re-encodes one or more prior
   segments and can satisfy retries.
3. **Shared hot prefix:** the last small segment window is written to shared
   storage, not the whole live session.

The failover SLO must include detection:

```text
owner failure
  │
  ├─ detect / lease expires
  ├─ elect or claim new owner
  ├─ open NAS input and seek
  ├─ produce first replacement segment
  └─ client playlist retry + append
```

A ≤5-second target is plausible only if the sum of those stages is budgeted
and measured. A 5-second lease alone leaves no time to encode.

**Acceptance:** kill the owner while a segment response is in flight, not
only between segments. The client either keeps playing from its buffer or
experiences one bounded rebuffer; it never loops on a missing prefix URI.
A partitioned old owner cannot publish after a new epoch is active.

## 11. R9 — telemetry needs attempt generations and recent speed

M0 adds `-progress pipe:1`, and the watchdog may replace the hardware child
with a software child inside the same session. The current session metadata
stores a static `encoder_label`; fallback changes the child but not that
label.

Progress-reader tasks create another race:

1. hardware attempt starts and owns stdout;
2. watchdog kills it;
3. software attempt starts;
4. old stdout closes and its reader reports process end;
5. a late old update can overwrite the new attempt's health.

FFmpeg's reported `speed` is useful but often cumulative. A fast initial
period can hide a recent slowdown. The watchdog and placement logic need a
recent delta:

```text
recent_speed =
    (out_time_now - out_time_previous)
    / (wall_time_now - wall_time_previous)
```

Smooth it with a short EWMA and retain the raw cumulative value for
diagnostics.

### Required revision

Every process attempt gets a monotonically increasing generation. Progress,
exit, stall, and fallback updates apply only when their generation still
matches the session's active attempt. Encoder label, pipeline label,
start-time, recent speed, and last progress all belong to the active
attempt.

Status polling must not call the same `touch` path as playlist and segment
delivery; otherwise leaving the Stats overlay open can keep an abandoned
encoder alive forever.

Client events need a playback-attempt ID and reason:

- cold start · resume · seek · audio switch · manual quality · automatic
  quality · failover;
- click/request start · first playlist · first segment · first `playing`;
- intentional pause and seek must not count as stalls.

**Acceptance:** synthetic progress tests prove a killed generation cannot
mark its replacement dead or overwrite its encoder. Recent speed reacts to
a forced slowdown within the documented window. Polling status alone does
not prevent idle reaping.

## 12. R10 — playback sessions need idempotent lifecycle APIs

`GET /api/v1/files/:id/hls/start` creates a process, mutates server state,
and kills a prior matching session. GET can be retried, prefetched, or
replayed by infrastructure that assumes it is safe. ABR and clustering
increase how often this endpoint is exercised and make ambiguous retries
more expensive.

Supersession is currently keyed by user name plus file ID. Two devices using
one account to watch the same file can kill each other's sessions. The code
documents this as a rare trade-off; automatic restarts and cluster routing
make it less rare.

### Required revision

Introduce a stable client-generated `playback_id` for one player instance.
Create or replace a session with an idempotent POST:

```text
POST /api/v1/files/:id/playback-sessions

{
  "playback_id": "uuid",
  "request_id": "uuid",
  "method": "transcode",
  "height": 720,
  "start_seconds": 123.4,
  "audio_index": 1,
  "replaces_session": "optional-session-id"
}
```

The same `request_id` returns the same result instead of spawning twice.
`replaces_session` makes supersession explicit and device-local.

Add an explicit stop:

```text
DELETE /api/v1/hls/:session
```

The client calls it with `fetch(..., {keepalive: true})` when closing or
replacing a player. The idle reaper remains the crash and AirPlay fallback.
Do not stop a session merely because the browser page detached if an
AirPlay target is still fetching it.

**Acceptance:** replaying the same start request never creates two
processes. Two devices under one account can watch the same file
independently. Closing a normal browser player releases its process without
waiting 60–75 seconds, while an active Apple TV continues to keep its
session alive through media requests.

## 13. R11 — remove the remaining play-path I/O and memory copies

Moving chapters to scan time removes the largest avoidable `/decision`
probe, but `/decision` still calls `tokio::fs::metadata` on the media path.
On a cold NAS mount, that is still remote I/O between click and response.

The endpoint also mixes decision calculation with a Trakt "watching now"
side effect. A decision endpoint should be pure and repeatable; playback
start is the point at which watching actually begins.

### Required revision

- Persist or cache file availability with a short age, updated by scans and
  failed opens. The actual open still remains authoritative.
- Move Trakt start notification to successful playback-session creation or
  first media delivery.
- Cache the pure part of a decision by file revision, capability profile,
  force mode, and settings revision.
- Consider combining decision and session creation into one playback
  endpoint after the contract is stable; this removes a network round trip
  for transcode starts.

HLS segment delivery currently waits for a completed file and then reads the
whole segment into a `Vec<u8>` before Axum begins responding. A 4-second
70 Mb/s segment is roughly 35 MB. Multiple sessions create avoidable
allocation and copying pressure.

Once `temp_file` has atomically renamed a segment, stream the immutable file
body. Set `Content-Length` from metadata. Keep playlists `no-store`, but
segments can use a private immutable cache policy because a session segment
URI never changes.

**Acceptance:** a second `/decision` for an unchanged file performs no NAS
operation. A segment response begins without allocating the full segment in
the server process. Repeated requests for the same immutable segment return
identical bytes and cannot observe a partial file.

## 14. R12 — reconcile the adaptive plan with the current player

ADAPTIVE-QUALITY Phase 1 says to add a manual Quality menu. The current
player already has:

- `Auto` · `Original` · `1080p` · `720p` · `480p`;
- localStorage persistence;
- forced decision modes;
- restart-at-position when the choice changes.

Phase 1 should be rewritten as the work that remains:

- server-owned, source-filtered ladder response;
- 360p rung;
- exact snapping and validation of requested heights;
- bounded hardware rate control;
- active rung and reason in Stats;
- tests that the menu consumes the server ladder rather than hardcoding it.

This correction matters because the current "small — one sitting" estimate
mixes already-shipped UI with unimplemented server contracts. A handoff
plan should make the remaining diff obvious.

## 15. Revised order — establish correctness before adding prediction

The original milestone direction remains useful, but the dependency order
should change:

```text
M0 measurement + repeatable benchmark
  │
  ├─▶ pure/cached decision path
  ├─▶ idempotent playback-session lifecycle
  └─▶ attempt-safe progress telemetry
          │
          ▼
duration/byte-based pacing window + segment streaming
          │
          ▼
startup-segment experiment + progress watchdog + validated rate control
          │
          ▼
real-HDR GPU tone-map spike and runtime fallback
          │
          ▼
restart-based Auto quality with an explicit interruption SLO
          │
          ▼
preemptible resource arbiter
          │
          ▼
pre-transcode cache with recipe/location split
          │
          ▼
cluster placement, fencing, and failover protocol
```

### Slice 0 — measurement and lifecycle

Build M0, but add playback-attempt IDs, recent-speed calculation, process
generations, and the repeatable benchmark harness. Convert session creation
to an idempotent lifecycle before automatic restarts multiply it.

**Result:** every later result has trustworthy attribution, and retries
cannot accidentally spawn duplicate work.

### Slice 1 — cheap play-path wins

Move chapters to scan time · cache availability checks · move Trakt start
out of `/decision` · stream immutable segment bodies · explicitly stop
closed sessions.

**Result:** lower click-path latency and less wasted process/memory work,
with little media-pipeline risk.

### Slice 2 — pacing and buffer

Replace copy `-re` with probed readrate support · account ahead using
completed media duration and bytes · suspend/resume on controller events ·
measure hls.js defaults before changing them.

**Result:** copy-HLS can build reserve without unbounded scratch use, and
the client configuration follows evidence rather than an incorrect byte-cap
assumption.

### Slice 3 — startup cadence, watchdog, and bitrate

Benchmark 1-second bootstrap plus 2- or 4-second steady segments against a
uniform 2-second cadence. FFmpeg exposes `hls_init_time`, but its behavior
with EVENT playlists and the selected keyframe expression must be verified
on the deployed build. Add the progress watchdog and full-command
rate-control probes in the same slice.

**Result:** shorter TTFF with a measured request/compression trade-off,
fewer false fallbacks, and ladder rates that mean what the client thinks
they mean.

### Slice 4 — GPU tone-mapping

Run the real-HDR candidate matrix on the target Intel and AMD nodes. Ship
only graphs that pass color, speed, and runtime fallback checks.

**Result:** a 4K HDR transcode becomes structurally realtime rather than
depending on a large opening buffer.

### Slice 5 — adaptive quality

Publish the ladder, preserve manual mode, implement severe-pressure direct
downgrades, seed estimates across Hls instances, and measure the restart
interruption.

**Result:** Auto responds to both wire and encoder pressure without taking
several cooldown periods to find a playable rung.

### Slice 6 — resource arbitration and cache

Land the live-first allocator before starting background cache production.
Use the recipe/location schema and fenced publication contract.

**Result:** predicted plays can be cached without stealing the GPU from an
unexpected viewer or blocking later cluster placement.

### Slice 7 — cluster

Add distributed reservations, owner epochs, placement, proxying, and the
tested takeover playlist. Treat shared cache distribution and live failover
as separate protocols even though they share membership.

**Result:** node loss has a bounded, explainable recovery path rather than a
best-effort restart with ambiguous prefix ownership.

## 16. Measurement contract — judge consequences, not guesses

One evening of watching is useful discovery, but it is not an acceptance
suite. Record distributions by method and source class.

| Metric | Report | Split by | Why it matters |
|---|---|---|---|
| TTFF | p50 · p95 · max | direct · remux · copy-HLS · transcode; cold/warm NAS | A mean hides the starts users remember |
| Rebuffer ratio | stalled time / played time | method · rung · client | Comparable across title duration |
| Stalls | count/hour · p95 duration | network vs producer attribution | Separates frequent nudges from long failures |
| Encode speed | p10 · p50 recent speed | encoder · pipeline · source class | p10 predicts whether reserve drains |
| Buffer runway | p10 · p50 · max | browser/client · source bitrate | Proves the pacing and client changes compose |
| Ahead window | media seconds · bytes | copy vs transcode | Verifies both latency reserve and disk bound |
| Rate control | average · peak sliding window | encoder family · rung | Makes ladder bandwidth claims honest |
| ABR behavior | switches/hour · time-to-safe-rung | cliff · recovery · jitter tests | Exposes oscillation and slow recovery |
| Resource delay | queue/preemption time | live · failover · cache | Proves background work yields to viewers |
| Failover | detect · claim · first segment · resume | owner failure point | Shows where the ≤5 s budget went |

The minimum fixture matrix should include:

- 1080p H.264 SDR with short GOP;
- 4K HEVC SDR;
- 4K HDR10 HEVC;
- 4K HLG;
- supported and unsupported Dolby Vision profiles;
- variable frame rate;
- sparse or irregular keyframes;
- grain-heavy content for bitrate peaks;
- positive and negative audio offsets;
- text subtitles and a bitmap-subtitle case that asserts the current
  unsupported result;
- cold and warm NAS attribute/data caches.

Client coverage: current Chrome with hls.js · Safari native HLS · AirPlay to
Apple TV · one constrained-memory browser/device if supported.

## 17. Decisions to record before implementation

The plan becomes ready to build when these choices are explicit:

1. **Client buffer policy:** which seconds-based limits ship, and what M0
   evidence justifies changing the byte target.
2. **Server ahead policy:** the media-time high/low marks, hard byte budget,
   and whether the reference frontier is fetched or client-reported
   playback.
3. **Playlist/GC contract:** EVENT with documented expired URIs, a sliding
   playlist, or served-playlist rewriting.
4. **ABR handoff:** accepted visible restart or prepared boundary switch,
   with a numeric interruption SLO.
5. **Over-capacity behavior:** lower software rung · short queue · another
   node · capacity response, in a stated order.
6. **Cache identity:** exact pipeline digest and when encoder families may
   share one recipe.
7. **Background preemption:** suspend or cancel, including how partial cache
   work is resumed or discarded.
8. **Failover prefix:** client-buffer reliance · overlap regeneration ·
   shared hot prefix.
9. **Owner fencing:** lease duration, epoch semantics, and recovery budget.
10. **Bitmap subtitles:** remain unsupported or become a prerequisite for
    cache recipes that advertise a bitmap burn.

Once those are resolved, [PERF-PLAN.md](PERF-PLAN.md) can return to
`ready to build`. Until then, its milestones are a strong design direction,
not yet an exact implementation contract.

## 18. Source map — where each finding comes from

| Finding | Plan anchor | Implementation anchor |
|---|---|---|
| R1 buffer semantics | PERF §2.3, §4.3 | `web/index.html::attachHls` |
| R2 ahead accounting | PERF §4.2, §4.4 | `transcode.rs::high_segment`, `gc_old_segments`; `hls_copy_args` |
| R3 ABR handoff | ADAPTIVE Phase 2 | `teardownHls`, `attachHls`, `switchAudio`, `seekTo` |
| R4 rate validation | PERF §4.6 | `encoder.rs::validation_args`, `Encoder::encode_args` |
| R5 GPU tone-map proof | PERF M2 | `decode_setup`, `video_filters`, encoder validation |
| R6 arbitration | PERF M2 concurrency, M3 producer | `TranscodeManager::start`, `active_sessions` |
| R7 cache topology | PERF M3 schema, M4 cache | proposed schema; no implementation yet |
| R8 failover | PERF M4, PHASE3-SPIKE | proposed replicated session state |
| R9 telemetry attempts | PERF M0, §4.5 | `spawn_ffmpeg`, hardware fallback task, static `encoder_label` |
| R10 lifecycle API | PERF seek/ABR paths | `http/hls.rs::start`, `reap_superseded` |
| R11 play-path I/O | PERF §4.1 | `stream.rs::decision`, `transcode.rs::segment` |
| R12 plan drift | ADAPTIVE Phase 1 | `QUALITIES`, `setQuality`, `transcodeHeight` |

External behavioral contracts:

- [hls.js API — buffer configuration and bandwidth estimate](https://github.com/video-dev/hls.js/blob/master/docs/API.md)
- [FFmpeg HLS muxer — segment timing, EVENT/VOD, and deletion](https://ffmpeg.org/ffmpeg-formats.html)
- [FFmpeg CLI — `-readrate` and initial burst](https://ffmpeg.org/ffmpeg.html)
- [FFmpeg filters — upstream hardware filter contracts](https://ffmpeg.org/ffmpeg-filters.html)

