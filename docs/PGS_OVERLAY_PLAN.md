# Dolby Vision-Safe PGS Subtitle Overlay Plan

> Status: Architecture approved for Milestone 0 feasibility work. Production
> implementation and rollout remain paused until the milestone is accepted.
> Current evidence: [PGS-OVERLAY-M0-FEASIBILITY.md](PGS-OVERLAY-M0-FEASIBILITY.md).
>
> Scope: Apple and Android playback clients, the plurxd subtitle service, and
> the playback policy that selects between text renditions, bitmap overlays,
> and legacy burn-in.

## 1. Executive summary

plurx currently has two subtitle delivery paths:

1. Text subtitles that can be represented as WebVTT are delivered as a native
   HLS subtitle rendition.
2. Bitmap subtitles, including Blu-ray PGS, are burned into a transcoded video.

The second path is why PGS subtitles and Dolby Vision cannot currently be used
together without a quality tradeoff. Selecting a PGS track asks the server to
decode the video, composite the subtitle pixels, and encode a new H.264 SDR
stream. That result can contain the visible subtitles, but it is no longer the
original Dolby Vision presentation.

This proposal adds a third path: **PGS bitmap overlay**.

The server will demux and decode PGS display sets into transparent image
objects and expose a source-timeline manifest. Apple and Android will draw
those objects above the video at the authored positions and times. The video
stream itself will remain untouched, so a direct-play or remuxed Dolby Vision
stream can continue through the existing hardware video path.

The proposal does not claim that PGS becomes an HLS-native subtitle format.
It does not. Apple HLS subtitle renditions are text formats, and Android
Media3's HLS subtitle list likewise does not include PGS. The new path is a
plurx application overlay synchronized to playback.

The recommended design is:

- retain the existing WebVTT text-rendition path without modification;
- add a versioned PGS overlay manifest and immutable PNG object API;
- use the source media timeline as the API timebase;
- render the overlay outside the video plane on both native clients;
- never silently replace Dolby Vision/HDR with SDR when overlay preparation
  or rendering fails;
- keep legacy burn-in only for clients and outputs that cannot render overlays;
- validate Dolby Vision, timing, Picture in Picture, and external playback on
  physical devices before enabling the feature by default.

This is a cross-server, cross-client feature. It should not be implemented as
two independent client-side PGS decoders. Centralizing the PGS bitstream parser
on the server produces one security boundary, one interpretation of display
sets, one set of fixtures, and a much smaller client contract.

## 2. Decision requested

Reviewers are asked to approve the following architectural decision:

> Decode PGS bitmaps on the plurxd server, deliver transparent cue images and
> source-relative timing in a versioned manifest, and composite those images
> in the Apple and Android clients without modifying the video stream.

Approval of this document authorizes implementation planning. It does not
authorize a production rollout, removal of burn-in, or a claim of Picture in
Picture/AirPlay support. Those require the acceptance evidence defined below.

