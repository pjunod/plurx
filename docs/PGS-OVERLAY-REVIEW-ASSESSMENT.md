# PGS overlay review assessment — accepted findings and requested re-review

**Status:** adjudication complete; Milestone 0 authorized · **Responds to:**
[`PGS_OVERLAY_PLAN_REVIEW.md`](https://github.com/pjunod/plurx/blob/docs/pgs-overlay-review/docs/PGS_OVERLAY_PLAN_REVIEW.md)
at `5262e791` · **Verified against:** `origin/main` at `452bcf4c` ·
**Written:** 2026-08-04

Companion to [PGS_OVERLAY_PLAN.md](PGS_OVERLAY_PLAN.md), which proposes the
Dolby Vision-safe PGS overlay architecture. This assessment adjudicates the
review findings before the plan is revised. Reviewers should read §1 first,
then confirm or challenge the requested dispositions in §9. The architecture
has since been approved for feasibility work only; live evidence is tracked in
[PGS-OVERLAY-M0-FEASIBILITY.md](PGS-OVERLAY-M0-FEASIBILITY.md). This document
does not authorize production rollout.

## 1. Verdict — approve the architecture and revise the contract

The review materially improves the plan. Its central conclusion is correct:
the architecture should remain server-decoded PGS compositions delivered as
immutable images and rendered by Apple and Android outside the encoded video.
Nothing in the review justifies returning to subtitle burn-in, OCR, or separate
client-side PGS parsers.

The plan should be revised before implementation, but the review should not be
accepted verbatim. It contains:

- one foundational blocking defect that must be fixed on the wire;
- several valid client, cache, and lifecycle corrections;
- valuable additions for fades, color, HDR legibility, and cold images;
- conclusions that should remain Milestone 0 experiments rather than become
  design mandates;
- findings made stale by six commits that landed after the review branch
  diverged from `main`;
- two factual errors in its version and API-error inventory.

The requested disposition is:

> **Architecture approved · plan revision required · review accepted with
> amendments.**

The media-origin protocol change in §4.1 is the only finding that must land
before either client overlay renderer begins. The other accepted findings must
be reflected in the revised plan and its milestone acceptance checks.

## 2. Review baseline — the response predates current `main`

The response branch is rooted at `448306d1`. The current remote mainline is
`452bcf4c`, six commits later. Those commits include the Apple playback-menu
lifetime fix, deployment changes, and most importantly the HDR subtitle guard
merged in PR #51.

That history changes how two findings must be read:

1. The review's description of automatic PGS burn-in on HDR was correct for
   its branch point but is no longer current behavior.
2. Build numbers read from the review branch do not describe the release train
   on current `main` or the active branding branch.

The response says it was verified against "the current working tree." That is
true only for its branch point. Re-review must compare behavioral claims with
the implementation branch's eventual merge base.

## 3. Adjudication summary

| Review topic | Disposition | Required action |
|---|---|---|
| True media origin | Accept as blocking | Add origin to HLS and progressive responses before client overlay work. |
| HDR/DV burn policy | Stale, retain useful matrix additions | Describe current guard behavior and old-server/forced-track outcomes. |
| Extraction coordination | Accept | Reuse existing single-flight, memo, deadline, and run-to-completion semantics. |
| Cache storage | Accept problem, amend solution | Use a byte-budgeted generated-artifact cache; do not bind blindly to transcode tables. |
| Apple layer hierarchy | Accept | Restructure the player surface into video and overlay children. |
| Apple cue clock | Experiment required | Compare boundary observers and `AVSynchronizedLayer` in Milestone 0. |
| Apple PiP/AirPlay | Accept direction | Disclose at selection time; observe external playback; verify on hardware. |
| Android presentation axis | Accept as blocking design correction | Separate video transport from subtitle presentation and test reopen rules. |
| Android timeline epoch | Accept | Key timers, requests, and visible state to the current item/session epoch. |
| Android PiP | Amend certainty | Expect overlay inclusion, but verify with `SurfaceView` and tunneling. |
| Android tunneling/video rect | Accept | Retain `SurfaceView`; include sample aspect ratio and physical-TV timing. |
| Fade/color/HDR legibility | Accept | Add measured fixtures and explicit conversion/legibility acceptance. |
| Generation and late images | Accept policy gap | Define bounded refresh and cold-image behavior before clients diverge. |
| Error variants | Partially incorrect | Reuse existing 422; explicitly add or map 415 and 503. |
| Version correction | Reject as stale/incorrect | Read versions when implementation branches are created. |

## 4. Accepted blocking findings

### 4.1 True media origin must cross the API boundary

This is the strongest finding in the response.

Copy sessions seek to the keyframe preceding the requested start. FFmpeg then
normalizes that keyframe to local time zero. The server already probes the real
source origin and uses it to shift WebVTT cues, but
[`StartResponse`](../crates/plurxd/src/http/hls.rs) exposes only the requested
`start_seconds`.

Both clients currently derive source position from that requested value:

```text
reported source time = player-local time + requested start
```

For copied video, the correct equation is:

```text
source time = player-local time + actual media origin
```

The difference can be a full GOP. A cue protocol using the current client
clock can therefore display PGS seconds early after a seek even if the renderer
itself is accurate to a frame.

**Required contract change:**

- add an optional `media_origin_seconds` or integer `media_origin_ms` to the
  HLS session start response;
- have Apple and Android prefer it and fall back to `start_seconds` for old
  servers;
- expose the equivalent origin for progressive remux seeks, either in a
  response header or a small metadata response available before attachment;
- add a seek-mid-GOP test with a cue spanning the requested start;
- use the corrected origin for progress and resume reporting as well as PGS.

**Acceptance check:** start a copied session at a non-keyframe position and
prove that local time zero maps to the probed keyframe origin on both clients.
The PGS cue and the matching WebVTT fixture must agree with the picture.

### 4.2 Android must separate transport from subtitle presentation

The review correctly identifies a modeling problem in
[`SubtitlePolicy.kt`](../clients/android/app/src/main/java/tv/plurx/app/player/SubtitlePolicy.kt).
`SubtitleDelivery` currently combines two questions:

1. What transports the video?
2. What presents the subtitle?

An application overlay answers only the second question. Adding `Overlay` as a
peer of `Plan`, `NativeSession`, and `Burn` would make ordinary overlay changes
look like transport changes and would reopen playback, violating the plan's
core no-restart requirement.

The revised plan must specify either orthogonal state:

```text
VideoTransport = Plan | NativeSession | BurnSession
SubtitlePresentation = None | PlayerText | AppOverlay | Burned
```

or an explicit routing table with equivalent behavior. In either form:

- Plan ↔ overlay is reopen-free;
- NativeSession ↔ overlay is reopen-free;
- overlay A ↔ overlay B is reopen-free;
- overlay ↔ off is reopen-free;
- leaving a burn session requires one reopen because the old frames contain
  subtitle pixels;
- overlay selection never writes `subtitle_burn` or changes the session body.

**Acceptance check:** a JVM routing matrix proves every transition above and
records the one legitimate burn-escape reopen.

### 4.3 Every client operation needs a timeline epoch

The response is right that a cue scheduler cannot safely use only a selection
identifier. Player items and sessions are replaced during seeks, audio and
quality changes, recovery, and compatibility fallback.

The revised plan must define a monotonically increasing timeline epoch. The
following belong to an epoch:

- current media origin;
- active player item or Media3 media item;
- manifest preparation response;
- image fetches and decodes;
- boundary timers or synchronized animations;
- currently displayed composition.

Any completion from an old epoch is discarded. On an epoch change, the client
clears the old composition, binds the new origin, reconciles the cue at the
new source position, and schedules the next boundary.

**Acceptance check:** delayed manifest and image completions from an old item
cannot display after a seek or session replacement.

## 5. Accepted server corrections

### 5.1 Reuse extraction semantics, not duplicated implementations

[`subtitles.rs`](../crates/plurxd/src/subtitles.rs) already supplies the hard
concurrency semantics the plan described abstractly:

- one extraction owner per cache key;
- waiters joining the same work;
- detached warming that continues if a client stops waiting;
- a 600-second extraction deadline;
- 120-second negative failure memoization;
- a bounded negative-memo registry;
- temporary-file publication followed by atomic rename;
- abandoned temporary-artifact cleanup.

The PGS producer should reuse or generalize those tested semantics. Client
deselection cancels polling and presentation work; it does not cancel a server
extraction already producing a cache generation. Run-to-completion is useful
because the next request receives the warm result.

The PGS path also needs a server-wide extraction semaphore. PGS preparation
may combine a NAS scan, RLE expansion, and many PNG encodes. Without a cap,
several cold tracks can saturate storage and memory at once.

**Acceptance check:** concurrent requests for one generation start one
producer, client cancellation does not leave a partial published generation,
and requests beyond the configured extraction capacity receive the documented
response.

### 5.2 Use a byte-budgeted generated-artifact cache

The review correctly rejects a count-only cache for PGS. A limit such as
256 MiB per track makes 256 count-bounded entries operationally unsafe.

The recommended revision is narrower than the response's specific database
prescription:

1. Generalize the existing claim/complete/LRU pattern into generated-artifact
   cache classes, or implement an equivalent PGS artifact class.
2. Give PGS a declared byte budget and eviction policy.
3. Keep its budget separate from completed transcodes unless the release owner
   explicitly wants images to evict video transcodes.
4. Publish a generation only after its manifest and every referenced object
   validate.
5. Reap abandoned temporary generation directories.

The existing transcode-cache machinery is a useful pattern. Its tables and
budget should not automatically become the storage contract for a different
artifact class.

**Acceptance check:** an over-budget sweep removes the least-recently-used
complete PGS generations without serving an incomplete directory or evicting
an unrelated cache class accidentally.

### 5.3 Tighten the route and error contract

The following review corrections should be accepted:

- use one dedicated `is_pgs_overlay_subtitle` classifier in
  [`tracks.rs`](../crates/plurx-core/src/tracks.rs), so VobSub and XSUB do not
  enter a PGS-only route through the broader bitmap classifier;
- define behavior when source duration is unknown;
- say "authenticated" rather than imply a per-user file ACL that does not
  exist;
- state that manifests never mint token-bearing object URLs while retaining
  the server's existing query-token compatibility where other surfaces need
  it;
- document 202 polling as a new overlay-API behavior rather than an existing
  subtitle convention.

The response is wrong that the API has no 422 response. `ApiError` already has
an `Unprocessable` variant in
[`error.rs`](../crates/plurxd/src/http/error.rs). The revised plan should say:

- reuse 422 for malformed or limit-exceeding PGS, with an appropriate error
  body;
- explicitly add or map 415 if unsupported overlay media needs that status;
- explicitly add or map 503 for extraction capacity;
- retain the existing JSON error envelope.

## 6. Accepted client integration corrections

### 6.1 Apple needs a real overlay host

[`PlayerSurfaceView`](../clients/apple/Sources/PlayerSurface.swift) uses
`AVPlayerLayer` as its backing layer. The plan's phrase "a sibling above the
AVPlayerLayer" is not implementable without changing that structure.

The revised design should use a parent player-surface view containing:

```text
PlayerSurfaceView
├── video view / AVPlayerLayer
└── transparent PGS overlay view or layer
```

The overlay remains noninteractive, clipped to the displayed video rectangle,
and below SwiftUI controls. It is cleared and rebound with the timeline epoch.

### 6.2 Apple PiP and AirPlay limitations need explicit state

iOS currently allows automatic PiP on backgrounding. A warning cannot reliably
wait until a manual PiP button is pressed. If a PGS overlay is selected, the
limitation should be disclosed at selection time or in a persistent diagnostic.

The PiP content source is the `AVPlayerLayer`; the product should assume the
separate application overlay is absent from PiP until hardware evidence proves
otherwise. tvOS PiP is currently disabled in this app and should not appear in
the validation matrix as a supported path.

AirPlay requires observation of external playback state. When video leaves the
device but the overlay remains local, the client must hide the local overlay
and report that PGS is not visible on the external output. It must not start an
SDR burn automatically.

### 6.3 Android must keep the HDR-capable surface path

The review correctly calls out tunneled playback as the mainline Android TV
path. The overlay must not prompt a switch from `SurfaceView` to `TextureView`.
Android's Media3 guidance prefers `SurfaceView` for accurate timing, HDR, secure
content, power use, and full-resolution TV output.

The visible video rectangle must be calculated from container bounds and
Media3 `VideoSize`, including `pixelWidthHeightRatio`. Subtitle canvas content
does not receive TV chrome safe-area padding, and the overlay must be
non-focusable so it cannot intercept D-pad seeking.

The existing 500 ms UI position update must not drive cue timing. A separate
boundary scheduler reconciles against the authoritative source position.

## 7. Findings that remain Milestone 0 decisions

### 7.1 Apple boundary observers versus `AVSynchronizedLayer`

The response recommends making boundary observers primary and demoting
`AVSynchronizedLayer`. That is plausible, but not established strongly enough
to become a pre-implementation mandate.

Apple documents both mechanisms:

- boundary observers invoke application code when player time crosses named
  points;
- `AVSynchronizedLayer` provides a Core Animation subtree whose time follows an
  `AVPlayerItem` and is explicitly intended for timed visual effects.

Item replacement requires reconciliation and rebinding in either design. The
correct choice depends on behavior under the app's frequent item changes,
backgrounding, HLS discontinuities, pause/rate changes, and large cue sets.

Milestone 0 should put both behind one cue-scheduler interface and measure:

- onset and clear error;
- seek reconciliation;
- item replacement;
- background/foreground recovery;
- HLS discontinuity behavior;
- layer/timer count and memory;
- implementation complexity and testability.

Choose the primary only after that evidence. The common contract is more
important than guessing the winning primitive in the plan.

### 7.2 Android PiP is likely, not guaranteed

Android PiP pins the activity window, and the current Compose chrome disappears
only because the app explicitly hides it. The expected outcome is therefore
that a Compose PGS overlay placed outside those guards remains visible.

The response overstates that expectation as certainty. The production path
uses `SurfaceView`, Compose interop, and tunneled playback on televisions.
Milestone 0 must verify the whole combination, including PiP scaling and
seamless resize. The plan should change its expected outcome, not remove the
physical test.

### 7.3 Cache durability should match a regenerable artifact

The response recommends fsyncing the manifest and directory before rename.
That may be justified, but existing caches do not fsync and PGS output is
regenerable rather than authoritative data.

Milestone 0 should compare:

- temp directory plus atomic rename;
- object hash and manifest-reference validation on read;
- delete-and-rebuild behavior after corruption;
- the latency and NAS/local-filesystem cost of fsync.

The plan must state the chosen durability contract. It should not inherit fsync
merely because the original wording was vague.

## 8. Valuable additions to absorb

### 8.1 Fade-heavy PGS can defeat naïve deduplication

Palette-only fade updates may create many visually distinct compositions in a
short interval. Complete-composition snapshots and content-addressed PNGs do
not deduplicate frames whose alpha or palette changes every video frame.

Milestone 0 must include a fade-heavy fixture and measure:

- display-set changes per second;
- manifest bytes;
- object count and encoded bytes;
- server encode time;
- client decode and memory cost.

Do not choose snap-to-final, perceptual coalescing, or an alpha-ramp schema
before measurement. The manifest must have client-side byte and cue-count
bounds regardless of the selected fade policy.

### 8.2 Color conversion must be a contract

PGS palette values require a defined YCbCr-to-RGBA conversion. The revised plan
must specify:

- how matrix and range are selected from reliable source/container metadata;
- the fallback when metadata is absent;
- sRGB treatment of emitted PNGs;
- straight-alpha encoding and platform decode expectations;
- fixture comparison against FFmpeg rendering;
- a semi-transparent cue test on both platforms.

Canvas dimensions alone should not silently choose a matrix unless the parser
specification requires that fallback and the fixture corpus validates it.

### 8.3 SDR UI over HDR video needs a legibility test

PGS images are application UI composed above HDR video. Their reference white
and the video's HDR highlights may be handled differently by Apple EDR and
Android display composition. A technically correct white subtitle may appear
dim over a bright Dolby Vision scene.

Milestone 0 must validate authored white, gray, and semitransparent subtitles
over bright DV/HDR highlights on every target device class. Version 1 should
preserve authored color by default, while recording luminance adjustment as a
future option if hardware evidence shows a product problem.

### 8.4 Generation expiry needs one bounded recovery

If an object returns 404 because the source changed or its generation was
evicted, the client should:

1. invalidate the local generation;
2. refetch the manifest once;
3. continue if a new valid generation is returned;
4. otherwise turn the overlay off with a diagnostic;
5. never loop manifest and object requests indefinitely.

Client image caches are keyed by `(generation, sha256)` and bounded by bytes.
Authorization-bearing object fetches must not rely on a shared platform URL
cache as the correctness mechanism.

### 8.5 Cold images require a shared presentation rule

The plan's 150 ms post-seek target is achievable only when the active image is
resident or immediately available. The revised requirement must distinguish:

- **resident cue:** render within the timing target;
- **cold cue:** show a preparation state or no overlay until decoded;
- **scrubbing:** clear the overlay until the final target is known;
- **late short cue:** apply one cross-platform skip/show rule rather than flash
  briefly and differently on each client.

The response's 50-percent remaining-duration threshold is a useful test value,
not an approved product constant. Milestone 0 should compare it with a minimum
remaining-time rule using representative dialogue cues.

## 9. Stale or incorrect findings

### 9.1 The HDR subtitle guard is current behavior

The response says the plan introduces a new invariant by refusing to silently
turn DV/HDR into SDR for a burned subtitle. That was true at its branch point.
It is not true on current `main`.

PR #51 added the guard to Apple, Android, and web policy. Current clients
refuse burn-only subtitles while DV, HDR10, or HLG is on the wire and show a
notice instead of replacing the video with SDR.

The plan should still add:

- new-client/old-server/forced-PGS compatibility cases;
- forced-track overlay-preparation failure behavior;
- a distinction between automatic and explicit user-requested SDR burn;
- web scope wording.

Those are policy completeness additions, not evidence that the preservation
invariant is being smuggled into this feature.

### 9.2 Version numbers must be read at branch creation

The response says the Apple build was 16, but its committed branch contains
Apple build 17. It correctly observed Android version code 8 on that old branch.
Current `origin/main` is Apple build 19 and Android version code 9, while the
active branding branch has already advanced further.

The revised plan should remove exact assumed starting values. Each Apple and
Android implementation branch must:

1. read the version at branch creation;
2. increment the build/version code at least once for its user-visible change;
3. keep the marketing version aligned across clients;
4. let the release owner decide whether the release train advances from 0.2.2.

### 9.3 A 422 API error already exists

[`ApiError::Unprocessable`](../crates/plurxd/src/http/error.rs) already maps to
HTTP 422. The response's combined statement that 415, 422, and 503 are all
absent is false.

The useful remainder stands: 415 and 503 need explicit variants or mappings,
and the route contract should state whether malformed PGS uses the existing
422 body shape or adds a dedicated structured error.

## 10. Decisions requested from re-review

Please confirm or challenge each item:

1. **Architecture:** server-decoded complete PGS compositions plus immutable
   images remains approved.
2. **Timing:** true media origin is an additive server/client prerequisite,
   including progressive remux.
3. **Android state:** overlay is an orthogonal subtitle-presentation axis and
   does not reopen video.
4. **Cache coordination:** existing extraction-flight semantics are reused or
   generalized, with a separate byte-budgeted PGS artifact class unless a
   shared eviction budget is explicitly approved.
5. **Apple clock:** boundary observers and `AVSynchronizedLayer` remain
   Milestone 0 candidates behind one interface.
6. **Output modes:** Apple PiP/AirPlay are assumed not to carry the overlay;
   Android PiP is expected to carry it, with both proven on hardware.
7. **Policy baseline:** the revised plan describes PR #51's HDR subtitle guard
   as current behavior rather than a new PGS-overlay policy.
8. **Color and fades:** measured fixtures decide conversion fallback, fade
   representation, cue limits, and cache limits.
9. **Cold images:** timing targets are scoped to resident objects and one
   shared late-cue rule is chosen from measurements.
10. **Versioning:** exact build numbers are read and incremented when separate
    implementation branches are created.

## 11. Requested re-review outcome

The requested response should classify this assessment as one of:

- **Accepted:** the dispositions above can be folded into the plan, after
  which Milestone 0 may begin;
- **Accepted with named exceptions:** list only the disputed decision numbers
  from §10 and the alternative contract proposed for each;
- **Rejected:** identify a defect that invalidates the server-image/client-
  overlay architecture, not merely an implementation preference.

Implementation remains paused until the plan is revised and the media-origin
contract, cache class, client scheduling experiment, and output-mode acceptance
matrix have an agreed owner and observable completion criteria.

## 12. Primary platform references

- Apple,
  [`AVPlayer`](https://developer.apple.com/documentation/avfoundation/avplayer):
  boundary time observers and synchronized custom visual presentation.
- Apple,
  [`AVSynchronizedLayer`](https://developer.apple.com/documentation/avfoundation/avsynchronizedlayer):
  Core Animation timing derived from an `AVPlayerItem`.
- Apple,
  [`AVPictureInPictureController.ContentSource`](https://developer.apple.com/documentation/avkit/avpictureinpicturecontroller/contentsource-swift.class):
  PiP content sourced from an `AVPlayerLayer` or sample-buffer layer.
- Android,
  [Use Picture in Picture](https://developer.android.com/develop/ui/views/picture-in-picture):
  activity-based PiP and overlay-UI lifecycle guidance.
- Android,
  [Media3 surface types](https://developer.android.com/media/media3/ui/surface):
  `SurfaceView` advantages for timing, HDR, secure output, and television
  resolution.
- Android,
  [`setTunnelingEnabled`](https://developer.android.com/reference/androidx/media3/exoplayer/trackselection/DefaultTrackSelector.Parameters.Builder):
  tunneled playback behavior and the requirement for manual device testing.
