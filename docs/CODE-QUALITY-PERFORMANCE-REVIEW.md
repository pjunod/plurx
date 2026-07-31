# Code quality and performance review — a strong data plane with lifecycle gaps

**Reviewed:** 2026-07-30  
**Scope:** commit `d3150674` plus the working tree  
**Outcome:** review only; no production code was changed

This review complements
[ARCHITECTURE.md](ARCHITECTURE.md), [PLAYBACK.md](PLAYBACK.md),
[PERF-PLAN.md](PERF-PLAN.md), and
[ADAPTIVE-QUALITY.md](ADAPTIVE-QUALITY.md). It records the current code-quality
assessment, the most important correctness and performance findings, and a
recommended order for addressing them.

## 1. Verdict — the streaming mechanics are better than the orchestration

The server core is above average. The code reflects deliberate engineering,
good test coverage, and the right high-level priority: direct play first,
remux when possible, and full transcoding only when necessary. The most
performance-sensitive data paths generally stream rather than buffer, and the
custom fMP4/GOP segmenter is careful and well tested.

The main weaknesses are not in codec handling. They are at the boundaries
between clients, session orchestration, process lifecycle, admission control,
and storage accounting.

| Area | Assessment | Main reason |
| --- | --- | --- |
| Clarity | Good, with concentrated hotspots | Crate boundaries are clear, but a few files own too many responsibilities |
| Efficiency | Good data plane; avoidable control-plane work | Media bodies stream efficiently, while playlist refresh and settings reads repeat growing work |
| Architecture | Sound foundation | Core policy is separated from HTTP and process control |
| Layout | Good between crates; weak within large modules | `transcode.rs`, `state.rs`, and the web UI have become monoliths |
| Streaming engine | Technically strong, operationally fragile | Segmenting and pacing are solid; retries, watchdogs, and permits have race or lifecycle gaps |

The best next investment is to make the existing engine predictable under
concurrency and failure. Adding more quality modes before that would compound
the session-state complexity.

## 2. Highest-priority findings

### 2.1 Native clients do not honor the server's playback verdict

**Evidence.** Android treats every non-direct result as `/stream.mp4`. That
endpoint is a remux/copy path even when the server selected transcoding. The
decision is handled in
`clients/android/app/src/main/java/tv/plurx/app/player/PlayerScreen.kt`, with
request construction in the adjacent controller code.

Apple takes the opposite shortcut: every non-direct result starts a hardcoded
1080p transcode in
`clients/apple/Sources/PlayerController.swift`. A remux verdict therefore
becomes an unnecessary full encode.

Both clients use the deprecated GET form of HLS session creation and do not
explicitly delete sessions. Abandoned sessions remain until the server reaper
finds them.

**Consequence.** The same media can be delivered differently by each client,
with incorrect quality, unnecessary CPU use, or a stream the client cannot
actually play. Session cleanup is delayed.

**Recommendation.** Return a tagged, server-owned delivery plan:

```text
direct     { url }
remux      { url, segmented }
transcode  { create-session request }
```

Migrate native clients to POST session creation with idempotency and playback
IDs, and issue DELETE when playback ends. Keep quality selection on the server
so every client follows the same capability and policy rules.

### 2.2 Session-creation idempotency has a check-then-act race

**Evidence.** In `crates/plurxd/src/transcode.rs`, the session creation path
checks the request map, releases the lock, starts FFmpeg, and then records the
result. Concurrent retries with the same request ID can both pass the check.

**Consequence.** A retry can create duplicate encoders. Depending on subsequent
cleanup and supersession, callers can receive a session that another retry has
already killed.

**Recommendation.** Reserve the request ID atomically before starting work.
Represent the entry explicitly as `InFlight`, `Ready`, or `Failed`. Concurrent
callers should await the same in-flight result rather than start another
process.

### 2.3 The stall watchdog exits after observing initial progress

**Evidence.** `watch_for_stall` in `crates/plurxd/src/transcode.rs` returns once
the session has produced a segment. The hardware wrapper also returns after
the first healthy segment, or after observing that encoder time is moving.

**Consequence.** A process that wedges later in playback has no active
watchdog. A slow process that advances timestamps without publishing its first
playable segment can also lose monitoring prematurely.

**Recommendation.** Run one lifetime watchdog per active encoder. Track both:

- encoder timestamp progress; and
- the end time of the newest published, playable segment.

Pause the watchdog during intentional suspension. Do not end it until the
process exits or the session is terminal.

### 2.4 Software transcodes have no CPU admission budget

