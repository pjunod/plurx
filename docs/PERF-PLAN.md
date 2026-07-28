# Performance — where the seconds go, and the plan to get them back

**Status:** M0 + §4.1 + §4.2 + §4.3 shipped 2026-07-28 (plus §4.5, pulled
forward — the telemetry made it a twenty-line change and it guards §4.2);
§4.4, §4.6, §4.7 next · **Diagnosed against:** `e7a12cf` (2026-07-28) ·
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
| 4K starts, buffers a few seconds in, almost always | copy-video HLS is paced at `-re` = 1× — the player's opening buffer is the only buffer it will ever have (§2.2) | hls.js caps forward buffer at 60 MB ≈ 7–11 s of 4K (§2.3) | §4.2, §4.3 |
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

### 2.3 The client's own ceilings

`attachHls` (`web/index.html:1790`) constructs hls.js with defaults:
`maxBufferLength: 30` (target 30 s) and `maxBufferSize: 60 MB`. At 4K
copy-HLS bitrates (45–70 Mb/s), 60 MB is **7–11 seconds** — the size cap
binds long before the 30 s target. So even with server pacing fixed, the
client would refuse to hold more than ~10 s of 4K. Safari-native and
Apple TV sessions have no knobs at all; for them the server pace from §4.2
is the entire fix.

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
- **B5 — client buffer caps.** §2.3. `index.html:1790`.
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

## 2.7 What shipped 2026-07-28, and what it measured

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
  that moment, `hls.bandwidthEstimate` when hls.js is driving).
- **New endpoint:** `GET /api/v1/hls/:session/status` →
  `{ out_time_ms, speed, suspended, encoder }` (capability-auth like its
  siblings). Powers the stats overlay's new "encode speed" line (§4.5)
  and makes M0 measurable from the browser alone.

