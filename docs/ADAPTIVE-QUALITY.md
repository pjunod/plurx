# Adaptive quality — the design for bandwidth-aware streaming

Companion to [ARCHITECTURE.md](ARCHITECTURE.md) (how it's built) and
[FEATURES.md](FEATURES.md) (what it does). This is a *design document*: what
"adjust quality automatically" means for a just-in-time transcoder, what's
already in place, and a phased plan with effort and risk called out.

The guiding fact: **Netflix and YouTube pre-encode a whole quality ladder
offline, then let the client hop between renditions per segment. cinemarr
encodes just-in-time on one GPU.** Encoding every rung of a ladder
simultaneously would multiply GPU load per stream — a non-starter on an iGPU
that can stall with two QSV sessions. So the design puts the *adaptation
brain in the client* and keeps *exactly one encode running*, switching what
that one encode produces. This is the same call Plex and Jellyfin made, and
it's the right one for a homelab.

One property makes restart-based adaptation unusually effective here: the
client measures segment download throughput, which on a JIT server is
`min(network speed, encode speed)`. Stepping down a rung cures **both**
bottlenecks — less data over the wire *and* a cheaper encode that runs
faster. The controller doesn't need to know which one was the problem.

## What's already in place

More than half the plumbing exists today; adaptive quality is an extension,
not a rebuild.

| Piece | Where | State |
|---|---|---|
| Rung parameter | `GET /files/:id/hls/start?height=` (clamped 144–2160) | done |
| Height → bitrate ladder | `bitrate_for_height()` in `plurxd/src/transcode.rs` (2160→20 Mb/s, 1080→8, 720→4, 480→2, else 1.2) | done |
| Segment-aligned keyframes | `-force_key_frames expr:gte(t,n_forced*4)` + `hls_time 4` in `hls_args` | done |
| Mid-stream session restart | the seek and audio-switch paths already call `hls/start?start=…` and re-attach via `attachHls()` | done |
| Never upscale | `video_filters()` refuses to scale above source height | done |
| Session lifecycle | idle reaper (60 s), first-segment watchdog, software self-heal | done |
| Client stream health | `hls.bandwidthEstimate`, `waiting` events, stall self-diagnosis | available |
| Bounded rung bitrate | `-maxrate`/`-bufsize` | **software & NVENC only** — QSV/VA-API/VideoToolbox get bare `-b:v` |

That last row is the one real server gap: a rung is only meaningful to an
adaptation controller if its bitrate is *bounded*. A "4 Mb/s" QSV encode that
bursts to 12 Mb/s on a grain-heavy scene defeats the estimate.

## The ladder

| Rung | Height | Video cap | Audio | ~Total |
|---|---|---|---|---|
| 1080p | 1080 | 8 Mb/s | AAC 160 kb/s | 8.2 Mb/s |
| 720p | 720 | 4 Mb/s | AAC 160 kb/s | 4.2 Mb/s |
| 480p | 480 | 2 Mb/s | AAC 160 kb/s | 2.2 Mb/s |
| 360p | 360 | 1.2 Mb/s | AAC 160 kb/s | 1.4 Mb/s |

Rungs at or above the source height are dropped (a 720p file offers 720p and
below). 4K output rungs are deliberately absent: a browser session that can
take 20 Mb/s sustained is better served by direct play or remux — transcoding
4K→4K burns GPU for nothing. "Original" (direct play / remux) sits above the
ladder and is not adaptive; see "Adjacent wins" for its rescue path.

## Phase 1 — the ladder made real, with a manual Quality menu

*Server*: add `-maxrate` (1.5×) and `-bufsize` (2×) to the QSV, VA-API, and
VideoToolbox arms of `Encoder::encode_args` — software and NVENC already have
them. Snap `height` to the nearest rung in `hls/start`, and return the
ladder (each rung's height + total kb/s, source-height filtered) in the
`start` and `decision` responses so the client never hardcodes it.

*Client*: a Quality entry in the player menu — `Auto · 1080p · 720p · 480p ·
360p` — using the exact restart machinery the audio-switch path uses today
(same `hls/start?height=…&start=<now>` call, same `attachHls`). Persist the
choice in localStorage; show the active rung in the Stats overlay.

Effort: **small** — one sitting. Risk: low; every mechanism is already
exercised elsewhere.

## Phase 2 — Auto (the actual feature)

A client-side controller, roughly 120 lines, extracted as a pure function so
it unit-tests without a video element:

- **Sample** every 5 s: `hls.bandwidthEstimate` (hls.js's EWMA over real
  segment downloads), stalls (a `waiting` event after playback started, or
  hls.js `bufferStalledError`), and buffer runway
  (`buffered.end − currentTime`).
- **Down** one rung when a stall lands, or when the estimate sits below
  1.3× the current rung's total bitrate for two consecutive samples.
  Restart at the current position; 20 s cooldown between any two switches.
- **Up** one rung when the estimate exceeds 1.8× the *next* rung's bitrate
  for 45 s with no stall in the last 60 s — and never above the player's
  actual pixel height (a 1080p encode into a 700-px window is waste).
- **Start** at the persisted last-good rung (default 720p), so a session on
  known-bad Wi-Fi doesn't relearn the lesson at 1080p.

Asymmetric thresholds (down fast, up slow) are the whole trick of ABR;
these constants are starting points to tune on real use.

The switch itself is the honest cost of the JIT model: a restart, not a
seamless splice. QSV spins up in ~1–3 s and the buffer usually covers it; if
the buffer runs dry the loading overlay says "Adjusting quality…" and the
toast names the move (`Quality → 480p — bandwidth`). Plex behaves the same
way. Every switch is logged to the Stats overlay with its reason, so "why did
it get blurry" always has an answer.

Server changes: none required beyond Phase 1.

Effort: **medium** — one focused session including tests. Risk: low-medium;
the failure mode of a mistuned controller is a visible switch, not a broken
player, and Manual remains one menu tap away.

## Phase 3 — seamless switching (optional, the majors' UX)

True multivariant HLS, adapted to JIT: `master.m3u8` advertises every rung
(`EXT-X-STREAM-INF` with `BANDWIDTH`/`RESOLUTION`); each variant's playlist
and segments live at `/hls/:session/:rung/…` and its encoder starts *lazily*
on first request. Because `-force_key_frames` already cuts every variant at
t = 4n, segment N is the same time window in every rung; a variant joining
mid-timeline starts with `-ss 4n`, `-start_number n`, and an
`-output_ts_offset` for PTS continuity. hls.js's native ABR then does all
switching seamlessly — the Phase 2 controller is deleted, replaced by
`capLevelToPlayerSize: true`. A variant reaper kills encoders nobody has
fetched from in 30 s, so steady-state stays at one active encode (brief
two-encode overlap during a switch; the existing watchdog already covers GPU
contention).

Effort: **large** — the session model in `plurxd/src/transcode.rs` becomes
session→variants, with per-variant watchdog, self-heal, and reaping, plus
real PTS-continuity testing across all five encoders. Risk: highest of the
three. **Decision gate:** ship Phases 1–2, live with the switch blip for a
week, and build this only if it actually grates.

## Adjacent wins along the way

Direct play and remux sit outside the ladder, but their failure mode is the
same starved network — a 69 Mb/s remux over hotel Wi-Fi buffers forever. The
player already rescues a *rejected* stream by restarting as a transcode;
extending that to *repeated stalls* (≥3 in 60 s → drop into transcode Auto)
closes the last "it just buffers" hole. Cheap, and worth folding into
Phase 2.

## Summary

| Phase | What you get | Effort | Risk |
|---|---|---|---|
| 1 — Ladder + menu | Bounded rungs, manual quality control, ladder API | Small | Low |
| 2 — Auto | Bandwidth-adaptive streaming (Plex-class) | Medium | Low-med |
| 3 — Seamless | Blip-free switching (Netflix-class UX) | Large | High |

Recommended: build 1 + 2 together; hold 3 behind the decision gate.

## Test plan

Offline, in the existing Playwright harness: the controller as a pure
function (`decideRung(estimate, stalls, rung, ladder)`) gets a table-driven
unit test; end-to-end, the stub server's throttle knob simulates a bandwidth
cliff mid-stream and the test asserts a `hls/start` re-call at a lower
height, then recovery after the throttle lifts. On nynuc: play a 4K HDR
title, clamp the client with browser DevTools network throttling to 3 Mb/s,
and watch the Stats overlay walk down to 480p and back.
