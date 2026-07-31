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
same `init.mp4`, same `segNNNNN.m4s`, same EVENT playlist — and a stream the
reader cannot follow falls back to ffmpeg's own muxer once, automatically, so
the worst case is the behaviour above.

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
  segment the client fetched, the reaper SIGSTOPs its ffmpeg, and SIGCONTs it
  once the viewer is within half that. This is the bound `-re` used to
  provide, minus the part where `-re` also capped the buffer. A stopped
  process costs nothing, resumes instantly, and — unlike a rate limit —
  adapts to a viewer who pauses. SIGKILL works on a stopped process, so the
  idle reaper and the admin stop button need no special case.

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

The overlay closes when *you* close it — the `i` key or its own ✕ — and stays
open while you use the Quality, Audio, Subtitles and Sync menus, which is the
point: the reason to have it open during a quality change is to watch what the
change does to it. The menu slides clear of the panel rather than sitting on
top of it, and falls back to overlapping only in a window too narrow to hold
both side by side. On a touch screen the two still take turns, because a phone
has room for one of them and no keyboard shortcut to escape whichever is
covering the other.

## Non-goals & known limits

- **HLS session disk.** An HLS session's playlist grows for its whole life, so
  the reaper prunes segments more than ~60 s behind the playhead — on both the
  transcode and copy paths — keyed off the highest segment the client has
  fetched. Ahead of the playhead, the suspend window bounds the other end, so
  a session's directory now holds roughly
  `hls_ahead_max_secs + 60 s` of content whatever the encoder's speed. One
  residual remains: the prune is disk-only — the playlist keeps listing pruned
  entries, which is safe precisely because every seek starts a fresh session
  rather than scrubbing back into a deleted window.
- **No client-side bitrate adaptation yet.** One encode runs at a time; the
  rung is chosen at start, not adapted per segment. The design for that is
  [ADAPTIVE-QUALITY.md](ADAPTIVE-QUALITY.md).
- **Bitmap subs (PGS/VobSub) cost a stream restart.** They can't be copied or
  `<track>`'d — a picture has no text to send — so selecting one re-opens the
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

Playback correctness is verified without ffmpeg or a browser: the decision
engine and the ffmpeg arg builders are pure functions with unit tests
(`plurx-core/src/playback`, `plurx-core/src/transcode`). What those tests can't
cover — that a given browser actually plays a given stream — is exactly what the
per-browser transport table above encodes, learned from device testing.
