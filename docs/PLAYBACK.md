# Playback — how a file becomes a stream

Companion to [ARCHITECTURE.md](ARCHITECTURE.md) §3 (the *founding* decisions —
why the pipeline exists and how it fails over) and
[ADAPTIVE-QUALITY.md](ADAPTIVE-QUALITY.md) (the height/bitrate ladder). This
doc is the **end-to-end map**: every path a file can take from "press Play" to
pixels, the choice made at each fork, and *why*. If a delivery path isn't drawn
here, the player doesn't use it.

The whole thing is built around one belief, stated in ARCHITECTURE and worth
repeating because every fork below inherits it: **the server's best move is to
send the file untouched.** Transcoding is the last resort, not the default —
and the player says so out loud in `/decision`.

## The end-to-end path

```
 you press Play
      │
      ▼
 probe THIS browser's decoders (once, cached)  ─▶  vcodec, acodec, container, hdr
      │            canPlayType() / MediaSource.isTypeSupported()
      ▼
 GET /api/v1/files/{id}/decision?<caps>&force=<auto|original|transcode>
      │
      ▼
 server pure fn:  (file streams, device profile, caps, prefs) ─▶ Decision
      │           { method, delivery, reasons[], transcode_audio, audio[], subs[], markers[] }
      │
      │    `delivery` is the server-owned EXECUTION PLAN for the verdict, so a
      │    client acts on it instead of re-deriving policy from `method`:
      ├─ { mode: direct,    url: /files/{id}/direct }                  (HTTP range)
      ├─ { mode: remux,     url: /files/{id}/stream.mp4,               (progressive fMP4)
      │                     sessions_url, aac }        (or POST sessions_url with copy:true
      │                                                 for players that need HLS transport)
      └─ { mode: transcode, sessions_url }             (POST it, omitting `height`: Auto is
      │                                                 the server's rung to pick)
      ▼
 client picks the transport its browser can actually play  (see "Delivery")
      │
      ▼
 <video> plays; every 5 s → POST /items/{id}/progress   (watch state + Trakt)
```

Two independent decisions live in that flow, and keeping them separate is the
key to reading the code:

