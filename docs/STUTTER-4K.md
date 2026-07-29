# 4K copy-path stutter — what it is, what it isn't, and what to try next

**Status:** unsolved, cause narrowed to the segmented copy-video path ·
**Symptom:** one held video frame per segment boundary on a 4K HEVC remux ·
**Reproduced on:** all four of Paul's nodes, Chrome only client tested ·
**Diagnosed against:** `95e88eb` (2026-07-29) · **Instrument:**
`armHitchDetector` in [index.html](../crates/plurxd/src/web/index.html),
surfaced by [`scripts/perf-report`](../scripts/perf-report) ·
**Companions:** [PERF-PLAN.md](PERF-PLAN.md) §4.3bis (the routing that
created this path), [PLAYBACK.md](PLAYBACK.md) (the delivery map)

This is a handoff document. It exists because the investigation on
2026-07-29 spent most of a day eliminating causes that turned out to be
innocent, and every one of those eliminations is worth more to the next
person than the remaining guesses are. Read §2 for the symptom, §3 for what
is already ruled out and how — **do not re-run those** — and §5 for the
hypotheses still standing.

Two standing instructions. First, the artifact is smaller than every
aggregate: buffer occupancy, dropped-frame counts, stall counts, storage
throughput and decode verdicts all read healthy on a session that visibly
stutters, so a healthy panel is not evidence. Second, §3 records four
hypotheses that were killed by direct experiment rather than argument; if a
new theory duplicates one of them, the experiment is already written and
lives in §6.

---

## 1. The delivery paths, because the bug lives in exactly one of them

plurx picks one of four ways to get a file to a browser
([PLAYBACK.md](PLAYBACK.md)). Only the third is implicated.

```
 browser-native container + codecs? ── yes ──▶ DIRECT PLAY
        │ no                                   plain file, HTTP range
        ▼
 video codec playable, only the
 container or audio in the way?  ──── no ───▶ TRANSCODE
        │ yes                                 re-encode, HLS, mpegts segments
        ▼
      REMUX  ── bitrate ≥ 40 Mb/s, or storage
        │       headroom < 8× ── yes ──▶ SEGMENTED REMUX  ← the bug is here
        │                                copy video, HLS, fmp4 segments, MSE
        └── no ──────────────────────▶ PROGRESSIVE REMUX
                                         copy video, one continuous fMP4
```

The routing rule is `prefer_segmented` in
[playback/mod.rs](../crates/plurx-core/src/playback/mod.rs):

```rust
const SEGMENTED_FLOOR_BPS: f64 = 40e6;      // absolute floor
const SEGMENTED_MIN_HEADROOM: f64 = 8.0;    // × the file's bitrate, off storage
```

It is a *hint*. The server offers it; the client verifies the browser will
actually take the codec through MediaSource (`segmentedRemuxOk`) and falls
back to progressive if not. That split matters for testing: the client can
decline, which is what §4's new menu entry exploits.

**Why the segmented path exists at all.** Chrome will only read ahead ~2.2 s
on a progressive stream unless the response carries *both* `Content-Length`
and `Accept-Ranges` (PERF-PLAN §4.3). At 69 Mb/s that is not enough runway,
so big remuxes were routed to HLS + MSE, where the buffer target is set
explicitly. **Re-verify this premise before building on it** — the
`Content-Length` + `Accept-Ranges` fix landed separately and took the
progressive path from 2.2 s to 10.8 s of read-ahead, which may have made
this whole routing decision obsolete. Nobody has re-measured progressive at
69 Mb/s *after* that fix.

---

## 2. The symptom, measured

The reference session is *Wicked (2024) Remux-2160p.mkv* on nuc4, Chrome,
Quality → Auto:

| Fact | Value |
|---|---|
| Method / transport | Remux · segmented fMP4 through MSE |
| Reason | container mkv not browser-native; audio codec truehd unsupported |
| Source video | HEVC · 10-bit · Dolby Vision Profile 7 (HDR10-compatible) |
| Resolution · bitrate | 3840×2160 · 69 Mb/s |
| Source audio | TrueHD 7.1 English → re-encoded to AAC 256k |
| Segments produced | 89 · median **1.75 s** · min 1.33 s · max 2.63 s |
| Held frames | **16 in 52 s, median spacing ~1.8 s** |
| Attribution | 7/16 within 150 ms of a segment boundary · 6/16 of a buffer flush |
| Dropped frames | 14 / 1249 |
| Rendered frame rate | 23.8 fps |
| Decoder | hardware (`mediaCapabilities.decodingInfo().powerEfficient`) |
| Buffer | 12.8 s held against an 11 s ahead / 4 s behind target |
| Delivery · client estimate | 63 Mb/s · 357 Mb/s |
| Server ahead · encode speed | 108 s · 4.58× |
| Storage under that file | 2428 Mb/s (303 MB/s), cold seek 0.5 ms |

