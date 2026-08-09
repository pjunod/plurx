# Performance — where the seconds go, and the plan to get them back

**Status:** M1, **M2** and **M3 complete** (2026-07-29). M2's acceptance ran
on nynuc: the QSV tone-map graph passed at **4.89× the CPU chain**, and
§4.6's bound held at **9.05 Mb/s peak over a 10 s window against a permitted
13.6** on the 1080p rung. The same run found the bug in §4.4bis below — 10.4
second segments from a 2-second request, the original start-up symptom still
alive on the only box with hardware. M4 waits on Phase 4's plumbing, which
does not exist yet · **Collected with:**
[`scripts/perf-report`](../scripts/perf-report) · **Diagnosed against:**
`e7a12cf`
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
  Client playlist/index refreshes and frontier advances run the controller.
  Nothing currently calls it directly when ffmpeg completes a segment; the
  15 s reaper is the producer-side repair loop that bounds that delay:

  ```text
  ahead_seconds = produced_playable_end − fetched_end
  ahead_bytes   = Σ bytes of completed segments past fetched_end

  suspend when: ahead_seconds > hls_ahead_max_secs (180)
            or  ahead_bytes   > hls_ahead_max_bytes (2 GB/session)
            or  global scratch across sessions > scratch budget
  resume  when: ahead_seconds ≤ max(half, ceiling − 30 s) (150 s default)
            and byte/global triggers are each below half their limit
  ```

  Time keeps its 30-second hysteresis so a fast producer does not toggle once
  per client segment near the boundary. A one-second configured ceiling floors
  its release at one second rather than zero, which keeps that limit active.
  Byte limits keep half-window hysteresis because they are hard disk bounds.
  Session status names the active `time`, per-session `bytes`, or `global`
  hold and reports its matching release value, so operators can distinguish a
  capacity hold from a time hold and both from an encoder stall.

  This controller bounds production; it is not a structural recovery for a
  client that stops fetching. Ahead media is already published and visible in
  the served playlist. A non-fetching client can leave the producer held while
  its own available reserve drains. The physical-iPad trace therefore remains
  a client/playlist investigation until device capture correlates server ahead,
  `loadedTimeRanges`, access-log segment counts, and the EVENT-to-sliding header
  transition.

- **Retention behind the frontier.** Because `fetched_end` can sit a full
  client forward-buffer past the true playhead, deleting at a fixed 60 s
  behind it can destroy media near where the viewer actually *is*. The
  keep-behind window is derived, not constant:

  ```text
  retention ≥ observed_forward_fetch (120) + back_buffer (30) + retry (30)
            = 180 s behind fetched_end
  ```

  The 120 s term is physical-iPad evidence, not the configured 60 s AVPlayer
  preference; `preferredForwardBufferDuration` is a hint rather than a cap.

  Deletion inside that window is forbidden. Outside it, the append-only EVENT
  file remains the writer's duration history, while the served media playlist
  removes the deleted prefix and advances `MEDIA-SEQUENCE` (decision 3,
  §8.6). A client reload therefore never discovers a URI the server has
  already pruned. Acceptance includes surviving a playlist reload and a
  forced segment retry after GC has begun, on Chrome, Safari, and AirPlay.
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

### 4.3bis The progressive remux has a buffer ceiling nothing can raise (measured 2026-07-29)

§4.3 tunes the buffer on the paths hls.js owns. The progressive `/stream.mp4`
path is not one of them, and it turns out to have a ceiling of its own that no
setting on either side can move.

The symptom was nine `supply` stalls on a 69 Mb/s Dolby Vision remux in
Chrome, with **5 dropped frames out of 2058** — so not decode — while the
overlay showed a 0.2 s buffer and a 263 Mb/s delivery rate. Those two look
contradictory until you notice the reading was taken *during* a stall: the
client's buffer is empty then, TCP back-pressure releases, and ffmpeg runs
flat out at its `-readrate 4` cap. 263 ÷ 69 ≈ 3.8×, which is exactly that.

Measured directly, in headless Chromium, against a 59 Mb/s fragmented MP4
carrying the production muxer flags, varying only the response headers:

| response headers | steady-state read-ahead |
| --- | --- |
| neither (what `/stream.mp4` sends) | **2.27 s** |
| `Content-Length` only | 2.23 s |
| `Accept-Ranges` only | 2.27 s |
| **both** (what direct play sends) | **10.83 s** |
| neither, delivered paced at 4× | 2.24 s |

Median and max agreed to four decimals on every capped run, which is what a
ceiling looks like rather than a coincidence. A 9.6 Mb/s file with both
headers reached 19.7 s, so the deep buffer is real and not a fluke of one
bitrate.

The rule this exposes: **Chrome only buffers deeply when it can treat the
response as a seekable file of known length.** Either header alone buys
nothing. A live remux can offer neither — the length is unknown until ffmpeg
finishes, and there are no byte ranges to serve of a stream that does not
exist yet. So the progressive path is pinned at ~2.2 s in Chrome, and pacing
does not move it.

Two consequences worth stating plainly:

