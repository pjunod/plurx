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
      │           { method, play_url, reasons[], transcode_audio, audio[], subs[], markers[] }
      │
      ├─ direct_play ─▶ play_url = /files/{id}/direct       (HTTP range)
      ├─ remux ───────▶ play_url = /files/{id}/stream.mp4    (progressive fMP4)
      └─ transcode ───▶ (client then calls /files/{id}/hls/start)
      │
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

## Runtime caps — what the client tells the server

Before the first `/decision`, the web player probes what this exact browser can
decode and sends it as query params (`PLAY_CAPS` in `web/index.html`). The
server folds them into an ad-hoc device profile (`caps_profile` in
`plurx-core/src/playback`), so a file only transcodes when *this* browser
genuinely can't play it — not because a fixed profile guessed conservatively.

| Cap | Probed with | Notes |
|---|---|---|
| `vcodec` | `canPlayType` + `MediaSource.isTypeSupported` | `h264` always; `hevc`/`av1`/`vp9` when the browser answers. Safari says yes to HEVC; Chrome-on-macOS via the OS decoder. |
| `acodec` | `canPlayType` | `aac`,`mp3` always; `ac3`/`eac3` where supported (Safari), `opus`/`flac` per browser. |
| `container` | fixed | `mp4,webm,mov` — what a browser `<video>` accepts as a file. Notably **not** `mkv`. |
| `hdr` | `matchMedia("(dynamic-range: high)")` | `1` only on an HDR display *and* an HDR-capable codec — else the server tone-maps, because HDR on an SDR screen looks washed-out. |

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

## Delivery — the client's transport choice

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

## Copy-video HLS — the remux path Safari can play

This is the fork that keeps Safari at source resolution. Without it, Safari
would reject the progressive fMP4 remux, and the player's error-fallback would
re-encode the whole 4K stream down to 720p (see next section) — that was a real
bug: identical file, Chrome kept 4K, Safari dropped to 720p.

**What it does:** copies the source video into HLS *untouched* — the original
4K HEVC/HDR bitstream, no re-encode — and transcodes only the audio when the
browser can't take the source codec. It is a remux, packaged as HLS.

**How it's built** (`hls_copy_args` in `plurx-core/src/transcode`, driven by
`TranscodeManager::start_copy`):

```
 ffmpeg -ss <resume> -re -i <file>
        -map 0:v:0 -c:v copy [-tag:v hvc1]        # video untouched; hvc1 so Safari decodes HEVC
        -map 0:a:<n> -c:a aac -b:a 256k           # audio → AAC only when needed (else -c:a copy)
        -f hls -hls_segment_type fmp4             # fMP4 segments, NOT mpegts
        -hls_fmp4_init_filename init.mp4          #   (Apple does not carry HEVC in a TS container)
        -hls_segment_filename seg%05d.m4s
```

Three details, each load-bearing:

- **fMP4 segments, not MPEG-TS.** Apple does not support HEVC inside a TS
  container; the transcode path's `.ts` segments would silently fail on Safari.
  The copy path emits `init.mp4` + `segNNNNN.m4s` and the segment handler serves
  them as `video/mp4`.
- **`-tag:v hvc1`.** MKV HEVC is usually tagged `hev1`, which Safari renders as
  a black frame; the sample entry must be `hvc1`. Harmless if already hvc1.
- **`-re`.** Copy runs as fast as the disk allows; without pacing, a 45 Mb/s 4K
  session would dump the whole file into the session dir at once. `-re` holds it
  to ~1× real time, so a seek or an abandoned session is reaped before much
  lands.

**Seek and audio-switch stay on this path.** A copy-HLS session sets
`PLAYER.method = 'remux'` (honest — no video re-encode) and `PLAYER.copyHls =
true`. The flag is what makes seeking and audio-switching re-open the HLS
session (`startCopyHls`) instead of falling back to the progressive
`/stream.mp4` Safari can't play. Without the flag, the first seek would
re-break it.

**Wiring:** `GET /files/{id}/hls/start?copy=1&aac=<0|1>` — `copy=1` selects the
copy session; `aac=1` says the audio needs transcoding (the client already
learned that from `decision.transcode_audio`). Everything else — playlist and
segment serving, the idle reaper, the fail-fast watchdog — is the shared HLS
session machinery.

## The error fallback — and the stale-reason trap

Any direct/remux stream the browser rejects gets exactly one automatic rescue:
restart as a guaranteed-compatible transcode.

```
 <video> fires "error" on a direct_play or remux stream   (once per session)
        │
        ▼
 startTranscodeFallback()  ─▶  /hls/start?height=720  ─▶  full H.264 transcode
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

## Resume & progress

- **Resume** rides the same input-seek on every path: `?start=<seconds>` on
  `/direct`/`/stream.mp4`, or `hls/start?start=…`. For HLS sessions (transcode
  and copy) the session begins at the resume point, so the player tracks a
  `PLAYER.offset` and reports `offset + video.currentTime` as the true position.
  Direct play needs no offset — `currentTime` is the timeline.
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

## Non-goals & known limits

- **HLS session disk.** An HLS session's playlist grows for its whole life, so
  the reaper prunes segments more than ~60 s behind the playhead — on both the
  transcode and copy paths — keyed off the highest segment the client has
  fetched. A 4K copy session is bounded to a rolling window instead of hoarding
  the ~17 GB a full watch would otherwise accumulate. Two residuals remain: a
  fast transcode can still race *ahead* of the playhead and write future
  segments up to the transcoded file's size (bounded by the pre-existing
  behavior, and at the lower transcode bitrate); and the prune is disk-only —
  the playlist keeps listing pruned entries, which is safe precisely because
  every seek starts a fresh session rather than scrubbing back into a deleted
  window.
- **No client-side bitrate adaptation yet.** One encode runs at a time; the
  rung is chosen at start, not adapted per segment. The design for that is
  [ADAPTIVE-QUALITY.md](ADAPTIVE-QUALITY.md).
- **Bitmap subs (PGS/VobSub) can't be copied or `<track>`'d.** They only appear
  via a transcode that burns them in; direct/remux/copy carry text subs as
  selectable `<track>`s only.
- **DTS/TrueHD never passthrough to a browser.** No browser decodes them, so a
  remux/copy always transcodes that audio to AAC. Passthrough is a
  native-client concern (see [CLIENTS.md](CLIENTS.md)).

Playback correctness is verified without ffmpeg or a browser: the decision
engine and the ffmpeg arg builders are pure functions with unit tests
(`plurx-core/src/playback`, `plurx-core/src/transcode`). What those tests can't
cover — that a given browser actually plays a given stream — is exactly what the
per-browser transport table above encodes, learned from device testing.