**How to read it.** The held-frame spacing equals the segment duration. A
150 ms attribution window at a 1.75 s cadence catches under one hitch in ten
by coincidence, so 44% at a boundary is a real association, not noise.
Buffer flushes are hls.js honouring `backBufferLength` and fire *on* fragment
change, so the flush and boundary columns are largely the same event seen
twice. Everything else on that table is healthy, which is the whole
difficulty.

### 2.1 What the viewer reports, and why it misled the investigation

Paul described it as "a bad stuttering issue every like 5-10 seconds… almost
like it backs up a frame and then resumes." Segment boundaries were
eliminated early *on the strength of that estimate* — 1.75 s is not 5–10 s —
and that was the single worst decision of the day. The measured cadence is
1.8 s. A subjective interval estimate is not a measurement, and the only
reason the mistake was caught is that the frame-level detector was built
anyway.

The "backs up a frame" wording also pointed the first detector design at
backwards media-clock steps. The actual fault is a **held** frame: the same
frame presented twice, media time not advancing, nothing lost. Perceptually
the same event; a different counter.

---

## 3. Eliminated, with the experiment that killed it

Everything in this section is closed. Each entry names what was tested, how,
and the number that settled it. The harnesses are in §6.

### 3.1 Storage — not it

Four traces on nynuc, per-mount, replayed against a simulated client buffer:
no 250 ms window ever fell below realtime for the source bitrate. Storage
under Wicked reads 2428 Mb/s against a 69 Mb/s need — 35× headroom. The
built-in probe (`storeprobe.rs`) reports this per mount; §4.9ter of
[PERF-PLAN.md](PERF-PLAN.md) has the baseline.

### 3.2 Network — not it

Delivery 63 Mb/s against a 357 Mb/s client estimate, on a 2.5–10 G LAN.
nynuc's wireless bridge is a real ceiling (46 MB/s bridged vs 301 MB/s wired)
but the stutter reproduces identically on wired nodes.

### 3.3 MSE buffer-quota churn — fixed, and it was a real bug, but not this

Chrome's SourceBuffer video quota is ~150 MB and no hls.js setting raises it.
The stock 60 s / 30 s targets ask for 776 MB of a 69 Mb/s copy, so hls.js
spent every session appending, hitting `QuotaExceededError`, evicting and
re-appending. Fixed in `946486b` by deriving the seconds from a byte budget
(`bufferTargets`, `MSE_TOTAL_BYTES = 144e6`). The panel now shows 11 s / 4 s
with no quota refusals — **and it still stutters.**

### 3.4 Decode capability — not it

`navigator.mediaCapabilities.decodingInfo()` returns `powerEfficient: true`
for this stream, 14 dropped frames in 1249, 23.8 fps rendered against 23.976
source. The decoder is in hardware and keeping up.

### 3.5 `SourceBuffer.remove()` near the playhead — not it

**Experiment:** drove MSE directly with a paced fMP4 append loop and called
`remove(0, currentTime - 4)` every two seconds — exactly what
`backBufferLength: 4` does — against a control that never removed, and a
third arm removing up to `t - 30`.