- At 69 Mb/s, 2.2 s is ~19 MB of margin. On a link with any contention that
  is not a buffer, it is a coin flip.
- `-readrate 4` is *actively harmful* on a shared or wireless link. Refilling
  that 2.2 s means pulling up to 4× the source bitrate from storage while
  pushing ~4× back out to the client — half a gigabit of demand, on the same
  link the playback it is rescuing has to cross. The stall feeds itself.

The fix is therefore not a header and not a pacing value. Every other path
plurx has already gets a real buffer: direct play is a range-served file
(10.8 s measured), transcode and cache hits go through hls.js where the buffer
is ours to set (§4.3). Only the progressive remux is stuck, and the way out is
to stop using it for high-bitrate sources — route them to the copy-video HLS
session that already exists for Safari, where hls.js manages the buffer in
seconds.

**Shipped 2026-07-29, as a hint plus a veto.** The split matters. The server
decides *whether* a remux wants segments (`playback::prefer_segmented`): above
40 Mb/s outright, or below 8× headroom against what `storeprobe` measured for
the mount holding the file — so a modest file on slow storage is caught too,
and a big one on fast storage is left alone. The client decides *whether it
can* (`segmentedRemuxOk`), because only the browser knows its own
MediaSource, and `<video src>` and MSE are different code paths with different
answers: Chrome decodes plenty progressively that MediaSource refuses. A no
from either side keeps today's behaviour — which is stuttery on a big file,
and stuttery is recoverable where black is not.

The codec question is asked the way hls.js will ask it: video and audio in one
`isTypeSupported` string, since checking them apart passes pairs the browser
rejects as a pair. The audio asked about is what will be *on the wire*, not
what is in the file — a TrueHD track re-encoded to AAC must not veto itself,
which would exclude every disc remux, i.e. precisely the set this exists to
fix.

Verified in headless Chromium against the shipped code, extracted from
`index.html` rather than reimplemented. That build has no HEVC and no AAC,
which makes it the ideal negative control: it *is* the refusing browser. HEVC
is refused and the routing declines; VP9/Opus is accepted and it routes; a
missing hint, an absent hls.js, and a copied TrueHD track each decline; and a
spy on `isTypeSupported` confirms the AAC-on-the-wire rule. What cannot be
verified here is the case that matters most — whether a real Chrome accepts
Dolby Vision HEVC through MediaSource. That is why there is a veto rather than
a version check.

### 4.3ter The buffer targets did not survive being reused (found 2026-07-29)

§4.3 picked 60 s forward and 30 s back. Correctly — for a **transcode**, whose
output is bounded by its rung (§4.6), so 90 seconds of an 8 Mb/s stream is
~90 MB and sits comfortably inside any browser's MSE quota.

§4.3bis then routed big remuxes down the same hls.js path, and a remux carries
the **source's** bitrate. The same 90 seconds of a 69 Mb/s film is **776 MB**,
against a quota nearer 150 MB. hls.js spent the whole playback appending,
hitting `QuotaExceededError`, evicting, and appending again — and the eviction
runs near the playhead. What that looks like from a sofa is a hitch every few
seconds that never lasts the 350 ms a stall needs, so it appeared nowhere in
the report and every other number read as healthy: full buffer, 1.1% dropped
frames, one stall.

Two things made it hard to see. The regression arrived with the fix for
§4.3bis, so it looked like a property of the file rather than of the change;
and the `buffer_limit` beacon that records exactly this had been in the client
since M0 but was never surfaced by `scripts/perf-report`.

**The lesson is the reuse, not the numbers.** A constant tuned against one
stream's bitrate was applied to a path where the bitrate is an order of
magnitude larger, and nothing in the type system, the tests, or the plan
noticed — because 60 and 30 are *seconds*, and the constraint they have to
satisfy is *bytes*.

Fixed by deriving the seconds from a byte budget whenever the video is copied,
and leaving §4.3's numbers exactly as they are for anything whose bitrate is
already bounded:

| source | forward | back | buffered |
| --- | --- | --- | --- |
| transcode (any) | 60 s | 30 s | ~90 MB |
| 8 Mb/s copy | 60 s | 30 s | 90 MB |
| 25 Mb/s copy | 31 s | 10 s | 128 MB |
| 69 Mb/s copy | 11 s | 4 s | 129 MB |
| 98 Mb/s copy | 8 s | 3 s | 135 MB |

The back buffer gets a much smaller floor than the forward one: it holds video
already watched, so every second of it is quota spent on data nobody will see
again. A total cap is applied last, because past roughly 150 Mb/s the floors
stop fitting and the honest answer is a small buffer that survives rather than
a comfortable one that gets evicted — a stream that big does not belong on the
copy path at all, which is a routing question rather than a buffering one.

### 4.3quater The budget was resident-only, and an append is not free (found 2026-07-30)

§4.3ter above is right about the bytes and wrong about *which* bytes. It
budgets what the browser will be **holding** — forward plus back — and the
quota is charged on what the browser holds **plus the segment being appended**,
because during an `appendBuffer` both exist at once.

