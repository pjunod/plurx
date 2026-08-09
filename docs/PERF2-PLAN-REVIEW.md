# Performance II plan review — the seam contracts are not implementation-ready

**Status:** review complete · 2026-08-09 · plan `dd903f59` · base `e8a910f`
· verdict: **request changes**

This is the adversarial companion to [PERF2-PLAN.md](PERF2-PLAN.md). It applies
the evidence rule from [PERF-PLAN.md](PERF-PLAN.md) §9: plurx claims below were
checked against the base tree at file and line; mechanism claims were checked
against the repository's prior probes, upstream documentation, or a new live
probe.

The review used an isolated clone. The main checkout was not switched or
modified.

## 1. Decision summary

Do not implement N3, N4, or N5.1 from this revision of the plan.

The plan is directionally useful, and much of its inventory is accurate. Its
central N3 recipe is not. A live FFmpeg probe reproducing the proposed
`-ss covered_ms -start_number K` handoff did all three things the design assumes
away:

- the second FFmpeg process replaced the prefix playlist rather than appending
  to it;
- `-start_number` changed filenames and media sequence, not packet timestamps;
- video and audio timestamps moved backward at the seam.

That result agrees with the repository's own Phase 3 evidence. Phase 3 says a
seeked restart resets PTS and audio DTS and therefore requires an HLS
discontinuity. N3 cites that work as proof of a seamless ordinary EVENT session
while omitting the discontinuity and playlist-owner contract that Phase 3
required.

There are six other implementation blockers. N1 does not define one effective
rate-control identity across live, producer, and durable offline work. N4 does
not carry enough prior-session state to make “one rung down” deterministic or
idempotent. N5.1 assumes caps are present at session creation when they are only
present on the earlier decision request, and leaves Apple's mandatory HEVC fMP4
container as an open question. Finally, several acceptance gates name harness
features that do not exist.

### Disposition

| Finding | Class | Required before implementation |
|---|---|---|
| R1. Prefix continuation does not append or preserve the timeline | Blocker | Yes |
| R2. Prefix identity and publication can produce a hybrid stream | Blocker | Yes |
| R3. The audio/frame seam has no defined correctness gate | Blocker | Yes |
| R4. Effective rate control is not pinned across all recipe paths | Blocker | Yes |
| R5. Stall recovery is not deterministic under replay or queued reopens | Blocker | Yes |
| R6. HEVC eligibility, container, and signaling contracts are incomplete | Blocker | Yes |
| R7. The named acceptance harnesses cannot run the proposed gates | Blocker | Yes |
| R8. Order, effort, and risk labels do not match the dependency graph | Should-fix | No, after R1–R7 |

“Blocker” here means the plan leaves an ownership, identity, or wire-contract
decision to the implementer. It does not mean the feature should be abandoned.

## 2. Review basis

The review spot-checked the plan's already-verified citation inventory and then
followed the risky mechanisms through their actual consumers.

### Repository evidence

- Recipe identity and all recipe constructors:
  [`recipe.rs`](../crates/plurx-core/src/transcode/recipe.rs),
  [`transcode.rs`](../crates/plurxd/src/transcode.rs), and
  [`offline.rs`](../crates/plurxd/src/offline.rs).
- HLS arguments, playlist publication, pruning, cache readers, and producer
  assembly:
  [`mod.rs`](../crates/plurx-core/src/transcode/mod.rs),
  [`produce.rs`](../crates/plurxd/src/produce.rs), and
  [`cache.rs`](../crates/plurx-core/src/store/sqlite/cache.rs).
- Request normalization and idempotency:
  [`hls.rs`](../crates/plurxd/src/http/hls.rs) and
  [`transcode.rs`](../crates/plurxd/src/transcode.rs).
- Apple reopen state and request bodies:
  [`PlayerController.swift`](../clients/apple/Sources/PlayerController.swift)
  and [`Models.swift`](../clients/apple/Sources/Models.swift).