**Acceptance:** play a 4K HDR title on nynuc from Chrome; the logs +
overlay answer, with numbers: TTFF, encode speed over time, each stall's
runway, and `markers_for`'s cost. One evening of watching produces the
baseline table the later milestones are judged against.

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
  with the mechanism Plex uses: **pause the transcoder**. The reaper tick
  (`transcode.rs:740-770`) already walks each live session's dir for GC;
  have that walk also return the max written segment index, and:

  ```
  ahead = (max_written − max(high_segment, 0)) × SEGMENT_SECONDS
  ahead > hls_ahead_max  (default 180 s)  → SIGSTOP  (suspended = true)
  ahead < hls_ahead_max / 2               → SIGCONT (suspended = false)
  ```

  SIGSTOP/SIGCONT via `libc::kill(child.id())` — Linux/macOS only, which
  is where plurxd runs. The idle reaper and admin stop still `kill()`
  suspended children fine (SIGKILL wakes stopped processes). The §4.5
  watchdog must treat `suspended` as healthy (a stopped encoder makes no
  progress on purpose). Fallback if an ffmpeg without `-readrate`
  (< 5.1) is detected: keep `-re` on copy (today's behavior) and log the
  recommendation — same graceful degradation `push_pacing` has today.
- **Why not just a bigger readrate?** Because unbounded ahead-writing is
  the thing `-re` was protecting against; suspend keeps the protection
  while un-linking it from the user's buffer. And why not SIGSTOP alone
  with no readrate? Flat-out bursts between suspends monopolize Wi-Fi
  airtime and NAS reads — the same DHCP-starving burst the remux
  pacing comment documents (`stream.rs:40-55`). Burst-then-2× is gentle
  on the link *and* fast to safety.

**Acceptance:** copy-HLS of a 4K HEVC file reaches ≥60 s of client-side
runway within 30 s of start (M0 beacons show it); session dir never
exceeds `hls_ahead_max + KEEP_BEHIND` × segment size (test: fake clock or
short window in an integration test); AirPlay from Safari starts in ≤5 s
and survives a 10 s Wi-Fi hiccup without a visible stall; suspend/resume
transitions are logged and visible in the activity page.

### 4.3 Let the client hold what the server now sends (kills B5)

In `attachHls`, override hls.js defaults per session kind:
`maxBufferLength: 60`, `maxBufferSize: copyHls ? 400e6 : 120e6`,
`backBufferLength: 30` (default is Infinity; the server prunes behind the
playhead anyway, and 30 s bounds tab memory on 4K). Worker stays off —
the CSP note at `index.html:1778-1780` still applies, and fMP4 copy
segments don't transmux, so main-thread cost is unchanged. Safari-native
and Apple TV need nothing: their buffer follows playlist availability,
which §4.2 already fixed.

**Acceptance:** on Chrome, a copy-HLS 4K session's `video.buffered` runway
climbs past 30 s (M0 beacon); memory stays bounded on a 2 h play
(no monotonic growth in `performance.memory` across an hour).

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

**Acceptance:** a deliberately slow software 4K session (no GPU) is
*not* killed while progressing; pointing at a nonexistent decoder still
fails inside the old windows; the unit test drives the state machine on
synthetic progress feeds — no ffmpeg needed.

### 4.6 Bound the hardware rate control (kills B6)

Add `-maxrate` (1.5×) / `-bufsize` (2×) to the QSV, VA-API, and
VideoToolbox arms of `Encoder::encode_args` (`encoder.rs:99-104`),
matching software/NVENC. This is verbatim the server half of
[ADAPTIVE-QUALITY.md](ADAPTIVE-QUALITY.md) Phase 1, promoted here because
unbounded bursts also sabotage today's fixed-rung sessions on Wi-Fi.
While in there: VA-API may need `-rc_mode VBR` alongside maxrate on some
drivers — validate on nynuc, and lean on the existing per-encoder
startup validation to catch a driver that rejects the flags.

**Acceptance:** `ffprobe`-measured segment bitrates for a grain-heavy
sample stay ≤1.6× the rung on QSV; the encoder validation still passes on
all four families; ADAPTIVE-QUALITY.md's "software & NVENC only" caveat
row is updated in the same commit.

### 4.7 The Auto rung (B8 — decision needed, small code)

Today Auto = 720p for every transcode. Once §5 makes 1080p cheap, the
better default is `min(source_height, 1080)` **when a hardware encoder
won**, falling back to 720p on software. This is a policy change on
Paul's stated "quality menu exists, Auto should just be right" direction —
implement behind one function (`transcodeHeight` caller side +
`hls/start` default), flag the default in the PR description, and let the
ladder API from ADAPTIVE-QUALITY Phase 1 carry it properly later.
User-facing strings, as always, `${APP_NAME}`-clean and theme-mixed.

### 4.8 Ops notes that ride along (no code)

- **Transcode scratch off the NAS.** `transcode_dir` is
  `data_dir/transcode`, recreated at boot (`main.rs:143-150`). If
  `PLURX_DATA_DIR` sits on the NAS, every segment write pays the network
  twice. Document in OPERATIONS: data dir local, ideally tmpfs for the
  transcode subdir on RAM-rich nodes (720p sessions are ~1.8 GB/h; a 4 GB
  tmpfs holds a bounded §4.2 session comfortably; 4K copy at
  `hls_ahead_max=180 s` peaks ≈ 1 GB ahead + the ~60 s back window).
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

**Gate by probe, not by version.** Extend the startup encoder validation
(`encoder.rs:167-254` — testsrc → null encode per family) with a
*tone-map* validation: a 10-bit HDR testsrc pushed through the candidate
GPU graph. Only a node that proves the graph gets it; everyone else keeps
today's CPU chain. This sidesteps the driver/build matrix (kernel `xe`
vs `i915`, oneVPL vs MSDK, tonemap_vaapi's HDR10-only support) exactly
the way encoder selection already does, and it gives the AMD NUCs the
right answer for free: VA-API decode/encode validate, GPU tone-map
probably doesn't (VCN has no tonemap filter) → they hardware-encode with
CPU tone-map, or take `libplacebo` (Vulkan) if its probe passes —
`PLURX_TONEMAP=libplacebo` already exists as the opt-in
(`transcode.rs:145-151`); promote it to a probed candidate rather than a
blind preference.

**Scope guards:** Dolby Vision profiles that hardware-decode to garbage
stay on the existing `PLURX_HWDECODE=off` escape hatch; HLG through
`tonemap_vaapi` is unreliable — route HLG to the CPU chain (or libplacebo)
regardless of probe result, and say so in the graph-selection log line.
Subtitle burn-in forces the CPU path today (the `subtitles` filter is
CPU-only) — accept the download/upload for burn-in sessions rather than
building an overlay-on-GPU graph now; log which pipeline a session chose.

**Concurrency cap, while in here:** the docs already warn two QSV
sessions can stall an iGPU. Add `transcode.max_hw_sessions` (default 2):
when live hardware sessions ≥ cap, a new session starts **directly on
software** instead of stalling into the watchdog. A predictable slow
session beats two wedged fast ones; the cluster milestone (§7) later
turns this cap into placement pressure.

**Acceptance:** on nynuc, the M0 telemetry shows a 4K HDR10 → 1080p
session at ≥3× (QSV graph) vs the recorded CPU baseline; a 2 h 4K HDR
play completes with zero stall beacons at the 1080p rung; the corpus adds
one HDR10 and one HLG asset asserting *which* pipeline was chosen (log
assertion), so a driver regression shows up as a pipeline downgrade in
CI, not a user stutter.

## 6. M3 — the pre-transcode cache

**Objective:** the transcodes that were going to happen anyway happen
before anyone presses Play. A cache hit starts in ≤1.5 s **and seeks like
direct play**, because a completed HLS asset is a VOD playlist whose every
segment already exists — the session-restart seek dance (`B9`) simply
doesn't apply to it.

### 6.1 The cache model

Content-addressed by **recipe hash**: SHA-256 over
`(cache_format_version, file_id, size, mtime, target_height,
video_bitrate, audio_bitrate, audio_channels, audio_index,
audio_action(copy|aac), tone_map_mode, subtitle_burn(idx,bitmap)?,
audio_offset_ms, SEGMENT_SECONDS)`. Size+mtime in the key makes
invalidation lazy and unskippable — a changed file simply never matches
again, and the orphaned entry ages out via LRU. The encoder family is
**excluded**: any encoder's output satisfies the recipe (byte-determinism
is a §7 option, not a requirement — per the spike, it never was one for
correctness).

