# Performance — where the seconds go, and the plan to get them back

**Status:** M1 complete; **M2 landed its selection half** (2026-07-28) —
the pipeline abstraction, the boot probe, per-session routing and the
runtime downgrade are shipped and tested. What remains in M2 is the
capacity/admission work (`transcode.max_hw_sessions`) and the acceptance
run, which needs nynuc: no GPU in CI means no candidate can pass its probe
there, only fail correctly · **Diagnosed against:** `e7a12cf`
· **Review record:** [PERF-PLAN-REVIEW.md](PERF-PLAN-REVIEW.md) →
[PERF-REVIEW-RESPONSE.md](PERF-REVIEW-RESPONSE.md) →
[PERF-REVIEW-ASSESSMENT.md](PERF-REVIEW-ASSESSMENT.md) ·
**Companions:** [PLAYBACK.md](PLAYBACK.md) (the delivery map),
[ADAPTIVE-QUALITY.md](ADAPTIVE-QUALITY.md) (the ABR design this plan
partially executes), [ARCHITECTURE.md](ARCHITECTURE.md) §3,
[PHASE3-SPIKE.md](PHASE3-SPIKE.md) (the failover property M4 builds on).

This is a handoff plan: an implementing agent should be able to build each
milestone from this document plus the code. Work milestone by milestone, in
order — M0 before anything, because two of the three symptoms have two
plausible causes each and the fix differs. Line references are against
`e7a12cf`; **re-verify every cited line at build time** — the file is the
truth, this doc is the map. If a step seems to require changing something
§9 forbids, stop and flag it instead of improvising.

The deployment this plan is tuned for (Paul, 2026-07-28): servers are
Intel NUC / Gigabyte Brix boxes (i5–i7 iGPUs, QSV/VA-API) plus a few recent
AMD NUCs; media lives on a NAS mount (NFS/SMB); symptoms observed in Chrome
and via AirPlay from Safari on a MacBook Pro; the cluster picture is 2–3
similar Intel boxes.

## 1. The symptoms, and what each one actually is

| Reported | Root cause (primary) | Contributing | Fixed by |
|---|---|---|---|
| ~10 s to start streaming | 4 s first segment × sub-realtime encode, plus player wants more than one segment (§2.1) | `/decision` runs a live ffprobe on the NAS (§2.4); 12 s watchdog can misfire into a *slower* restart (§2.5) | §4.4, §4.1, §4.5, §5 |
| 4K starts, buffers a few seconds in, almost always | copy-video HLS is paced at `-re` = 1× — the player's opening buffer is the only buffer it will ever have (§2.2) | client buffer targets provisional, M0-owned (§2.3 — the original byte-cap claim was wrong; see §2.8) | §4.2, §4.3 |
| Stutter/buffer every ~30 s | transcode production runs *below* 1×: the CPU float tone-map chain can't feed a 4K session (§2.2) | unbounded QSV/VA-API bitrate bursts defeat the client's estimate (§2.6-B6) | §5, §4.6, §6 |
| AirPlay "does not work well" | same `-re` pacing — an Apple TV wants ~3 segments before it plays, so start ≈ 12 s and runway ≈ 0 forever | native HLS: no client-side knobs at all, the server pace is the whole story | §4.2 |

None of these is network-versus-server guesswork after M0: the
instrumentation milestone exists precisely to attribute each stall to
"encoder can't keep up" vs "link can't keep up" before code changes ride on
the assumption.

## 2. Where the time goes today

### 2.1 Press play — the serial path

```
 click Play
   │
   ▼
 GET /decision ──────── ffprobe chapters on the NAS ── 0.1–2 s   (§2.4)
   │                    (every play, cached nowhere)
   ▼
 GET /hls/start ─────── reap + spawn ffmpeg ────────── ~0.1 s
   │
   ▼
 ffmpeg opens input ─── NAS open + seek + analyze ──── 0.3–1 s
   │
   ▼
 first segment ───────── 4 s of content ÷ encode speed  4 s at 1.0×
   │                                                    8 s at 0.5×
   ▼
 playlist appears ────── (only after seg0 completes; the
   │                     playlist endpoint long-polls it)
   ▼
 player buffers ──────── hls.js starts after ~1 segment;
   │                     Safari/Apple TV want ~3         (+0–8 s)
   ▼
 first frame
```

Every stage is serial. The floor is roughly
`ffprobe + spawn + open + SEGMENT_SECONDS/speed + player_threshold`.
With a healthy hardware encode (≥2×) that lands around 3–5 s; with the CPU
tone-map chain at 0.5–0.8× it is exactly the reported ~10 s.

### 2.2 The pacing table — the core finding

Each delivery path paces production differently, and the two HLS paths got
the wrong ends of the stick:

| Path | Pacing today | Effect |
|---|---|---|
| Direct play | client-driven (HTTP range) | fine |
| Remux `/stream.mp4` | `-readrate 4.0` + 30 s flat-out burst (`stream.rs:56-58`) | fine — this is the model the others should follow |
| **Copy-video HLS** | **`-re` = 1.0×, no burst** (`plurx-core/src/transcode/mod.rs:343`, second input `:354`) | start waits realtime for every opening segment; steady-state runway permanently ≈ 0; any jitter is a visible stall. This is the Safari/AirPlay 4K experience. |
| **Transcode HLS** | **unpaced** (`hls_args` has no rate flags) | production is encoder-bound. Faster than 1× → races ahead of the playhead, writing the whole film at transcode bitrate (disk, GPU-time waste — acknowledged in [PLAYBACK.md](PLAYBACK.md) non-goals). Slower than 1× → the every-30 s stutter: the opening buffer drains at (1 − speed) per second played, stalls, rebuilds a little, stalls again. |

The `-re` on the copy path was deliberate — an unpaced `-c copy` of a 4K
film would land ~20 GB/h in the session dir (`mod.rs:340-343` comment).
That concern is real but is now solved twice over by better tools: the
behind-playhead segment GC (`transcode.rs:42,804`) and the ahead-window
suspend this plan adds (§4.2). Pacing at exactly 1× to protect the disk
spends the user's buffer to save gigabytes the reaper already reclaims.

Why transcode production is below 1× on this hardware: the HDR→SDR chain
(`transcode/mod.rs:105-109`) is
`zscale → format=gbrpf32le → tonemap=hable → zscale → yuv420p` — planar
**32-bit float RGB** on the CPU. At 3840×2160 that intermediate is ~100 MB
*per frame*, ~2.4 GB/s of memory traffic at 24 fps, before the tonemap
math itself. On a NUC-class CPU this chain alone caps well below realtime
for 4K sources, and the pipeline is decode(GPU) → `hwdownload` →
**CPU wall** → `hwupload` → encode(GPU) (`decode_setup`,
`transcode/mod.rs:183-199`). The GPU sits mostly idle while the CPU
tone-maps. §5 moves the whole chain onto the GPU.

### 2.3 The client's own ceilings — corrected after review

**The first version of this section was wrong**, and shipped a wrong code
comment before review R1 caught it. It claimed `maxBufferSize` (60 MB)
binds before `maxBufferLength` (30 s), capping 4K at 7–11 s. The vendored
hls.js 1.6.16 says otherwise, in code:

```js
getMaxBufferLength = function(e){ var t, r = this.config;
  return t = e ? Math.max(8*r.maxBufferSize/e, r.maxBufferLength)
               : r.maxBufferLength,
  Math.min(t, r.maxMaxBufferLength) }
```

`maxBufferLength` is a **floor** — the byte value can only extend the
target beyond it, never cap below. The stock player already targeted 30 s
of 4K. What actually bounds a 4K forward buffer on Chromium is the
browser's own MSE quota, whose limits vary by platform, device memory
class, and version (desktop ≠ Android ≠ low-memory), surfaced as
`QuotaExceededError` → hls.js `BUFFER_FULL_ERROR` → hls.js shrinks its own
target. No hls.js number raises that quota, and no single figure for it
belongs in this plan — which is why the client values are **provisional
and M0-owned** (§8.6, decision 1). Safari-native and Apple TV sessions
have no knobs at all; for every client class the server-side reserve from
§4.2 is the primary fix, and the client config is a second-order tune.

### 2.4 `/decision` does NAS I/O on the critical path

`markers_for` (`http/stream.rs:457-471`, called at `:559`) runs a live
`ffprobe -show_chapters` against the media file on **every** play, and the
scan-time probe never captured chapters (`scan/probe.rs:55-63` has
`-show_format -show_streams` only). On a NAS mount with a cold attribute
cache that is an avoidable 0.1–2 s of the click-to-frame path, paid every
single time for data that never changes between scans.

### 2.5 The watchdog can't tell slow from stalled