**Evidence.** Hardware starts are guarded by a slot model. Software starts
bypass that model and allow each x264 process to choose its normal thread
count. The cache producer checks whether live work is waiting, but software
live starts never enter the hardware waiter path that drives that signal.

**Consequence.** Several software sessions can oversubscribe every core,
increase latency for all sessions, and prevent background work from yielding
to live playback.

**Recommendation.** Add a CPU-weighted permit pool for software encoders.
Account for resolution, preset, and configured x264 thread count. Put all live
starts, not only hardware starts, into the priority model. Make the per-process
thread budget explicit.

### 2.5 Hardware-to-software fallback retains the hardware permit

**Evidence.** The fallback path replaces the failed hardware process with a
software process but does not release the hardware slot at the transition.
The session's encoder class also continues to describe the original encoder.

**Consequence.** A hardware slot can remain unavailable until the software
session ends. Telemetry and admission measurements then attribute software
work to the wrong class.

**Recommendation.** Release the hardware permit as part of the atomic fallback
transition and update the session's active encoder class. Cover the transition
with a test that proves another hardware session can start immediately.

### 2.6 Playlist flow control repeats growing work

**Evidence.** `refresh_segments` in `crates/plurxd/src/transcode.rs` rereads and
reparses the complete growing EVENT playlist, then rebuilds a `HashMap` and
segment strings. `flow_control` also resolves ahead limits through three
sequential SQLite settings reads on playlist reload or segment publication.

The SQLite store currently serializes access through one mutex-protected
connection in `crates/plurxd/src/store/sqlite/mod.rs`.

**Consequence.** Playlist maintenance approaches quadratic work over a long
session. Repeated settings queries add lock contention to a hot control path.

**Recommendation.**

1. Cache immutable or rarely changing stream limits in process memory.
2. Pass already-read playlist bytes to indexing code.
3. Maintain an append-oriented segment index instead of reconstructing all
   prior entries.
4. Rebuild from disk only after truncation, replacement, or recovery.

### 2.7 The global scratch limit does not measure total live scratch use

**Evidence.** `global_ahead_bytes` counts files after each session's fetched
frontier. Retained files behind that frontier still occupy disk but are
excluded from the global limit.

**Consequence.** The documented 8 GB cap is not a true bound on live scratch
space and can be exceeded materially with several active sessions.

**Recommendation.** Track two separate values:

- `total_live_bytes`, used for the hard disk budget; and
- `ahead_bytes`, used for producer pacing.

Admission should use the total. Per-session flow control can continue to use
the ahead value.

## 3. Secondary improvements offer measurable returns

### 3.1 Automatic software quality should not advertise an upscaled height

The automatic software path can choose 720p for a 480p source. The filter
avoids actual upscaling, but the bitrate target and response metadata still
describe 720p.

Cap the chosen height at the source height before deriving bitrate and response
metadata.

### 3.2 SQLite reads should not share the writer's single connection

Write serialization is reasonable. Read-only settings, metadata, and status
queries do not need to queue behind it, particularly with WAL enabled.

Add a small read pool or dedicated read connections. Cache configuration values
that are consulted on every segment boundary.

### 3.3 Subtitle extraction should be cached

The VTT endpoint starts FFmpeg and buffers the entire output for every request.
Cache extracted subtitles by media identity, stream index, and source
fingerprint. Stream a cache miss to disk or the response instead of collecting
the whole file in memory.

### 3.4 Audio-offset handling can avoid a second source read

For A/V offset correction, FFmpeg can open the source a second time. On NAS
media this duplicates reads and competes with the video path.

When audio is already being encoded, prefer filter-graph or timestamp
adjustment from a single input. Retain the two-input technique only where copy
semantics make it necessary.

### 3.5 Copy-segmenter memory should be profile-driven

The custom copy segmenter can have a documented peak near 105 MB per session.
That may be acceptable relative to the simplicity and correctness it buys.

If concurrent remux load makes it material, profile allocation sites before
rewriting. Likely candidates are reusable `BytesMut` buffers and vectored
writes. This is lower priority than the lifecycle and control-plane findings.

### 3.6 The quality ladder should follow lifecycle stabilization

The server-owned ladder and automatic quality controller described in
[PERF-PLAN.md](PERF-PLAN.md) and
[ADAPTIVE-QUALITY.md](ADAPTIVE-QUALITY.md) are the right direction. Implement
them after idempotency, watchdog, admission, and cleanup behavior are reliable.

## 4. The existing design has several strong foundations

### 4.1 Crate boundaries express the intended architecture

Pure playback and transcoding decisions live in `plurx-core`. HTTP handlers and
process ownership live in `plurxd`. Plex-specific behavior is isolated. This
keeps most policy testable without a running server or FFmpeg process.