- Codec choice, client caps, HLS codec signaling, and delivered dynamic range:
  [`encoder.rs`](../crates/plurx-core/src/transcode/encoder.rs),
  [`stream.rs`](../crates/plurxd/src/http/stream.rs),
  [`playback/mod.rs`](../crates/plurx-core/src/playback/mod.rs), and
  [`hls.rs`](../crates/plurxd/src/http/hls.rs).
- Prior project evidence:
  [PHASE3-SPIKE.md](PHASE3-SPIKE.md),
  [PERF-PLAN.md](PERF-PLAN.md), and
  [ADAPTIVE-QUALITY.md](ADAPTIVE-QUALITY.md).

### Third-party evidence

- [FFmpeg Formats Documentation](https://ffmpeg.org/ffmpeg-formats.html),
  specifically the HLS `append_list` and `discont_start` behaviors.
- [HLS Authoring Specification for Apple Devices](https://developer.apple.com/documentation/http-live-streaming/hls-authoring-specification-for-apple-devices/),
  including Apple's HEVC/fMP4 and timeline-continuity requirements.
- Apple's [Preparing Audio for HTTP Live Streaming](https://developer.apple.com/documentation/http-live-streaming/preparing-audio-for-http-live-streaming),
  which makes audio priming an explicit authoring concern rather than a
  stall-beacon concern.

### Live evidence

The N3 probe used FFmpeg 8.1.2 and a synthetic 12-second H.264/AAC input. It
produced a four-second EVENT prefix, then invoked a new FFmpeg process at four
seconds with `-start_number 2` into the same directory. The exact result is in
R1 and R3.

No physical Apple-device HEVC claim was accepted as proven. Those tests remain
an implementation gate.

## 3. Findings

### R1 — Blocker: prefix continuation neither appends the playlist nor preserves the media timeline

N3's core recipe says to copy cached prefix segments and playlist entries into
a live session directory, then start FFmpeg at `-ss covered_ms` with
`-start_number K`. It calls the result one ordinary EVENT session, says the
publish gate can open immediately, and says there is no playlist splicing or
new client semantic
([PERF2-PLAN.md](PERF2-PLAN.md):465-574).

That recipe is false for the current writer.

The transcode HLS builder always writes one EVENT playlist with MPEG-TS
segments and hard-codes `-start_number 0`
([`mod.rs`](../crates/plurx-core/src/transcode/mod.rs):957-978). Merely making
the start number configurable does not make FFmpeg import the existing
playlist. FFmpeg documents `append_list` as the flag that appends old segments
to a newly produced list; N3 does not select it or define an equivalent
server-owned merge.

The server also assumes the FFmpeg playlist is the complete writer history.
`Manager::playlist` reads that file and only applies `served_live_playlist`
after retention starts
([`transcode.rs`](../crates/plurxd/src/transcode.rs):4410-4444). The helper's
concurrent restart branch deliberately returns the raw writer playlist when
the retained index and writer contents disagree
([`transcode.rs`](../crates/plurxd/src/transcode.rs):503-578). It is not a
prefix-plus-continuation owner.

The live probe reproduced the consequence:

```text
$ ffmpeg ... -hls_playlist_type event -start_number 0 prefix/index.m3u8
$ ffmpeg -ss 4 ... -hls_playlist_type event -start_number 2 prefix/index.m3u8

playlist after prefix process:
  #EXT-X-MEDIA-SEQUENCE:0
  #EXT-X-PLAYLIST-TYPE:EVENT
  seg00000.ts
  seg00001.ts
  #EXT-X-ENDLIST

playlist after continuation process:
  #EXT-X-MEDIA-SEQUENCE:2
  #EXT-X-PLAYLIST-TYPE:EVENT
  seg00002.ts
  seg00003.ts
  #EXT-X-ENDLIST

old seg00000.ts and seg00001.ts remained on disk but disappeared from playlist
```

The second process rewrote the playlist. A client that fetched `seg00001.ts`
before the rewrite sees a new playlist beginning at sequence 2 with a reset
timeline. A client that reloads between hydration and the rewrite may see two
different histories for the same session. Opening the current publish gate on
the hydrated playlist therefore exposes an unowned race, not a ready session.

The repository already has the missing conclusion. Phase 3 found that seeked
restart PTS begins near zero and audio DTS is discontinuous, and required
`EXT-X-DISCONTINUITY` at the boundary
([PHASE3-SPIKE.md](PHASE3-SPIKE.md):223-277). PERF's accepted failover
contract additionally specifies overlap, media sequence, discontinuity
sequence, and an owner fence
([PERF-PLAN.md](PERF-PLAN.md):1535-1590). The adaptive-quality design uses both
`-start_number n` and `-output_ts_offset` because these solve different
problems
([ADAPTIVE-QUALITY.md](ADAPTIVE-QUALITY.md):155-175).
Apple's authoring rules independently require an encoding-continuity break to
be marked, and require `EXT-X-DISCONTINUITY-SEQUENCE` on a live playlist that
can contain a discontinuity (items 8.13 and 8.17).

Required plan change:

1. Pick one playlist owner. Either a server-owned playlist stitches an
   immutable prefix to newly observed writer segments, or FFmpeg is started
   with an explicitly proven append contract. The raw writer must never replace
   a playlist already published to clients.
2. Specify the exact first continuation media sequence, URI, EXTINF, PTS/DTS
   policy, and `EXT-X-DISCONTINUITY` / `DISCONTINUITY-SEQUENCE` behavior.
3. Hydrate into staging, complete validation, establish the continuation
   owner, and atomically publish one playlist generation before the gate opens.
4. Test the race where a client fetches `K-1` before continuation publication
   and reloads during and after the first new segment.

Until that contract exists, “no playlist splicing” and “same restart contract
Phase 3 proved” must be removed.

### R2 — Blocker: prefix identity and publication can create an old-prefix/new-source hybrid

The proposed identity adds prefix length to `Recipe` and `covered_ms` to the
cache row. That distinguishes two coverages of the same stored recipe, but it
does not prove that the bytes FFmpeg opens at play time are the bytes that
produced the prefix.

Current `Recipe::hash` includes the database file id, size, and mtime
([`recipe.rs`](../crates/plurx-core/src/transcode/recipe.rs):81-145). That is a
useful invalidation boundary after a scanner updates the row. The play-start
availability cache, however, stores only “present” for 60 seconds. It calls
metadata but discards the live size and mtime
([`playstart.rs`](../crates/plurxd/src/playstart.rs):18-64). Session start then
hashes the database values and opens the path without comparing the live file
identity
([`transcode.rs`](../crates/plurxd/src/transcode.rs):3505-3554).

If a file is replaced in place after prefix production but before the library
row is refreshed, the cache lookup can return the old prefix while FFmpeg
continues from the new file. Full cached playback has a stale-cache exposure
too, but prefix hydration makes the corruption intra-stream: old bytes before
the seam, new bytes after it.

The proposed decision-facts memo is keyed by the same database
`(file_id, mtime)` and does not repair this exposure
([PERF2-PLAN.md](PERF2-PLAN.md):540-550). N3 should instead define an immutable
source snapshot for the entire hydrated session. Acceptable implementations
include a verified open-file identity held across hydration and encoder launch,
or a fresh stat tuple that must match the recipe before any hydrated playlist
is published.

The cache lifecycle is also underspecified:

- `serve_cached` starts an active-reader guard before returning a cache hit and
  holds it in the live Session
  ([`transcode.rs`](../crates/plurxd/src/transcode.rs):2410-2537). N3 promises
  the same guard but does not say whether hydration copies, reflinks, hardlinks,
  or symlinks, nor when the guard may be released.
- The producer currently publishes a completed VOD playlist assembled from
  parts. It writes `PLAYLIST-TYPE:VOD`, `MEDIA-SEQUENCE:0`, and `ENDLIST`
  ([`produce.rs`](../crates/plurxd/src/produce.rs):113-155). N3 must define how
  that artifact becomes an open EVENT playlist without exposing `ENDLIST`.
- Cache hit means `complete=1`
  ([`cache.rs`](../crates/plurx-core/src/store/sqlite/cache.rs):35-59). N3 says
  `complete=1` can also mean a partial-duration prefix. That changes the
  semantic of an existing field for every cache consumer, not just the schema.
- “Kill cache mid-hydration” cannot exercise eviction if the promised reader
  guard correctly pins the entry. The failure test must distinguish source
  deletion, forced cache corruption, and ordinary LRU eviction.

Required plan change:

1. Add an artifact kind or an equally explicit completeness contract. A full
   VOD and a playable prefix must not be distinguished only by
   `covered_ms < source_duration` inferred at use time.
2. Persist and verify boundary metadata: segment count, covered duration,
   playlist/container generation, and the source identity actually opened.
3. Define copy/link semantics and reader-guard lifetime.
4. Make hydration all-or-nothing. Any identity mismatch, missing segment,
   parse failure, or continuation-start failure before publication falls back
   to a fresh session with no prefix URI ever advertised.

### R3 — Blocker: zero stall beacons cannot establish audio or frame correctness at the seam

Every transcode creates a fresh AAC encoder
([`mod.rs`](../crates/plurx-core/src/transcode/mod.rs):933-938). A continuation
therefore creates a second priming boundary. The producer computes its resume
point by summing decimal EXTINF values
([`produce.rs`](../crates/plurxd/src/produce.rs):103-111), not by retaining the
last packet timestamp or the exact keyframe selected by the next encoder.
Apple's audio-preparation guidance specifically requires AAC priming samples
before the first video frame and describes separate timestamp mechanisms for
MPEG-TS and fMP4. A second encoder start is therefore a media contract, not an
implementation detail.

The packet probe showed the actual reset:

```text
$ ffprobe -show_packets seg00001.ts   # last prefix segment
first video PTS: 2.021333
first audio PTS: 2.026667

$ ffprobe -show_packets seg00002.ts   # first continuation segment
first video PTS: 0.021333
first audio PTS: 0.000000
```

`-start_number 2` did not offset either track. An HLS discontinuity can tell a
player to remap that timeline; it does not by itself prove there is no repeated
or dropped video, audible AAC priming gap, channel-layout change, or EXTINF
error. N3's proposed “zero stall beacons over 10 plays” can pass while every
play has a short click, silence gap, or duplicated frame.

Required plan change:

- Gate the seam with packet-level monotonicity/discontinuity assertions for
  both streams and decoded-frame/audio comparisons around the boundary.
- Include integer-frame-rate, 24000/1001, 30000/1001, VFR, audio-first,
  non-48-kHz, and multichannel fixtures.
- Define an allowed audio gap/overlap in samples and an allowed video
  gap/overlap in frames. “Player tolerated it” is not a correctness threshold.
- Run the packet gate on every enabled encoder family. The earlier plan already
  treats timestamp behavior as encoder-specific.
- Keep the player stall/error run as a separate integration gate after the
  media gate passes.

### R4 — Blocker: N1 does not pin one effective rate-control identity across live, producer, and offline work

The intended digest split is correct: existing VBR entries should retain the
current digest and QVBR entries must get a different digest. The proposed
placement does not make that invariant true.

`PipelineDigest` is manager-level today and feeds a constant VBR string
([`recipe.rs`](../crates/plurx-core/src/transcode/recipe.rs):50-73). N2's chosen
quality is per title/session. A manager-level `rate_control` string therefore
cannot encode the effective QVBR quality unless recipe construction accepts the
resolved options.

There are four recipe constructors in the current tree: cached live lookup,
speculative production, offline production, and a test helper
([`transcode.rs`](../crates/plurxd/src/transcode.rs):2430,2607,2694,6804).
Changing their strings independently creates a drift trap. The plan should
require one `effective_recipe(...)` builder whose input is the normalized,
validated `TranscodeOptions`, including output codec and effective rate
control.

The hot-setting contract creates a second identity race. The plan makes
`transcode.rate_mode` and quality live settings with a two-second snapshot, but
quality support is boot-probed. A runtime switch can activate a mode/value that
was never validated. Any fallback selected after hashing would cache one
encoding under another encoding's identity. Either make this setting
restart-required or re-probe and atomically publish a validated effective mode;
hash the effective result, never the requested value.

The durable offline path is the concrete failure case. An offline package may
yield and requeue during preparation
([`offline.rs`](../crates/plurxd/src/offline.rs):452-539). Each pass recomputes
the recipe, while `set_offline_package_recipe` only accepts the original hash
or a null hash
([`offline.rs`](../crates/plurx-core/src/store/sqlite/offline.rs):423-438). If a
hot setting changes between passes, the package fails instead of resuming its
original recipe. The offline package model stores source and track choices but
not the effective rate-control snapshot
([`domain.rs`](../crates/plurx-core/src/domain.rs):520-541).

Required plan change:

1. Define `EffectiveRateControl` after validation/fallback and feed it to one
   recipe builder used by live, producer, and offline paths.
2. Preserve the exact legacy VBR digest bytes. Add golden hash fixtures for the
   old mode and inequality tests for every effective QVBR value.
3. Persist the effective recipe inputs on offline-package creation and resume
   that snapshot even if settings change later.
4. Test same options across all three paths produce the same hash, and a change
   in effective mode or quality changes all three hashes.

N1 is at least medium effort once settings validation, offline persistence,
migration, and cross-path fixtures are included; “small, one sitting” is not a
credible execution budget.

### R5 — Blocker: `reopen_reason=stall` is not deterministic under request replay or queued Apple reopens

The desired behavior is reasonable: on one established-playback network stall,
retry the same delivery at one lower rung. The proposed API field is not enough
to produce that behavior safely.

The create body carries `playback_id`, `request_id`, and an optional requested
height but no previous session id or previous resolved rung
([`hls.rs`](../crates/plurxd/src/http/hls.rs):79-122). Auto height is resolved in
the HTTP layer before the manager's idempotency reservation
([`hls.rs`](../crates/plurxd/src/http/hls.rs):124-208). The request fingerprint
contains the resolved height but not the request id or playback id
([`transcode.rs`](../crates/plurxd/src/transcode.rs):1656-1721).

Consequences:

- The server cannot know whether “one rung down” means one rung below the
  client's prior actual session, below a newly sampled auto rung, or below the
  original policy rung.
- A transport retry of the same request can observe different prior-session
  state after the first attempt supersedes the old session. Re-normalizing can
  produce a different fingerprint and a 409 conflict, or step down twice.
- Two devices sharing a playback id need a binding stronger than “latest
  session for playback.”

Apple's local reopen machinery loses the cause as well. `PlayerReopenQueue`
stores only a pending millisecond position and collapses queued requests by
last writer wins
([`PlayerController.swift`](../clients/apple/Sources/PlayerController.swift):92-125).
`reopen(at:)`, `openAndDrain`, and the create body carry no cause
([`PlayerController.swift`](../clients/apple/Sources/PlayerController.swift):1063-1226;
[`Models.swift`](../clients/apple/Sources/Models.swift):467-489). A seek, audio
switch, subtitle change, growing-file reopen, or HDR compatibility retry that
races a stall can erase or accidentally inherit the stall reason.

The plan also calls the existing guard one-shot. It is one-shot only for the
immediate episode: `SameDeliveryStallRecoveryState` is reset after five seconds
of established playback
([`PlayerController.swift`](../clients/apple/Sources/PlayerController.swift):236-249,1757-1764).
The revised plan must name whether the budget is per episode, source session,
playback id, or title.

Required plan change:

1. Bind recovery to an explicit previous session id and validate that it belongs
   to the same user/playback/source. Read its resolved rung exactly once.
2. Normalize and persist the resulting target rung under `request_id` before
   superseding the previous session. A replay of the same request must return
   the same session and target without reapplying the step.
3. Carry a typed reopen cause through `PlayerReopenQueue`, `reopen`,
   `openAndDrain`, and `CreateSessionRequest`. Define cause-merging precedence.
4. State floor behavior, manual-height behavior, and the exact reset scope of
   the Apple recovery budget.
5. Add tests for a transport replay, queued seek during stall, audio/subtitle
   change during stall, silent/HDR recovery, two devices on one playback id,
   and a stall at the lowest rung.

### R6 — Blocker: N5.1 lacks an HEVC eligibility path and leaves a mandatory Apple container rule undecided

N5.1 says client caps already declare HEVC eligibility per play. They do, but
only on the decision endpoint. `Caps` is a query object for `GET /decision`
([`stream.rs`](../crates/plurxd/src/http/stream.rs):111-149). Neither
`CreateSession` nor `SessionRequest` carries caps, chosen output codec, or a
decision token
([`hls.rs`](../crates/plurxd/src/http/hls.rs):79-122;
[`transcode.rs`](../crates/plurxd/src/transcode.rs):1656-1680). A JIT session
cannot reproduce the alleged eligibility decision, and a predictive producer
has no client-cap context at all.

Codec is also not independent from encoder family in the current core.
`Encoder::video_codec` returns H.264 for every family and `encode_args` emits
family-specific H.264 flags
([`encoder.rs`](../crates/plurx-core/src/transcode/encoder.rs):20-39,128-177).
The exhaustive test asserts that every encoder is H.264
([`encoder.rs`](../crates/plurx-core/src/transcode/encoder.rs):548-597).
Recipe digesting derives codec from that family method
([`recipe.rs`](../crates/plurx-core/src/transcode/recipe.rs):60-73). Reusing the
existing serialized `codec` tag is possible, but the in-memory contract still
needs an explicit `OutputCodec` separate from `EncoderFamily`.

The container question is already resolved for the stated client gate. Apple's
HLS authoring specification item 1.5 requires HEVC video in fragmented MP4.
The plan's current JIT HLS builder emits MPEG-TS
([`mod.rs`](../crates/plurx-core/src/transcode/mod.rs):957-978). “MPEG-TS carries
HEVC fine; verify whether Apple requires fMP4” must become “HEVC JIT uses fMP4”
before implementation.

That change reaches more than muxer flags:

- The producer assembler moves numbered media segments and writes a new VOD
  playlist, but has no initialization-segment or `EXT-X-MAP` contract
  ([`produce.rs`](../crates/plurxd/src/produce.rs):113-155).
- Live and cached transcode Sessions hard-code
  `avc1.640034,mp4a.40.2`
  ([`transcode.rs`](../crates/plurxd/src/transcode.rs):2496,3739). HEVC bytes
  would be advertised as AVC.
- fMP4 resume must prove that every part uses a compatible initialization
  segment or publish a discontinuity and a new map at the boundary.
- Output codec must be in the request fingerprint, recipe, Session, response,
  cache metrics, and offline snapshot.

There is useful infrastructure to reuse. The server already serves `.m4s` and
initialization objects, and the copy path derives an exact `hvc1` codec string
from the init data
([`hls.rs`](../crates/plurxd/src/http/hls.rs):634-671,1472-1495). The plan should
make that exact derivation the transcode signaling contract rather than use a
hard-coded profile guess.

The stated SDR behavior is otherwise sound. Every transcode is deliberately
reported as SDR today
([`playback/mod.rs`](../crates/plurx-core/src/playback/mod.rs):334-360;
[`hls.rs`](../crates/plurxd/src/http/hls.rs):160-177), and an 8-bit BT.709 HEVC
output should retain that result. Keep the dynamic-range and badge tests, but
add independent output-codec assertions; “HEVC” is not a dynamic range.

Required plan change:

1. Add a typed output-codec selection to the create/manager contract, bind it
   to declared caps or an opaque decision token, and include it in the request
   fingerprint and persisted effective recipe.
2. Make fMP4 mandatory for Apple HEVC and define init-segment ownership across
   live, prefix, resumed producer, cache, and offline paths.
3. Derive master-playlist `CODECS` from produced media and fail closed on a
   mismatch.
4. Define what predictive production makes without a requesting client's caps.
5. Require validator, ffprobe, simulator, and physical-device gates before the
   feature can leave its default-off state.

### R7 — Blocker: the acceptance criteria are prose, and two named harness capabilities do not exist

The plan says every acceptance criterion is runnable and observable
([PERF2-PLAN.md](PERF2-PLAN.md):21-26). The critical criteria are neither.

For N1, `scripts/bench` explicitly says it does not decode pixels
([`scripts/bench`](../scripts/bench):18). It has no VMAF mode, reference
normalization, model selection, or dual-rate-control corpus command. “No
regression by VMAF” therefore cannot be run from the named harness.

For N4, the playback-lab document explicitly excludes WAN and congested-Wi-Fi
simulation and calls network shaping a separate fault-injection layer
([PLAYBACK-TESTING.md](PLAYBACK-TESTING.md):223-232). The script exposes steady,
seek, audio-switch, and subtitle-toggle operations; it has no 8 Mb/s to
1.5 Mb/s cliff. The acceptance criterion cites an existing feature that is not
there.

For N3, ten player runs and stall beacons do not inspect the packet seam. For
N5.1, “plays on Apple devices” does not identify the manifest validator, device
matrix, profile/level, codec string, init map, or pass/fail trace.

Required plan change: make harness slices predecessors, and give each gate an
exact command plus fixture and output contract. For example, the final plan may
define new commands with these shapes:

```text
scripts/bench rate-control --corpus testdata/perf2-corpus.json \
  --modes vbr,qvbr --vmaf-model vmaf_v0.6.1 --json out/rate-control.json

cargo test -p plurxd prefix_hydration_seam -- --nocapture
scripts/perf2-seam-probe --all-encoders --fixtures testdata/perf2-seams

scripts/playback-lab run --suite stall-recovery \
  --network-profile 8mbps-to-1.5mbps --json out/stall-recovery.json

scripts/perf2-hevc-validate --fixture testdata/perf2/hevc-sdr \
  --device-matrix docs/fixtures/perf2-apple-devices.json
```

Those commands need not keep these names. They must exist, fail nonzero on the
specified invariant, emit a stable artifact, and be exercised in the feature's
acceptance section. “Controller off is byte-identical” must also define which
stable bytes are compared after normalizing UUIDs, ports, wall-clock times, and
temporary paths.

### R8 — Should-fix: the proposed order and effort labels hide the real critical path

The macro-order puts N1 and N2 before broad prefix production. That part is
correct: do not fill a cache until the effective recipe is stable.

The remaining order should change:

1. Add the N1 measurement and identity harness before N1 changes.
2. Add an N3 seam spike before the N3 schema or producer rollout. It must select
   playlist ownership, timestamp policy, source identity, and atomic publish.
3. Land the N4 network-shaping harness and normalized retry contract before the
   controller.
4. Land an output-codec/container abstraction before N5.1. Because HEVC requires
   fMP4 and N3 consumes producer artifacts, make prefix hydration
   container-aware before producing HEVC prefixes.
5. Only then expand predictive fill and policy automation.

The effort/risk labels should be recalibrated:

| Slice | Plan label | Evidence-based label |
|---|---|---|
| N1 | Small / one sitting | Medium: settings, validation, digest, offline snapshot, three production paths |
| N3 | Medium/high | Highest / large: two-writer playlist and media-boundary protocol |
| N4 | Medium | Medium/large: server normalization, idempotency, Apple queue semantics, fault harness |
| N5.1 | Medium | Large: codec model, create API, fMP4 live/cache/offline, signaling, device matrix |

The plan also assigns telemetry schema version 16 in both N0 and N3. The stated
order makes that survivable in one coordinated branch, but independent slices
would conflict. Give each migration a unique version in the plan.

## 4. Required revision checklist

A revised plan is implementation-ready when all of the following are explicit:

- one component owns the published prefix-plus-continuation playlist;
- first continuation sequence, PTS/DTS, discontinuity, EXTINF, init-map, and
  publish-gate behavior are specified;
- the source opened by FFmpeg is proven identical to the prefix source;
- cache artifact kind, guard lifetime, atomic hydration, and fallback are
  specified;
- packet, decoded-video, decoded-audio, and client integration seam gates are
  runnable;
- one normalized effective recipe feeds live, producer, and offline paths;
- hot-setting validation/fallback happens before hashing and offline work pins
  its original effective recipe;
- stall recovery is bound to a previous session, normalized once under the
  idempotency key, and carries a typed cause through the Apple reopen queue;
- output codec is carried from eligibility decision through create,
  fingerprint, recipe, Session, response, playlist signaling, and metrics;
- Apple HEVC is fMP4, with init-segment behavior specified for every production
  and resume path;
- every critical acceptance item names a command that exists, an input fixture,
  a nonzero failure condition, and a stable output artifact;
- effort and order reflect the required harness and contract slices.

## 5. Checked and clean

The following claims survived adversarial checking and should be preserved:

- Cache lookup occurs before admission for current full-cache hits.
- Active cache-reader guards correctly pin a full cached Session for its
  lifetime.
- Recipe identity already includes persisted file id, size, mtime, encoder
  family, derived codec, pixel format, colorspace, muxer, segment policy, and
  the legacy VBR string.
- Preserving the exact legacy VBR digest while assigning QVBR a new digest is
  the correct migration intent.
- Current live, speculative producer, and offline paths all pass through
  `Recipe`; consolidating construction is feasible without a second cache key
  system.
- POST session creation already has a useful `request_id` reservation and
  fingerprint conflict model. N4 should extend it rather than add a parallel
  retry store.
- Apple's buffering and silent-stall predicates are distinct, and the current
  same-delivery retry is bounded within an episode.
- `.m4s`, init-object serving, and exact `hvc1` derivation already exist in the
  copy path and are appropriate building blocks for HEVC transcode output.
- Keeping 8-bit BT.709 HEVC transcodes labeled SDR is correct; codec and dynamic
  range should remain separate response/UI fields.
- N1/N2 before predictive prefix fill is the right dependency direction.
- Default-off controls, conservative fallbacks, and the “no required external
  service” boundary are appropriate guardrails.

## 6. Probe transcript and reproducibility note

The following is the condensed transcript of the N3 mechanism probe. The
commands intentionally matched plurx's current EVENT/MPEG-TS path, including
zero mux delay and a second input seek.

```text
ffmpeg version 8.1.2

# Generate a deterministic 12 s H.264/AAC input.
ffmpeg -f lavfi -i testsrc2=size=640x360:rate=30 \
  -f lavfi -i sine=frequency=1000:sample_rate=48000 -t 12 \
  -c:v libx264 -g 60 -keyint_min 60 -sc_threshold 0 \
  -c:a aac -b:a 128k input.mp4

# Prefix process: two 2 s segments.
ffmpeg -i input.mp4 -t 4 -c:v libx264 -g 60 -keyint_min 60 \
  -sc_threshold 0 -c:a aac -muxdelay 0 -muxpreload 0 \
  -f hls -hls_time 2 -hls_playlist_type event \
  -hls_segment_filename 'session/seg%05d.ts' -start_number 0 \
  session/index.m3u8

# Continuation process: N3's proposed seek and numbered restart.
ffmpeg -ss 4 -i input.mp4 -c:v libx264 -g 60 -keyint_min 60 \
  -sc_threshold 0 -c:a aac -muxdelay 0 -muxpreload 0 \
  -f hls -hls_time 2 -hls_playlist_type event \
  -hls_segment_filename 'session/seg%05d.ts' -start_number 2 \
  session/index.m3u8

observed:
  - index.m3u8 contained only seg00002.ts onward after restart
  - old prefix segment files remained unreferenced on disk
  - seg00001 first video/audio PTS: 2.021333 / 2.026667
  - seg00002 first video/audio PTS: 0.021333 / 0.000000
```

This probe establishes a mechanism failure, not universal production quality.
The revised plan still needs the cross-encoder and physical-device matrix
listed above.