The hardware→software fallback fires on a binary check — "is a finished
segment listed?" — after `FIRST_SEGMENT_GRACE` = 12 s
(`transcode.rs:29,88`). A healthy-but-slow session (heavy 4K decode, CPU
tone-map at 0.7×) that needs 13 s for its first segment gets **killed and
restarted on software**, which is strictly slower, turning a slow start
into a ~45 s one (12 s wasted + software window `:35` = 30 s more) or a
spurious failure. ffmpeg can report progress (`-progress`); nothing reads
it today, so "stalled" and "slow" are indistinguishable. §4.5 fixes the
telemetry; it also feeds the activity page and M0.

### 2.6 The full bottleneck inventory

Numbered for citation; each names its evidence.

- **B1 — copy-HLS `-re`.** §2.2. `transcode/mod.rs:343`.
- **B2 — CPU float tone-map wall.** §2.2. `transcode/mod.rs:105-109`.
- **B3 — live ffprobe in `/decision`.** §2.4. `stream.rs:559`.
- **B4 — 4 s first-segment floor.** `SEGMENT_SECONDS = 4`
  (`transcode/mod.rs:20`), keyframes forced to that grid, playlist appears
  only when seg0 completes (`transcode.rs:651-662` long-polls it).
- **B5 — client buffer targets (hypothesis, retired as stated).** §2.3.
  The byte-cap mechanism was disproven against the vendored source; what
  remains is "the defaults were sized for 1080p web video and the right 4K
  values are unknown" — an M0 measurement, not a diagnosed cause.
- **B6 — unbounded hardware bitrate.** QSV/VA-API/VideoToolbox get bare
  `-b:v`, no `-maxrate`/`-bufsize` (`encoder.rs:99-104`) — the gap
  [ADAPTIVE-QUALITY.md](ADAPTIVE-QUALITY.md) already calls "the one real
  server gap": a "4 Mb/s" rung that bursts to 12 defeats both the wire and
  the client's bandwidth estimate.
- **B7 — binary watchdog.** §2.5. `transcode.rs:29,88,417-480`.
- **B8 — Auto quality transcodes at 720p.** `transcodeHeight()` returns
  720 unless the user picked a height (`index.html:1835`) — every
  tone-mapped 4K session renders at 720p by default. A policy choice worth
  revisiting once encode speed allows (§4.7), not a bug.
- **B9 — seeks restart everything.** Every non-direct seek tears down the
  session and pays the full §2.1 path again (`index.html:2331-2378`,
  by design — `reap_superseded`). Cached VOD assets (§6) make seeks free
  for cached items; live-session seek cost is otherwise accepted.

### 2.7 What shipped 2026-07-28, and what it measured

M0, §4.1, §4.2, §4.3 and §4.5 are built. Verified on a live server (not just
unit tests) with a 5-minute 720p fixture; the numbers below are from that run
and are the baseline the remaining milestones are judged against.

- **Chapters no longer probe on play (§4.1).** Proved by breaking ffprobe
  outright (`PLURX_FFPROBE=/nonexistent`): a file with chapters in its scan
  probe still returned both real markers, and a file whose `chapters` key was
  stripped fell back to the duration guess. The one-time backfill was observed
  writing the key back into `probe_json`.
- **Pacing reaches the command line (§4.2).** Live session args carry
  `-readrate_initial_burst 30.0 -readrate 2.00`; the copy path's `-re` is gone.
- **The head start is real.** 33 s of content produced in the first 5 s of a
  session — under the old realtime pacing that number was 5 s, which is the
  whole "starts, then buffers" bug in one measurement.
- **The suspend cycle works.** With a deliberately small 20 s window: suspend
  at 48 s ahead → output frozen at exactly the same `out_time` across four
  polls → viewer fetched 13 segments → `ahead` went to −1 s → resumed → refilled
  to 59 s → suspended again. SIGKILL on a suspended child still works, so the
  reaper and the stop button needed no special case.
- **Telemetry (M0).** `GET /hls/:session/status` reports `speed`,
  `out_time_ms`, `ahead_seconds`, `suspended`; the player's stats overlay grew
  a Server block, and TTFF/stall beacons land in Settings → Logs.
- **Not yet measured on real hardware:** every number above is from software
  x264 on a fixture, in a sandbox with no GPU. The 4K-HDR encode-speed
  baseline that §5 must beat still has to be captured on nynuc — that is the
  first thing to do before starting M2.

Two deviations from the plan as written, both deliberate:

1. **§4.5 came early.** The plan scheduled the progress-aware watchdog for
   the second slice, but §4.2 forces the question — a suspended session makes
   no progress, and the old watchdog would have read that as a wedge and
   killed it. Given the telemetry already existed, the honest fix was the real
   one. It also became a *poll* rather than a single verdict at a deadline,
   which catches a session that produces a few seconds and then wedges — a
   case the old one-shot check missed entirely.
2. **A settings-UI bug found by testing, fixed in passing.** A stored value
   that isn't one of the dropdown presets (say `20`, set through the API)
   rendered as a different preset, and the next Save would have written that
   one back — silently changing a setting nobody touched. `presetOpts` now
   shows the stored value as its own option. The pre-existing Delivery-speed
   dropdown has the same shape and could adopt it.

### 2.8 The review cycle, and the correction pass it produced

After Weekend 1 shipped, the plan went through a two-round review — the
review ([PERF-PLAN-REVIEW.md](PERF-PLAN-REVIEW.md)), this plan's response
([PERF-REVIEW-RESPONSE.md](PERF-REVIEW-RESPONSE.md)), and an assessment of
that response ([PERF-REVIEW-ASSESSMENT.md](PERF-REVIEW-ASSESSMENT.md)).
Every contract in this document now reflects the cycle's outcome; the
record of who argued what lives in those three files, not here. Three
findings touched shipped code, and they define the **correction pass — the
next work item before anything else in §10**:

1. **R1 — the client buffer diagnosis was backwards** (§2.3 has the
   corrected mechanism, verified against the vendored hls.js). Code:
   revert `maxBufferSize` to default, keep `maxBufferLength: 60` and
   `backBufferLength: 30` as *provisional*, rewrite the `attachHls`
   comment, and add classified buffer-recovery beacons (quota pressure ·
   append failure · media error · network starvation) so M0 owns the final
   values. The `performance.memory` acceptance is void — JS heap does not
   see SourceBuffer; long-run memory checks use process/media measurements.
2. **R2 (as tightened by the assessment) — three frontiers, one honest
   clock each.** The shipped ahead-window mixes ffmpeg's `out_time` (which
   includes frames muxed into the in-progress `.tmp` segment — encoder
   progress, not fetchable media) with index×duration on the fetched side
   (wrong for variable-GOP copy). The corrected accounting is §4.2's
   contract: `encoder_out_time` for the watchdog only,
   `produced_playable_end` from completed playlist segments (`EXTINF`) for
   pacing, `fetched_end` from the served segment's actual `EXTINF`
   boundary. Retention grows to cover the client's forward-fetch distance
   (formula in §4.2), and a per-session **and** global byte budget joins
   the time window.
3. **R9 — attempt generations.** The hardware→software fallback resets a
   `Progress` shared with the old child's still-draining stdout reader; a
   stale line can land after the reset. Every process attempt gets a
   generation; updates apply only when it matches. The activity page's
   `encoder` label moves into per-attempt state (it currently keeps saying
   the hardware name after a software fallback), and a recent-speed EWMA
   joins the cumulative figure. The already-true invariant that status
   polling never touches the idle clock gets a regression test.

The pass also lands the review's lifecycle correction (§8.5 — idempotent
`POST` creation, explicit `DELETE`, `playback_id`-scoped supersession) and
the cheap play-path items from R11 (availability cached with a short age,
Trakt "watching now" moved from `/decision` to real media delivery, completed
segments streamed from disk instead of buffered as ~35 MB `Vec`s, immutable
cache headers on segment bodies).

**Shipped 2026-07-28**, six commits, each independently revertable:

| Commit | Contents |
|---|---|
| `1d8d2e2` | R1: byte override reverted, buffer pressure classified, beacon measurements actually reaching the log (the `ClientLog` DTO named none of them, so serde had been dropping every number), attempt ids + reasons, deliberate seeks no longer counted as stalls |
| `36b56b8` | R9: attempt generations, per-attempt encoder label, recent-speed EWMA as a pure function, the status-vs-idle-clock regression test |
| `8561879` | R2: `SegmentMeta`/`SegmentIndex` from `EXTINF`, the three frontiers separated, retention derived from the client's forward buffer, per-session + global byte budgets, event-driven flow control |
| `5f75fc7` | Segments streamed from an open handle instead of a ~35 MB `Vec`, `private, immutable` caching |
| `3fd1d38` | §8.5 lifecycle: `POST …/hls/sessions` with idempotency, `DELETE …`, supersession by `playback_id`, GET kept as a deprecated bridge |
| `18f8802` | `/decision` made pure: availability cached (presence only), Trakt start moved to real delivery in a detached task |