That was invisible while segments were 2 s. At 61 Mb/s a 2-second segment is
15 MB, so a 128 MB resident budget peaked at 143 MB and squeaked under a
~150 MB quota. Then the segmenter (§5.6 of STUTTER-4K) took the cutting away
from ffmpeg and moved the floor to 6 s to stop losing a frame at every
boundary. At 61 Mb/s a 6-second segment is **46 MB**, and one that runs to the
byte ceiling is **64 MB** — so every append began asking for ~190 MB and being
refused.

What that costs is worse than the churn of §4.3ter, because hls.js escalates:
`BUFFER_FULL_ERROR` → evict → halve its own target → retry, and after
`appendErrorMaxRetry` (3) failures on the same segment it fails **fatally**.
Reported from the sofa on *Tron* in Chrome, 2026-07-30: several rebuffers and
then a freeze that would not resume without leaving and re-entering the
stream. Safari never saw any of it — native HLS uses none of these numbers,
which is why the same file was fine there and is the same asymmetry that made
§4.3ter hard to see.

The invariant is now stated as the thing the quota actually charges:

```
forward + one segment + back  <=  MSE_QUOTA_BYTES (144 MB)
```

and the segment term is **measured, not assumed**: `EXT-X-TARGETDURATION` is
the longest segment the session has published (copyseg keeps it honest), so
the client re-derives the targets on the first playlist and again whenever a
longer segment appears.

| source | segment | forward | back | peak |
| --- | --- | --- | --- | --- |
| transcode (any) | — | 60 s | 30 s | ~90 MB |
| 8 Mb/s copy | 15 s | 60 s | 12 s | 87 MB |
| 25 Mb/s copy | 15 s | 27 s | 3 s | 141 MB |
| 61 Mb/s copy | 9 s | 8 s | 1 s | 137 MB |
| 69 Mb/s copy | 9 s | 6 s | 1 s | 138 MB |
| 98 Mb/s copy | 7 s | 3 s | 1 s | 135 MB |
| 200 Mb/s copy | 4 s | 1 s | 1 s | 150 MB — does not fit, and says so |

Three things changed besides the invariant:

- **The forward *seconds* floor is gone.** It existed so a very large file
  would not buffer almost nothing, and it was the mechanism by which the
  budget promised bytes the quota would refuse. It is also no longer needed:
  the segment is the runway. hls.js refills as soon as what is buffered ahead
  dips under the target, so a 3 s target with 7 s segments oscillates between
  3 s and 10 s of real runway. Targets round *down* for the same reason — a
  target rounded up spends bytes nobody allocated.
- **`maxBufferSize` moves with `maxBufferLength`.** hls.js takes the *larger*
  of the seconds target and `8*maxBufferSize/bitrate`, so the stock 60 MB is a
  floor under the seconds rather than a cap on them: it pinned the forward
  buffer at 8 s of a 61 Mb/s stream regardless of what the seconds said. Same
  class of mistake as the one this section is about — a limit that reads like
  a cap and behaves like a floor.
- **The back buffer is byte-capped at 12 MB** (was 32 MB). On this path a seek
  starts a fresh session, so the back buffer buys a scrub of a second or two
  and nothing else; 32 MB of it at 61 Mb/s was a fifth of the whole budget
  spent on video nobody would watch again.

**And the decode rescue no longer learns from these sessions.** Quota pressure
produces exactly the signature the rescue looks for — visible hitches on a
copy session — so on Auto it fired, blamed the decoder, and wrote a per-device
per-codec limit that steered every future playback of anything that shape.
That is how Chrome taught itself that an M3 Max cannot play HEVC 2160p. A
session with any `buffer_limit quota` beacon still falls back (a transcode
genuinely fixes it, by being smaller) but no longer remembers it as a decode
limit, because it was never a decode limit.

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

### 4.5bis Asking for a key frame is not getting one (found 2026-07-29)

§4.4 shipped, and on nynuc the segments were **10.4 seconds long**. The
request was two. This is the first measurement the plan has of a hardware
encoder doing what §4.4 asked, and it was not doing it.

`-force_key_frames` asks the encoder for a key frame. QSV and NVENC answer
with an I-frame that is *not an IDR*, and a non-IDR I-frame does not carry
`AV_PKT_FLAG_KEY`. The HLS muxer can only cut at a flagged key frame, so it
ignores every boundary asked for and falls back to the encoder's own GOP:
250 frames at 23.976 fps is 10.43 s, against 187.7 s over 18 segments
measured. Reproduced locally at 2.00 s with effective forced key frames and
10.00 s without.

The consequence is the whole plan's headline symptom. Nothing plays until a
segment exists, so the start floor was still ten seconds of encoding on the
one box that has hardware — while §4.4's acceptance, run in CI on software,
passed. **A milestone validated only where the bug cannot occur is not
validated.**

Fixed by passing `-forced_idr` (QSV) / `-forced-idr` (NVENC); VA-API needs
nothing, its `idr_interval` already defaults to making every I-frame an IDR,
which is why VA-API never showed the symptom. The flag is *measured* by the
startup probe rather than assumed: the probe runs production's arguments
with it and retries without on failure, so a build that will not take the
option keeps its hardware and gets a warning instead of vanishing. Losing a
GPU to an unrecognised option is a worse trade than the latency the option
removes — the same reasoning that left VA-API's `-rc_mode` alone below.