Storage: `data_dir/cache/transcode/<hash>/` — deliberately **not** under
`data_dir/transcode`, which is wiped at every boot (`main.rs:149`).
Index: migration **v11** (v10 is the watched-outbox — re-verify against
`store/sqlite/mod.rs:291`):

```sql
CREATE TABLE transcode_cache (
    id           INTEGER PRIMARY KEY,
    file_id      INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    recipe_hash  TEXT    NOT NULL UNIQUE,
    dir          TEXT    NOT NULL,
    bytes        INTEGER NOT NULL,
    complete     INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    last_used_at INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;
```

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
runs the normal `hls_args`/`hls_copy_args` with no pacing, converts the
playlist to VOD (`#EXT-X-ENDLIST` + type VOD), writes into a temp dir and
publishes with an atomic rename + `complete=1`. Gate: **run only while
`active_sessions == 0`** (setting to override) — live viewers own the
GPU; the docs' two-QSV-sessions warning applies doubly to background
work.

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
mtime: old entry never served, ages out. `SEGMENT_SECONDS` bump (§4.4)
orphans old-format entries via `cache_format_version` — test asserts the
stale hash misses.

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

### 7.3 Failover — already designed, just wire it

The spike's contract, verbatim: surviving node restarts the session from
the replicated recipe, input-seeked to the last-served boundary
(`N × SEGMENT_SECONDS` — §4.4's constant discipline pays off here),
serves a stitched playlist (served-prefix entries + `EXT-X-DISCONTINUITY`
+ new entries), client keeps its buffer, one small rebuffer. §4.2's
suspend interacts trivially: a suspended session that dies restarts on
the survivor un-suspended and immediately re-earns its window.

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
mid-4K-transcode → playback resumes ≤5 s with exactly one discontinuity
(client beacon + playlist assertion); start a session from a node with no
GPU → it lands on the capable node and plays; run the producer with all
nodes idle → jobs spread across nodes (queue rows show distinct lessees)
and a subsequent hit serves from the shared dir on a node that didn't
produce it.

## 8. Contract — exact interfaces (re-verify at build time)

### 8.1 Constants & args

- `SEGMENT_SECONDS: u32 = 4 → 2` — `plurx-core/src/transcode/mod.rs:20`.
- `Pacing` struct (§4.2) in `plurx-core::transcode`; new parameters on
  `hls_args` / `hls_copy_args`; delete both `-re` pushes
  (`mod.rs:343,354`). `PacingCaps` probe moves out of `http/stream.rs`
  into a module both stream.rs and the transcode manager reach
  (suggestion: `plurxd/src/ffmpeg.rs`).
- `encoder.rs` `encode_args`: QSV/VA-API/VideoToolbox gain
  `-maxrate {1.5×}k -bufsize {2×}k` (§4.6).
- Session ffmpeg spawns gain `-progress pipe:1`; `spawn_ffmpeg`
  (`transcode.rs:46-79`) takes over stdout parsing alongside its stderr
  drain.

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
| `playback.hls_ahead_max_secs` | `180` | suspend when written-ahead exceeds this |
| `transcode.max_hw_sessions` | `2` | live hardware sessions before new ones start on software |
| `cache.max_gb` | `50` | pre-transcode cache budget |
| `cache.enabled` | `true` | master switch for producer + serving |
| `cache.shared_dir` | *(empty)* | §7.4 shared cache root; empty = node-local |
| `jobs.pretranscode_mins` | `0` (off) | producer cadence, 15-min floor like its siblings |

Validation mirrors `stream_readrate`'s (`system.rs:554-571`): rates ≥1.0
or 0, windows sane, floors enforced server-side.

### 8.4 Endpoints

- `GET /api/v1/hls/:session/status` (§3) — capability auth, no-store.
- `/hls/start` response gains `vod: bool` and (informational)
  `cached: bool` (§6.3). Additive; existing clients ignore them.

### 8.5 Docs this plan's commits must update (same-commit rule)

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
| Weekend 2 | §4.4 + §4.6 (+§4.7 decision) | starts ~2–3 s; Wi-Fi-stable rungs |
| Focused week | §5 (M2) | 4K HDR fully hardware; the 30 s stutter class gone; Auto=1080p viable |
| Next | §6 (M3) | predicted plays start ≤1.5 s and seek like direct play |
| With Phase 4 | §7 (M4) | pool of nodes; failover ≤5 s; overnight cluster pre-caching |

Every slice leaves the tree releasable; nothing in a later slice is
load-bearing for an earlier one. If only one thing ships, ship
Weekend 1 — it converts both reported 4K symptoms and most of the start
latency with ~200 lines of change.