### 2.9 The corpus, measured (software, 2026-07-28)

`scripts/bench` builds the fixture matrix and plays it against a running
server. First run, all software x264 in a GPU-less sandbox — which is not
nynuc, but *is* enough to isolate the one cost this plan is built around:

| Fixture | TTFS | speed p10 | p50 | What it isolates |
|---|---|---|---|---|
| 1080p-h264 | 2.7 s | 1.99× | 2.04× | baseline; pacing holding at the configured 2× |
| sparse-gop | 2.5 s | 1.98× | 2.07× | long-GOP copy segmentation |
| grainy | 4.3 s | 1.11× | 1.15× | rate-control under grain |
| **4k-hevc-sdr** | 5.4 s | **0.98×** | 1.00× | 4K decode **without** tone-mapping |
| **4k-hdr10** | 6.7 s | **0.71×** | 0.75× | 4K decode **with** the CPU float tone-map |
| 4k-hlg | 7.1 s | 0.69× | 0.73× | same, HLG transfer |

**The middle two rows are the plan's central claim, measured.** Same
resolution, same codec, same rung, same encoder — the only difference between
them is the HDR→SDR chain, and it costs roughly a quarter of the pipeline's
throughput and takes it from realtime to *below* realtime. A session at 0.75×
drains a viewer's reserve by a second for every four seconds played, which is
precisely the "stutters every ~30 s" report §2.2 attributes to it. §5 is
therefore aimed at the right thing.

Two honest limits on the table. It is **software x264** — on hardware the
absolute numbers all move, and the question M2 must answer is whether the GPU
graph clears 1.0× on *Paul's* silicon, which only nynuc can say. And TTFS is
the **server's** half of time-to-first-frame (start → first playable segment);
the client's buffering threshold sits on top, which is why §4.4's shorter
segments matter more than these figures alone suggest.

Two things worth carrying forward. The **client buffer values remain
provisional** — the beacons that were supposed to justify them were dropping
their numbers on the floor until `1d8d2e2`, so M0 still owes the decision in
§8.6-1 its evidence. And the **fixture matrix has not been run**: everything
above was verified on a 720p software fixture and a live browser, which
proves the plumbing and nothing about 4K HDR on real hardware.

## 3. M0 — instrument first

**Objective:** attribute each real-world stall to encoder speed, server
pacing, or the link, with numbers, before touching the pipeline. Also the
before/after evidence for every later milestone.

- **Server: encode telemetry.** Spawn session ffmpeg with
  `-progress pipe:1` (stdout is currently `null` for HLS sessions —
  repoint and parse the `key=value` blocks: `out_time_us`, `speed`,
  `frame`). Store per session: last out_time, speed, updated-at. Extend
  `SessionInfo` and the activity page with `speed` (e.g. `1.7×`). This is
  shared plumbing with §4.5.
- **Server: stage timings.** `tracing` spans (or explicit `Instant` logs)
  around: `/decision` total + `markers_for` alone; `/hls/start` total;
  spawn→playlist-exists; playlist-exists→first-segment-served. All under
  `plurxd::transcode` so the log view's filter shows a session's whole
  start story in order.
- **Client: TTFF + stall beacons.** The player already ships structured
  client logs (`clientLog`, `index.html` — `hls_fatal` event). Add:
  `ttff` (click → first `playing`, with method/encoder/height),
  `stall` (each `waiting` after start, with `video.buffered` runway at
  that moment, `hls.bandwidthEstimate` when hls.js is driving). Every
  beacon carries a **playback-attempt id and a reason** (cold-start ·
  resume · seek · audio-switch · manual quality · automatic quality ·
  failover) so restarts don't pollute cold-start numbers, and intentional
  pause/seek never counts as a stall. Buffer-recovery events are
  classified (§2.8-1).
- **New endpoint:** `GET /api/v1/hls/:session/status` →
  `{ out_time_ms, speed, suspended, encoder }` (capability-auth like its
  siblings). Powers the stats overlay's new "encode speed" line (§4.5)
  and makes M0 measurable from the browser alone. Invariant, with a
  regression test: polling status never extends a session's idle
  lifetime — watching a stream's numbers is not fetching from it.

**Acceptance:** `scripts/bench fixtures` then `scripts/bench run` produces
the table in §2.9 on the target machine; the player's own beacons supply the
client half (runway, TTFF, stalls by kind) from Settings → Logs.
One evening of watching is discovery;
the acceptance *suite* is the review's measurement contract
([PERF-PLAN-REVIEW.md](PERF-PLAN-REVIEW.md) §16): distributions (p50/p95,
p10 for encode speed and runway) split by method, source class, and
client, over the fixture matrix listed there. That table is the baseline
every later milestone is judged against.

## 4. M1 — single-node quick wins

Each item is one sitting, independently shippable, ordered by expected
payoff-per-effort. §4.1 + §4.2 + §4.3 together are the "this weekend"
slice; expected combined effect is transcode starts ≈ 3–5 s (from ~10),
copy-HLS starts ≈ 2–3 s (from ~12), and steady-state runway that actually
grows.

### 4.1 Markers move to scan time (kills B3)

Add `-show_chapters` to the scan probe (`scan/probe.rs:61`) so chapters
ride in `probe_json` like everything else. `markers_for` reads the stored
JSON first (`Store::get_file_probe_json` already exists,
`stream.rs:619-626` uses it for the declared A/V offset); only when the
stored JSON has no `chapters` key (pre-upgrade probes) fall back to the
live ffprobe **once and persist the result** back into `probe_json`
(new store method, §8.2), so each legacy file pays at most one more probe.
The heuristic credits-guess fallback is unchanged.

**Acceptance:** second play of any title logs `markers: from probe_json`
and `/decision` total drops by the M0-measured ffprobe cost; a
chapterless file still gets its credits marker; unit test drives
`markers_for` from a fixture JSON with chapters, no ffprobe binary needed.

### 4.2 Pacing: burst everywhere, then hold a window (kills B1, bounds B4's tail)

Adopt the remux path's model — burst then throttle — for both HLS paths,
with an ahead-window suspend replacing `-re` as the disk bound.

- **Plumb pacing into the pure builders.** The `-readrate` capability
  probe lives in `http/stream.rs:63-120` (`PacingCaps`, correctly parsing
  declarations so `-re`'s help text can't fake it). Move it to a shared
  spot in plurxd and pass resolved values into `plurx-core` as data, so
  the builders stay pure and testable:

  ```rust
  /// Input pacing, resolved by plurxd (which probed the ffmpeg build).
  /// None = flag unsupported or pacing disabled.
  #[derive(Debug, Clone, Copy, Default)]
  pub struct Pacing {
      pub readrate: Option<f64>,       // multiple of realtime
      pub initial_burst: Option<f64>,  // seconds delivered flat-out (6.1+)
  }
  ```

  `hls_args(&file, encoder, &opts, pacing, out_dir)` and
  `hls_copy_args(&file, start, audio, aac, pacing, out_dir)` emit
  `-readrate_initial_burst` / `-readrate` before **each** `-i` (both
  inputs, like `-ss` — the A/V-offset second input included), replacing
  the copy path's two `-re`s. Defaults: **readrate 2.0, burst 90 s** for
  both HLS paths, settings-backed (§8.3). Remux keeps its existing
  `STREAM_READRATE` knob untouched.
- **Ahead-window suspend.** With burst 90 + readrate 2.0, a session gets
  ~90 s of runway almost immediately and then builds more at +1×/s —
  which un-bounded would still write tens of GB for a 4K copy. Bound it
  with the mechanism Plex uses: **pause the transcoder** —
  SIGSTOP/SIGCONT via `libc::kill(child.id())`, Linux/macOS only, which
  is where plurxd runs. The idle reaper and admin stop still `kill()`
  suspended children fine (SIGKILL wakes stopped processes), and the §4.5
  watchdog treats `suspended` as healthy (a stopped encoder makes no
  progress on purpose). Note the scope: this SIGSTOP is a **disk**
  mechanism on a *live* session, which keeps its hardware slot on purpose
  — its viewer owns it. It is not a capacity-release mechanism; see §6.2
  for why background work must terminate instead. Fallback if an ffmpeg
  without `-readrate` (< 5.1) is detected: keep `-re` on copy (the old
  behavior) and log the recommendation — same graceful degradation the
  pacing resolve has today.