- **The verdict** — direct / remux / transcode — is the *server's* call, a pure
  function of the file and the reported caps. Covered in
  [ARCHITECTURE.md §3](ARCHITECTURE.md#3-playback-pipeline--get-out-of-the-way-first);
  echoed below only enough to stand on its own.
- **The transport** — progressive `<video>` vs native HLS vs hls.js — is the
  *client's* call, because only the browser knows what its `<video>` element
  will actually accept. This is the part ARCHITECTURE doesn't cover and the
  part that bites (see [the fallback](#the-error-fallback--and-the-stale-reason-trap)).

## Routing inventory — every fork has one owner and one regression pin

This is the auditable map. A **routing decision** is any rule that changes the
video bytes, container/transport, selected track, encoder/tone-map chain,
height, or recovery path. Telemetry, UI labels, play/pause, and progress
reporting are deliberately outside the inventory because they observe a route;
they do not choose one.

The machine-readable twin is
[`tests/playback/routing-decisions.toml`](../tests/playback/routing-decisions.toml).
[`test_playback_routing_inventory.py`](../tests/validation/test_playback_routing_inventory.py)
keeps the two lists identical and fails when a source or test anchor disappears.
That test does not pretend an anchor proves behavior: the named unit test is the
behavioral pin. The inventory test proves every documented fork still points to
one.

<!-- playback-routing-inventory:start -->

### Server — decide the bytes, recipe, and hardware path

| ID | Fork | Current rule | Regression pin |
|---|---|---|---|
| `server.capability-profile` | Reported caps vs named profile | Runtime caps win when any codec/container cap is present; missing fields get conservative browser defaults. Otherwise use the named profile, then `web-h264`. Explicit DV profiles override the legacy all-DV bit. | Rust unit in `http/stream.rs` |
| `server.verdict` | Direct vs remux vs transcode | Video codec, height, bitrate, or HDR failure means transcode. Container, audio, or A/V correction alone means remux. No failures means direct. Unknown container is not permission to direct-play it. | Table-driven Rust unit in `playback/mod.rs` |
| `server.dolby-vision` | Preserve vs strip vs re-encode DV | A client-approved profile is preserved. An unsupported profile with a compatible base and `dovi_rpu` becomes a strip remux. Without both, re-encode. Apple-supported DV profiles still request a normalized copy-HLS envelope. | DV profile matrix in `playback/mod.rs` |
| `server.manual-quality` | Auto vs Original vs a rung | Auto uses the ordinary verdict. Original never re-encodes video; it may direct or remux and lets the client rescue a rejection. Any numbered rung forces transcode. Unknown force values degrade to Auto. | Force matrix in `playback/mod.rs` |
| `server.execution-plan` | Verdict to API action | Direct owns `/direct`; remux owns `/stream.mp4` plus a copy-session URL and flags; transcode owns the HLS-session URL. Clients execute this plan instead of rebuilding it from `method`. | Exhaustive `DeliveryPlan` Rust unit |
| `server.selected-audio-compatibility` | Selected track to remux audio recipe | Audio compatibility follows the track selected by language policy or the viewer, not the container's scan-time default. An unsupported preferred TrueHD track is converted to AAC while the HDR video remains copied. | Mixed E-AC-3/TrueHD Dolby Vision Rust regression |
| `server.apple-high-tier-master` | High-tier Apple HLS playlist envelope | HDR HEVC High-tier sessions without native text renditions serve their media playlist directly, avoiding AVPlayer's multivariant eligibility rejection without changing video samples. | High/Main-tier, HDR/SDR, and subtitle-gate Rust matrix |
| `server.apple-high-tier-init` | High-tier Apple HLS initialization record | The generated initialization segment clears only its HEVC High-tier declaration for the same narrowly gated HDR sessions, allowing VideoToolbox to inspect otherwise decodable picture data. | Synthetic High-tier `hvcC` Rust regression |
| `server.segmented-remux` | Progressive vs segmented remux hint | Every probed remux prefers segments. Average bitrate and storage speed cannot predict a transient path gap, while progressive fMP4 has only about 2.2 s of browser runway. The browser must still prove MSE accepts the exact codec pair; **Original · one stream** remains an explicit veto. | Eligibility units in `playback/mod.rs`; browser policy matrix |
| `server.track-selection` | Initial audio/subtitle | Explicit viewer choice wins later. Cold start uses one shared language policy: original-language anime, configured languages, subtitle Auto/Always/Off, then container defaults. | Track-policy Rust matrix |
| `server.subtitle-classification` | Sidecar vs rendition vs burn | Bitmap has no text route. SRT/SubRip/WebVTT may become native HLS renditions. Other text, including ASS and `mov_text`, can be extracted but not advertised as a native rendition, so session selection burns it. | Classifier Rust unit |
| `server.hdr-subtitle-burn-guard` | Old-client burn request on HDR | Session creation independently refuses `subtitle_burn` for a probed DV, HDR10, or HLG source with a machine-readable 422 before playback accounting or encoder creation. SDR and unprobed sources retain burn support. There is deliberately no override: missing client-plan context is not permission to replace HDR with SDR. | Rust HTTP refusal contract plus helper matrix |
| `server.pgs-overlay` | PGS capability and artifact delivery | Only a PGS track receives additive `overlay: "pgs-v1"`, and only while the default-off gate is enabled. Authenticated cold manifests return preparation without blocking playback; warm manifests and content-addressed PNGs are published atomically. This does not choose or change video transport. | Auth/type/cache HTTP contract plus parser/cache Rust units |
| `server.session-kind` | Copy HLS vs transcode HLS | `SessionKind::Copy` preserves video and optionally converts audio/strips DV; `Transcode` runs the video recipe. A matching completed cache entry bypasses the encoder but does not change the logical kind. | Transcode-manager lifecycle unit |
| `server.live-playlist-window` | Writer history vs client window | A live writer keeps its full EVENT history internally. The default served view becomes sliding only after retention deletes a prefix. The default-off iPad experiment instead serves a typeless playlist plus `EXT-X-START:TIME-OFFSET=0` from the first response, preserving one envelope across that boundary. Completed cached VOD stays whole. | Rust retention/serving integration + typeless shape-stability unit; physical-iPad comparison pending |
| `server.auto-rung` | Auto output height | Software follows the source up to 720p; proven hardware follows it up to 1080p. Both clamp to the source and never upscale. An explicit rung is snapped to the published ladder. | Encoder-aware async Rust unit |
| `server.encoder` | Hardware family vs software | Honor a usable admin preference; otherwise take the first probed usable hardware encoder; software x264 is the unconditional fallback. Probe success, not advertised presence, is authority. | Every-family encoder units |
| `server.tone-map-pipeline` | GPU graph vs CPU graph | A probed vendor graph is used only with its matching encoder and PQ HDR source. Bitmap overlay or a failed/incompatible graph declines to the recorded fallback; CPU is total. | Pipeline decision and fallback units |

### Web — choose the browser transport and bounded rescues

| ID | Fork | Current rule | Regression pin |
|---|---|---|---|
| `web.initial-route` | First browser delivery | Transcode verdict → HLS. Remux → copy HLS when Safari needs it or the server hint passes the MSE codec gate; otherwise progressive fMP4. Direct stays range-served unless a nonzero preferred audio track requires remux. | Node table over the shipped policy module |
| `web.hls-transport` | Native HLS vs hls.js | Native capability counts only with WebKit's playback-target API, or when hls.js/MSE is unavailable. Even on Safari, native HLS is reserved for copied HEVC; other HLS uses hls.js so plurx keeps its controls and timeline. | Node native/MSE matrix |
| `web.manual-quality` | Force and session height | Auto omits height unless a burn/refused remux must preserve a direct/remux resolution promise. Original and `nomse` preserve source height. Explicit 1080/720/480 requests that exact rung. | Node force/height matrix |
| `web.compatibility-fallback` | Rejection to rescue | A direct/remux media rejection before real playback gets one H.264/AAC transcode. A transcode failure, a repeated failure, established playback, or an hls.js network failure does not loop into another encode. | Node fallback matrix |
| `web.hdr-subtitle-guard` | Burn-only subtitle on HDR | PGS, VobSub, and styled text require the SDR burn pipeline. While HDR is on the wire, the selection is refused with a visible notice and the current stream is untouched; SDR playback may still burn it. | Node subtitle/dynamic-range matrix |
| `web.decode-rescue` | Accepted-but-choppy original | On Auto, after ≥150 s and ≥15 lost frames at ≥6/min, switch the copy/direct route to transcode. Pipeline latency is diagnostic only. Remember by codec/height unless buffer quota, and allow explicit Original/retests to clear it. | Node threshold/boundary unit |

### Native clients — execute the plan without inventing a second server

| ID | Fork | Current rule | Regression pin |
|---|---|---|---|
| `apple.transport-and-dv` | AVPlayer direct vs HLS | Execute the server mode, except normalize even a legacy direct Dolby Vision answer through preserving copy HLS. Overrides, audio changes, and subtitle needs decide whether that session copies or transcodes. | XCTest for legacy direct-DV normalization |
| `apple.compatibility-fallback` | Apple startup decode failure recovery | Before real playback, a preserved DV stream with an HDR10/HLG base strips to that base first; only the next media rejection uses the universal transcode. Each rescue is once and resumes the last truthful film position. | XCTest recovery ladder |
| `apple.established-hdr-recovery` | Interruption after HDR rendered | Once the item has advanced for ≥5 s, a stall or item failure reconnects the same HDR delivery once. An immediate repeat stops visibly instead of falling through to the SDR compatibility transcode. | XCTest established-delivery guard |
| `apple.buffering-recovery` | Sustained or self-recovered wait after playback began | Each newly opened item must advance for ≥5 s before six stagnant two-second samples during an explicit AVPlayer buffer wait may reopen the same delivery. The third sample first nudges Play; an immediate repeat stops with network-specific copy. Recovery actions report method, film position, and observed duration, while deltas from AVPlayer's access-log stall counter plus any sub-threshold stagnant interval ride the 10 s progress cadence to the same bounded client log. None of this is codec/HDR evidence. | XCTest per-item gate, monitor, retry-state, access-log delta, and beacon-payload suite; physical-iPad throttling pending |
| `apple.item-end` | Temporary live edge vs completed item end | A premature end gets one reopen. If the replacement ends at the same playhead position, a growing or known-duration stream stops visibly instead of looping sessions; a direct/offline item with no usable duration finishes at that corroborated boundary. | XCTest duration/growing matrix + same-position retry bound |
| `apple.hls-buffer-window` | Growing HLS forward buffer | AVPlayer prefers a 60 s buffer on live copy/transcode sessions, but a physical iPad fetched about 120 s ahead; the preference is not a cap. The server therefore retains 180 s behind the download frontier: the measured 120 s lead plus 30 s of back buffer and 30 s for retry/reload. Direct files and completed cached HLS keep AVPlayer's default. | Rust retained/pruned segment-set regression + XCTest item-configuration guard |
| `apple.player-system-chrome` | Custom iOS full-screen player chrome | In a full-screen iOS window, the status bar and Home indicator stay available while controls, playback info, a failure, a notice, or recovery/next progress is visible. They retire only after all player surfaces leave; a windowed iPad status bar remains system-owned. | Routing inventory pins the modifier application + iOS hosting-controller propagation XCTest |
| `apple.marker-control-row` | Marker action placement | The PlayerView call site hosts the shared one-line marker label inside `PlayerTrailingControlRow`, which owns full-width expansion and trailing placement; the button itself must not reclaim the row with a max-width frame. | XCTest shared label/row component contract; physical narrow-phone accessibility-size check pending |
| `apple.quality-session` | Picked rung vs Auto/burn height | A picked rung forces a transcode at that height. An otherwise-copyable subtitle burn preserves source height; an ordinary Auto transcode leaves the encoder-aware rung to the server. | XCTest source/rung/burn height matrix |
| `apple.subtitle-route` | Media selection vs reopen | Native rendition switches stay inside AVPlayer once the session exists. A recognized PGS overlay stays on the current item. Entering from direct, selecting/leaving a burn, or changing a burn reopens; other bitmap/styled tracks still burn. | XCTest route matrix |
| `apple.hdr-subtitle-guard` | Burn-only subtitle on HDR | A recognized PGS overlay is allowed because it does not change video. Unknown overlay versions, VobSub, and styled tracks are refused while the current delivery is DV, HDR10, or HLG. Native text still selects, and SDR playback may still burn. | XCTest subtitle/dynamic-range matrix |
| `apple.pgs-overlay` | PGS application overlay vs video mutation | Only `overlay: "pgs-v1"` selects the authenticated manifest/PNG renderer. It schedules complete compositions against the current `AVPlayerItem`, maps authored coordinates into `videoRect`, and never sends subtitle/burn session fields or reopens video. PiP and external playback are blocked while active rather than falling back to an SDR burn. | XCTest manifest, timeline, layout, item-replacement, and no-reopen policy suite |
| `android.capability-profile` | Decoder/display/sink claims | Claim DV only when both decoder profile and display agree, never claim dual-layer P7, and claim passthrough audio when either the decoder or active sink can take it. | JVM capability matrix |
| `android.compatibility-fallback` | Media3 startup decode recovery | Before a frame renders, failed preserving direct DV first gets a normalized remux; any remaining direct/remux failure gets one compatibility transcode; a failed transcode is terminal. | JVM fallback matrix |
| `android.established-hdr-recovery` | Interruption after HDR rendered | Once Media3 renders a frame, a later HDR error retries the same delivery once. A repeat is terminal instead of silently becoming the SDR compatibility stream. | JVM established-delivery matrix |
| `android.manual-quality` | Auto/Original/rung force | Auto asks for the normal verdict, Original forbids video re-encode, and every server-advertised rung requests a transcode. The menu never invents a rung above the source. | JVM force and ladder matrix |
| `android.session-height` | Quality/burn to session height | A burn and Original preserve source height; Auto omits height; an explicit rung sends that height. A copy session omits height because copied video cannot honor a rung. | JVM height and session-body matrix |
| `android.subtitle-route` | Embedded vs native session vs overlay vs burn | Direct embedded text stays in the plan. Remux/transcode text uses a native rendition session. Recognized PGS stays on the plan video as an application overlay; unsupported bitmap and styled tracks retain burn/refusal behavior. | JVM subtitle, overlay, and session-body matrix |
| `android.hdr-subtitle-guard` | Burn-only subtitle on HDR | Recognized `pgs-v1` is allowed because it does not mutate video. Unsupported bitmap/styled tracks are refused while the current delivery is DV, HDR10, or HLG, with a transient notice. Native text still selects, and SDR playback may still burn. | JVM subtitle/dynamic-range matrix |
| `android.pgs-overlay` | PGS application overlay vs video mutation | Only `overlay: "pgs-v1"` selects the bounded authenticated manifest/PNG renderer. It binary-searches source-time cues, schedules exact boundaries, maps authored coordinates into the video content rectangle, and rejects stale selection/item/window work. PiP is blocked while active rather than falling back to SDR burn-in. | JVM manifest, timeline, memory, layout, and policy suite plus Compose switch/off instrumentation |

<!-- playback-routing-inventory:end -->

**How to read it:** start with the ID named by the behavior you are changing,
then read its source and pin from the TOML catalog. A new rule that changes
bytes or transport is incomplete until it has a new ID, a source anchor, a
behavior test, and a row here. Changing an existing rule means changing its
unit test and this prose in the same commit.

## Runtime caps — what each client tells the server

Before the first `/decision`, each player reports what that device can decode.
The server folds those query parameters into an ad-hoc device profile
(`caps_profile` in
`plurx-core/src/playback`), so a file only transcodes when *this* device
genuinely can't play it — not because a fixed profile guessed conservatively.

| Cap | Probed with | Notes |
|---|---|---|
| `vcodec` | `canPlayType` + `MediaSource.isTypeSupported` | `h264` always; `hevc`/`av1`/`vp9` when the browser answers. Safari says yes to HEVC; Chrome-on-macOS via the OS decoder. |
| `acodec` | `canPlayType` | `aac`,`mp3` always; `ac3`/`eac3` where supported (Safari), `opus`/`flac` per browser. |
| `container` | fixed | `mp4,webm,mov` — what a browser `<video>` accepts as a file. Notably **not** `mkv`. |
| `hdr` | `matchMedia("(dynamic-range: high)")` | `1` only on an HDR display *and* an HDR-capable codec — else the server tone-maps, because HDR on an SDR screen looks washed-out. |
| `dvprofile` | platform codec APIs | Exact Dolby Vision profiles the decoder and current display both accept. Apple and Android advertise single-layer delivery profiles, never infer P7 from generic HDR. |
| `dvhls` | platform policy | Apple sends `1`: an approved DV stream still needs normalized copy HLS rather than a raw progressive file. Browser/Android omit it unless they need the same envelope. |

The native clients use platform codec/display APIs instead of browser probes.
Android also includes audio support exposed by the active HDMI/audio sink,
because passthrough eligibility changes with the route. Apple limits progressive
containers to MP4/MOV/M4V and asks the server to remux everything else into HLS.

**How to read it:** the caps are why the same file behaves differently across
browsers. A 4K HEVC/HDR MKV with DTS audio reports the *same* verdict on Chrome
and Safari — `remux`, because the container (mkv) and audio (dts) fail but the
HEVC/HDR video passes on both. What differs is the *transport*, below.

## The verdict — direct / remux / transcode

The engine is a pure function; its full decision tree and reasons live in
[ARCHITECTURE.md §3](ARCHITECTURE.md#3-playback-pipeline--get-out-of-the-way-first).
The three outcomes and what they cost:

- **Direct play** — `/files/{id}/direct`, HTTP range, zero transcode CPU. The
  goal state. Everything already matches.
- **Remux** — copy the video stream untouched (`-c:v copy`), fix only the
  container and (if needed) the audio codec. Pennies of CPU. The "right codecs,
  wrong container" case — MKV with HEVC the browser can decode.
- **Transcode** — re-encode the video (hardware first, HDR→SDR tone-map, sub
  burn-in). The expensive path, taken only when the *video itself* won't decode
  (codec / resolution / bitrate / HDR mismatch). Delivered as HLS.

`reasons[]` names every dimension that failed, so the stats overlay can explain
itself. An empty `reasons[]` means direct play.

## Dynamic range — source, delivered, rendered

A badge built from the source probe answers "what is this file?" while the
viewer is asking "what am I getting?" — which is how a Dolby Vision disc
remux came to show a full-colour "DV P7" over a tone-mapped SDR transcode of
itself. Three layers, one owner each:

| Layer | Question | Owner |
|---|---|---|
| Source | What does the file carry? | server probe (`MediaFile.hdr` / `hdr_format`) |
| Delivered | What grade is in the bytes this session sends? | server (`delivered_dynamic_range`) |
| Rendered | Is the display actually showing that grade? | client (display/decoder APIs) |

**Delivered** rides on both responses a client already reads, with the same
vocabulary as `MediaFile.hdr` plus `"sdr"`, so a client compares source
against delivered with string equality:

- `GET /files/{id}/decision` → `delivered_dynamic_range` (always present),
  describing the plan as decided.
- `POST /files/{id}/hls/sessions` → `delivered_dynamic_range` (nullable; the
  key is omitted when the source row could not be read), describing the
  session actually created. It **overrides** the decision's value the moment
  a session attaches — a burn or a manually-picked rung produces a transcode
  the decision never promised.

Both come from one function, `plurx_core::playback::delivered_dynamic_range`,
so the two answers cannot drift. It is a reporter, not a decider: the three
deliveries are total, and the values fall out of what the pipeline already
does.

```
 direct play ────────────────▶ the source's grade, untouched
 remux / copy ─┬─ preserve DV ▶ dolby_vision   (dvh1 tag, RPUs kept)
               ├─ strip DV ───▶ the base layer: "(HLG-compatible)" → hlg,
               │                else → hdr10
               └─ non-DV ─────▶ the source's grade (the video is copied)
 transcode ──────────────────▶ sdr, always (H.264 8-bit + tone-map)
```

A client combines that with the strongest *local* signal it has — the display
is HDR-capable, and where a platform exposes it, the decoder confirming what
it engaged — and renders one of three badge states:

| State | Condition | Rendering |
|---|---|---|
| **lit** | rendered == source grade | the full-colour chip (gold DV / teal HDR) |
| **downgraded** | rendered ≠ source grade | chip dimmed, arrow suffix names what is on screen (`DV P7 → HDR10`) |
| **source-only** | no active session (detail screens) | the source's chip, undimmed |

An older client never reads the field; a newer one treats an absent field as
"unknown" and falls back to the source-only chip. Nothing breaks either way.

## Subtitles — three independent delivery questions

Every subtitle in `/decision` carries `text` and `native` plus an optional
`overlay` capability. They are not interchangeable claims. A client that reads
one for another either offers a track the server will refuse, hides a route it
can serve, or restarts/re-encodes video that could remain untouched.

| Flag | The question it answers | True when | The route it unlocks |
|---|---|---|---|
| `text` | Is there text here at all? | the codec isn't a bitmap (`is_bitmap_subtitle`) | `GET /files/{id}/subs/{index}.vtt` — the extracted WebVTT sidecar, and a `<track>` on a direct/remux `<video>` |
| `native` | Can this be an HLS rendition? | `is_native_text_subtitle`: `subrip \| srt \| webvtt \| vtt` | an `EXT-X-MEDIA` line in the native master, and an accepted `subtitle` index on `POST /files/{id}/hls/sessions` |
| `overlay` | Can the server supply an application-rendered bitmap protocol? | `"pgs-v1"` only for PGS, only while the staged server producer is enabled | the authenticated manifest and immutable PNG routes below; it never changes video transport |

`native` implies `text`; the reverse is false, and the gap is the whole point
of having two flags. **`mov_text`** — the timed text every MP4 WEB-DL carries
— and **ASS/SSA** are `text: true, native: false`: the sidecar extracts fine,
but the rendition path would have to convert them to WebVTT while slicing
segments, and for ASS/SSA that conversion discards the positioning and
typography the release was authored with. So they are absent from the native
master, and asking for one by index is a 400 (`"the selected subtitle
requires burn-in"`) rather than a rendition that silently plays wrong.

The practical rule for a client: **gate a session-mode subtitle pick on
`native`, use `text` for the sidecar, and recognize only overlay protocol
values the client implements.** An absent or unknown `overlay` value is
unsupported, not a preparation state. VobSub and XSUB remain burn-only.

### PGS overlay server contract — staged and default-off

The server side of `pgs-v1` is implemented behind `PLURX_PGS_OVERLAY=1` and is
off by default while physical-device HDR/Dolby Vision acceptance remains
incomplete. Apple and Android have automated application renderers, but that
does not make the server capability production-ready. When off,
`/decision` omits `overlay` and the
overlay routes return 404. Enabling the gate changes subtitle delivery only;
it does not select an overlay automatically and does not alter video bytes.

The manifest route is:

```http
GET /api/v1/files/{file_id}/subs/{stream_index}/overlay.json
Authorization: Bearer <user-token>
```

A warm generation returns `200 application/json`, `Cache-Control: private,
no-cache`, and a generation ETag. A cold generation starts one detached,
capacity-bounded producer and returns immediately:

```json
{
  "state": "preparing",
  "retry_after_ms": 1000
}
```

That `202` response also carries `Retry-After: 1`. The client may keep video
playing, poll while the same selection/timeline epoch remains active, and stop
polling without cancelling server preparation.

The version 1 manifest is a source-time list of complete composition snapshots:

```json
{
  "schema": 1,
  "generation": "opaque-source-fingerprint",
  "file_id": 42,
  "track_index": 3,
  "kind": "pgs",
  "timebase": "source_ms",
  "duration_ms": 7200000,
  "cues": [
    {
      "id": "c00000001",
      "start_ms": 1250,
      "end_ms": 4810,
      "canvas_width": 1920,
      "canvas_height": 1080,
      "objects": [
        {
          "image": "overlay/opaque-source-fingerprint/objects/abc123.png",
          "x": 430,
          "y": 874,
          "width": 1060,
          "height": 120
        }
      ]
    }
  ]
}
```

Intervals are half-open (`start_ms <= t < end_ms`), sorted, and non-overlapping.
Every cue is the complete active composition. Authored clears become gaps, not
empty cues. If several display sets resolve to the same source millisecond, the
producer processes them in file order and keeps the last complete state; the
intermediate states have zero display duration and never become duplicate cue
starts. Consecutive identical compositions and duplicate PNG content are also
coalesced. Both clients therefore receive one unambiguous state at a boundary.
Canvas coordinates remain authored PGS coordinates; clients map that canvas
into the current video rectangle.

Each `image` value is relative to the subtitle route and resolves to:

```http
GET /api/v1/files/{file_id}/subs/{stream_index}/overlay/{generation}/objects/{sha256}.png
Authorization: Bearer <user-token>
```

Objects return `image/png`, `Cache-Control: private,
max-age=31536000, immutable`, and a content-hash ETag. Both routes require an
ordinary signed-in user. A generation is derived from file id, subtitle index,
source size/mtime, extractor version, and schema version; a replaced source or
protocol change therefore cannot serve the previous cache.

The producer demuxes the selected stream to bounded raw SUP, opens that SUP
once, and uses the same file identity for structural preflight and parsing. It
also compares a content digest across both passes, so an in-place change is a
failure rather than permission to parse bytes that were not preflighted. The
owned raw-SUP/PCS adapter unwraps the 32-bit 90 kHz PTS clock, treats only a
jump larger than half the clock range as wrap, and rejects ordinary backwards
time. It recognizes `0xC0` Epoch Continue with the same cache reset FFmpeg
applies to every non-normal state. The normalizer then emits complete RGBA
snapshots, content-addresses PNG objects, validates the finished manifest and
every referenced object, and atomically renames the generation directory into
view. Limits include a 4096×2160 canvas, 64 objects per composition, 250,000
display sets, 256 MiB normalized/output bytes per track, a 10-minute deadline,
two concurrent producers, a two-minute negative failure memo, and a separate
2 GiB/128-generation LRU cache budget.

Route failures are stable: 404 for file/track/generation/object absence, 415
for a non-PGS codec, 422 for malformed or over-limit PGS, 409 when the source
changes during preparation, and 503 for temporary demux/capacity/timeout
failure. None of those failures authorizes a silent fallback to SDR burn-in.

## Web delivery — the browser's transport choice

A verdict names *what* to send; the client still has to pick *how*, because a
`<video>` element's tolerances differ by engine. The rule that matters:
**Safari's `<video>` will not play a progressive fragmented-MP4 — only HLS —
whereas Chromium plays progressive fMP4 fine.** So `remux` forks by browser.

```
 decision.method?
   │
   ├─ direct_play ─▶ <video src="/direct?token=…">                every browser: native range seek
   │
   ├─ transcode ──▶ /hls/start ─▶ HLS ──┬─ Safari ─▶ native HLS (video.src = playlist)
   │                                    └─ others ─▶ hls.js over MSE
   │
   └─ remux ──────┬─ Safari (useNativeHls) ─▶ COPY-VIDEO HLS ─▶ native HLS
                  │        progressive fMP4 is unplayable in Safari, so we
                  │        repackage the SAME copied video as HLS instead
                  └─ others ───────────────▶ progressive fMP4  <video src="/stream.mp4">
```

The full matrix:

| `decision.method` | Chromium (Chrome/Edge/Firefox) | Safari / iOS |
|---|---|---|
| `direct_play` | `<video>` HTTP range | `<video>` HTTP range |
| `remux` | progressive fMP4 (`/stream.mp4`) | **copy-video HLS** (fMP4 segments) |
| `transcode` | HLS via hls.js (MSE) | HLS native |

`useNativeHls()` gates the Safari column: it keys off the WebKit AirPlay API
(`WebKitPlaybackTargetAvailabilityEvent`), **not** `canPlayType('…mpegurl')` —
Chrome answers "maybe" to that query but has no native HLS, so the naive gate
would push Chrome onto a path it can't run.

## Native delivery — Apple and Android execute the same plan differently

Native players do not use the web transport table. They consume the same
`delivery` object, then apply only the platform decisions the server cannot
make.

| Server plan / override | Apple AVPlayer | Android Media3 |
|---|---|---|
| Direct, no override | Raw `/direct`; supported DV is normalized through copy HLS | Raw `/direct` |
| Remux, no override | Copy-video HLS; AVPlayer does not take plurx's progressive fMP4 reliably | Progressive `/stream.mp4` |
| Transcode | HLS session | HLS session |
| Native text subtitle needs a session | Copy HLS for a direct/remux plan; existing rendition switches stay in AVPlayer | Direct keeps an embedded text track; remux/transcode opens a native-rendition HLS session |
| PGS with recognized `pgs-v1` capability | Authenticated application overlay synchronized to the unchanged player item; PiP/external playback unavailable while active | Authenticated PNG composition scheduled from `realPosition()` and drawn in the video content rectangle; PiP unavailable while active |
| Other bitmap/styled subtitle | HLS transcode with burn-in on SDR; refused on HDR/DV | HLS transcode with burn-in on SDR; refused on HDR/DV |
| Direct + manual A/V correction | Not currently exposed by the Apple player | Progressive remux, because ffmpeg must apply the correction |
| Finished cache hit | HLS VOD; seek in place | HLS VOD; seek in place |

**How to read it:** direct/remux plans mean the source video is copyable, not
that every platform must use the same container. A subtitle rendition or audio
selection may move either native client into an HLS *session* while leaving the
video a remux. Only a transcode verdict, burn, picked rung, or compatibility
fallback authorizes different video bytes.

## Copy-video HLS — the remux path Safari can play

This is the fork that keeps Safari at source resolution. Without it, Safari
would reject the progressive fMP4 remux, and the player's error-fallback would
re-encode the whole 4K stream down to 720p (see next section) — that was a real
bug: identical file, Chrome kept 4K, Safari dropped to 720p.

**What it does:** copies the source video into HLS *untouched* — the original
4K HEVC/HDR bitstream, no re-encode — and transcodes only the audio when the
browser can't take the source codec. It is a remux, packaged as HLS.

**How it's built** (`copy_pipe_args` / `hls_copy_args` in
`plurx-core/src/transcode`, driven by `TranscodeManager::start_copy`):

```
 ffmpeg -ss <resume> -readrate_initial_burst 90 -readrate 2 -i <file>
        -map 0:v:0 -c:v copy [-tag:v hvc1]        # video untouched; hvc1 so Safari decodes HEVC
        -map 0:a:<n> -c:a aac -b:a 256k           # audio → AAC only when needed (else -c:a copy)
        -movflags frag_keyframe+empty_moov+…      # one continuous fMP4, one fragment per GOP
        -f mp4 pipe:1                             #   → plurxd cuts the segments (copyseg)
                                                  # non-HEVC/H.264 sources keep ffmpeg's HLS muxer
```

**Who cuts the segments.** On an HEVC or H.264 source plurx does, not ffmpeg:
ffmpeg writes one continuous fragmented stream down a pipe and
[`copyseg`](../crates/plurxd/src/copyseg.rs) publishes a boundary only in front
of a keyframe a player will not discard a leading picture at, because on an
open-GOP remux every ordinary boundary costs exactly one frame
([STUTTER-4K.md](STUTTER-4K.md) §5.6). Everything downstream is unchanged —
same `init.mp4`, same `segNNNNN.m4s`, same append-only EVENT writer history —
and a stream the reader cannot follow falls back to ffmpeg's own muxer once,
automatically, so the worst case is the behaviour above. The HTTP view becomes
a sliding media playlist after retention starts; the writer history stays
complete so pacing and subtitle timing still know the real segment durations.

Three details, each load-bearing:

- **fMP4 segments, not MPEG-TS.** Apple does not support HEVC inside a TS
  container; the transcode path's `.ts` segments would silently fail on Safari.
  The copy path emits `init.mp4` + `segNNNNN.m4s` and the segment handler serves
  them as `video/mp4`.
- **`-tag:v hvc1`.** MKV HEVC is usually tagged `hev1`, which Safari renders as
  a black frame; the sample entry must be `hvc1`. Harmless if already hvc1.
- **Burst-then-hold pacing.** Copy runs as fast as the disk allows; without
  pacing, a 45 Mb/s 4K session would dump the whole file into the session dir
  at once. This was a bare `-re` — ~1× real time — until 2026-07-28, and that
  was the bug behind "4K starts, then buffers a few seconds in": producing
  segments at exactly the rate they are consumed means the player's runway is
  whatever it fetched before playback began and never grows, so every hiccup
  after that is a stall. Worse on an Apple TV, which wants ~3 segments before
  it starts at all. Now the session delivers a configurable head start
  flat-out (`-readrate_initial_burst`, default 90 s) and then settles to a
  small multiple of real time (`-readrate`, default 2×), while the disk is
  bounded by the ahead-window suspend below instead of by starving the viewer.
  An ffmpeg older than 5.1 has neither flag and falls back to `-re`.
- **The ahead-window suspend.** Once a session is more than
  `playback.hls_ahead_max_secs` (default 180 s) of content ahead of the last
  segment the client fetched, the request-side flow controller SIGSTOPs its
  ffmpeg. A later media fetch that brings the reserve to 150 s or less
  SIGCONTs it; the 30-second gap prevents a fast encoder from toggling once per
  segment near the ceiling. The per-session byte limit releases at half. The
  global limit enters on total live scratch, but releases when the drainable
  sum of bytes ahead reaches half; retained history behind every client is
  still real disk usage but cannot make release structurally unreachable. The
  15-second reaper is a repair pass for a
  producer nobody is requesting from, not the normal trigger. Session status
  reports the active hold as `time`, per-session `bytes`, or `global`, plus the
  matching release value; the web and Apple overlays show both beside a held
  session. This is the bound `-re` used to provide, minus the part where `-re`
  also capped the buffer. A stopped process costs nothing, resumes instantly,
  and — unlike a rate limit — adapts to a viewer who pauses. SIGKILL works on a
  stopped process, so the idle reaper and the admin stop button need no special
  case.

  A hold is not proof that the producer starved the client. Ahead media is
  already published in the served playlist. If AVPlayer stops fetching while
  that reserve remains available, changing the release point produces more
  media the client is still declining to request. The observed iPad fetch-loop
  stop therefore remains open pending a device capture of `loadedTimeRanges`,
  access-log segment counts, and the playlist header across the internal
  EVENT-to-sliding-window transition.

**Playlist-envelope decision (2026-08-09): experiment, not yet adopted.** The
default remains the established EVENT-then-sliding behavior until the batched
physical-iPad run compares a long control playback with the gated variant.
Enable **Settings → Playback → Experimental typeless sliding HLS** for the
variant; it is snapshotted when a new session opens, strips EVENT before the
first client response, adds `EXT-X-START:TIME-OFFSET=0`, and retains that same
header shape when `MEDIA-SEQUENCE` begins advancing. The server logs the first
slide once with the session id, first retained index, and seconds since start;
compare that timestamp with client stall evidence. Keep the setting off if the
variant does not materially reduce stalls or changes AVPlayer seek behavior.

**Seek and audio-switch stay on this path.** A copy-HLS session sets
`PLAYER.method = 'remux'` (honest — no video re-encode) and `PLAYER.copyHls =
true`. The flag is what makes seeking and audio-switching re-open the HLS
session (`startCopyHls`) instead of falling back to the progressive
`/stream.mp4` Safari can't play. Without the flag, the first seek would
re-break it.

**Wiring:** `GET /files/{id}/hls/start?copy=1&aac=<0|1>` — `copy=1` selects the
copy session; `aac=1` says the selected audio needs transcoding. A cold start
gets that answer from `decision.transcode_audio`. An audio switch re-evaluates
the newly selected track against the transport that will actually attach:
native HLS uses the browser's audio capability list, while hls.js uses the
exact video/audio MediaSource pair. Without the second check, an AAC-default
file switching to AC-3 fed unsupported AC-3 into Chrome MSE and failed at
`bufferAddCodecError`. Everything else — playlist and segment serving, the
idle reaper, the fail-fast watchdog — is the shared HLS session machinery. The
start response includes additive `media_origin_ms`, the source timestamp
represented by player-local time zero. Copy sessions report the preceding
keyframe actually selected by the demuxer; accurate transcodes report the
requested start, and cached whole-title sessions report zero. New clients use
this integer origin for timeline mapping and old clients continue to fall back
to `start_seconds`.

## Persistent stalls — one bounded recovery, with an outcome

The startup watchdog diagnoses a stream that never starts. Mid-playback was
different: `waiting` began a timer only so `playing` could calculate the
finished gap. If `playing` never arrived, no stall beacon was emitted and no
deadline existed. The overlay could say **Buffering…** forever even after the
server's own transcode watchdog had correctly failed a stuck producer.

Now an uninterrupted wait has an eight-second deadline. The deadline records
the stall immediately, including runway and the active session id, then asks
the pure `stallRecoveryAction` policy for one action:

- An Auto remux switches at the current film position to H.264/AAC transcode.
  This covers both a source faster than the client link and a copy stream the
  browser cannot present reliably.
- Direct play, explicit Original, cached VOD, and an existing transcode
  reconnect the same route. This is a real source/session replacement; the old
  **Try again** path only assigned `currentTime` to the already-stalled element
  for direct and VOD playback.
- A second persistent stall never loops automatically. The viewer gets **Try
  again**, **Force transcode** where meaningful, and **Close**.

The attempt and its terminal `recovered` or `failed` outcome are separate
`stall_recovery` playback events. They join the same live session truth as
other client beacons and project to
`plurx_stall_recoveries_total{outcome=...}`. A stall that never resumes is
therefore visible both as the original supply/decode failure and as whether
the player repaired it.

## The error fallback — and the stale-reason trap

Any direct/remux stream the browser rejects gets exactly one automatic rescue:
restart as a guaranteed-compatible transcode.

```
 <video> fires "error" on a direct_play or remux stream   (once per session)
        │
        ▼
 startTranscodeFallback() ─▶ POST /files/:id/hls/sessions ─▶ full H.264 transcode
                             (height omitted = Auto: min(source,1080) on
                              hardware, 720p on software — see PERF-PLAN §4.7)
```

This is a good safety net and a bad first choice. The trap it created, worth
documenting because the symptom is confusing: the fallback flips
`PLAYER.method` to `'transcode'` but **does not rewrite `PLAYER.reasons`.** So
the stats overlay would read `Method: Transcode` next to the *remux* reasons
("container mkv…; audio codec dts…") and a 720p picture — which looks like a
decision-engine bug but is actually "the remux failed and we re-encoded." On
Safari that fired every time, on every HEVC remux.

The [copy-video HLS](#copy-video-hls--the-remux-path-safari-can-play) path
fixes the cause: Safari's remux now plays natively, so it never reaches the
fallback. The fallback remains for genuinely undecodable picks (a codec profile
even the copy path can't hand to the browser).

**How to read it:** `Method: Transcode` with a low "Now decoding" resolution
*and* reasons that only mention container/audio is the fallback firing — the
browser rejected a cheaper stream. `Method: Transcode` with a "video codec …"
or "HDR …" reason is a real, up-front transcode verdict.

## The decode-margin rescue — routing around a pipeline with no slack

The error fallback catches streams the browser *refuses*. This one catches
streams the browser accepts and then cannot present smoothly — found the
hard way ([STUTTER-4K.md](STUTTER-4K.md) §5.3): a client whose median
decode of a 4K HEVC remux was 41.6 ms against a 41.7 ms frame budget. Read
that number carefully — it is **slack, not capability**. A median pinned to
the frame budget is a pipeline delivering frames just-in-time (the client
in question was an M3 Max, whose media engine loafs through this stream);
whatever the reason the pipeline holds no reserve, every spike lands on
screen. The browser's `mediaCapabilities` claimed `powerEfficient: true`
throughout; the measurement outranks the claim, and the rescue triggers on
the measurement — zero slack *plus* visible hitches — which is the right
trigger whichever component is eating the reserve.

The player measures per-frame decode cost (`requestVideoFrameCallback`
`processingDuration`, median over a rolling window) and, on an **Auto**
session playing a copy path, rescues on **frames the viewer lost**: at least
150 s of actual playback observed, at least 15 lost frames, and a rate of 6 or
more per minute. "Lost" is `drop + gap + back` — never presented, or presented
out of order. The rescue is the same `startTranscodeFallback()` restart-at-
position as the error path, once per session, and it writes a `decode_rescue`
beacon with the numbers.

The verdict is remembered per `codec@height` in the browser
(`plurx_decode_limits`), so the next Auto play of a matching stream routes
straight to a transcode — the Reason row says so, with the measured numbers.
Two things keep the memory honest: an explicit **Quality → Original** always
wins (the limit only steers Auto, never the viewer), and an explicit-Original
session that plays **60 s under the same 6-per-minute rate** clears the entry
and logs `decode_limit_cleared`, so a device that stops needing the rescue is
noticed rather than distrusted forever.

**The decode figure is not allowed to decide this, and getting there took
three passes.** Clearing originally required `decodeMs` under 60% of the frame
budget, which reads `processingDuration` as decode cost — guardrail 8's
mistake, and worse here than usual, because on a pipelined decoder that figure
measures how *deep* the pipeline is. A client holding a healthy reserve reports
a **larger** one. On the 4K remux it went from 41.6 ms to 91 ms against a
41.7 ms budget once the `dvcC` fix restored the hardware path — playback
improved and the clearing condition moved further away, so the entry could
never clear and Auto stayed on a transcode permanently.

That fixed the clearing rule and left the **trigger** reading the same number
the same wrong way (`decodeMs >= 80%` of budget), which is the inconsistency
that should have been the tell. What settled it was one screenshot, 2026-07-30:
a 1920x1080 transcode at 7.5 Mb/s, **zero** dropped frames, hardware decode,
reporting **83 ms/frame against a 42 ms budget** — the identical "no slack"
verdict as the 4K remux it had just rescued the viewer away from. A gate that
condemns an 8 Mb/s 1080p stream on the machine it is judging has no
discriminating power at all, so in practice the rescue was firing on *four
hitch events in twenty seconds* — which any two-hour film produces for a dozen
transient reasons.

Two consequences worth stating plainly, because both were live bugs:

- Trigger and clear are now literally the same measure inverted, rather than
  two heuristics that can drift. They had drifted: a session the viewer
  described as flawless (6 compositor holds, 3 lost frames, two and a half
  minutes) still counted 9 "faults" against a bar of 4, so it could not clear
  its own entry no matter how well it played.
- Entries written by the old gate are **discarded on read** rather than
  migrated. They carry `decode_ms` and no `rate`, they were produced by a test
  that reads true on every stream, and they are not measurements of anything.

Found from the couch, 2026-07-30: Safari on Auto played 4K while Chrome on
Auto would not, same machine, same file — only Chrome carried the remembered
entry. The entry itself turned out to be a buffer-quota problem
(PERF-PLAN §4.3quater) wearing a decode costume, which is why a session the
browser refused buffered data on now falls back **without** recording a decode
limit.

**Three ways out, because this is invisible state that steers playback.** The
self-clearing rule above is the automatic one, and it was unreachable for a
month without anyone noticing — so it is not the only one:

| Where | What it does |
|---|---|
| Player → **Quality** menu | Names the measurement and its age when one is steering this stream, with a **Forget this measurement** button that clears it and re-measures on the spot |
| Settings → **Measured playback limits** | Lists every entry this browser carries and forgets them all |
| By itself, weekly | One Auto session ignores an entry older than 7 days and plays the source instead. A clean minute clears it; hitches re-stamp it and push the next re-test out another week |
| By itself, monthly | Entries expire 30 days after they were taken |

The expiry is the part worth arguing for: an entry is evidence about one
device, one browser build and one stream at one moment, and every part of that
moves. This mechanism exists because a 4K remux stuttered — and it stopped
stuttering without the device changing at all. Evidence with no shelf life
stops being a measurement and becomes a belief.

The weekly re-test is the same argument at a shorter horizon. Every other way
out requires the viewer to know this mechanism exists, to know it is why their
picture is soft, and to know which menu undoes it — and almost nobody knows any
of the three, so in practice the verdict was permanent. Once a week an Auto
session simply plays the source and finds out. A re-test that goes badly costs
what the rescue always cost — roughly twenty seconds of hitches before it
switches back, once a week — against the alternative of never getting the
source back at all.

And when the player *does* move a session off the original, the overlay carries
a **Switched** row saying why, for as long as that session lasts. The toast
says it once and vanishes; the Reason row above it is the server's verdict on
the *file* and does not change when the client switches, so neither of them
could answer "why am I watching 1080p?" ten minutes later.

**How to read it:** the **Hitches** row is the one that means something —
`drop` and `skip` are frames the viewer lost, and their rate is what the rescue
judges. The Decoder row states the pipeline figure flat and grey
(`83ms/frame in the pipeline against a 42ms budget — latency, not load`),
deliberately without a warning colour: it is `processingDuration`, it grows as
the pipeline gets healthier, and dressing it in amber is how it came to be
believed. A `Reason` beginning "this device measured …" is the remembered limit
steering an Auto session, a **Switched** row is this session having been moved
off the original, and `decode_rescue` / `decode_limit_cleared` lines in the
perf report are the same events server-side.

## Resume & progress

- **Resume** rides the same input-seek on every path: `?start=<seconds>` on
  `/direct`/`/stream.mp4`, or `hls/start?start=…`. For HLS sessions (transcode
  and copy), the response reports `media_origin_ms`; clients that understand
  it calculate `media origin + player-local time` as the true position. That
  distinction is load-bearing for copied video: a request between keyframes
  begins at the preceding keyframe after FFmpeg normalizes its timestamp to
  zero. The progressive remux exposes the same value in
  `X-Plurx-Media-Origin-Ms`; direct play needs no offset because `currentTime`
  is already source time. Older clients, and new clients talking to an older
  server, fall back to the requested start.
- **Progress** posts every 5 s and on `ended` to `POST /items/{id}/progress`,
  which drives the resume bar, "Continue watching", and the server-side Trakt
  scrobble. Best-effort: a dropped beat is not surfaced.

## Reading the stats overlay

Press `i` in the player. The fields, and what each is telling you:

| Field | Meaning |
|---|---|
| **Method** | The verdict *as currently running* — `Direct play` / `Remux` / `Transcode`. If it disagrees with the reasons, see [the fallback](#the-error-fallback--and-the-stale-reason-trap). |
| **Reason** | Why it isn't direct play, one clause per failed dimension. Empty ⇒ direct. |
| **Source** | The file's real specs (video codec/bit-depth/HDR, resolution, bitrate, container, audio) — from the server-side ffprobe, numbers the browser can't see. |
| **Now decoding** | What the `<video>` element is actually decoding *right now*. For remux/copy this equals Source resolution (video untouched); for transcode it's the target rung. Dropped frames + buffer health live here. |

The one comparison that matters: **Source resolution vs Now-decoding
resolution.** Equal ⇒ you're getting the original video (direct or remux/copy).
Lower ⇒ the video is being re-encoded down — expected for a true transcode
verdict, a red flag if the reason is only container/audio.

The overlay closes when *you* close it — the `i` key or its own ✕ — and stays
open while you use the Quality, Audio, Subtitles and Sync menus, which is the
point: the reason to have it open during a quality change is to watch what the
change does to it. The menu slides clear of the panel rather than sitting on
top of it, and falls back to overlapping only in a window too narrow to hold
both side by side. On a touch screen the two still take turns, because a phone
has room for one of them and no keyboard shortcut to escape whichever is
covering the other.

## Non-goals & known limits

- **HLS session disk.** A live HLS writer's history grows for its whole life,
  so the reaper keeps 180 s behind the download frontier on both the transcode
  and copy paths. That covers the 120 s fetch lead measured on a physical iPad
  — even though AVPlayer was given a 60 s preference — plus 30 s of back
  buffer and 30 s for a retry or playlist reload. Ahead of that frontier, the
  suspend window bounds the other end, so a session's directory holds roughly
  `hls_ahead_max_secs + 180 s` of content whatever the encoder's speed. At
  the default 180 s ahead limit this is about 360 s, up from the previous
  300 s span (+20%). The 8 GiB global scratch cap is unchanged. It enters on
  total scratch and releases at half based on bytes ahead across live
  sessions, so unprunable retention floors cannot deadlock every producer.
  Session status
  exposes the active `hold_reason` plus `resume_below_seconds` or
  `resume_below_bytes`, and the web and Apple overlays show the matching value
  while a session is held so that state is distinguishable from an encoder
  stall.
  Once the reaper removes an older prefix, the client
  playlist advances `MEDIA-SEQUENCE` and stops advertising those files. The
  internal index retains duration-only history for native subtitle timing;
  seeking outside the retained window still opens a fresh session at that
  film position.
- **Apple seeks route by the advertised window first.** The served playlist's
  seekable span — everything published and not yet pruned — is a real
  random-access surface, and AVPlayer seeks inside it instantly. The Apple
  client (`PlayerController.seekRoute`) maps a film-time target through the
  session's `media_origin_ms` base and seeks the item's own clock whenever
  the target lands inside `seekableTimeRanges`, holding 1.5 s short of the
  live edge and snapping targets up to 2.5 s past it onto that holdback.
  Only targets outside the window — ahead of the transcoder, or behind
  retention — replace the server session, and those replacements coalesce
  for 350 ms so remote-mashing costs one create, not one per press. While a
  replacement is in flight the predecessor item's failures are ignored:
  supersession has already deleted its playlist, so its dying fetches 404 by
  design, and reacting to them raced a second open against the first (the
  successor's own status is re-checked once the change lands). Web and
  Android still reopen for every non-VOD seek; adopting the same window
  routing there is open work.
- **No client-side bitrate adaptation yet.** One encode runs at a time; the
  rung is chosen at start, not adapted per segment. The design for that is
  [ADAPTIVE-QUALITY.md](ADAPTIVE-QUALITY.md).
- **Burn-only bitmap subs cost a stream restart.** VobSub and PGS without a
  client-recognized overlay capability can't be copied or `<track>`'d — a
  picture has no text to send — so selecting one re-opens the
  stream as a transcode with the subtitle composited into the frames, and
  turning it off restarts again — back through the decision, so the viewer
  returns to the direct play / remux (and the resolution) the burn took away.
  The burn's rung may not downgrade a resolution already promised: under
  **Original** it is the source's own height, and under **Auto** it is also
  the source's height whenever the verdict was remux/direct — the decision had
  already chosen to send this client the full-resolution stream, so the burn
  adds an encode, not a downscale (and costs *less* bandwidth than the remux
  it replaces). Only a burn on a genuine transcode verdict keeps the server's
  Auto rung (`min(source, 1080)` on hardware), where the cap is the bandwidth
  call it was designed to be. A bitmap burn keeps the node's proven GPU
  tone-map (PERF-PLAN §5): the graph scales and maps on the GPU pinned to the
  overlay's exact frame, comes down to system memory once for the composite,
  and the encoder's upload runs after it — and a Dolby Vision source whose
  base layer is HDR10-compatible counts as HDR10 for that routing, since
  neither chain reads the RPUs. Text burns still take the CPU chain (libass
  lives there). Whether a given box holds realtime on a 2160p burn remains a
  measurement, not a promise. Direct/remux/copy carry text subs as selectable
  `<track>`s, which toggle for free.
- **DTS/TrueHD never passthrough to a browser.** No browser decodes them, so a
  remux/copy always transcodes that audio to AAC. Passthrough is a
  native-client concern (see [CLIENTS.md](CLIENTS.md)).

Playback policy is verified without ffmpeg or a browser: the server decision,
encoder, pipeline, and argument builders are Rust units; the web transport and
fallback policy is a Node unit; Apple uses XCTest; Android uses JVM tests. Run
the fast policy layer with:

```bash
cargo test -p plurx-core playback        # server verdicts and remux hint
cargo test -p plurxd                     # execution plan and session routing
node tests/playback/web-policy.test.js   # shipped browser policy
make apple-test                          # Apple transport/fallback/subtitles
make android-test                        # Android caps/fallback/subtitles
```

The inventory itself is kept honest by
`tests/validation/test_playback_routing_inventory.py`, which runs in the commit
gate. What those units cannot prove — that a shipping decoder actually presents
the selected stream — belongs to the source × quality × operation playback lab
in [PLAYBACK-TESTING.md](PLAYBACK-TESTING.md), not to another guessed codec
table.