**Accepted 2026-07-29:** nynuc reads **2.00 s median** (min 1.96, max 2.00)
across 92 segments, where the same command read 10.4 s before the fix. The
start floor is where §4.4 said it was.

**The lesson worth keeping:** every acceptance in §4 that is about what an
*encoder* does was run in CI, on software, where the encoder in question is
absent. §4.4's segment length is simply the one that got caught. The others
are worth re-reading with that in mind.

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

**Measured on nynuc, 2026-07-29 — the bound holds.** A 1080p QSV session:
mean 7.74 Mb/s against an 8 Mb/s nominal, peak **9.05 Mb/s over a 10-second
window** where the model permits `maxrate + bufsize/W` = **13.6**. The worst
single segment was also 9.05, so the peak is not a windowing artefact.

The measurement lives in [`scripts/perf-report`](../scripts/perf-report)
rather than `scripts/bench`, and starts a real session rather than
reconstructing the flags — a Python copy of the argument list would only
prove that two copies agree, which is the drift review R4 already objected
to. Re-run it after any rate-control change:

```bash
scripts/perf-report --url http://nynuc:32400 \
  --ratecontrol <file_id> --height 1080   # pick the grainiest 4K HDR title
```

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
  by `hls_ahead_max_bytes`) + the ~180 s retention window behind the
  frontier. At the defaults that raises the media span from about 300 s to
  360 s (+20%), so the unchanged 8 GiB global cap may bind roughly one 4K
  stream sooner. Its byte release threshold remains half the cap; size tmpfs
  from `hls_scratch_max_bytes` plus headroom.
- **NFS/SMB read-ahead.** One paragraph with mount suggestions
  (`rsize=1048576`, larger readahead for NFS; SMB3 multichannel where the
  NAS offers it) and the reminder that §4.2's burst reads at NAS speed —
  a starved mount now shows up as `speed < readrate` in the §4.5
  telemetry instead of as a mystery.
- **jellyfin-ffmpeg everywhere** (already the OPERATIONS recommendation)
  — §5 leans on its filter set; the pacing flags need ≥6.1 for burst.

### 4.9 The input side had never been measured (shipped 2026-07-29)

Everything in §4 measures what the server did with a file once it had it. The
encoder is probed at boot, the tone-map graph is raced against a CPU
reference (§5), the output bitrate is bounded and checked (§4.6), and the
client reports what it received (§3). Nothing measured whether the source
could be *read* — and that is the one link in the chain that is usually
neither local nor fast nor constant.

The gap is not academic. A remux copies the video untouched, so a 69 Mb/s
file needs 8.6 MB/s off storage forever just to break even, and `-readrate 4`
asks for 34 MB/s. When either is unavailable, nothing in the system says so:
it reaches the viewer as a client `supply` stall, and the encoder
simultaneously reports under 1×. Both readings are true, neither is the
cause, and both point away from it. §4.8's ops note — "a starved mount now
shows up as `speed < readrate`" — was the closest the plan came, and it is a
symptom, not a measurement.

`storeprobe.rs` measures it. Per **mount** (roots grouped by device id, so
two libraries on one array cost one probe): sustained sequential read, and
median cold-seek latency — the second because it is what `-ss` pays before
ffmpeg can emit anything, and on a remote mount it frequently explains a slow
scrub on its own. Reported in bits per second, deliberately, so it can be
compared to a source bitrate by looking at it.

Measuring honestly is most of the work, and the enemy is the page cache. A
probe that writes its own fixture and reads it back measures RAM and reports
a NAS at 6 GB/s. So it reads a **real media file already in the library**, at
offsets deep inside it, drops the range first via `posix_fadvise(DONTNEED)`,
samples twice at unrelated offsets and keeps the **lower** result, and flags
anything past 64 Gb/s as cache-warm rather than reporting it. That threshold
is deliberately high: low enough to catch every cached read would
permanently label an ordinary SATA SSD as "not a measurement", which is a
worse lie in the other direction.

It runs a few seconds after boot rather than during it — the numbers are
worth having, but not at the price of a server that will not answer until a
sleeping array has spun up — and re-runs on demand via
`POST /api/v1/system/storage`, because a mount can be re-exported and a link
re-cabled without a restart. Surfaced on Settings → System, in `/system`, and
in `scripts/perf-report`, which states the answer in the unit the decision
needs: the largest source this mount can feed at realtime, and at the
configured pace.

### 4.9bis An average is the wrong question (shipped 2026-07-29)

nynuc's first numbers made the point immediately. Four mounts — `/8t-2`,
`/8tb`, `/media`, `/20t`, four separate devices — came back at 242, 265, 242
and 258 Mb/s, with cold seeks of 3.0–3.3 ms. Four different arrays do not read
at the same speed by coincidence: that number is **the path they share**, not
the disks. On a NAS with 2×2.5G, ~250 Mb/s means the current wireless bridge
is costing about 90% of the available bandwidth.