- **The accounting — three frontiers, corrected per §2.8-2.** Three
  distinct clocks, never conflated, each named for what it measures:

  ```text
  encoder_out_time        ffmpeg -progress out_time. Includes frames in
                          the in-progress .tmp segment. Watchdog + speed
                          telemetry ONLY — it is proof of motion, not of
                          fetchable media.
  produced_playable_end   end time of the newest COMPLETED, published
                          playlist segment, from summed EXTINF. The
                          pacing clock.
  fetched_end             the actual EXTINF end of the highest segment
                          served. A DOWNLOAD FRONTIER — the client may
                          be up to its forward buffer ahead of the true
                          playhead — and named as such everywhere.
  ```

  Per-segment metadata (`index`, `start_ms`, `end_ms`, `bytes`) is
  recorded as each segment completes, derived from the playlist — never
  `index × SEGMENT_SECONDS`, which lies on variable-GOP copy sources.
  The controller runs on segment-completion and frontier-advance events
  (the 15 s reaper stays as the repair loop, not the flow controller):

  ```text
  ahead_seconds = produced_playable_end − fetched_end
  ahead_bytes   = Σ bytes of completed segments past fetched_end

  suspend when: ahead_seconds > hls_ahead_max_secs (180)
            or  ahead_bytes   > hls_ahead_max_bytes (2 GB/session)
            or  global scratch across sessions > scratch budget
  resume  when: every trigger is below half its limit
  ```

- **Retention behind the frontier.** Because `fetched_end` can sit a full
  client forward-buffer past the true playhead, deleting at a fixed 60 s
  behind it can destroy media near where the viewer actually *is*. The
  keep-behind window is derived, not constant:

  ```text
  retention ≥ client_forward_buffer (60) + back_buffer (30) + retry (30)
            = 120 s behind fetched_end
  ```

  Deletion inside that window is forbidden; outside it, the EVENT
  playlist's older URIs 404 by documented contract (decision 3, §8.6).
  Acceptance includes surviving a playlist reload and a forced segment
  retry after GC has begun, on Chrome, Safari, and AirPlay.
- **Why not just a bigger readrate?** Because unbounded ahead-writing is
  the thing `-re` was protecting against; suspend keeps the protection
  while un-linking it from the user's buffer. And why not SIGSTOP alone
  with no readrate? Flat-out bursts between suspends monopolize Wi-Fi
  airtime and NAS reads — the same DHCP-starving burst the remux
  pacing comment documents (`stream.rs:40-55`). Burst-then-2× is gentle
  on the link *and* fast to safety.

**Acceptance:** copy-HLS of a 4K HEVC file reaches ≥60 s of client-side
runway within 30 s of start (M0 beacons show it); a variable-GOP copy
fixture proves the media window matches summed `EXTINF` durations; a
high-bitrate fixture cannot exceed the byte budget beyond one in-progress
segment, and a multi-session test holds the global budget; nothing inside
the retention window is ever deleted; AirPlay from Safari starts in ≤5 s
and survives a 10 s Wi-Fi hiccup without a visible stall; suspend/resume
transitions are logged and visible in the activity page.

### 4.3 Client buffer targets — provisional, M0-owned (B5 as revised)

In `attachHls`, tune **seconds only**: `maxBufferLength: 60` and
`backBufferLength: 30` (default Infinity; the server prunes behind the
frontier anyway, and 30 s bounds tab memory on 4K). `maxBufferSize` stays
at the hls.js default — the shipped `400e6` override encoded the disproven
§2.3 model of browser capacity and reverts in the correction pass. Raising
the byte value again requires M0 evidence that byte targeting, not the
browser quota, is the binding limit (a recorded runway plateau *without*
`BUFFER_FULL_ERROR` at the plateau). Worker stays off — the CSP note at
`index.html:1778-1780` still applies, and fMP4 copy segments don't
transmux, so main-thread cost is unchanged. Safari-native and Apple TV
need nothing: their buffer follows playlist availability, which §4.2
already fixed.

**Acceptance:** on the 4K copy fixture, the chosen values show no repeated
buffer-full recovery loop across the client matrix, and p10 runway
improves against the default-config baseline without a TTFF penalty.
Long-run memory is judged by browser-process/media measurements, not
`performance.memory` (JS heap does not see SourceBuffer). Results are
segmented by browser, codec, bitrate, and copy-vs-transcode.

### 4.4 2-second segments (halves the start floor, B4)

`SEGMENT_SECONDS: 4 → 2` (`transcode/mod.rs:20`) — the constant already
feeds both the keyframe grid and `hls_time`, so the change is one line
plus expectations in tests. First-segment wall time halves; hls.js and
Safari reach their start thresholds in half the content. Costs, called
out honestly: ~2× playlist churn and segment-request rate (trivial at
these sizes), a few % worse compression from the denser keyframe grid at
a given bitrate, and double the per-segment mux overhead on copy. The
copy path can't force keyframes, so its real segmentation still follows
source GOPs (`hls_time` is a floor there) — 2 s helps copy only when the
source GOP is short, and never hurts.

Failover note for Phase 4: the deterministic-segment property
([PHASE3-SPIKE.md](PHASE3-SPIKE.md)) holds for any fixed segment length;
the spike measured d=4 but nothing in the restart-at-boundary contract
depends on the value — boundaries are `N × SEGMENT_SECONDS` wherever the
constant says. Keep it a single source of truth; never hardcode 4 or 2 in
cluster code.

**Acceptance:** M0 TTFF for a hardware transcode drops by ~2 s;
`ffprobe` of produced segments shows 2 s cadence; the full corpus
(HDR/DV/VFR) still passes.

**Shipped 2026-07-28.** The playlist of a live transcode session reads
`#EXT-X-TARGETDURATION:2` with every `#EXTINF:2.000000`, so the cadence is
confirmed end to end rather than inferred from the argument list. The
keyframe-grid and `hls_time` assertions are now written against the constant
itself, so the two can never be tuned apart — a session whose keyframes miss
its segment boundaries produces segments that are not independently
decodable, which is the one thing HLS requires of them.

### 4.5 A watchdog that reads the speedometer (kills B7)

Replace the binary produced/not-produced checks with the `-progress`
telemetry from M0:

- **stalled** = `out_time` not advancing for 10 s while not suspended →
  hardware session: kill, clear dir, restart on software (today's
  fallback, now fired only when actually stuck); software session: fail
  fast with the existing clear error.
- **slow but advancing** (speed < 1.0, out_time moving) → leave it
  alone, log the speed, let §4.2's burst absorb what it can. The player
  survives a slow starter far better than a gratuitous 12 s + restart.
- The fixed `FIRST_SEGMENT_GRACE`/`SOFTWARE_GRACE` sleeps
  (`transcode.rs:29,35`) become bounds on *no-progress-ever* (input
  refused, device wedged), not on slowness.

Surface `speed` in the stats overlay via the §3 status endpoint
("encode 1.7× · 38 s ahead"), because "is the server keeping up" is the
first question every buffering report asks, and today nothing answers it.

Two hardenings from the review cycle (§2.8-3):