The main decisions that need explicit reviewer answers are collected in
[Section 22](#22-reviewer-decision-log).

## 3. Terminology

The word "native" has been used in existing plurx code and documentation to
mean "an HLS subtitle rendition handled by the platform player." That wording
is accurate for WebVTT but is ambiguous when discussing client-side bitmap
composition. This proposal uses more precise terms.

| Term | Meaning in this document |
| --- | --- |
| Text rendition | WebVTT delivered as an HLS subtitle rendition. |
| PGS | Blu-ray Presentation Graphic Stream bitmap subtitles. |
| Bitmap overlay | Transparent subtitle images drawn by the plurx client. |
| Burn-in | Subtitle pixels composited into decoded video before encoding. |
| Source time | Absolute time in the source media timeline. |
| Item time | Time in the currently playing AVPlayer/Media3 item. |
| Video range | Direct play, remux, or transcode range selected by policy. |
| DV | Dolby Vision. |
| HDR | High dynamic range, including HDR10 and Dolby Vision. |

The existing text rendition is still a platform-native subtitle experience.
The proposed bitmap overlay is native application UI, but not a native HLS
subtitle rendition.

## 4. Current behavior

### 4.1 Track classification

The server currently exposes text-related track attributes through
[tracks.rs](../crates/plurx-core/src/tracks.rs). SubRip/SRT and WebVTT tracks
are classified as text and can be represented by the HLS WebVTT path.

`hdmv_pgs_subtitle` is a bitmap codec. It is not classified as text and it is
not classified as HLS-native. That classification is correct and should remain
correct after this work.

### 4.2 Text subtitle delivery

The existing extraction and cache in
[subtitles.rs](../crates/plurxd/src/subtitles.rs) produce WebVTT. The HTTP
surface in [stream.rs](../crates/plurxd/src/http/stream.rs) and
[hls.rs](../crates/plurxd/src/http/hls.rs) exposes file and HLS-session text
subtitle routes.

The important properties of this path are:

- it is text-only;
- it is compatible with the platform HLS player;
- it is cached and coalesces duplicate extraction work;
- it does not need to change for the PGS proposal.

### 4.3 PGS delivery

PGS currently reaches the viewer through the burn-in transcode path. The
server scales the subtitle canvas and composites it with the video in an
FFmpeg filter graph. Because the subtitle pixels become video pixels, the
server must encode a new video stream.

The current transcode output is H.264 8-bit SDR. Selecting PGS can therefore
change all of the following at once:

- direct or remuxed video becomes a transcode;
- the original Dolby Vision bitstream is no longer delivered;
- HDR is tone-mapped to SDR;
- playback may restart in a new session/range;
- server GPU/CPU resources are consumed for video encoding.

This behavior is documented today in
[PLAYBACK.md](PLAYBACK.md) and the text-subtitle boundary is described in
[APPLE-NATIVE-SUBTITLES-HANDOFF.md](APPLE-NATIVE-SUBTITLES-HANDOFF.md).

### 4.4 Client timeline mapping

Both clients already distinguish player-local time from source time.

On Apple,
[PlayerController.swift](../clients/apple/Sources/PlayerController.swift)
computes the real position as the current player time plus a base offset. On
Android,
[Controller.kt](../clients/android/app/src/main/java/tv/plurx/app/player/Controller.kt)
performs the same mapping.

That existing model is the right basis for an overlay protocol. Cue times can
remain stable in source time across direct play, remux, live HLS windows,
seeks, and session replacements.

## 5. Why Dolby Vision and PGS conflict today

PGS is a sequence of bitmap composition instructions, not text. It contains
palette, object, window, and display information that ultimately describes
pixels positioned on an authored video canvas.

A player has three broad options:

1. understand PGS and draw it above the video;
2. convert it to another subtitle format without losing required semantics;
3. decode the video and permanently composite the PGS pixels into new video.

plurx currently uses option 3. Re-encoding changes the video essence. Dolby
Vision includes dynamic metadata associated with the graded video, so a
generic runtime H.264 SDR encode cannot preserve the original Dolby Vision
presentation.

There are professional workflows for adding subtitles to a finished Dolby
Vision master. Those are controlled post-production and authoring workflows,
not a drop-in runtime media-server operation. They should not be confused with
preserving an already-authored Dolby Vision stream byte-for-byte while playing
consumer media.

The safe runtime rule is therefore:

> If Dolby Vision/HDR preservation is required, subtitle pixels must stay out
> of the encoded video path unless plurx has a separately designed and
> validated HDR/Dolby Vision authoring pipeline.

This plan deliberately does not propose such an authoring pipeline.

## 6. Goals and non-goals

### 6.1 Goals

The first version must:

- display PGS subtitles on Apple and Android clients;
- preserve the already-selected video range and video bytes;
- preserve Dolby Vision/HDR when the device and current video path support it;
- preserve authored subtitle placement and multiple-object compositions;
- keep video startup independent from cold subtitle preparation;
- support selecting, replacing, and disabling PGS without a playback restart;
- use one server contract and equivalent behavior on both clients;
- handle seeks, discontinuities, playback-rate changes, and item replacement;
- fail without silently degrading the video to SDR;
- be observable enough to diagnose extraction, timing, and rendering failures;
- coexist with old servers and old clients during a staged rollout.

### 6.2 Non-goals for version 1

The following are explicitly out of scope:

- OCR conversion of PGS to text;
- ASS/SSA, VobSub, DVD subpictures, or arbitrary image subtitle formats;
- Web, Roku, or other client implementations;
- user-controlled PGS font, color, style, or position changes;
- offline subtitle downloads;
- Dolby Vision metadata regeneration or runtime Dolby Vision authoring;
- guaranteed Picture in Picture, AirPlay, casting, or external-display overlay
  support before the physical-device spike is complete;
- replacing the existing WebVTT text rendition;
- removing the legacy burn path.

## 7. Requirements

### 7.1 Functional requirements

| ID | Requirement |
| --- | --- |
| F1 | A PGS track can be selected during active playback. |
| F2 | Selection does not create or restart a video session. |
| F3 | The complete active PGS composition is visible at each cue time. |
| F4 | Explicit clear display sets remove the prior composition. |
| F5 | Multiple objects in one display set render together. |
| F6 | Authored coordinates are preserved relative to the video canvas. |
| F7 | Seeking immediately reconciles the overlay with the target time. |
| F8 | Turning subtitles off immediately clears the overlay. |
| F9 | Switching tracks cancels preparation and display for the old track. |
| F10 | Playback begins even if the PGS cache is cold. |
| F11 | Existing text subtitle selection remains unchanged. |

### 7.2 Quality requirements

| ID | Requirement |
| --- | --- |
| Q1 | Direct/remuxed video bytes are unchanged by PGS selection. |
| Q2 | No video encoder is started by PGS overlay selection. |
| Q3 | Steady-state cue onset and clear are within 100 ms of source time. |
| Q4 | After a seek, the correct state appears within 150 ms or one frame. |
| Q5 | Overlay preparation cannot block initial video playback. |
| Q6 | Images are authenticated, bounded, and safe to decode. |
| Q7 | Failures retain the current video range and HDR/DV state. |
| Q8 | Repeated requests coalesce and use an atomic, bounded cache. |

The timing targets are product acceptance targets, not promises that every
display refresh, network path, and operating system will be exact to the
millisecond. Measurements must be made on supported physical devices.

## 8. Proposed architecture

```text
Matroska / PGS track
        |
        v
plurxd PGS extractor + parser
        |
        +--> source-timeline manifest
        |
        +--> immutable transparent PNG objects
                    |
                    v
       Apple / Android authenticated fetch
                    |
                    v
       overlay renderer above video plane
                    |
                    v
          unchanged DV/HDR video output
```

The server owns bitstream interpretation. The clients own presentation.

The server converts display sets into a normalized sequence of complete
compositions. Each composition has a start time, derived end time, authored
canvas, and zero or more positioned image objects. Clients do not interpret
PGS segment types, palettes, cropping flags, or RLE data.

The clients fetch the manifest and required images, map source time into the
current player item, and draw the active composition in a transparent overlay
that shares the displayed video rectangle.

### 8.1 Why PNG objects

PNG is recommended for version 1 because it is:

- lossless;
- transparent;
- decoded by both platform image stacks;
- easy to validate and inspect;
- cacheable by a content hash;
- independent of a client-side PGS parser.

PGS object images are often small relative to the full video canvas. The
manifest carries their authored placement, so the server should not emit a
full-frame transparent PNG for every cue.

### 8.2 Complete compositions, not deltas

PGS display sets can update state incrementally. A client protocol based on
raw deltas would require every client to reproduce PGS state-machine behavior
and would make random access difficult.

The normalized manifest must instead describe the complete active composition
after each display-set change. That makes seeking a binary search followed by
one render operation.

## 9. Track capability contract

Add one optional field to the existing subtitle-track response:

```json
{
  "index": 2,
  "codec": "hdmv_pgs_subtitle",
  "text": false,
  "native": false,
  "overlay": "pgs-v1"
}
```

Contract rules:

- `text` retains its current meaning;
- `native` retains its current HLS text-rendition meaning;
- `overlay` is absent when the server cannot provide an overlay;
- `overlay: "pgs-v1"` means the manifest contract in this document;
- clients must treat unknown overlay values as unsupported;
- old clients ignore the additive field and retain current behavior;
- the field describes capability, not readiness of a warm cache.

The client routing order becomes:

1. use the existing text-rendition path for compatible text tracks;
2. use `pgs-v1` overlay for a PGS track when supported by both ends;
3. otherwise apply the legacy burn/refusal policy and HDR guardrails.

The word `native` should not be expanded to include bitmap overlays. Doing so
would change a shipped field's meaning and make compatibility harder to reason
about.

## 10. HTTP API

### 10.1 Manifest endpoint

```http
GET /api/v1/files/{file_id}/subs/{stream_index}/overlay.json
Authorization: Bearer <token>
Accept: application/json
```

The server validates that:

- the file exists and is accessible to the authenticated user;
- the stream belongs to that file;
- the stream is a supported PGS codec;
- the source fingerprint still matches any cached generation.

### 10.2 Ready response

```http
HTTP/1.1 200 OK
Content-Type: application/json
Cache-Control: private, no-cache
ETag: "<manifest-etag>"
```

```json
{
  "schema": 1,
  "generation": "opaque-source-fingerprint",
  "file_id": 5559,
  "track_index": 2,
  "kind": "pgs",
  "timebase": "source_ms",
  "duration_ms": 8523000,
  "cues": [
    {
      "id": "7be3e747",
      "start_ms": 123456,
      "end_ms": 127890,
      "canvas_width": 1920,
      "canvas_height": 1080,
      "objects": [
        {
          "image": "overlay/opaque-source-fingerprint/objects/abc123.png",
          "x": 220,
          "y": 850,
          "width": 1480,
          "height": 110
        }
      ]
    }
  ]
}
```

Field rules:

- `schema` is the manifest schema version and must equal `1`;
- `generation` changes whenever the source identity or extraction semantics
  change;
- `timebase` is always `source_ms` in version 1;
- cue intervals are half-open: `start_ms <= t < end_ms`;
- cues are sorted by `start_ms` and must not overlap;
- every cue is a complete active composition;
- a clear period is represented by the end of one cue and no active next cue;
- canvas dimensions are positive integers;
- object coordinates are integers in authored canvas space;
- `image` is a relative same-origin API path, never a bearer-token URL;
- one cue may contain multiple objects;
- an empty `objects` array is permitted but should normally be normalized into
  a gap rather than serialized;
- unknown fields must be ignored by clients.

The manifest must preserve the PGS authored canvas even when it differs from
the encoded video resolution. A common example is a 1920x1080 PGS canvas over
a 3840x2160 video. Clients scale the authored canvas to the displayed video
rectangle; they do not assume pixel-for-pixel equality with decoded video.

### 10.3 Preparing response

A cold extraction must not hold an HTTP connection open for minutes and must
not delay video playback.

```http
HTTP/1.1 202 Accepted
Content-Type: application/json
Retry-After: 1
```

```json
{
  "state": "preparing",
  "retry_after_ms": 1000
}
```

The optional response may later add coarse progress, but version 1 clients
must not require it. The client polls with bounded backoff, cancels immediately
when the track is deselected, and ignores any completion belonging to a stale
selection generation.

Recommended polling behavior:

- begin at 250 ms;
- back off to a maximum of 2 seconds;
- stop at the server's extraction deadline;
- stop on track change, subtitles off, player teardown, or authentication
  failure.

### 10.4 Object endpoint

The manifest's relative path resolves under:

```http
GET /api/v1/files/{file_id}/subs/{stream_index}/overlay/{generation}/objects/{sha256}.png
Authorization: Bearer <token>
```

Successful objects should use:

```http
Content-Type: image/png
Cache-Control: private, max-age=31536000, immutable
ETag: "<sha256>"
```

The generation and object hash make the representation immutable. The route
still requires authentication. Tokens must not appear in paths, query strings,
manifests, logs, or cache keys visible to other users.

### 10.5 Errors

Use the existing API error envelope and distinguish at least:

| Status | Meaning |
| --- | --- |
| 400 | Invalid stream index or malformed request. |
| 401/403 | Authentication or authorization failure. |
| 404 | File, stream, generation, or object not found. |
| 409 | Source changed while preparation was running; client may retry. |
| 415 | Subtitle codec is not supported by this overlay version. |
| 422 | PGS stream is malformed or exceeds safety limits. |
| 500 | Unexpected extraction or cache failure. |
| 503 | Extractor capacity is temporarily unavailable. |

An overlay error must not be converted into an automatic video transcode when
the current video is HDR or Dolby Vision.

## 11. PGS normalization

### 11.1 Display-state model

The extractor must process PGS segment state in presentation order, including:

- presentation composition segments;
- window definition segments;
- palette definition segments;
- object definition segments;
- end-of-display-set markers;
- composition state changes and explicit clears;
- object cropping and multiple object references.

At the end of each relevant display set, the normalizer snapshots the complete
visible composition. Identical consecutive snapshots should be coalesced.

### 11.2 Cue boundaries

For each non-empty snapshot:

- `start_ms` is the display set's presentation time;
- `end_ms` is the first later presentation time that changes or clears the
  visible composition;
- a final unterminated composition is clamped to media duration;
- zero or negative duration cues are rejected or normalized away;
- timestamps outside a small, documented tolerance of the media duration are
  rejected as malformed.

The implementation must include fixtures for:

- normal show/clear sequences;
- palette updates that reuse an object;
- object updates that reuse a palette;
- multiple simultaneous objects;
- cropped objects;
- 1080p subtitle canvas on 4K video;
- missing clear at end of stream;
- malformed RLE and truncated display sets;
- out-of-order or duplicate timestamps.

### 11.3 Image generation

The server resolves the palette and RLE into RGBA pixels and emits the smallest
useful object image. It must preserve the authored object's position after any
crop is applied.

Images are content-addressed by the encoded PNG bytes or canonical RGBA input.
The exact choice must be stable within a generation. Identical objects should
deduplicate even when reused by many cues.

The server must not apply SDR/HDR color conversion to the video. Subtitle image
color is ordinary UI color composed by the platform after video presentation.
Physical-device validation is required to confirm acceptable luminance and
legibility over Dolby Vision content. Version 1 should preserve the authored
PGS palette rather than invent an unreviewed brightness transform.

## 12. Extraction, dependency, and cache design

### 12.1 Parser evaluation

The leading Rust candidate for the feasibility spike is `libpgs` 0.6.x. It
advertises Matroska/M2TS/SUP support, cue-indexed extraction, time-range
extraction, and incremental display-set handling.

It is also a young dependency with a small adoption signal. It must not be
accepted solely from its API description. Milestone 0 must review:

- license compatibility;
- exact source and transitive dependency tree;
- parser behavior on the project fixture corpus;
- malformed-input behavior and memory bounds;
- Matroska cue-index assumptions;
- extraction completeness when a file has sparse or missing cue metadata;
- fuzzing suitability;
- maintenance and pinning strategy.

If accepted, place it behind a plurx-owned adapter and pin an exact reviewed
version or commit. The manifest contract must not expose dependency types.

### 12.2 Fallback implementation

If the dependency is rejected, a fallback is:

1. use FFmpeg to copy the selected subtitle stream into raw SUP data while
   preserving timestamps;
2. parse the bounded SUP stream with a plurx-owned parser;
3. feed the same normalizer and cache contract.

This fallback is simpler at the container boundary but may scan the entire
file. It is acceptable as a correctness fallback, not automatically acceptable
for production first-use latency on remote storage.

### 12.3 Source fingerprint

The cache generation must include enough source identity to invalidate stale
data. At minimum:

- file identifier;
- selected stream index;
- file size;
- source modification time;
- overlay schema/extractor version.

If the repository already has a stronger stable media fingerprint, prefer it.
Do not hash a multi-gigabyte media file on every request solely for this cache.

### 12.4 Cache layout

Recommended logical layout:

```text
cache/subs/pgs/<generation>/
  manifest.json
  objects/
    <sha256>.png
```

Generation creation must be atomic:

1. write into a unique temporary directory under the same filesystem;
2. validate the final manifest and every referenced object;
3. fsync where the existing cache policy requires it;
4. rename the completed directory into place;
5. make concurrent readers observe either no generation or a complete one.

Duplicate extraction requests for the same generation must share one task.
Failures should be negatively memoized for a short period, consistent with the
existing subtitle cache, so malformed tracks cannot trigger an extraction
storm.

### 12.5 Resource limits

Initial review limits should be explicit constants, covered by tests, and
tunable after fixture measurements. Proposed starting points are:

| Limit | Proposed initial value |
| --- | ---: |
| Canvas dimensions | 4096 x 2160 |
| Canvas pixels | 8,847,360 |
| Decoded RGBA object bytes | 36 MiB |
| Objects per composition | 64 |
| Cues per track | 250,000 |
| Cached bytes per track | 256 MiB |
| Cold extraction deadline | 10 minutes |
| Negative failure memo | 2 minutes |

These are review proposals, not measured final values. Milestone 0 must report
real distributions from representative media and adjust them before merging
the extractor.

Cache eviction should integrate with a bounded LRU or the project's shared
cache budget. A permanent unbounded directory is not acceptable.

## 13. Apple client design

### 13.1 Rendering hierarchy

[PlayerSurface.swift](../clients/apple/Sources/PlayerSurface.swift) currently
owns an `AVPlayerLayer`, and the SwiftUI player view places controls around
that surface. Extend the surface so that the subtitle overlay is:

- a sibling above the `AVPlayerLayer`;
- clipped to the displayed video rectangle;
- below playback controls and diagnostics;
- owned by the player surface lifecycle, not by a screen-global singleton.

The recommended synchronization primitive is `AVSynchronizedLayer` associated
with the active `AVPlayerItem`. Its Core Animation subtree follows the player
item timeline, which is more appropriate than the existing one-second UI time
observer.

For each relevant cue/object, the client can create or reuse a `CALayer` whose
contents are the decoded PNG and whose visibility is controlled by item-time
animations. The implementation should window scheduled layers around the
current playback position rather than install hundreds of thousands of layers
at once.

### 13.2 Time conversion

The manifest uses source time. The synchronized layer uses current item time.

```text
item_start_ms = cue.start_ms - base_ms
item_end_ms   = cue.end_ms   - base_ms
```

For direct play and ordinary VOD, `base_ms` is normally zero. For a live HLS
session or range that begins partway through the source, it is the source start
of that item.

On every `AVPlayerItem` replacement, the client must:

1. invalidate the old overlay generation token;
2. detach the old synchronized layer;
3. update `base_ms` from the new playback range;
4. attach a new synchronized layer to the new item;
5. reconcile the active cue at the new real position.

### 13.3 Coordinate conversion

Use `AVPlayerLayer.videoRect` as the destination rectangle. If the authored PGS
canvas is `Cw x Ch` and the displayed video rectangle is `Vw x Vh`, the usual
aspect-fit mapping is:

```text
scale = min(Vw / Cw, Vh / Ch)
origin_x = videoRect.minX + (Vw - Cw * scale) / 2
origin_y = videoRect.minY + (Vh - Ch * scale) / 2

screen_x = origin_x + object.x * scale
screen_y = origin_y + object.y * scale
screen_w = object.width * scale
screen_h = object.height * scale
```

If `videoRect` already exactly represents the aspect-fitted source canvas, the
extra centering term is zero. The implementation must confirm the appropriate
relationship with fixture screenshots rather than applying both letterbox
calculations accidentally.

Recompute layout when bounds, orientation, safe-area behavior, or video
presentation size changes.

### 13.4 Fetch and memory behavior

The Apple client should:

- fetch and validate the manifest off the main thread;
- use the existing authenticated API client;
- prefetch only images around the current position;
- maintain a bounded in-memory decoded-image cache;
- cancel image work when the selection generation changes;
- publish only the currently active selection on the main actor;
- discard images whose dimensions do not match the manifest bounds.

### 13.5 Picture in Picture and external playback

Custom overlays cannot be assumed to appear in Apple Picture in Picture. The
custom PiP API presents video using system-owned playback presentation, and
sibling application layers may not be carried into that output. AirPlay and
external playback have a similar architectural risk.

Milestone 0 must test this on the supported Apple devices. If the overlay is
not present:

- do not silently start a burn transcode;
- show a clear, one-time limitation before entering the affected mode when a
  PGS overlay is active;
- either disable the overlay for that output or require the user to choose a
  compatible text track/off state;
- keep Dolby Vision/HDR video unchanged.

Whether PiP or external playback is a release blocker is a reviewer decision,
not an assumption in the implementation.

## 14. Android client design

### 14.1 Rendering hierarchy

[PlayerScreen.kt](../clients/android/app/src/main/java/tv/plurx/app/player/PlayerScreen.kt)
already hosts `PlayerView` inside a Compose `Box`. Add a dedicated subtitle
overlay above `PlayerView` and below player chrome.

The overlay should render the current composition as positioned image nodes on
a transparent canvas whose bounds match the visible video content rectangle.
It must not use Android's text-caption styling APIs, because the source is an
authored bitmap.

### 14.2 Synchronization

Use
[Controller.kt](../clients/android/app/src/main/java/tv/plurx/app/player/Controller.kt)'s
real source position as the authoritative lookup time.

Do not recompose the complete player UI on every video frame. A suitable model
is:

1. binary-search the active cue at selection, seek, discontinuity, and resume;
2. publish only the active composition to Compose;
3. schedule the next wake-up at the next cue start or current cue end;
4. reconcile against `realPosition()` when the wake-up fires;
5. cancel and reschedule on playback speed, pause, seek, item, or track change.

Media3 includes a PGS parser in its extractor text package, but the shared
server-image architecture does not need to expose it to production playback.
It may be useful as a test oracle against selected fixtures.

### 14.3 Coordinate conversion

Map the authored canvas into the actual video content rectangle using the same
aspect-fit model as Apple. Do not use the full device window or Compose box if
the video is letterboxed.

The implementation must handle:

- phone and tablet rotation;
- Android TV overscan/safe presentation behavior;
- videos whose aspect ratio differs from the display;
- PGS canvas dimensions that differ from encoded video dimensions;
- density-independent UI coordinates versus source pixels.

### 14.4 Fetch and memory behavior

Use the existing authenticated network layer and a bounded image loader/cache.
Manifest and image work must be scoped to the player/selection lifecycle.
Stale asynchronous results must be rejected with a monotonically increasing
selection token.

### 14.5 Picture in Picture

The current Android implementation changes its Compose chrome during PiP.
Whether a Compose sibling overlay is included in the system PiP surface can
vary with the activity and device implementation. Milestone 0 must test the
actual supported PiP path on physical hardware.

If it is not visible, apply the same rule as Apple: disclose the limitation,
keep the video unchanged, and never silently switch Dolby Vision/HDR to SDR.

## 15. Selection policy and user experience

### 15.1 Policy

[SubtitlePolicy.kt](../clients/android/app/src/main/java/tv/plurx/app/player/SubtitlePolicy.kt)
and the equivalent Apple selection logic must gain an overlay outcome distinct
from text rendition and burn.

Conceptually:

```text
subtitle off
    -> no subtitle delivery

text track supported as rendition
    -> existing text rendition

PGS + server advertises pgs-v1 + client supports pgs-v1
    -> bitmap overlay; video range unchanged

otherwise
    -> legacy compatibility policy
       - SDR may offer burn where already supported
       - HDR/DV must not auto-fallback to SDR burn
```

Selection and deselection must not call the video-plan endpoint merely to
change overlay state.

### 15.2 Cold preparation

Video starts normally. If a user selects an uncached PGS track, show a modest
non-blocking state such as:

> Preparing bitmap subtitles…

When preparation completes, reconcile the cue for the current source position
and begin display. Do not rewind or restart the video.

If preparation fails, clear the pending selection and show:

> This subtitle could not be prepared. Dolby Vision playback was kept
> unchanged.

The exact copy can be refined during implementation, but it must communicate
both the subtitle failure and the preserved video state.

### 15.3 Defaults and forced tracks

Once a client/server pair has passed acceptance, default or forced PGS tracks
may auto-select the overlay. A cold cache still must not block video start.

Old clients must not receive new automatic behavior from the server. The
client remains responsible for choosing a delivery path based on advertised
capabilities.

### 15.4 Playback diagnostics

Playback diagnostics should distinguish:

- `Text rendition · WebVTT`;
- `Overlay · PGS`;
- `Burn-in · PGS`.

The displayed video range must remain independently visible so a report can
show, for example, `Direct · Dolby Vision` with `Overlay · PGS`.

## 16. Failure behavior and guardrails

The feature must preserve these invariants:

1. Overlay selection never mutates the selected video bytes.
2. Overlay selection never starts a video encoder.
3. Overlay failure never silently changes HDR/DV to SDR.
4. An old server's missing `overlay` field is not treated as preparation
   failure; it is simply unsupported capability.
5. A stale manifest or image response cannot re-enable a deselected track.
6. A player-item replacement cannot leave the old item's synchronized overlay
   attached.
7. Tokens never appear in manifest image URLs.
8. Malformed PGS cannot allocate unbounded memory or disk.
9. The existing text-rendition path is not rewritten as part of this feature.
10. Logs and metrics are supporting evidence; the visible Dolby Vision/HDR
    state on physical hardware remains the release oracle.

For an unsupported PGS track during HDR/DV playback, the UI should prefer a
clear explanation over a destructive fallback. If product later wants an
explicit user choice to convert to SDR with burn-in, that is a separate policy
decision and must be unmistakable.

## 17. Security and privacy

PGS is untrusted media input. The extractor and API must be designed as an
untrusted-file boundary.

Required controls:

- authenticate manifest and image requests;
- authorize access against the source file, not only a cache path;
- validate that the requested stream belongs to the requested file;
- accept only supported subtitle codecs;
- use integer overflow-safe dimension and buffer calculations;
- cap canvas, object, cue, track, memory, disk, and extraction time;
- reject truncated RLE, impossible palette references, invalid cropping, and
  out-of-range coordinates safely;
- write only beneath a server-owned cache root;
- derive cache paths from internal fingerprints, never user-supplied paths;
- use atomic cache publication;
- avoid following unexpected symlinks in the cache;
- never log bearer tokens or tokenized URLs;
- fuzz the PGS parser/normalizer boundary before enabling untrusted libraries;
- run image decode tests against both client platform decoders.

The image route must not become a public static-file bypass. Long-lived cache
headers apply to the authenticated immutable representation, not to public
access.

## 18. Observability

### 18.1 Server events and metrics

Record structured events or counters for:

- `pgs_overlay_prepare_started`;
- `pgs_overlay_prepare_completed`;
- `pgs_overlay_prepare_failed`;
- cache hit, miss, negative hit, and eviction;
- extraction duration and bytes read;
- manifest cue count and object count;
- encoded and decoded object bytes;
- object deduplication ratio;
- first-manifest-ready latency;
- limit rejection reason;
- concurrent/coalesced waiter count.

Do not include media titles, tokens, or raw filesystem paths in ordinary
metrics labels.

### 18.2 Client events and metrics

Record:

- overlay capability advertised;
- overlay track selected/off/switched;
- preparation duration and result;
- manifest schema rejection;
- active cue reconciliation after seek;
- late cue onset/clear beyond the acceptance threshold;
- image fetch/decode failure;
- memory-cache hit rate;
- item replacement while overlay is active;
- PiP/external-output limitation shown.

Client logs should include opaque file/track identifiers where existing privacy
policy permits them, plus playback range and `base_ms`. They should not include
authorization material.

## 19. Compatibility and rollout

### 19.1 Compatibility matrix

| Client | Server | Expected behavior |
| --- | --- | --- |
| New | New | PGS overlay when `pgs-v1` is advertised. |
| New | Old | No overlay field; retain current text/burn/refusal behavior. |
| Old | New | Ignores additive field; retains current behavior. |
| Web/other | New | No change unless separately implemented. |

The new server must not reinterpret existing `native` or `text` values. This
is what makes the rollout additive.

### 19.2 Feature gating

Use separate server and client gates during development:

- server extraction/API available to authenticated development clients;
- Apple overlay selectable only in debug/feature-gated builds;
- Android overlay selectable only in debug/feature-gated builds;
- default/forced auto-selection enabled only after full acceptance.

The server may ship its additive API before both clients. Clients must ship
only after they handle missing, preparing, malformed, and failed responses.

### 19.3 Versioning

This is a user-visible, cross-client playback feature. At implementation time:

- increment the Apple build number from 19 to at least 20;
- increment Android `versionCode` from 9 to at least 10;
- keep marketing version 0.2.2 only if this remains in the same unreleased
  release train;
- otherwise use the next minor product version, expected to be 0.3.0;
- apply the same marketing version to Apple and Android;
- let the release owner decide whether the Rust workspace/server package also
  changes version rather than bumping it mechanically.

The final implementation PRs must re-read current version values because other
work may land before this feature.

## 20. Alternatives considered

| Alternative | Assessment |
| --- | --- |
| Server-decoded PNG manifest | Recommended. One parser and security boundary; small client contract; preserves video. |
| Send raw PGS to both clients | Rejected for v1. Duplicates parser/state-machine work, especially on Apple, and widens the untrusted-input surface. |
| Android Media3 PGS plus custom Apple parser | Rejected for v1. Platform behavior can diverge and fixture equivalence becomes harder to guarantee. |
| OCR PGS into WebVTT | Rejected. Loses typography, positioning, signs, multiple objects, and non-text content. |
| Always burn and tone-map to SDR | Rejected for HDR/DV preservation. It solves visibility by discarding the requested video presentation. |
| Runtime Dolby Vision subtitle authoring | Rejected for this scope. It is materially more complex and is not preservation of the original video bytes. |
| Full-frame transparent images | Rejected as default. Simple but wasteful in bandwidth, decode memory, and cache size. |
| WebSocket cue push | Rejected for v1. Static immutable media is easier to cache and seeking is simpler with a manifest. |

The recommended design trades some first-use server preparation and client UI
work for preservation of the original video path and consistent semantics.

## 21. Milestones and acceptance evidence

### Milestone 0: feasibility and physical-device spike

Work:

- audit and prototype the candidate PGS parser behind an adapter;
- extract representative PGS tracks, including production files 5559 and
  5698, without identifying titles in committed fixtures;
- compare normalized compositions with FFmpeg or Media3 reference output;
- prototype one timed transparent overlay on Apple and Android;
- verify the hardware/display Dolby Vision or HDR indicator remains active;
- test seek, pause, rate change, PiP, AirPlay/external output, and Android PiP;
- measure cold extraction bytes, time, memory, cue count, and cache size.

Acceptance evidence:

- written dependency/security assessment;
- fixture corpus description and expected hashes/screenshots;
- timing and resource measurement table;
- physical-device photos or captured diagnostics proving the video mode;
- explicit PiP/AirPlay/external-output result;
- go/no-go recommendation for the server-image architecture.

No production code should depend on an unreviewed parser before this milestone
is accepted.

### Milestone 1: server contract, extractor, and cache

Work:

- add the optional `overlay` track capability;
- implement PGS parsing and complete-composition normalization;
- implement manifest and authenticated immutable object routes;
- implement coalescing, atomic publication, limits, negative memoization, and
  bounded eviction;
- add metrics and structured failures;
- document the final schema in playback documentation.

Acceptance evidence:

- unit tests for display-state transitions and malformed data;
- golden manifests/images for representative fixtures;
- route authentication and authorization tests;
- concurrency/atomicity tests;
- limit and timeout tests;
- cache invalidation tests;
- API compatibility test proving old track fields are unchanged.

### Milestone 2: Apple client

Work:

- add overlay policy and manifest models;
- extend the player surface with a synchronized overlay layer;
- implement source/item time conversion and image prefetch;
- handle seek, rate, item replacement, selection cancellation, and layout;
- expose diagnostics and limitation UX;
- update tests and app version.

Acceptance evidence:

- model and policy unit tests;
- timeline conversion tests with non-zero `base_ms`;
- player-item replacement regression test;
- layout snapshots for 1080p-on-4K and multiple aspect ratios;
- simulator test for selection/switch/off;
- physical iPhone/iPad/Apple TV timing and Dolby Vision/HDR validation;
- documented PiP and AirPlay behavior.

### Milestone 3: Android client

Work:

- add overlay policy and manifest models;
- implement bounded fetch/decode/cache behavior;
- add the Compose video-aligned overlay;
- implement boundary scheduling and discontinuity reconciliation;
- expose diagnostics and limitation UX;
- update tests and app version.

Acceptance evidence:

- JVM policy, parsing, and timeline tests;
- Compose layout tests for aspect ratios and authored canvases;
- seek/switch/off instrumentation test;
- Android TV and handheld physical-device validation;
- Dolby Vision/HDR mode evidence on supported hardware;
- documented PiP behavior.

### Milestone 4: policy integration and compatibility

Work:

- enable default/forced PGS overlay selection for approved clients;
- implement explicit HDR/DV failure guardrails;
- verify new/old client/server combinations;
- update [PLAYBACK.md](PLAYBACK.md) and release notes;
- reconcile final product versions with the active release train.

Acceptance evidence:

- compatibility matrix executed, not only reasoned about;
- no subtitle selection creates a video session in overlay-capable cases;
- forced/default cold-cache behavior does not block video start;
- failure injection proves HDR/DV remains unchanged;
- user-facing copy reviewed.

### Milestone 5: release validation

Work:

- run the repository's complete server, Apple, and Android gates;
- run live playback against representative direct, remux, and HLS ranges;
- inspect server resource use and client timing metrics;
- prepare rollback/feature-gate instructions;
- publish only after review evidence is attached to the implementation PRs.

Acceptance evidence:

- all required CI checks pass;
- physical validation matrix is complete;
- no unresolved high-severity review findings;
- version numbers and release notes match across clients;
- rollback is a feature-gate change, not a database or cache recovery event.

## 22. Reviewer decision log

Reviewers should answer these before implementation begins:

1. **Architecture:** Approve server-decoded PNG objects and a manifest instead
   of raw PGS parsing in each client?
2. **Dependency:** Approve a Milestone 0 audit of `libpgs`, with acceptance
   contingent on fixtures, security limits, and a pinned adapter boundary?
3. **Output modes:** Is lack of custom overlay in PiP, AirPlay, casting, or an
   external display acceptable for v1 if clearly disclosed and video quality
   is preserved?
4. **Scope:** Is PGS-only correct for v1, leaving VobSub and ASS/SSA for later
   protocols?
5. **Cold behavior:** Approve video-first playback with a non-blocking
   "Preparing bitmap subtitles" state?
6. **Defaults:** After acceptance, may forced/default PGS tracks auto-select
   without restarting video?
7. **Limits:** Are the proposed extraction/cache limits suitable starting
   bounds for the measurement spike?
8. **Fallback:** For HDR/DV, approve failure-with-disclosure instead of silent
   SDR burn-in? If an explicit user-requested SDR fallback is desired, should
   it be a later issue?
9. **Release:** Should the feature ship as 0.3.0, or is 0.2.2 still an
   unreleased train when implementation begins?

Record answers in the design-review issue or implementation epic. Any answer
that changes the API, video-quality invariant, or output-mode scope must update
this document before code is merged.

## 23. Test matrix

At minimum, execute the following combinations.

### 23.1 Media

| Video | Subtitle | Expected result |
| --- | --- | --- |
| Dolby Vision Profile 7 compatible source | PGS | DV video unchanged; overlay visible. |
| Dolby Vision Profile 8 compatible source | PGS | DV video unchanged; overlay visible. |
| HDR10 | PGS | HDR10 unchanged; overlay visible. |
| SDR direct play | PGS | Video unchanged; overlay visible. |
| Remuxed video | PGS | Remux session unchanged; overlay visible. |
| Live HLS range with non-zero base | PGS | Correct source-time cue displayed. |
| Any supported video | WebVTT | Existing text-rendition behavior unchanged. |

### 23.2 PGS content

- simple single-line cue;
- two-line cue;
- overlapping signs and dialogue objects;
- forced-only track;
- default track;
- non-English palette/object reuse;
- position near every canvas edge;
- 1080p canvas over 4K video;
- clear followed by a long gap;
- cue active at seek target;
- malformed/truncated stream;
- track that exceeds each configured limit.

### 23.3 Player actions

- start with PGS selected and warm cache;
- start with PGS selected and cold cache;
- select PGS while playing;
- PGS A to PGS B;
- PGS to WebVTT;
- PGS to off;
- seek forward and backward into/out of a cue;
- scrub repeatedly while images are loading;
- pause on a cue boundary;
- change playback rate;
- background/foreground;
- network loss after manifest but before image fetch;
- authentication expiry;
- video item/range replacement;
- rotate or resize the player;
- enter and exit PiP;
- start and stop external playback where supported.

## 24. Documentation changes required with implementation

When implementation lands, update:

- [PLAYBACK.md](PLAYBACK.md) with the three subtitle delivery paths and policy;
- the API documentation with `overlay` capability and `pgs-v1` schema;
- Apple and Android publishing/version references;
- release notes with output-mode limitations;
- troubleshooting guidance for extraction, cache, and timing failures;
- the existing native-subtitle handoff only to point readers to the new PGS
  plan, without rewriting its historical record.

Use "text rendition," "bitmap overlay," and "burn-in" consistently. Avoid
saying "all native subtitles" when only WebVTT renditions are meant.

## 25. Primary references

- Apple,
  [HLS Authoring Specification for Apple Devices](https://developer.apple.com/documentation/http-live-streaming/hls-authoring-specification-for-apple-devices/):
  subtitle rendition formats and Apple HLS requirements.
- Apple,
  [`AVSynchronizedLayer`](https://developer.apple.com/documentation/avfoundation/avsynchronizedlayer):
  Core Animation timing synchronized to an `AVPlayerItem`.
- Apple,
  [`AVPlayerLayer`](https://developer.apple.com/documentation/avfoundation/avplayerlayer):
  video presentation layer, gravity, and displayed video rectangle.
- Apple,
  [Adopting Picture in Picture in a Custom Player](https://developer.apple.com/documentation/avkit/adopting-picture-in-picture-in-a-custom-player):
  custom-player PiP model.
- Android,
  [Media3 supported formats](https://developer.android.com/media/media3/exoplayer/supported-formats):
  HLS, subtitle, HDR, and Dolby Vision platform support boundaries.
- Android,
  [`PgsParser`](https://developer.android.com/reference/androidx/media3/extractor/text/pgs/PgsParser):
  Media3 PGS parsing behavior and API stability classification.
- Dolby,
  [Dolby Vision Color Grading Best Practices](https://professional.dolby.com/siteassets/content-creation/dolby-vision-for-content-creators/dolby_vision_color-grading_best-practices_v4.2.pdf):
  Dolby Vision analysis and dynamic metadata context.
- Dolby,
  [Adding Subtitles to a Finished Dolby Vision Master](https://professional.dolby.com/siteassets/content-creation/dolby-vision-for-content-creators/dolby_vision_workflow_subtitles_v1.4.pdf):
  professional authored-subtitle workflow, distinct from runtime playback
  overlay.
- docs.rs, [`libpgs` 0.6.x](https://docs.rs/crate/libpgs/latest): candidate
  dependency capabilities for the Milestone 0 audit, not an approval to ship.

## 26. Final acceptance statement

The feature is complete only when reviewers can observe all of the following
on supported physical Apple and Android hardware:

- a PGS track is visible and correctly positioned;
- cue timing meets the agreed threshold across play, seek, and item changes;
- selecting, switching, and disabling PGS does not restart the video session;
- the video range and Dolby Vision/HDR presentation remain unchanged;
- cold preparation does not delay video startup;
- extraction and rendering failures preserve the current video quality;
- the documented compatibility and output-mode behavior matches reality;
- versioning, release notes, diagnostics, tests, and rollback gates are in
  place.

Until that evidence exists, the implementation must remain feature-gated and
must not replace the current default subtitle policy for production users.