But it also refuted the explanation §4.3bis was reaching for. 250 Mb/s against
a 69 Mb/s source is 3.6× headroom, and steady-state playback needs 138 Mb/s
even if the read and the delivery cross the same hop. **The average is fine.**
Whatever produced nine stalls is not visible in it.

Which is the general lesson: an average cannot show a gap, and a gap is what
stalls a viewer — a stretch where supply fell under demand for longer than the
client's buffer covers. So the probe grew a second, opt-in mode: read
continuously for N seconds, sample every **250 ms** (short enough to resolve
the ~2.2 s that matters), and keep the trace rather than only its statistics.

The trace is then *replayed*. `Sustained::dry_spells` simulates a client
holding `buffer_secs` of a `bitrate_bps` stream against the measured windows
and counts how often it would run dry, how long it spent dry, and how close it
came at its worst. Run over a ladder of source bitrates at both buffer depths —
2.2 s for the progressive path, a conservative 15 s for MSE — and the value of
a deeper buffer stops being an argument and becomes a number measured on the
operator's own hardware. `0 stalls` alone would not be enough, so the low-water
mark rides along: a run that survived with 0.1 s to spare is a near miss, not a
pass.

Two failure modes the first version had, both of which reported success:
reading off the end of a small file produced a trace too short to contain a gap
(now wraps, and says it wrapped, so later windows are marked optimistic), and
a probe shorter than one window produced no windows at all (now flushes a
partial tail). Both would have answered "no gaps found" from evidence that
could not have held one.

### 4.9ter nynuc's measured baseline (2026-07-29)

Recorded so later runs have something to diff against. All four mounts are
NFSv4.1 exports from one QNAP; nynuc reaches it over a wirelessly-bridged hop
while every other node is wired at 2.5G.

Before, with the original mount options (`rsize=wsize=32768`, `timeo=14`, no
`nconnect`), 30 s traces at 250 ms:

| mount | min | p10 | median |
| --- | --- | --- | --- |
| `/8t-2` | 84 | 134 | 161 Mb/s |
| `/media` | 100 | 183 | 235 Mb/s |
| `/20t` | 98 | 172 | 253 Mb/s |

After (`rsize=wsize` negotiated to 1 MB, `timeo=600`, `nconnect=8`):

| mount | min | p10 | median |
| --- | --- | --- | --- |
| `/8t-2` | 159 | 200 | 328 Mb/s |
| `/media` | 112 | 249 | 300 Mb/s |
| `/20t` | 103 | 261 | 335 Mb/s |
| `/8tb` | 142 | 255 | 333 Mb/s |

Medians up 28–104%, p10 up 36–52% — well outside the ±25% run-to-run variance
the quick probe shows on its own. 32 KB reads were costing a third to a half
of the available bandwidth.

Two things the after-numbers say that the before-numbers could not. All four
mounts **re-converged**, at 300/333/328/335 — four separate arrays and four
separate exports agreeing to within 10% again, just higher. Individual windows
burst past 500 Mb/s. That is a shared ceiling that the per-mount inefficiency
had been masking, and it is still there. And under playback the whole path
loses 25–40% (`/20t` median 253 → 149 on the old options), with cold-seek
latency more than doubling on `/8t-2` — real contention, measured.

**What none of it shows is a gap.** Across four traces on two mount
configurations, no 250 ms window ever fell below the 69 Mb/s the stalling file
needs; the worst on `/20t` was 103 Mb/s. At the observed rate of one stall per
ten seconds, a 30 s trace should have caught three every time. It has never
caught one. The read side is not the trigger — which is what sent the
investigation to §4.3bis and the browser's own buffer.

The one place the probe currently changes behaviour is a log line at remux
start:
under 1.2× headroom it warns that the stream will stall, and under the
configured readrate it notes that the stream can play but will never build a
reserve. That is deliberately advisory for now. Capping `readrate` at what
the storage can actually deliver is the obvious next step and the right one —
§4.3bis shows the refill burst can be the thing that causes the stall — but
it should be made against real numbers from a real library rather than
against a plan's guess at them.

## 5. M2 — the real 4K fix: tone-map on the GPU (kills B2)

**Status:** the graph is accepted; how much of a real library it reaches is
now *measurable* — `GET /api/v1/system/library-shape` and the `library shape`
section of `scripts/perf-report` count what is on disk by HDR flavour, at 4K
and overall.

That question needed asking because the probe cannot answer it. The graph is
raced against a generated HDR10 fixture, and the same graph is **declined
outright for Dolby Vision** — the vendor filter cannot read its dynamic
metadata, so those titles take the CPU chain regardless of how fast the GPU
one measured. A node can therefore report a 5x tone-map and get it on almost
nothing. The census states the split directly: of N 4K HDR files, the GPU
graph can take the HDR10 and HLG ones, and the Dolby Vision remainder is not
a fallback but the correct answer.

Aggregated in SQL, one scan, on its own route rather than as a field on
`/system` — the settings page polls `/system` every few seconds, and a table
scan behind a UI timer is a different kind of mistake.

**Measured on nuc4, 2026-07-29 — and the answer is 24%.** 5427 probed files,
1359 of them 4K:

| | 4K files | share |
| --- | --- | --- |
| Dolby Vision | 614 | 45.2% |
| SDR | 555 | 40.8% |
| HDR10 | 190 | 14.0% |

Of **804 4K HDR files the GPU tone-map can take 190**. The other 614 are Dolby
Vision and go to the CPU chain — correctly, not as a fallback, and entirely
unaffected by M2's speedup.

So M2 is real and M2 is narrow. The graph does what §5 said it does, on a
quarter of the 4K HDR in this library. That does not make it wrong to have
built — a 4.9x tone-map on 190 titles is 190 titles — but "4K HDR is fast now"
was never the claim the probe supported, and on this library it would have been
three-quarters false. **The pre-transcode cache (M3) is the lever for the other
614**, because a cached Dolby Vision title pays the CPU chain once, overnight,
instead of every time somebody presses play.

Two more numbers worth keeping from the same run: 186 files sit at or above the
40 Mb/s segmented-remux floor (§4.3bis), the largest at 98 Mb/s; and the codec
split is 58% H.264, 37% HEVC, with 96 AV1 files already on disk. `vpp_qsv` passed nynuc's boot probe at **4.88–4.89× the CPU chain**,
above the ≥3× objective. Every node reports its own verdicts on Settings →
System and in `/api/v1/system`.

**But the first 4K session measured on that box ran the CPU chain anyway** —
and correctly. `Pipeline::handles` sends Dolby Vision and HLG to the float
chain, because the vendor tone-map cannot read DV's dynamic metadata and
handles HLG inconsistently across drivers. Most 2024-era UHD remuxes are DV.
So the question this milestone did not think to ask is **what fraction of a
real library is HDR10** — the only part 4.88× applies to. Sessions now log
`proven`, `hdr` and `declined`, so a report answers that rather than raising
it; not enough sessions have been collected yet to say.

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
graph-selection log line says so. Subtitle burn-in of **either** kind selects the
compatible CPU/hybrid graph — the `subtitles` filter is CPU-only, and a
bitmap composite is system memory here too — so accept the download/upload
for those sessions. **Bitmap** subtitles are burned as of 2026-07-28
(decision 10, reversed): `[0:s:N]scale=W:H[sburn]` against the *output*
frame, composited with `overlay=eof_action=pass`. The scale is the whole
trick — a UHD Blu-ray's PGS canvas is usually 1920×1080 over 3840×2160
video, and compositing that as-is puts every subtitle at quarter size in
the upper-left quadrant. Verified against a real PGS stream, which is also
now the thing the corpus should carry.

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

**Admission shipped 2026-07-28.** `transcode.max_hw_sessions` (default 2),
acquired by compare-and-swap rather than by a scan under the sessions lock —
the property that matters is that two racing starts cannot both see the same
free slot, and a CAS gives exactly that without a second lock to order against
the first. The slot is a guard held by the session, released explicitly when
the session ends and by `Drop` as the backstop: explicit because the watchdog
holds an `Arc` to the session for its whole grace window, so waiting for the
last reference would keep a slot for twelve seconds after a viewer closed the
tab; by `Drop` because the ways a session can end outnumber the branches
anyone writes.

The ladder is queue (≤5 s) → measured-safe software → refusal, and
"measured-safe" is measured *here*: sessions record their recent speed per
class of work, and admission reads that class back. Two things are in the class
key on purpose — the SOURCE's resolution and HDR rather than the output's (the
decode and the tone-map happen at source resolution however small the output,
which is what makes "admit it at 480p" the wrong answer), and the encoder (a
QSV session at 6× has measured nothing about what x264 would do with the same
file). A node that has measured nothing yet guesses from the shape, and guesses
toward refusal for 4K and heavy-codec HDR, because §2.9 measured both
sub-realtime in software on exactly this hardware and an optimistic guess there
costs a viewer their whole session.

**M2 acceptance completed 2026-07-29 on nynuc.** The QSV tone-map path ran at
4.89× the CPU chain and stayed inside the measured bitrate bound. The commands
below remain the reproducible acceptance procedure for a new driver or host.

**Acceptance:** on nynuc, the M0 telemetry shows a 4K HDR10 → 1080p
session at ≥3× (QSV graph) vs the recorded CPU baseline; a 2 h 4K HDR
play completes with zero stall beacons at the 1080p rung; the corpus adds
one HDR10 and one HLG asset asserting *which* pipeline was chosen (log
assertion), so a driver regression shows up as a pipeline downgrade in
CI, not a user stutter; concurrent-start tests cannot exceed the slot
cap; every admitted fallback sustains its speed margin over the
validation window.

## 6. M3 — the pre-transcode cache

**Status:** shipped 2026-07-28. Everything in §6.1–§6.3 is built and
tested; the deviations from what is written below are recorded in §6.4.

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
`applied` is now always true for a requested burn — the skip it
recorded no longer exists — but the field stays: a future path that cannot
burn must not be able to hash as though it did.

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

### 6.4 What was built, and where it differs from the above