### 4.2 The delivery hierarchy avoids unnecessary work

The code consistently prefers direct play, then remux, then full transcode.
That is the largest performance decision in a media server, and it is modeled
explicitly rather than scattered through handlers.

### 4.3 FFmpeg process isolation is pragmatic

Child processes contain codec failures, avoid linking a large native codec
surface into the daemon, and make build and deployment flexibility easier.
The hardware probe validates actual pipeline execution instead of relying only
on version strings or codec listings.

### 4.4 The segmenting path is careful and well tested

The fMP4/GOP segmenter handles timing, keyframe boundaries, and fragmented MP4
structure deliberately. Large segment bodies are served from open files rather
than copied into a single `Vec`.

### 4.5 The surrounding operational controls are thoughtful

Cache recipes, pacing, telemetry, admission guards, supersession, and cleanup
show awareness of real playback workloads. The repository contains 463 Rust
test annotations, and the streaming modules have focused test suites rather
than relying only on end-to-end coverage.

## 5. Module layout now hides important state transitions

The repository is organized well between crates, but several files have crossed
the point where local clarity declines.

| Hotspot | Current concern | Suggested boundary |
| --- | --- | --- |
| `crates/plurxd/src/transcode.rs` | About 3,100 production lines covering process startup, cache production, lifecycle, admission, telemetry, flow control, and garbage collection | Split into `session`, `attempt_watchdog`, `flow`, and `cache_producer` modules |
| `crates/plurxd/src/state.rs` | Unrelated server-state jobs accumulate together | Separate runtime registries, configuration snapshots, and background maintenance |
| `crates/plurxd/src/web/index.html` | About 5,827 lines in one untyped file | Split UI state, API client, player controls, and presentation |

The transcoder split should follow ownership, not merely file size. Each module
should own an explicit state machine and its cleanup obligations. Prefer enums
such as `Starting`, `Running`, `Suspended`, `FallingBack`, and `Terminal` over
combinations of atomics and detached tasks whose valid combinations are
implicit.

Some comments also lag implementation. For example, the producer-deadline
comment says cross-run resume is discarded while the implementation and tests
preserve it. Comments around lifecycle policy should be treated as contracts
and updated with the code.

## 6. Recommended sequence minimizes rework

1. **Unify the playback contract.** Make native clients execute the
   server-selected direct, remux, or transcode plan and close sessions
   explicitly.
2. **Make session creation and lifecycle atomic.** Reserve idempotency keys,
   keep the watchdog alive, and make hardware fallback release its permit.
3. **Budget software CPU.** Add weighted permits, explicit encoder thread
   limits, and priority for every live start.
4. **Remove hot-path reconstruction.** Cache settings and update playlist
   indexes incrementally.
5. **Enforce a real scratch ceiling.** Separate total disk use from bytes ahead.
6. **Reduce shared-service contention.** Add SQLite read capacity and subtitle
   extraction caching.
7. **Build out automatic quality.** Add the quality ladder and controller once
   session state and resource accounting are dependable.

This order fixes correctness problems that can invalidate later performance
measurements. It also makes load-test results easier to interpret.

## 7. Verification was broad but not completely green

Formatting passed. Focused streaming suites produced these results:

| Suite | Result |
| --- | ---: |
| fMP4 | 28 passed |
| playback | 16 passed |
| copy segmenter | 16 passed |
| admission | 10 passed |
| progressive streaming | 3 passed |
| meter | 3 passed |
| targeted transcode | 26 passed, 2 failed |

The two targeted transcode failures were producer-timing tests. Their synthetic
240-second input completed before the tests could interrupt it. The tests need
deterministic synchronization rather than wall-clock timing.

The full core run reported 231 of 256 tests passing. Twenty-four failures came
from the managed environment forbidding local test sockets. One macOS failure
was a `/var` versus `/private/var` path assertion. These are test-environment
or test-determinism issues, not evidence that the corresponding production
paths failed.

Clippy found one local style warning: a collapsible match in
`crates/plurx-core/src/transcode/fmp4.rs`. The available Rust toolchain was
1.95, while the repository specifies 1.97.1, so this was not a clean
repository-toolchain Clippy run.

## 8. Review limits keep the recommendations honest

- No changes were made to production code during this review.
- No real-device Apple, Android, NVIDIA, or NAS load benchmark was performed.
- This was a code-quality and performance review, not a formal security audit.
- Memory recommendations for the segmenter are hypotheses until a concurrent
  workload profile shows allocation pressure.
- Test failures caused by the managed environment were not treated as product
  regressions, but they should be made portable where practical.
- Existing unrelated working-tree files were left untouched.