- **Attempt generations.** Each spawned process gets a monotonically
  increasing generation; progress, exit, stall, and fallback updates apply
  only while their generation matches the session's active attempt. This
  closes the fallback race (a killed attempt's stdout reader draining
  stale lines over the replacement's fresh telemetry) and moves the
  encoder/pipeline label into per-attempt state, so the activity page
  stops naming the hardware encoder after a software fallback.
- **Recent speed, not only cumulative.** ffmpeg's `speed=` is cumulative
  and hides a recent slowdown behind a fast start (and reads nonsense
  across a suspend). Keep the raw value for diagnostics; compute
  `recent_speed` as a windowed delta of `out_time` over wall time with a
  short EWMA — it is what the watchdog, the overlay, §4.7's admission
  rule, and §7.1's placement actually want.

**Acceptance:** a deliberately slow software 4K session (no GPU) is
*not* killed while progressing; pointing at a nonexistent decoder still
fails inside the old windows; a synthetic-progress test proves a killed
generation cannot mark its replacement dead or overwrite its label;
recent speed reacts to a forced slowdown within its documented window.

### 4.6 Bound the hardware rate control (kills B6)

Add `-maxrate` (1.5×) / `-bufsize` (2×) to the QSV, VA-API, and
VideoToolbox arms of `Encoder::encode_args` (`encoder.rs:99-104`),
matching software/NVENC. This is verbatim the server half of
[ADAPTIVE-QUALITY.md](ADAPTIVE-QUALITY.md) Phase 1, promoted here because
unbounded bursts also sabotage today's fixed-rung sessions on Wi-Fi.
VA-API may need `-rc_mode VBR` alongside maxrate on some drivers.

**Validation must exercise what production runs** (review R4): the
current startup probe never calls `encode_args`, so a driver could
validate and then reject the real session's flags. The probe becomes a
short encode with the complete production argument set — rate-control
mode, bitrate, maxrate, bufsize, pixel format, representative frame rate
— per family. And the acceptance language is a *window*, not a
per-segment cap: `maxrate`+`bufsize` describe a buffering model, so the
bound is measured over a stated sliding window with a stated permitted
overshoot, and the ladder's advertised `BANDWIDTH` covers the measured
peak.

**Acceptance:** every validated family accepts the exact production
rate-control arguments (no-flag-rejection smoke test); a grain-heavy
fixture stays inside the documented sliding-window bound on QSV;
ADAPTIVE-QUALITY.md's "software & NVENC only" caveat row is updated in
the same commit.

**Shipped 2026-07-28**, with `-rc_mode` deliberately left at auto. ffmpeg's
VAAPI encoder already selects VBR when maxrate exceeds bitrate and falls back
to what the driver implements; forcing VBR would turn a CBR-only driver from
"works, roughly bounded" into "fails validation, no hardware at all" — a worse
outcome than the looser bound it was meant to tighten. The probe is now
`validation_args() → encode_args()`, proven by comparison in a test rather
than by restating the flags, so a rate-control argument added to sessions and
not to the probe is a test failure rather than a driver surprise.

Two things fell out of making the probe representative. It had been a 64×64
still at 1 fps for 0.1 s — one frame, which an encoder with any lookahead
turns into zero packets, which ffmpeg reports as "nothing was written into
output file": a *working* GPU failing validation. It is now 720p30 for half a
second. And the failure line the operator reads was ffmpeg's last, which is
that same generic summary; it is now the first real cause ("Operation not
permitted" — a missing device — rather than "nothing was written").

Still owed: the grain-heavy sliding-window measurement, which needs QSV
hardware. `scripts/bench` is where it goes.

### 4.7 The Auto rung (B8 — decision needed, small code)

Today Auto = 720p for every transcode. Once §5 makes 1080p cheap, the
better default is `min(source_height, 1080)` **when a hardware encoder
won**, falling back to 720p on software. This is a policy change on
Paul's stated "quality menu exists, Auto should just be right" direction —
implement behind one function (`transcodeHeight` caller side +
`hls/start` default), flag the default in the PR description, and let the
ladder API from ADAPTIVE-QUALITY Phase 1 carry it properly later.
User-facing strings, as always, `${APP_NAME}`-clean and theme-mixed.

**Shipped 2026-07-28, decided the other way round from the sketch.** The
policy lives on the server (`TranscodeManager::auto_height`), not in
`transcodeHeight()`, because the rung depends on which encoder wins and the
player only learns that from the response to the request it is making. So the
player sends no height at all for Auto and the server answers: `min(source,
1080)` on hardware, 720p on software, never upscaling. Capped at 1080 rather
than at the source deliberately — a 4K rung is a bandwidth decision as much as
a CPU one, and Auto should not put 20 Mb/s on somebody's Wi-Fi without being
asked. 4K stays a menu choice, and direct play and remux still deliver the
source untouched.

Note the default this changes: Auto on a QSV box now transcodes 1080p source
at 1080p where it used to give 720p.

### 4.8 Ops notes that ride along (no code)

- **Transcode scratch off the NAS.** `transcode_dir` is
  `data_dir/transcode`, recreated at boot (`main.rs:143-150`). If
  `PLURX_DATA_DIR` sits on the NAS, every segment write pays the network
  twice. Document in OPERATIONS: data dir local, ideally tmpfs for the
  transcode subdir on RAM-rich nodes. Session size is predictable from
  §4.2's bounds: ahead window (≈1 GB at 180 s of 45 Mb/s copy, hard-capped
  by `hls_ahead_max_bytes`) + the ~120 s retention window behind the
  frontier — size tmpfs from `hls_scratch_max_bytes` plus headroom.
- **NFS/SMB read-ahead.** One paragraph with mount suggestions
  (`rsize=1048576`, larger readahead for NFS; SMB3 multichannel where the
  NAS offers it) and the reminder that §4.2's burst reads at NAS speed —
  a starved mount now shows up as `speed < readrate` in the §4.5
  telemetry instead of as a mystery.
- **jellyfin-ffmpeg everywhere** (already the OPERATIONS recommendation)
  — §5 leans on its filter set; the pacing flags need ≥6.1 for burst.

## 5. M2 — the real 4K fix: tone-map on the GPU (kills B2)

**Objective:** a 4K HDR10 → 1080p SDR transcode runs ≥3× realtime on a
Gen11+ Intel iGPU, making the ~30 s stutter class structurally impossible
and 4K starts hardware-fast. This executes the "zero-copy GPU filter
graphs" refinement ARCHITECTURE §3 and the Phase 2 exit notes already
name as the known next step.

**The change:** when the encoder family and the source allow it, keep
frames on the GPU end-to-end instead of the
`hwdownload → CPU float chain → hwupload` round trip:

```
 QSV    : -hwaccel qsv  … -vf scale_qsv=w=-1:h=1080,vpp_qsv=tonemap=1
 VA-API : -hwaccel vaapi … -vf scale_vaapi=w=-1:h=1080:format=nv12,tonemap_vaapi=format=nv12
 (exact graphs to be validated on nynuc — driver/ffmpeg-build dependent;
  jellyfin-ffmpeg also offers tonemap_opencl as the portable GPU alternate)
```

**Gate by probe, not by version — and probe real HDR** (review R5). A
synthetic 10-bit pattern proves a graph parses and moves frames; it does
not prove the decoder preserved HDR metadata, that the output is correct
SDR rather than clipped gray, that it is tagged BT.709, or that it is
*fast*. The validation is therefore end-to-end against a small
redistribution-safe **HDR10 fixture** (real compressed HEVC), per
candidate pipeline, per node:

1. decode through the candidate hardware path;
2. tone-map + scale with the exact production graph;
3. encode a short H.264 output;
4. `ffprobe` asserts BT.709 transfer/primaries/matrix on the output;
5. sampled luma/chroma ranges compare against the CPU reference within a
   deliberately broad tolerance (encoders differ; gray-screen doesn't);
6. the run must clear a minimum recent speed — exit status zero is not a
   capability.

Only a node that proves a graph gets it; everyone else keeps today's CPU
chain, and runtime fallback stays regardless (a probe cannot cover every
codec profile or driver state — a chosen graph that stops progressing
restarts once on the CPU graph and logs the pipeline downgrade). This
gives the AMD NUCs the right answer for free: VA-API decode/encode
validate, GPU tone-map probably doesn't (VCN has no tonemap filter) →
hardware encode with CPU tone-map, or `libplacebo` (Vulkan) if *its*
probe passes — `PLURX_TONEMAP=libplacebo` already exists as the opt-in
(`transcode.rs:145-151`); promote it to a probed candidate rather than a
blind preference. The example QSV/VA-API graphs above stay hypotheses
until they pass this probe on the deployed jellyfin-ffmpeg build.

**Scope guards:** Dolby Vision profiles that hardware-decode to garbage
stay on the existing `PLURX_HWDECODE=off` escape hatch; HLG routes to the
CPU chain (or a probe-passed libplacebo) regardless, and the
graph-selection log line says so. **Text** subtitle burn-in selects the
compatible CPU/hybrid graph (the `subtitles` filter is CPU-only) — accept
the download/upload for those sessions. **Bitmap** subtitles are not
burned at all today (`video_filters` skips them); per decision 10 (§8.6)
that stays true, and it is *disclosed*: the decision/start response says
the selected subtitles can't be shown on this path, the player tells the
viewer, and nothing downstream (cache identity included) may claim a burn
that didn't happen.

**Capacity and admission, while in here** (review R6/assessment §8): the
docs already warn two QSV sessions can stall an iGPU. Add
`transcode.max_hw_sessions` (default 2), acquired **atomically under the
sessions lock** — two racing starts cannot both observe a free slot. When
the cap is full, the order is: brief queue (≤5 s) → a *measured-safe*
software recipe → an explicit capacity error. "Software-safe" is decided
by **measured pipeline speed, not output height** — a 4K HDR source is
sub-realtime in software decode+tonemap no matter how small the output —
so admission requires the M0/§4.5 recent-speed record for that pipeline
class to clear ~1.2×, and a session expected to stall is refused rather
than started. The cluster milestone (§7) later inserts "another node"
ahead of the queue.

**Shipped 2026-07-28 — the selection half.** `Pipeline` (plurx-core) is the
video path as a value: candidate graphs, which encoder each can feed, which
sources each may touch, its decode and device flags, and what it falls back
to. `pipeprobe` (plurxd) runs each candidate at boot against a *generated*
4K HDR10 HEVC fixture — generated rather than shipped, so it is
redistribution-safe by construction, and real compressed HEVC with PQ/BT.2020
in the stream so the decoder has metadata it must actually carry through.

Two decisions worth recording against the plan's text:

*The speed gate is a comparison, not a threshold.* The plan said "must clear a
minimum recent speed". An absolute number would have to be guessed for
hardware nobody has measured, and would be wrong on both the fast and slow
ends. Running the CPU chain first as a reference and requiring a candidate to
beat it by 20% costs one extra run, cancels process startup on both sides, and
states the real requirement: a graph that merely matches the chain it replaces
has bought nothing and taken on a driver dependency.

*The runtime downgrade drops the graph before it drops the encoder.* A GPU
graph that stops producing is evidence about the graph — a driver state the
probe couldn't reach, a codec profile its fixture didn't cover, contention
from a second session. It is not evidence about the encoder, and swapping to
software would trade a stalled hardware session for a slower one. So the first
retry keeps the hardware and drops to the CPU chain; only a session already on
the CPU chain falls back to software.

Per-session routing (`Pipeline::for_session`) sends HLG, Dolby Vision, burned
text subtitles, light sources, and any graph/encoder mismatch to the CPU chain
regardless of what the node proved — each of those fails *quietly* otherwise,
which is the kind nobody reports. The chosen pipeline is on the session's
ffmpeg log line and the probe's full verdict list is on `GET /api/v1/system`,
because falling back is silent: everything plays, 4K just stays slow.

**Still owed in M2:** `transcode.max_hw_sessions` and the admission ladder
(queue → measured-safe software → capacity error), and the acceptance run
below, which needs real hardware.

**Acceptance:** on nynuc, the M0 telemetry shows a 4K HDR10 → 1080p
session at ≥3× (QSV graph) vs the recorded CPU baseline; a 2 h 4K HDR
play completes with zero stall beacons at the 1080p rung; the corpus adds
one HDR10 and one HLG asset asserting *which* pipeline was chosen (log
assertion), so a driver regression shows up as a pipeline downgrade in
CI, not a user stutter; concurrent-start tests cannot exceed the slot
cap; every admitted fallback sustains its speed margin over the
validation window.

## 6. M3 — the pre-transcode cache

**Objective:** the transcodes that were going to happen anyway happen
before anyone presses Play. A cache hit starts in ≤1.5 s **and seeks like
direct play**, because a completed HLS asset is a VOD playlist whose every
segment already exists — the session-restart seek dance (`B9`) simply
doesn't apply to it.

### 6.1 The cache model — identity and location are different data

Content-addressed by **recipe hash**: SHA-256 over
`(cache_format_version, pipeline_digest, file_id, size, mtime,
target_height, video_bitrate, audio_bitrate, audio_channels, audio_index,
audio_action(copy|aac), tone_map_mode, subtitle_burn(idx, bitmap,
applied?), audio_offset_ms, SEGMENT_SECONDS)`. Size+mtime in the key makes
invalidation lazy and unskippable — a changed file simply never matches
again, and the orphaned entry ages out via LRU. `pipeline_digest` is the
review-cycle addition (R7): a single digest over the ffmpeg/jellyfin-ffmpeg
build fingerprint, output codec · profile · pixel format, color contract
(transfer/primaries/matrix/range), muxer + segment format, GOP/segment
policy, rate-control policy, and **encoder family** — included until QSV,
VA-API, NVENC, VideoToolbox, and software are demonstrated to satisfy one
declared output contract (decision 6, §8.6). A build upgrade that changes
tone-map output therefore misses the old entries instead of serving them.
`subtitle_burn.applied` records whether the burn actually happened —
a bitmap request that was skipped hashes as burn-omitted, honestly.

Identity and physical copies are **separate tables** — M4 needs "this
recipe exists on nodes A and B, and B may evict its copy without lying to
A", which one row with one `dir` cannot say. Migration **v11** (v10 is the
watched-outbox — re-verify against `store/sqlite/mod.rs:291`):

```sql
CREATE TABLE transcode_cache_recipes (
    recipe_hash    TEXT PRIMARY KEY,
    file_id        INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    recipe_version INTEGER NOT NULL,
    created_at     INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

CREATE TABLE transcode_cache_locations (
    recipe_hash   TEXT NOT NULL REFERENCES transcode_cache_recipes(recipe_hash)
                                ON DELETE CASCADE,
    node_id       TEXT NOT NULL,          -- single-node: the instance id
    storage_class TEXT NOT NULL,          -- 'local' | 'shared'
    relative_dir  TEXT NOT NULL,          -- under the configured cache root
    bytes         INTEGER NOT NULL,
    complete      INTEGER NOT NULL DEFAULT 0,
    last_used_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    last_seen_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (recipe_hash, node_id, storage_class)
) STRICT;
```

Directories are **relative** to the configured cache root
(`data_dir/cache/transcode/` locally, `cache.shared_dir` when shared) —
absolute paths don't replicate across nodes with different mounts. The
local root is deliberately **not** under `data_dir/transcode`, which is
wiped at every boot (`main.rs:149`). On a single node this schema costs
one extra table and buys M4 for free.

### 6.2 Producing entries

A new `JobManager` job (`schedule.rs` — same pattern as probe retries:
pure `due_jobs` logic + a settings interval), **leader-scheduled** like
every other job once Phase 4 lands (ARCHITECTURE §2.2 already claims jobs
are a leader singleton — the scheduler HA question the founding notes
flagged rides on that, unchanged).

Candidates, in priority order, capped by budget: per-user **Next Up**
episodes and in-progress items whose decision (against the caps the user
last played with — persist the last-seen caps per user, one settings row)
would be a transcode or copy; then recently-added 4K/HDR items. Producer
runs the normal `hls_args`/`hls_copy_args` with no pacing, **generates the
playlist as VOD** (not by string-appending `ENDLIST` to an EVENT file —
an avoidable corruption path), writes into a unique temp dir on the same
filesystem as the final path, and publishes with an atomic rename; only
after the rename does the location row flip `complete=1`. Under M4's
distributed producers, the worker re-proves its job lease *before* the
rename, so an expired worker cannot publish over a newer result.

**Preemption is terminate, not suspend** (assessment §5 — the sharpest
correction in the cycle). A SIGSTOPped ffmpeg still holds its hardware
codec session, GPU context, and mapped buffers; on an iGPU where the
*session* is the scarce resource, a suspended producer blocks the live
viewer exactly as if it were running. So: the producer admits through the
same §5 slot machinery as live sessions, at strictly lower priority, and
the moment a live session wants the slot the producer **checkpoints and
terminates** — record the last published segment boundary, kill the
process, release the slot, discard the unpublished temp segment, resume
later from that boundary (cheap by construction: boundary restart is the
spike's own property). It cannot re-acquire while a live waiter exists.
Contrast with §4.2's live-session SIGSTOP, which is a *disk* mechanism on
a session whose viewer owns its slot — the two uses share a syscall and
nothing else.

Budget & eviction: `cache.max_gb` (default 50), LRU by `last_used_at`,
evict until under budget before each producer run; `complete=0` rows
older than a day are crash leftovers — delete dir + row (the existing
`sweep_orphan_dirs` pattern, pointed at the cache root, minus live
recipe dirs).

### 6.3 Serving hits

`/hls/start` computes the would-be recipe hash first. On a complete hit:
touch `last_used_at`, register a lightweight session (no child process;
`encoder: "cached"`) so the activity page shows the viewer, and return
the playlist URL into the cache dir. Response gains `vod: true`; the
player, seeing it, treats the stream like direct play for seeking
(`v.currentTime`, no session restart) and skips the stall watchdog's
session-restart arm — cached segments can't be late. Reaper skips
child-management for cached sessions but still expires them (touch
tracking). On a miss, everything proceeds exactly as today — the cache
is an accelerator, never a dependency.

**Not doing (yet):** prefix-only warm caches (first N minutes at the top
rung to bridge live-session spin-up). Designed to compose with
restart-at-boundary later, but with §5 making live starts ~2 s and full
pre-caching covering the predictable cases, the added session-splice
complexity isn't buying enough. Revisit after M2/M3 telemetry.

**Acceptance:** play a pre-cached Next Up episode: TTFF ≤1.5 s, seek
lands in ≤0.5 s (M0 beacons), zero ffmpeg processes spawned. Fill the
cache past `max_gb`, run the producer: LRU entries evicted, budget held
(integration test with tiny fixture "segments"). Change a source file's
mtime: old entry never served, ages out. A `SEGMENT_SECONDS` bump (§4.4)
or a pipeline change orphans old entries via the digest — test asserts
the stale hash misses. Start a producer encode, then request live
playback: the producer yields its slot within a bounded interval, live
TTFF stays inside its SLO, and the producer later resumes from its
checkpoint; slot release survives fallback, suspension, and process
death without leaking.

## 7. M4 — cluster transcode (rides Phase 4)

**Objective:** the 2–3 Intel boxes act as one transcode pool: sessions
land on the node best able to run them, survive that node dying, and the
pre-cache producer fans out across every idle GPU in the house. This
milestone deliberately **waits for Phase 4's plumbing** (hiqlite store,
membership, node lists) and adds only the transcode-shaped pieces on top.

### 7.1 What each node knows

Each node publishes into replicated state: its validated capability set
(encoder families + GPU-tonemap probes from §5 — the *validated* caps,
not the compiled ones), live session count vs `max_hw_sessions`, and
recent speed telemetry (§4.5 gives it per session; a rolling per-node
"4K-tonemap speed" is placement gold on a heterogeneous fleet — an AMD
NUC that tone-maps at 0.8× should lose 4K HDR sessions to an Intel box
that does 4×, and win plain 1080p ones).

### 7.2 Placement and serving

`/hls/start` on any node scores candidates
(capability match for this recipe → free hardware slots → recent speed)
and starts the session there — locally when local wins, else an internal
forward. Session→owner lives in the replicated ephemeral class (exactly
the "session recipe" replication PHASE3-SPIKE's consequences section
prescribes). Playlist/segment requests hitting a non-owner **proxy** to
the owner (streamed body, LAN bandwidth is free); capability-URL auth
(`http/hls.rs` module docs) already anticipated this — any node may
serve a session id without seeing a login. Redirects (307 to the owner's
address) are the cheaper alternative once clients carry the node list;
note it, ship proxying first — it works for Apple TV's dumb fetcher too.

### 7.3 Failover — the spike property, plus the serving protocol it lacked

The spike proved the *media* property: any node can restart an encode at a
boundary and emit valid segments after a discontinuity. The review cycle
(R8 + assessment §7) established that this is not yet a serving protocol —
"stitched playlist with served-prefix entries" hand-waved where prefix
*bytes* live when their owner is dead (they were node-local: nowhere), and
"last served" is not "last received." The takeover contract, six fields,
defined per event:

```text
SessionOwner {
    session_id, owner_node, owner_epoch,
    lease_expires_at,             -- 6 s lease, renewed every 2 s
    recipe,
    produced_playable_through,    -- §4.2's playable clock, replicated
    fetched_through,              -- the download frontier
}

1  trusted prefix        last segment identity + end time the dead owner
                         durably published
2  resume boundary       input timestamp + sequence where the survivor's
                         encoder starts: ONE segment before fetched_through
3  publication boundary  that one overlap segment IS published — it covers
                         the served-but-never-acknowledged gap; everything
                         earlier is client-buffer reliance (decision 8)
4  playlist sequence     the takeover playlist's first MEDIA-SEQUENCE
5  discontinuity         EXT-X-DISCONTINUITY before the overlap segment;
                         DISCONTINUITY-SEQUENCE advances by one
6  fence                 every playlist, segment, and cache-location
                         publication carries owner_epoch; a stale epoch is
                         rejected even if the old owner's ffmpeg lives on
```

The playlist never references bytes neither owner durably published. The
resume SLO is a **budget**, not a wish: detect (lease expiry, ≤6 s) +
claim + NAS open/seek + first replacement segment + client playlist retry
— initial target ≤10 s end-to-end, tightened only by measurement. §4.2's
suspend interacts trivially: a suspended session that dies restarts on the
survivor un-suspended and immediately re-earns its window.

### 7.4 The distributed pre-cache — the embarrassingly parallel win

Producer jobs become rows in a replicated queue with lease semantics
(node, lease_expires — the outbox pattern from v10 is the local
precedent). Each node leases jobs its validated caps can run, produces
into the shared cache, publishes atomically. Cache placement: a shared
directory on the NAS (`cache.shared_dir`) is the natural fit — every
node mounts the media NAS already, cache reads are cheap sequential NAS
reads, and a cache hit is servable by *any* node with zero proxying.
Degradation: no shared dir configured → node-local caches, and placement
(§7.2) simply prefers the node holding the entry. Do **not** replicate
cache bytes through raft — ARCHITECTURE §2.2 classes segment caches as
node-local regenerable, and that stays true; only the index row is
replicated state.

**Not doing:** splitting one file's encode across nodes by segment
ranges. The determinism spike proves it *could* work, but the seams
(per-range PTS resets, audio joins needing DISCONTINUITY markers or
overlap-trim) buy throughput the fleet doesn't need — three nodes × §5
speeds pre-cache a night's viewing in minutes as whole-file jobs.
Revisit only if M3 telemetry shows producer backlog, and say why in the
PR that does it.

**Acceptance:** Phase 4's own drill list gains: kill the owner node
mid-4K-transcode — **including mid-segment-response, not only between
segments** — → playback resumes within the §7.3 budget with exactly one
discontinuity and never loops on a missing prefix URI (client beacon +
playlist assertion); a partitioned former owner cannot publish after a
new epoch is active; start a session from a node with no GPU → it lands
on the capable node and plays; run the producer with all nodes idle →
jobs spread across nodes (queue rows show distinct lessees), a worker
that loses its lease cannot publish, and a subsequent hit serves from the
shared dir on a node that didn't produce it.

## 8. Contract — exact interfaces (re-verify at build time)

### 8.1 Constants & args

- `SEGMENT_SECONDS: u32 = 4 → 2` — `plurx-core/src/transcode/mod.rs:20`.
  Now that segment bounds come from `EXTINF` rather than from this constant
  (§4.2), changing it moves only the *forced keyframe grid* and the muxer's
  target: nothing downstream multiplies by it any more, which is what makes
  the change one line instead of an audit.
- *(shipped)* `Pacing` struct (§4.2) in `plurx-core::transcode`; pacing
  parameters on `hls_args` / `hls_copy_args`; both `-re` pushes deleted;
  `PacingCaps` probe lives in `plurxd/src/ffmpeg.rs`.
- `encoder.rs` `encode_args`: QSV/VA-API/VideoToolbox gain
  `-maxrate {1.5×}k -bufsize {2×}k`, and the validation probe runs the
  full production argument set (§4.6).
- *(shipped)* Session ffmpeg spawns carry `-progress pipe:1`;
  `spawn_ffmpeg` parses stdout alongside its stderr drain. The correction
  pass adds per-attempt generations and `recent_speed` (§4.5).
- Correction pass: per-segment `SegmentMeta { index, start_ms, end_ms,
  bytes }` recorded from the playlist as segments complete; the
  suspend/GC controller consumes it (§4.2).

### 8.2 Store trait additions

```rust
/// §4.1 — persist chapters fetched for a legacy probe (merged into probe_json).
async fn merge_file_probe_chapters(&self, file_id: i64, chapters_json: &str) -> Result<()>;
/// §6 — transcode cache index CRUD (get by recipe_hash, upsert, touch,
/// list LRU over budget, delete). Shapes follow the v11 table in §6.1.
```

### 8.3 Settings keys (all runtime, Settings → Playback / new Cache block)

| Key | Default | Meaning |
|---|---|---|
| `playback.hls_readrate` | `2.0` | HLS input pace, ×realtime (0 = unpaced) |
| `playback.hls_burst_secs` | `90` | flat-out seconds before the pace engages |
| `playback.hls_ahead_max_secs` | `180` | suspend when playable-ahead exceeds this |
| `playback.hls_ahead_max_bytes` | `2 GB` | per-session byte ceiling on ahead media (§4.2) |
| `playback.hls_scratch_max_bytes` | `8 GB` | global scratch budget across all live sessions (§4.2) |
| `transcode.max_hw_sessions` | `2` | hardware slots; admission order in §5 (queue → measured-safe software → capacity error) |
| `cache.max_gb` | `50` | pre-transcode cache budget |
| `cache.enabled` | `true` | master switch for producer + serving |
| `cache.shared_dir` | *(empty)* | §7.4 shared cache root; empty = node-local |
| `jobs.pretranscode_mins` | `0` (off) | producer cadence, 15-min floor like its siblings |

Validation mirrors `stream_readrate`'s (`system.rs:554-571`): rates ≥1.0
or 0, windows sane, floors enforced server-side.

### 8.4 Endpoints

- `GET /api/v1/hls/:session/status` (§3) — capability auth, no-store,
  never touches the idle clock.
- Session-creation responses gain `vod: bool` and (informational)
  `cached: bool` (§6.3), plus `subtitle_unavailable: bool` when the
  selected subtitles can't ride this delivery path (§5, decision 10).
  Additive; existing clients ignore them.

### 8.5 Session lifecycle (review R10, as tightened by the assessment)

Session creation is today a state-changing `GET`; identifiers alone don't
fix the semantics (GET is idempotent *by definition*, so infrastructure
may replay it). The canonical lifecycle becomes:

```text
POST   /api/v1/files/{id}/hls/sessions   create — or idempotently recover
       body: { playback_id, request_id, height | copy+aac,
               start_seconds, audio_index }
GET    /api/v1/hls/{session}/status      read (exists today)
DELETE /api/v1/hls/{session}             release — idempotent, keepalive-safe
```

- `playback_id` — client-generated, stable for one player instance.
  **Supersession is keyed by it**, not by (user, file): two devices on one
  account watching the same file stop killing each other's sessions,
  which automatic quality restarts would otherwise make routine.
- `request_id` — same id + same normalized recipe returns the same
  session instead of spawning twice; same id + different recipe is a
  conflict.
- `DELETE` — the player calls it with `fetch(..., {keepalive: true})` on
  close/replace, ending the 60–75 s zombie-encoder window. The idle
  reaper remains the crash/AirPlay fallback, and a session an Apple TV is
  still fetching is not deleted just because the browser page detached.
- The existing `GET /hls/start` remains as a **deprecated bridge**
  implemented over the same creation path — it cannot bypass admission,
  identity, or cleanup rules — and is removed once no client uses it (we
  own every client).

Playlist/segment capability-URL auth is untouched — AirPlay depends on it.

### 8.6 Decisions ledger — the review cycle's ten choices, final

| # | Decision | Choice |
|---|---|---|
| 1 | Client buffer policy | Seconds only (`60`/`30`), provisional; byte value stays default; M0 evidence required to change it (§4.3) |
| 2 | Server ahead policy | Three named frontiers (§4.2); pace on playable−fetched from `EXTINF`; suspend on time **or** per-session bytes **or** global scratch; resume when all below half; retention ≥ forward-fetch + back-buffer + retry ≈ 120 s |
| 3 | Playlist/GC contract | EVENT + documented 404s outside the retention window; deletion inside it forbidden |
| 4 | ABR handoff | Visible restart (Option A) with measured p95 interruption SLO — proposed ≤2.5 s on LAN, **operator confirms**; estimate seeded across Hls instances; severe pressure jumps straight to the highest safe rung |
| 5 | Over-capacity & fallback admission | Queue ≤5 s → *measured-safe* software recipe (recent speed ≥ ~1.2× for that pipeline class — never "720p is safe") → capacity error; cluster inserts "another node" first |
| 6 | Cache identity | Encoder family + pipeline digest in the hash; relaxation only after per-family output contracts are proven equivalent (**operator's call** whether ever worth it) |
| 7 | Background preemption | Checkpoint-and-terminate — SIGSTOP does not release hardware sessions (§6.2); resume from last published boundary; discard unpublished temp |
| 8 | Failover prefix | Hybrid: one regenerated overlap segment covers served-but-unacknowledged; client-buffer reliance for everything earlier (§7.3) |
| 9 | Owner fencing | 6 s lease / 2 s renewal; epoch on every publication; resume SLO is a measured ≤10 s budget (§7.3) |
| 10 | Bitmap subtitles | Remain unsupported; **disclosed, not rejected** — the server picks the burn track today, so there is no client request to refuse, and blocking a film over subtitles is worse than a labeled omission the UI announces. Cache identity records `applied=false`. (Deviates from the assessment's reject-or-negotiate wording; this is the negotiation, shaped for a living-room player.) |

### 8.7 Docs this plan's commits must update (same-commit rule)

[PLAYBACK.md](PLAYBACK.md) copy-video `-re` rationale + non-goals disk
paragraph (§4.2 obsoletes both) · [ADAPTIVE-QUALITY.md](ADAPTIVE-QUALITY.md)
"bounded rung bitrate" row (§4.6) and segment-length references (§4.4) ·
[OPERATIONS.md](OPERATIONS.md) settings table (§8.3), tmpfs/NAS notes
(§4.8), tone-map pipeline troubleshooting rows (§5) ·
[ARCHITECTURE.md](ARCHITECTURE.md) §3 tone-map sentence: alternates →
chosen-by-probe (§5) · [ROADMAP.md](ROADMAP.md) — this plan's slice line.

## 9. Non-goals & guardrails

- **Do not** implement LL-HLS/partial segments — 2 s segments + burst
  pacing reach the latency target without a protocol upgrade nobody's
  clients need.
- **Do not** run per-segment independent encodes for *live* sessions —
  the spike measured that cost and rejected it; restart-at-boundary is
  the design (`PHASE3-SPIKE.md` §"Decision").
- **Do not** encode multiple rungs simultaneously — ADAPTIVE-QUALITY
  Phase 3 stays behind its decision gate; this plan must not
  accidentally build it.
- **Do not** move the remux path off its existing pacing defaults or
  reuse `STREAM_READRATE` for HLS — separate knobs, separate failure
  modes.
- **Do not** put the live-session scratch dir or the SQLite DB on the
  NAS to make §7.4 easier — only `cache.shared_dir` may live there, and
  every cache read path must degrade to a live transcode on NAS
  unavailability.
- **Do not** derive markers, chapters, or cache keys from anything the
  scanner didn't persist — a play-path that quietly re-probes on the NAS
  is this plan's original sin coming back.
- **Do not** use SIGSTOP as a capacity-release mechanism — a stopped
  ffmpeg still holds its hardware codec session (§6.2). Suspend is for a
  live session's disk window only; background work terminates.
- **Do not** conflate the three frontiers (§4.2). `encoder_out_time`
  never feeds pacing or GC; nothing multiplies a segment index by
  `SEGMENT_SECONDS` to get a timestamp; a field holding the download
  frontier is not named "playhead".
- **Do not** delete media inside the retention window, whatever any
  budget says — pause production instead; a budget breach with no
  evictable media is a capacity error, not a license to break the
  playlist contract.
- **Do not** state third-party mechanisms (hls.js, FFmpeg muxer, browser
  quotas) as root causes without checking the vendored source or a live
  probe — R1 is what skipping that costs. Plurx code gets file:line
  citations; external behavior gets the same standard.
- Keep every user-facing string `${APP_NAME}`-based and every new UI
  color theme-mixed (`color-mix(… var(--panel))`) — the light theme
  renders hardcoded dark hexes as black boxes, and the web UI is
  `include_str!` — embedded, so UI edits mean rebuild + restart.
- The sandbox has no GPU and headless Chromium lacks H.264 — §5
  acceptance runs on nynuc; CI asserts *pipeline selection* from logs,
  not pixels, exactly like the existing playback corpus.

## 10. Order of work, and what it buys

| Slice | Contents | Expected result |
|---|---|---|
| Weekend 1 ✅ | M0 + §4.1 + §4.2 + §4.3 + §4.5 | starts ~3–5 s; 4K copy-HLS and AirPlay stop stalling; numbers for everything else |
| Correction pass ✅ | §2.8: R1 client config · three-frontier accounting + retention + byte budgets · attempt generations + live labels · §8.5 lifecycle · R11 cheap wins | shipped code now matches the reviewed contracts |
| Weekend 2 | §4.4 + §4.6 (+§4.7 decision) | starts ~2–3 s; Wi-Fi-stable rungs; validation exercises the production flags |
| Focused week | §5 (M2) — real-HDR probe matrix on nynuc + one AMD node first | 4K HDR fully hardware; the 30 s stutter class gone; Auto=1080p viable; admission by measured speed |
| Next | §6 (M3) — slot arbiter v1 lands with it | predicted plays start ≤1.5 s and seek like direct play; producers yield to viewers |
| With Phase 4 | §7 (M4) — fencing + takeover protocol per §7.3 | pool of nodes; failover inside the measured budget; overnight cluster pre-caching |

Every slice leaves the tree releasable; nothing in a later slice is
load-bearing for an earlier one. The correction pass went first because
building §4.4 onward on accounting the review cycle disproved would have
compounded the debt; with it landed, the remaining uncertainty is
measurement uncertainty owned by M0 — which is where uncertainty belongs.
The next thing that should happen is not code: it is running the fixture
matrix on nynuc, because §4.4's segment-length trade-off and §5's whole
premise are both claims about numbers nobody has measured on the hardware
this runs on.