Shipped 2026-07-28 across seven commits. The design above survived
contact; these are the places the code says something the plan did not.

**Track selection is part of the recipe, and the producer has to run it.**
Not stated in §6.1's field list, and the omission cost a full debugging
cycle: the producer named its output for a session with no audio track
chosen while every real playback picked track 0, so the cache filled and
never hit. From outside that is indistinguishable from a cold cache, and
every unit test in the suite was happy with it. One `select_tracks`
function now serves both paths, beside `options_for`.

**The claim is a bookmark as well as a lock.** §6.2 says a preempted
producer resumes "from that boundary" without saying what carries the
boundary across a process restart. It is the claim row plus the staged
parts, read back off disk — which keeps the bookmark and the bytes the
same fact, so a crash between writing one and the other cannot leave them
disagreeing. Resuming therefore rests on one producer per node at a time;
`JobManager::produce_pass` enforces it and `TranscodeManager::produce`
documents it. A `kill -9` mid-encode is tested: the next process picks up
at the boundary and publishes a gapless asset.

**Staging needs its own lifecycle rule.** Putting the temp directory under
the cache root (so publication is a rename on one filesystem) puts it at
exactly the depth of a fanout prefix, and the orphan sweep treated it as
one — deleting a producer's work mid-encode, from the maintenance job
running beside it. What keeps a staging directory alive is the *claim on
its recipe*, not a published location, because a producer part-way through
a two-hour film has the former and cannot have the latter.

**The cache budget is soft only while a viewer owns the bytes.** A cached HLS
session holds process-local ownership on its recipe. LRU, stale-claim, and
orphan cleanup all take exclusive ownership before deleting, so a source-file
reconcile that cascades the location row cannot turn the next segment into a
404. Housekeeping logs `protected` on every affected sweep and
`plurx_cache_protected_entries{reason="active_playback"}` reports the current
number of held recipes. A non-zero value with stable free space means cleanup
is waiting correctly; a value that never returns to zero means a session is
not being reaped.

**A software producer needs the same standing-down as a hardware one.**
§6.2 describes yielding in terms of the slot machinery, which software
sessions never touch. Without an explicit check, the loop spawned ffmpeg
and killed it microseconds later on the first poll, hundreds of times a
second, for as long as anybody was queuing to play something.

**Candidate selection under-selects on purpose.** §6.2 says candidates are
filtered by whether their decision "would be a transcode or copy against
the caps the user last played with". Those caps are not persisted yet, so
the filter is the source instead: 4K, HDR, or HEVC — what nothing plays
natively, and therefore what almost any client will transcode. A missed
candidate costs a viewer the ordinary two-second live start; a wrong one
costs gigabytes and an hour of GPU on a transcode nobody asks for.

**Resume is within a producer run and across passes, but a run that
exhausts its six-hour window still discards its progress.** The parts are
named and numbered on disk and the claim row is where a longer-lived
checkpoint would live, so lifting this is contained — it has simply not
been needed, because a pass that gets any hardware at all finishes a film
well inside the window.

**Still not measured:** the *cached* TTFF and seek numbers. The mechanism is
verified end to end — a hit starts with no ffmpeg spawned and seeks by
`currentTime` — and nynuc's live starts now measure ~684 ms median, so the
≤1.5 s target is already met before a cache hit is involved. Nobody has
played a cached title yet, because the producer has not been switched on
there (`cache_produce_mins: 0`).

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
    session_id, owner_node_id, owner_epoch,
    lease_expires_at_unix_ms,      -- 6 s lease, renewed every 2 s
    recipe: SessionRecipeInputs,
    produced_playable_through_ms,  -- §4.2's playable clock, replicated
    fetched_through_ms,            -- the download frontier
    media_origin_ms,
    media_sequence,
    discontinuity_sequence,
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

`SessionRecipeInputs` carries `file_id`, target height, video/audio rates,
channel count, audio index, subtitle-burn identity, `tone_map_required`, and
whether audio was copied. `tone_map_required = false` means the original
session used `ToneMap::None`; `true` lets the survivor select the locally valid
Zscale or Libplacebo path. The initial `start_seconds` is not takeover state:
the two published frontiers and `media_origin_ms` define the resume point.