**Result:** 0 held frames in 34 s in all three arms.
[`mse-eviction-experiment.js`](#6-the-harnesses)

### 3.6 hls.js live / EVENT-playlist handling — not it

The copy path serves `#EXT-X-PLAYLIST-TYPE:EVENT` with no `#EXT-X-ENDLIST`
until the film is fully written, so hls.js treats the session as LIVE for its
whole duration and runs latency control against a live edge that recedes at
2× realtime (`-readrate 2.00` after a 90 s burst). That machinery can nudge
`currentTime` and `playbackRate`, either of which would look exactly like the
symptom.

**Experiment:** hls.js against a growing EVENT playlist revealed at 1× and at
2×, plus a complete-with-ENDLIST control, all with the production
11 s / 4 s config.

**Result:** 0 seeks, no `playbackRate` change, 0 held frames in 40 s per arm.
[`hlsjs-delivery-experiment.js`](#6-the-harnesses)

### 3.7 Audio seams at segment boundaries — not it

Audio is re-encoded (TrueHD → AAC) and muxed into the *same* fragments, which
are cut at *video* keyframes; AAC frames are 21.3 ms and cannot align with a
video cut, so a micro-gap per boundary was the obvious candidate.

**Experiment:** ran plurx's exact copy-HLS command against a synthetic
open-GOP HEVC + TrueHD source and read every segment's per-stream
presentation range out of the files.

**Result:** audio seam between consecutive segments is **0.000 ms** at every
join. Audio is continuous. [`segment-ranges.py`](#6-the-harnesses)

### 3.8 Whole-frame open-GOP / RASL presentation overlap — not it

An open-GOP HEVC segment beginning at a CRA carries RASL leading pictures
whose presentation times precede the CRA, so segment N+1's timeline should
overlap segment N's by a frame, and MSE evicts on overlap.

**Experiment:** built a source verified to have leading pictures (packet
after each keyframe has an earlier PTS — confirmed with `ffprobe
-show_packets`, not `-show_frames`, which outputs in presentation order and
hides reordering), then measured the joins in the fMP4's own integer sample
units by parsing `tfdt`/`trun`.

**Result:** ffmpeg keeps the leading pictures with the correct segment. The
overlap is **0.977 ms**, sub-frame, not the 41.7 ms a lost frame would be.
[`fmp4-join-overlap.py`](#6-the-harnesses)

### 3.9 The sub-frame (1 ms) join overlap — not it either

That 0.977 ms is still a real defect, and it is *absent* from a stream that
plays clean: a VP9 copy from an MKV source with identical millisecond
timestamps joins at exactly 0 units, while the reordered HEVC copy overlaps
by one source timestamp tick at every boundary. By the letter of the MSE
spec a frame whose presentation time lands inside an existing frame's
interval causes that frame to be removed — one held frame per join.

**Experiment:** took the VP9 stream that played clean and injected precisely
this defect — extended the last video sample of every segment by 16 units
(1 ms at timescale 16000) by rewriting `trun` in place, changing nothing
else.

**Result:** 0 held frames across all three delivery arms. Chrome tolerates a
sub-frame join overlap. [`inject-join-overlap.py`](#6-the-harnesses)

### 3.10 Segment container shape in general — not reproducible

A muxed VP9 + Opus fMP4 HLS stream built to match production's shape — MKV
source with millisecond timestamps, 1.75 s segments cut at keyframes, audio
re-encoded, both tracks muxed into the same fragments, EVENT playlist
growing at 2× — played 40 s clean, three arms, zero held frames.

**What this rules out:** the *shape* of the segmentation. **What it does not
rule out:** anything specific to HEVC, 10-bit, 4K, 69 Mb/s or Dolby Vision,
because the container this investigation ran in has no HEVC decoder at all.
That is the boundary of what could be reproduced off Paul's hardware.

---

## 4. The one thing that did change the symptom

**Quality → 1080p is clean.** No held frames. This is the only intervention
that has fixed it.

It is also a six-variable change, which is why it narrows less than it
appears to:

| | 4K copy (stutters) | 1080p transcode (clean) |
|---|---|---|
| Resolution | 3840×2160 | 1920×1080 |
| Bitrate | 69 Mb/s | 8 Mb/s |
| Video codec | HEVC 10-bit | H.264 8-bit |
| Video handling | `-c:v copy` | re-encoded, `h264_qsv` |
| GOP | source's, open, keyint varies | forced IDR every 2 s, closed |
| HDR | Dolby Vision P7 passed through | tone-mapped to SDR |
| **Segment container** | **fmp4** (`seg%05d.m4s`) | **mpegts** (`seg%05d.ts`) |

That last row is the one to notice, and it was missed all day. The two paths
use different segment containers — see
[transcode/mod.rs](../crates/plurx-core/src/transcode/mod.rs) around the two
`-hls_segment_type` arguments. With MPEG-TS, **hls.js transmuxes to fMP4 in
the browser itself**, generating its own `moof`/`tfdt`/`trun` from the TS
PTS/DTS. With fMP4 it passes ffmpeg's fragments straight to the SourceBuffer.
So the clean case never exercised ffmpeg's fragment timing at all.

### 4.1 The one-variable test that now exists

Shipped in `95e88eb`: **Quality → "Original · one stream"**. Same server-side
decision as Original — it only makes the client decline the segmented
transport, so the same file at the same bitrate in the same codec is
delivered as one continuous fMP4 by range request instead of 1.75 s
fragments through MSE. The stats panel also now names the transport outright
(`Transport: segmented fMP4 · MediaSource` / `one continuous file · range
requests`).

**Acceptance:** play Wicked with that selected and read the Hitches row.

- No held frames → fragmenting through MSE is the cause. Next step is §5.1.
- Held frames every ~1.8 s → **impossible**, there are no segments; a
  non-zero count means the cadence was never the segments and §5.3 is where
  to look.
- Held frames at some other cadence, or a supply stall → progressive still
  cannot carry 69 Mb/s, and the §1 premise about the `Content-Length` fix
  needs re-measuring before anything else.

This result was not available at the time of writing.

---

## 5. Hypotheses still standing

Ordered by how cheaply each can be tested.

### 5.1 ffmpeg's fMP4 fragment timing, versus hls.js's own transmuxer

The strongest surviving lead, and the newest — it comes from the `mpegts` vs
`fmp4` row in §4. Every clean comparison in §3 either used hls.js's TS
transmuxer or a codec whose copy produced perfectly-joined fragments. Nothing
has yet tested *ffmpeg's fMP4 fragment timing on a reordered HEVC stream*
against a browser that can decode it, because no such browser was available.

**Test:** serve the copy path as `-hls_segment_type mpegts` instead of
`fmp4`, changing nothing else. HEVC in MPEG-TS is standardised; confirm
hls.js 1.6.16's TS demuxer accepts HEVC before spending time on it (it
handled AVC only for years). If the stutter vanishes, the fault is in the
fMP4 fragments ffmpeg writes for a copied, reordered HEVC stream, and §3.9
proves it is not the join overlap — so look next at `trun` composition
offsets, per-fragment `sidx`/`elst` interaction, and whether `-tag:v hvc1`
belongs on a Dolby Vision stream (`dvh1`/`dvhe` are the DV sample entries).

### 5.2 Dolby Vision Profile 7's second layer, riding along under `-c:v copy`

DV P7 is dual-layer: an HDR10-compatible base layer plus an enhancement
layer and RPU metadata. Chrome decodes only the base layer and must skip the
rest. `-c:v copy` passes all of it through, and `-tag:v hvc1` declares it as
plain HEVC.

**Test:** the cheapest version needs no code — play a 4K remux over 40 Mb/s
that is **not** Dolby Vision (Paul's library has 190 4K HDR10 and 555 4K SDR
files; §5 of the perf report enumerates them). Clean → DV is implicated.
Still stuttering → DV is innocent and it is 4K/bitrate/fMP4.

A code-level test is `-bsf:v filter_units=remove_types=…` to drop the EL and
RPU NAL units on the way out, which is legal for a copy and would make the
stream plain HDR10.

### 5.3 4K HEVC through MediaSource, independent of transport

If §4.1 shows progressive is clean, this is dead. If §4.1 shows the cadence
persists without segments, this is the whole answer: Chrome's HEVC decode
path fed by MSE appends at 4K, and the fix is a routing decision — cap the
copy path by bitrate or resolution and transcode above it, which costs GPU
and quality and should be the last resort.

### 5.4 Segment length, as mitigation rather than cause

If the artifact is one held frame per boundary and the cause proves
unfixable, raising the copy path's `hls_time` from 2 s to ~6 s cuts the
frequency by roughly 3.4×. **This is mitigation, not a fix, and it is not
free:** `SEGMENT_SECONDS` is single-sourced in
[transcode/mod.rs](../crates/plurx-core/src/transcode/mod.rs), feeds the
cache recipe hash, and is the unit of the cluster failover contract in
[PHASE3-SPIKE.md](PHASE3-SPIKE.md). A copy-path-only override has to enter
the recipe hash or cached entries from the two settings will collide.

---

## 6. The harnesses

All of these ran in a Linux container with `/opt/pw-browsers/chromium` via
Playwright. **That browser has no H.264, HEVC or AAC decoder** — which makes
it an excellent negative control for MSE-refusal tests and useless for
anything codec-specific. VP9, Opus and WebM work.

| Script | What it answers |
|---|---|
| `hitch-detector-test.js` | Does the detector report nothing on clean playback, and each fault as the right fault? 17 assertions, half against a real `<video>`, half against synthetic frame sequences. |
| `buffer-targets-test.js` | Does `bufferTargets()` stay under the ~150 MB MSE quota at every bitrate in the library? 16 assertions. |
| `mse-eviction-experiment.js` | §3.5 — does `remove()` near the playhead disturb presentation? |
| `hlsjs-delivery-experiment.js` | §3.6 — does hls.js's live handling of a growing EVENT playlist? |
| `segment-ranges.py` | §3.7 — per-segment audio and video presentation ranges, and the seam between them. |
| `fmp4-join-overlap.py` | §3.8/§3.9 — the join measured in integer sample units, by parsing `tfdt`/`trun`. Prints the video timescale and observed sample durations. |
| `inject-join-overlap.py` | §3.9 — injects a 1 ms join overlap into a clean stream by rewriting `trun` in place. |

The two `*-test.js` files extract their subject straight out of
`index.html` by string slice, so they test the shipped code rather than a
copy of it. If they stop finding their anchors, the anchors moved — fix the
slice, do not fork the code.

### 6.1 Building the fixtures

```bash
# open-GOP HEVC + TrueHD, the shape of a remux ffmpeg has to cut
ffmpeg -f lavfi -i "testsrc2=size=1280x720:rate=24:duration=30" \
       -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=30" \
       -c:v libx265 -preset veryfast \
       -x265-params "keyint=42:min-keyint=42:open-gop=1:bframes=4:b-pyramid=2:scenecut=0" \
       -pix_fmt yuv420p -strict -2 -c:a truehd -ac 2 -shortest og.mkv

# confirm it really is open-GOP: a packet AFTER each keyframe with an earlier
# PTS is a leading picture. -show_frames will not show this — it outputs in
# presentation order and hides the reordering entirely.
ffprobe -select_streams v:0 -show_packets \
        -show_entries packet=pts_time,dts_time,flags -of json og.mkv
```

---

## 7. What shipped today (2026-07-29)

Six commits, `946486b..95e88eb`. Full gate green on each: `make check`
(fmt-check + clippy + 416 tests), plus a JS syntax extract from
`index.html` and the two browser suites in §6.

### 7.1 `946486b` — the buffer targets were ten times too big

§3.3. `bufferTargets()` derives the forward and back buffer *seconds* from a
byte budget for copied streams, leaving transcodes on the 60 s / 30 s pair
that PERF-PLAN §4.3 tuned for a rate-limited rung. Constants:
`MSE_FORWARD_BYTES 96e6` · `MSE_BACK_BYTES 32e6` · `MSE_TOTAL_BYTES 144e6` ·
floors 6 s forward, 2 s back. A real bug, independently worth having; not
the stutter.

### 7.2 `7aeac39` — catch the hitch itself, frame by frame

The instrument that found the cadence. `requestVideoFrameCallback` is the
only API that sees individual presented frames. Four faults, kept apart
because their causes differ: `back` (media clock moved backwards), `held`
(same frame presented twice — the actual symptom), `gap` (media time moved
past frames never presented, which `droppedVideoFrames` cannot count because
they were never decoded), `slow` (see §7.5).

Three things ship with the counts, and the counts alone would not have been
worth having:

- **Spacing** — the median interval between hitches. This is what identified
  the segment cadence and overturned §2.1.
- **Attribution** — each hitch checked against what the player was doing
  within `HITCH_NEAR_MS = 150`: a buffer flush, a segment boundary
  (`FRAG_CHANGED`), an append. Flushes also record how far behind the
  playhead the eviction ended.
- **Clock skew** — how far the presented frame is from the element's own
  playback position, which *is* the audio clock when audio is present.

The frame interval is measured from history, never from the step under test:
`dt` across one presented frame *is* the frame interval, so a skip derived
that way defines itself as normal. The first version did exactly that and
scored a five-frame jump as healthy; the test caught it.

### 7.3 `3bc0bbb` — the pre-transcode pass ran in secret

Unrelated to the stutter, found during it. The cache producer holds an
encoder for up to six hours and reported nothing: no activity entry, no
session, no stop control. `ps auxwww` on the box was the only way to learn
that plurx itself was pinning the GPU. It now publishes title, which rail the
candidate came off (in progress / next up / recently added), and position in
the pass — *before* each encode starts, since the encode is the part that
takes the hours. `DELETE /api/v1/activity/producer` stops it cooperatively
after the current title, because the producer resumes from published segment
boundaries and a kill discards the part already made.

### 7.4 `62d1d1a` — the server could not say which commit it ran

The System page read `plurx 0.1.0` through weeks of deploys. `.git` is
outside the Docker build context, so `build.rs` falls back to `"unknown"`,
and the escape hatch it offers — `PLURX_BUILD_REF` — was declared in the
Dockerfile but never passed by `docker compose up -d --build`. `buildTag()`
then *hid* the word "unknown" as noise. Compose now forwards the arg, and an
unstamped build says so. Deploys supply it with
`PLURX_BUILD_REF=$(git describe --tags --always --dirty)`; the Ansible
playbook computes it per host.

### 7.5 `9a9179b` — `slow` measured pipeline depth, not health

The original threshold was "`processingDuration` over 40 ms", which reported
**993 slow frames out of 1249** on a session whose decoder was keeping up
fine — directly above the row that had actually found something.
`processingDuration` runs from packet submission to frame-ready, so on a
pipelined hardware decoder fed from a 12 s buffer it is mostly queue
latency; the absolute value is a property of pipeline depth, not of health,
and comparing it to a 41.7 ms frame budget compares unrelated quantities.
Now relative: three times *this session's own median*, and it must also miss
the frame budget. The median ships as context (`decode 90ms typical`) because
it is worth knowing and is not a fault.

### 7.6 `95e88eb` — ask for one stream instead of fragments

§4.1. `Quality → "Original · one stream"`, and the stats panel names the
transport.

### 7.7 Earlier the same day, before the stutter investigation

`e59bd69` password confirm on account creation · `cd8aa60` first/last
pagination · `1191834` metadata-key warnings with a repair action ·
`485f103` `a153e0e` `84f679e` stats and rail layout · `d8ee3fa` rendered
frame rate · `201690b` library shape by HDR flavour (`media_shape()`) ·
`53ec0ee` perf-report could not see copy-video sessions · `77b87d8`
`mediaCapabilities` hardware-decode verdict.

---

## 8. Non-goals and guardrails

Written for whoever picks this up, because each of these was considered and
rejected for a reason.

1. **Do not "fix" the 0.977 ms join overlap.** §3.9 proves Chrome tolerates
   it. It is a real difference between the clean and stuttering streams and
   it is still not the cause; shipping a fix for it would be shipping a
   change with no measured effect.
2. **Do not raise `SEGMENT_SECONDS` globally.** §5.4. It feeds the cache
   recipe hash and the failover contract.
3. **Do not route 4K copies to a transcode as the first fix.** It burns GPU
   on every 4K remux and throws away the quality the library exists for.
   It is the answer only if §5.3 turns out to be the cause.
4. **Do not trust a healthy stats panel.** Every aggregate on it read clean
   through the entire investigation.
5. **Do not treat a viewer's interval estimate as a measurement.** §2.1.
6. **Do not use `ffprobe -show_frames` to reason about decode order.** It
   outputs in presentation order and will tell you a reordered stream is not
   reordered. §6.1.
7. **Do not add absolute thresholds to per-frame browser metrics.** §7.5.
   Pipeline depth, decoder generation and buffer depth all move them.

---

## 9. Open questions for a fresh pair of eyes

1. Does hls.js 1.6.16's MPEG-TS demuxer handle HEVC? If yes, §5.1 is a
   one-line change to the copy args and the most informative test left.
2. Is the `prefer_segmented` routing still needed at all, now that
   progressive reads ahead 10.8 s instead of 2.2 s? Nobody re-measured
   progressive at 69 Mb/s after the `Content-Length` + `Accept-Ranges` fix.
   If it holds, the whole segmented copy path — and this bug with it — can be
   deleted rather than debugged.
3. Should `-tag:v hvc1` be `dvh1`/`dvhe` for Dolby Vision, and does Chrome
   care? Every DV file in the library currently ships as plain HEVC.
4. Is the held frame *lost* or *repeated*? The detector says media time did
   not advance a full frame, which is consistent with either the frame being
   removed from the buffer or the renderer holding it. Distinguishing them
   would separate an MSE buffer problem from a renderer/sync problem, and
   `VideoPlaybackQuality.totalVideoFrames` against the source frame count
   over a known interval should settle it.
5. Does it reproduce in Firefox or Safari? Only Chrome has been tested, on
   one browser across four servers. A second engine would separate "Chrome's
   MSE" from "the stream".