The replacement writer starts at `media_sequence`, not ffmpeg's current
hardcoded zero, so a URI is never reused for different bytes. When the
failover marker slides out of the live window, the playlist still emits the
replicated `discontinuity_sequence`; pruning a prefix must not renumber the
remaining discontinuities.

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
  `cached: bool` (§6.3). `subtitle_unavailable` is no longer needed —
  decision 10 reversed, and a bitmap subtitle is burned rather than
  disclosed as missing.
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
| 2 | Server ahead policy | Three named frontiers (§4.2); pace on playable−fetched from `EXTINF`; suspend on time **or** per-session bytes **or** global scratch. Time holds above 180 s and releases at 150 s; byte/global limits release at half. Status names the active reason and matching release value. Retention covers the measured 120 s forward lead + back-buffer + retry ≈ 180 s. The build-36 iPad fetch-loop stop remains a client/playlist device investigation; raising the release point did not make already-published media more fetchable. |
| 3 | Playlist/GC contract | ~~EVENT + documented 404s outside the retention window.~~ **Reversed 2026-08-04 after live AVPlayer evidence:** keep EVENT as the internal writer history, but serve a sliding window that omits the pruned prefix and advances `MEDIA-SEQUENCE`; deletion inside the 180 s retention window remains forbidden. A native player may revisit a playlist entry after a reload or decoder reset, so “normal forward clients will not ask again” was not a safe wire contract. |
| 4 | ABR handoff | Visible restart (Option A) with measured p95 interruption SLO — proposed ≤2.5 s on LAN, **operator confirms**; estimate seeded across Hls instances; severe pressure jumps straight to the highest safe rung |
| 5 | Over-capacity & fallback admission | Queue ≤5 s → *measured-safe* software recipe (recent speed ≥ ~1.2× for that pipeline class — never "720p is safe") → capacity error; cluster inserts "another node" first |
| 6 | Cache identity | Encoder family + pipeline digest in the hash; relaxation only after per-family output contracts are proven equivalent (**operator's call** whether ever worth it) |
| 7 | Background preemption | Checkpoint-and-terminate — SIGSTOP does not release hardware sessions (§6.2); resume from last published boundary; discard unpublished temp |
| 8 | Failover prefix | Hybrid: one regenerated overlap segment covers served-but-unacknowledged; client-buffer reliance for everything earlier (§7.3) |
| 9 | Owner fencing | 6 s lease / 2 s renewal; epoch on every publication; resume SLO is a measured ≤10 s budget (§7.3) |
| 10 | Bitmap subtitles | ~~Remain unsupported; disclosed, not rejected~~ — **reversed 2026-07-28 at the operator's call: they are burned.** The decision assumed disclosure was the cheap answer and burn-in the expensive one, which held only while nobody had written the overlay graph. Written, it is one composite and a scale, and the disclosure it replaced would have been a label explaining why a film's subtitles are missing — worse than the thing it was standing in for. Selecting a bitmap track re-opens the stream as a transcode with the subtitle drawn in; cache identity records `applied=true` for a burn that happened. |

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
| Focused week ✅ | §5 (M2) — real-HDR probe on nynuc | `vpp_qsv` at 4.89×; the 30 s stutter class gone; admission by measured speed |
| Weekend 3 ✅ | §6 (M3) — slot arbiter v1 landed with it | predicted plays start with no encoder and seek like direct play; producers yield to viewers |
| The nynuc run ✅ | §5 + §4.6 accepted; §4.5bis found | 4.89× tone-map, 9.05 of 13.6 Mb/s — and the 10.4 s segments nobody had measured |
| The second run ✅ | §4.5bis accepted; the DV question found | 2.00 s segments — and a 4K session on the CPU chain, correctly, for a reason nothing logged |
| The third run ✅ | starts measured; the beacon label fixed | median ~684 ms — and five supply stalls on 4K remux that the mislabelled series had been hiding inside the start times |
| With Phase 4 | §7 (M4) — fencing + takeover protocol per §7.3 | pool of nodes; failover inside the measured budget; overnight cluster pre-caching |

Every slice leaves the tree releasable; nothing in a later slice is
load-bearing for an earlier one. The correction pass went first because
building §4.4 onward on accounting the review cycle disproved would have
compounded the debt; with it landed, the remaining uncertainty is
measurement uncertainty owned by M0 — which is where uncertainty belongs.

M3 shipped ahead of M2's acceptance run, which was the right order for a
reason worth writing down: the cache is the only milestone whose value does
not depend on which encoder wins, so it could be built and proved on a box
with no GPU at all. M2's could not be, and the delay cost something — §4.4
had been "done" for a day while the symptom it exists to kill was still
there on the only machine that could show it.

**What the nynuc run actually taught.** Two acceptances passed on the first
try, which is the boring half. The valuable half is §4.5bis: a milestone
about segment length, validated in CI on software, shipped with segments
five times too long on hardware. **An acceptance run on a machine where the
bug cannot occur is not an acceptance run** — and §4 is full of criteria
about what an *encoder* does, all of which were checked exactly that way.

**Measured on nynuc, 2026-07-29.** Start times, once the beacon labels were
corrected (a stall's duration had been entering the series as a start):
**197 ms direct play, 651–1536 ms remux, median ~684 ms** — against the
~2–3 s M1 aimed at and the ~10 s this plan opened with.

**Still open, and it is a real one:** five `supply` stalls on the same 4K
HEVC progressive remux, each with **0.1 s buffered** — the client running
dry over and over on a title it never builds a reserve for. That is symptom
#2 from §1, on the remux path rather than the copy-HLS one §4.2 fixed. The
delivery-rate telemetry added in §4.3 is what should settle whether it is
the NAS read, the link, or `stream_readrate`; it has not been read yet.

**Next, in order.** The 4K remux stall above — it is the last of the four
original symptoms with no explanation. Then how much of the library M2
actually reaches (§5): 4.88× applies only to HDR10, and a library of Dolby
Vision remuxes gets none of it. Both need playback rather than code. Then
M4, which waits on Phase 4.
