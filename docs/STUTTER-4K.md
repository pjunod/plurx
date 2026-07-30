# 4K copy-path stutter — what it is, what it isn't, and what to try next

**Status:** measured fact: the client pipeline holds zero decode slack on
this stream (§5.3) — first misread as decoder capacity, corrected the same
night by the client itself (an M3 Max); the DV P7 signaling fix
(§5.3) is CONFIRMED — VideoToolbox flipped software → hardware the moment
the false declaration disappeared; the residual — one dropped
frame per segment boundary — is PROVEN segment-triggered (§5.3ter: the
same bitstream unsegmented plays 1781 frames with zero drops); the copy
segment floor moved 2 s → 6 s to cut boundary count ~3×, and since
2026-07-30 plurx cuts the copy path's segments itself, placing a boundary
only where a player has no leading picture to discard — so the residual is
now bounded by the source's clean-cut density, which `scripts/gop-census`
measures (§5.6, [SEGMENTER-PLAN.md](SEGMENTER-PLAN.md)); the
decode-margin rescue
([PLAYBACK.md](PLAYBACK.md#the-decode-margin-rescue--routing-around-a-pipeline-with-no-slack))
stands as the measured mitigation
· **Symptom:** one held video frame per segment boundary on a 4K HEVC remux
· **Reproduced on:** all four of Paul's nodes, Chrome only client tested ·
**Instrument:** `armHitchDetector` in
[index.html](../crates/plurxd/src/web/index.html), surfaced by
[`scripts/perf-report`](../scripts/perf-report) ·
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

### 5.0 The reframe, and the defect that fell out of it (fix shipped)

Everything above treats "at a segment boundary" as evidence about
*segmentation*. It is not, on its own: the copy path can only cut segments at
source keyframes, so **every segment boundary is an IRAP by construction** —
the boundary attribution in §2 supports a per-IRAP cause exactly as strongly
as a per-fragment one. The 1080p transcode being clean does not break the
tie either: its I-frames are ~50× smaller, a different codec, and a
different decoder path. With that lens, the question becomes: what happens
*in the bitstream* at every IRAP that does not happen elsewhere?

Answer, measured on a fixture built to match a Blu-ray remux
(`repeat-headers=1`, open GOP): the source repeats VPS/SPS/PPS at every
IRAP, `-c:v copy` preserves them, and the production command therefore
opened **the first sample of every fragment** with in-band parameter sets —

```
NAL 32 VPS x18  (in first sample of a fragment: 15)
NAL 33 SPS x18  (in first sample of a fragment: 15)
NAL 34 PPS x18  (in first sample of a fragment: 15)
```

— while tagging the stream `hvc1`, whose definition (ISO 14496-15) is that
parameter sets live in the sample entry *only*. Every segment boundary
handed Chrome's decoder a spec violation and an invitation to reconsider its
configuration, at exactly the cadence of the stutter. Dolby Vision sources
additionally carry EL/RPU NAL units (62/63) in every access unit that
nothing on an `hvc1` wire can use.

The fix is `hevc_copy_bsf` in
[transcode/mod.rs](../crates/plurx-core/src/transcode/mod.rs):
`filter_units=remove_types=32-34` on every copied HEVC stream, `32-34|62-63`
when the source is Dolby Vision, applied to both the segmented and the
progressive copy paths. Verified on the fixture, not argued: `framemd5` of
the decoded output is **bit-identical** with and without; HDR10 static
metadata (SEI, type 39) survives; hvcC in the init segment is untouched; and
23.976 content lands exactly on the frame grid (uniform 1001/24000
durations, last pts on-grid, audio end identical — no drift over the
fixture's length).

Two further defects fell to the same change, unlooked-for. Routing copy
through a bitstream filter makes ffmpeg re-derive packet timing through the
parser, so the MKV's millisecond rounding (sample durations jittering
504/516 at timescale 12288) becomes uniform frame-grid durations — and the
**0.977 ms presentation overlap at every segment join (§3.9) goes to
exactly zero**. §3.9 proved Chrome tolerates that overlap in isolation; it
is still gone, and the stream a browser now receives is clean on every axis
this investigation learned to measure.

**Outcome (same evening):** the strip shipped and the stream is clean on
every measured axis, but the visible hitch rate did not move materially —
and the forensics it shipped alongside produced the zero-slack measurement
whose reading, misreading and correction are §5.3. The strip stays: it is
spec-correctness with zero cost, it removed three real defects, and it is
half of the DV signaling fix §5.3 completes.

The strongest surviving lead, and the newest — it comes from the `mpegts` vs
`fmp4` row in §4. Every clean comparison in §3 either used hls.js's TS
transmuxer or a codec whose copy produced perfectly-joined fragments. Nothing
has yet tested *ffmpeg's fMP4 fragment timing on a reordered HEVC stream*
against a browser that can decode it, because no such browser was available.

**Test:** serve the copy path as `-hls_segment_type mpegts` instead of
`fmp4`, changing nothing else. The bundled hls.js 1.6.16 **does** demux
HEVC-in-TS — confirmed in the minified source: `case 36` (the HEVC TS
stream type) sets `segmentVideoCodec="hevc"` and logs "HEVC in M2TS found",
and there is an `hvc1()` box builder fed by a parsed VPS. Two constraints
before building it: Apple requires fMP4 for HEVC in HLS, so a global flip
breaks Safari/AirPlay — the container has to be chosen per session, which
means the session key must carry it; and if §5.0's strip fixes the stutter,
this whole test is moot. If the stutter survives §5.0 and the forensics rule
out decode (§5.3), this is the next experiment: it swaps ffmpeg's fragment
timing for hls.js's own transmuxer with the bitstream unchanged.

### 5.2 Dolby Vision Profile 7's second layer, riding along under `-c:v copy`

DV P7 is dual-layer: an HDR10-compatible base layer plus an enhancement
layer and RPU metadata. Chrome decodes only the base layer and must skip the
rest. `-c:v copy` passes all of it through, and `-tag:v hvc1` declares it as
plain HEVC.

**Status: shipped as part of §5.0** — DV sources now shed their EL/RPU
units (types 62/63) on both copy paths, since `-tag:v hvc1` already
forecloses any client engaging DV. If the stutter survives, the no-code
test still separates DV from generic 4K HEVC: play a 4K remux over 40 Mb/s
that is **not** Dolby Vision (the library has 190 4K HDR10 files; §5 of the
perf report enumerates them).

### 5.3 Zero slack, measured — and the correction the client itself supplied

The forensics settled the *measurement* in one round of screenshots
(2026-07-29 evening, same title, same client):

| | 4K remux (hitches) | 1080p transcode (smooth) |
|---|---|---|
| Typical decode | **41.6 ms/frame** | 6 ms/frame |
| Frame budget (23.976) | 41.7 ms | 41.7 ms |
| Slack | **none — median = budget** | ~7× |
| Slow spikes (>3× typical) | 324, and they surface | 540, absorbed silently |
| Visible hitches | 7 held · 1 late, every ~2.6 s | 3 late at startup, then none |
| Decode at the hitch itself | 5 ms (a small B-frame) | — |

The first reading of that table — "the decoder is out of headroom" — was
**wrong**, and the client is what proved it: an M3 Max MacBook Pro, whose
media engine decodes this stream at many times realtime, with
`chrome://gpu` confirming hardware video decode (HEVC Main-10 to
8192×8192), ANGLE Metal, zero GPU-process crashes and no video-related
workarounds. A median pinned to exactly 1.000× the frame budget is not what
a saturated decoder produces — it is what a **cadence-clocked pipeline**
produces: frames decoded just-in-time, one in flight, ready precisely when
consumed. The silicon loafs; the pipeline holds no reserve, so every spike
lands on screen. `processingDuration` measures submission-to-ready, which
in a just-in-time pipeline is the consumption interval, not the decode
cost. The same number on the 1080p stream (6 ms) is decode-clocked because
that pipeline holds a reserve. Guardrail 8 exists because this misread
shipped before the client corrected it.

Then Safari made the picture sharper. On the same machine, the same file
through native HLS — the path this stream format was built for — played
*worse*, with `mediaCapabilities` answering `powerEfficient: false, smooth:
false` for plain 4K HEVC Main-10 L5.1 on silicon with a dedicated HEVC
block, and the overlay reading "software — this machine has no GPU path
for this stream". Both macOS browsers sit on VideoToolbox. When both
misbehave on capable, correctly-configured hardware, the remaining suspect
is **what the stream declares to VideoToolbox**.

And this stream declares Dolby Vision Profile 7 — the dual-layer Blu-ray
profile that no browser and no Apple device supports. The NAL-level strip
(§5.0) removed the RPU/EL *data* but left the DOVI configuration **side
data**, which the muxer writes as a `dvcC` box in the init segment: a
stream that promises DV P7 and delivers none of it. Chrome mostly ignores
the box; VideoToolbox honours it, and refusing the hardware path over an
unsupported-profile declaration is exactly Safari’s observed behaviour.

**Fix shipped:** the DV strip now runs `dovi_rpu=strip=1` ahead of
`filter_units` (`hevc_copy_bsf`), which removes the RPUs *and* the DOVI
side data, so nothing remains to write a `dvcC` from and the stream is
signalled as what it is: plain HDR10. Version-gated on ffmpeg ≥ 7.1
(production runs jellyfin-ffmpeg 7.1.4; the gate is tested against real
version lines, and an older ffmpeg falls back to the NAL-only strip rather
than hard-exiting on an unknown filter). The perf report now fetches the
live session’s init segment and states outright whether any DV box
survives — the wire is proven clean or the report says why not.

Also shipped: the detector discloses blindness. Safari’s stutter produced
an *empty* Hitches row, which read as exoneration but meant
`requestVideoFrameCallback` never fired — Safari declines it on some
pipelines. A session that has played 10 s with zero frame callbacks now
says so on the panel ("faults here are invisible, not absent").

The decode-margin rescue stands, reworded: it triggers on measured zero
slack *plus* visible hitches, which is the right trigger whichever
component is eating the reserve, and it never overrides an explicit
Original. If the dvcC fix restores VideoToolbox’s hardware path, the slack
reappears, the rescue stops firing, and the remembered limit clears itself
on the next explicit-Original session.

### 5.3bis One dropped frame per boundary — the leading-picture discard

The `drop` counter's first real session (Chrome, forced Original, 45 s
watched) characterized the residual exactly:

- **22 drops**, and `droppedVideoFrames` reads 22 — the same events,
  counted by two unrelated instruments.
- Spacing **every ~1.8 s**; **15/23 events within 150 ms of a segment
  boundary** against an ~8% coincidence baseline.
- The dropped frame decodes in **5 ms**; typical decode is now 91 ms
  (the pipeline holds ~2 frames since the dvcC fix — which is why `held`
  and `late` fell to ~zero). Nothing is slow. A frame is being
  *discarded*, not delayed.

One thing in this stream is discarded deliberately, once per boundary: the
**open-GOP leading picture**. Every segment opens with a CRA keyframe
followed by a RASL frame that presents *before* it. In continuous decode
RASL frames are legal and needed; at a random-access point the HEVC spec
says to drop them. Blu-ray-typical structure carries one leading picture
per CRA — one GOP per segment — one dropped frame per boundary. The
count, the cadence and the attribution all fit, and so does the fault
surviving every bitstream strip: it is the GOP *structure*, which a copy
cannot change.

What shipped now: the copy path no longer advertises
`#EXT-X-INDEPENDENT-SEGMENTS`. That tag claims every segment is
independently decodable — true for the transcode path (closed GOP, forced
IDR), **false** for an open-GOP copy, and an explicit invitation for a
player to treat each segment start as random access and discard the
leading pictures. This was flagged days ago as "proved, not fixed"; it is
now fixed. Whether any given player honours the tag, a false claim in a
spec tag was not a thing to keep shipping. The rescue verdict also now
counts `drop` events — a session that only drops frames previously could
never earn the rescue, which was exactly the session that needed it.

**Safari, after the withdrawal (same night):** still dropping at segment
cadence — 41 drops in ~86 s, every ~1.8 s, held down to 2 — so AVFoundation
never needed the tag's permission: it treats every fMP4 segment start as a
random-access point by design, and the leading picture dies with or without
the invitation. Two other things that session settled: the realized-rate
line stayed silent while the fps counter read 31.3 on a 23.976 film, which
disproves live-edge chasing and convicts the counter (`totalVideoFrames`
counts Safari's decode-ahead; the panel now derives "rendered" from
presented frames instead), and native transports now mark playlist-boundary
crossings themselves — cumulative `EXTINF` against `mediaTime` — so
Safari's next screenshot carries the same boundary attribution hls.js
sessions get.

**Discriminators, all one click on the existing build:**

- **Quality → "Original · one stream"**: same bitstream, no MSE, no
  per-fragment appends (the progressive fMP4's moof boundaries are
  invisible to a demuxer reading a continuous stream). Drops vanish →
  the discard is triggered by segmented/MSE delivery, and the fix space
  is fragment structure or the hls.js transmux route. Drops persist →
  the decoder discards RASL at every CRA regardless of transport, and no
  copy-path change can fix it: mitigation is the measured rescue, or a
  GOP-aware segmenter that cuts only at closed points.
- **A 4K HDR10 (non-DV) remux on Auto**: whether every open-GOP HEVC copy
  does this, or the DV lineage still matters post-strip.
- **Safari, same build**: whether removing the false independence claim
  changes native HLS behaviour, and what the realized-rate line says
  about live-edge chasing on the EVENT playlist.

### 5.3ter The verdict: segment-triggered, and the segment floor is the dial

The one-stream experiment ran on the reference file (Chrome, ~75 s,
resumed mid-film): **zero drops in 1781 frames**, one 41 ms late blip, on
the same open-GOP bitstream that loses a frame at every boundary when
segmented. Continuous decode keeps the leading pictures; segment starts
are what discard them. The discard is proven segment-triggered in
Chrome's MSE, and Safari's cadence (§5.3bis) says the same for
AVFoundation.

The same panel also answered §9.2, in the negative: progressive at
69 Mb/s runs with a **2.3 s buffer** at 1.18× encode pace — Chrome
consumes the pipe at exactly realtime and holds almost nothing. Smooth,
but one storage hiccup from a stall, which is the failure mode that
created the segmented path in the first place. So the routing stays, and
the dial that remains is boundary *count*: only the segment-start
keyframe gets the random-access treatment, so drops scale with segment
length. The copy path's floor moved from 2 s to 6 s
(`COPY_SEGMENT_SECONDS`) — ~3× fewer drops in both browsers, no start-up
cost because the copy path bursts its first 90 s at disk speed, and 6 s
at 69 Mb/s (~52 MB) still fits the byte-budgeted forward buffer.
Transcode segments stay at 2 s: real-time encoder, closed GOP, nothing
to lose at a boundary.

What a 6 s floor does not do is reach zero. The residual is one discard
per ~6 s on forced-Original open-GOP playback in browsers; Auto sessions
rescue past it, TVs and players that direct-play the MKV never segment
at all, and the only true zero for browsers would be a GOP-aware
segmenter that cuts exclusively at closed points — designed and scoped as
a buildable handoff in [SEGMENTER-PLAN.md](SEGMENTER-PLAN.md).

### 5.4 Segment length, as mitigation rather than cause

If the artifact is one held frame per boundary and the cause proves
unfixable, raising the copy path's `hls_time` from 2 s to ~6 s cuts the
frequency by roughly 3.4×. **This is mitigation, not a fix, and it is not
free:** `SEGMENT_SECONDS` is single-sourced in
[transcode/mod.rs](../crates/plurx-core/src/transcode/mod.rs), feeds the
cache recipe hash, and is the unit of the cluster failover contract in
[PHASE3-SPIKE.md](PHASE3-SPIKE.md). A copy-path-only override has to enter
the recipe hash or cached entries from the two settings will collide.

### 5.6 The GOP-aware segmenter — plurx takes over the cutting

Built 2026-07-30 from [SEGMENTER-PLAN.md](SEGMENTER-PLAN.md). §5.3ter left one
residual and named its only true fix: a segmenter that cuts exclusively at
points a player finds nothing to discard at. That is now what runs.

**What changed.** A copy session on an HEVC or H.264 source no longer uses
ffmpeg's HLS muxer at all. ffmpeg writes one continuous fragmented MP4 down a
pipe — `frag_keyframe` puts one fragment per GOP on the wire — and
[`copyseg`](../crates/plurxd/src/copyseg.rs) reads it, classifies each
fragment's opening keyframe, merges consecutive fragments, and publishes a
segment boundary **only in front of a clean one**:

```
 fragment opens on …                          verdict
 ─────────────────────────────────────────    ───────
 IDR (HEVC NAL 19/20, H.264 NAL 5)            CLEAN — nothing can lead it
 CRA/BLA (NAL 16-18, 21), no sample in the    CLEAN — nothing to discard
   fragment presenting before its first
 CRA/BLA with a leading picture               DIRTY — a cut here costs a frame
 anything unparseable                         DIRTY, and counted
```

The cut rule, in full:

```
 under COPY_SEGMENT_SECONDS (6 s)? ─── yes ──▶ keep accumulating, always
        │ no
 next fragment CLEAN? ──────────────── yes ──▶ cut (clean)
        │ no
 past COPY_SEGMENT_MAX_BYTES (48 MB)
   or COPY_SEGMENT_MAX_SECS (15 s)? ── yes ──▶ cut anyway (CEILING CUT)
        │ no
        ▼
   keep accumulating
```

**The floor gates the ceilings, and that ordering is load-bearing.** 48 MB
arrives in 5.6 seconds at the reference file's 69 Mb/s, so a byte ceiling
allowed to fire on its own would cut *below* the six-second floor and hand a
viewer MORE boundaries than the muxer it replaces — on precisely the file this
whole investigation is about. Every boundary costs a frame; a segmenter that
adds boundaries to a source with no clean point would be worse than doing
nothing, and that is the one thing it is not allowed to be. The floor is the
measured decision from `e212c55`, which weighed 6 s ≈ 52 MB at 69 Mb/s and
took it. The ceilings bound how long the segmenter is willing to wait *past*
the floor for a clean point, and nothing else. (Asserted:
`no_ceiling_may_cut_below_the_floor`.)

A cut taken at a ceiling with no clean point in reach is **a ceiling cut**: it
still costs the leading picture, and it is counted as such in the session's
closing log line and in [`scripts/perf-report`](../scripts/perf-report).

**What it does not change.** Same `init.mp4` (the pipe's `ftyp`+`moov`,
verbatim), same `segNNNNN.m4s` from zero, same EVENT playlist, same
tmp-then-rename. The segment index, the ahead-window suspend, the
behind-playhead GC and the serving layer all read the playlist and never
learned anything happened. No player-side change of any kind.

**And the segments have the muxer's shape, box for box — learned the hard
way.** The first cut of the merger diverged twice from what ffmpeg's HLS muxer
writes: it omitted the two `sidx` boxes (optional in HLS fMP4, ignored by
hls.js's passthrough) and it wrote one `trun` per source fragment rather than
one per track, because that let each source `mdat` payload be copied whole.
Chrome played it. **Safari refused the stream outright**, and the player's
error fallback re-encoded a 4K remux down to 1080p — the exact failure this
whole path exists to prevent, on the exact browser it exists for, reported by
Paul within an hour of the deploy. A `stream_rejected` beacon is what it
leaves in the log.

Both divergences are closed. Each track's slices go into the merged `mdat`
consecutively, so its whole contribution is contiguous and one `trun` with one
data offset covers it; the `sidx` boxes are ffmpeg's shape byte for byte,
`earliest_presentation_time` (which ffmpeg fills with the track's `tfdt`)
included. `styp sidx sidx moof[traf traf] mdat`, asserted by
`a_published_segment_has_the_muxers_shape`. The only remaining difference is
that every sample writes its own duration, size and flags where ffmpeg leans
on `tfhd` defaults — a density choice, and the most-exercised encoding in any
MP4 parser.

**And the segments carry only the tracks the session asked for — the actual
bug.** Closing the two structural divergences above did not fix Safari, and
the reason was in the init rather than the segments. ffmpeg's mp4 muxer turns
a source's chapters into a QuickTime `text` track plus a `chpl` box, with
`tref` links from the real tracks; its HLS muxer does not. So the segmenter's
pipe declared and carried a **third track** nothing asked for. Safari refused
the stream — `MEDIA_ERR_DECODE`, 708 ms in, `stream_rejected` in the log —
and the error fallback re-encoded a 4K remux to 1080p. Chrome ignored the
extra track and played on.

`-map_chapters -1` on the copy path stops it at the source (chapters reach
the player from ffprobe at playback start, never from the media stream), and
an init that declares a non-media track now takes the legacy fallback rather
than reaching a player at all.

Why nothing here caught it: **every disc remux has chapters and no `lavfi`
fixture does.** The suite is built from synthetic sources, and this is the
class of difference synthetic sources cannot have. The test that exists now
runs BOTH muxers on a chaptered fixture and requires them to declare the same
tracks — the golden comparison SEGMENTER-PLAN §8.4 asked for, which would
have caught this before it shipped and which was the one milestone check
skipped.

The lesson worth keeping, twice over: "optional in the spec, and one player
ignores it" is not evidence that another player does — and the bytes a browser
sees should differ from the muxer being replaced **only** in the thing being
changed. Both failures were the same mistake: a difference introduced without
being noticed, in a stream whose whole promise was that one thing changed.

**The proof it is a byte mover, not a transcode.** `framemd5` of `init.mp4`
plus every published segment is identical to `framemd5` of the continuous
stream they were cut from, on the video and the audio track both — and
`framemd5` prints a hash per frame *with* its pts and duration, so a
reordered, retimed, dropped or duplicated frame all fail it. Asserted in CI
(`cargo test -p plurx-core fmp4::merge`) and re-run against the browser
harness's real session output (1152 video frames, 2401 audio frames,
identical). Consecutive segments join at exactly zero ticks on both tracks —
the 0.977 ms overlap of §3.9 is not merely gone, it is asserted against in
integer sample units.

**End to end, in this container.** VP9 has no leading pictures, so the
*discard* cannot be reproduced here; what the browser run proves is the
serving contract. A 48 s VP9+Opus session cut by the segmenter, served
statically, played by the repo's own `hls.min.js` in headless Chromium with
`armHitchDetector` extracted from `index.html`: 40 s played, ~965 frames,
`back=held=drop=gap=0`, no hls.js fatal, zero `droppedVideoFrames`, realized
rate 0.999. The same run against ffmpeg's own hlsenc output scores identically
— which is the point: on a codec with nothing to discard the two are
equivalent, so the segmenter has not introduced anything of its own.

One incidental finding from that comparison. On the same source our playlist's
`EXTINF` sums to 47.999000 s against the container's own 47.999000 s, while
hlsenc's sums to 47.904 s — 95 ms, or 2.3 frames, short. `EXTINF` here is
summed video sample durations in the video timescale, so it is exact by
construction rather than by luck.

**What it was worth on the reference file, measured (2026-07-30).** The first
census of *Wicked* said 32% of cuts clean and looked like a source problem. It
was not. Clean points on that file come every 2.3 seconds (median, 189 IDRs in
a 600 s sample) — but at 58 Mb/s the 48 MB ceiling arrived 6.6 seconds in, so
past the six-second floor the segmenter had **0.6 seconds**, less than one GOP,
to find one. The ceiling was starving the search. `scripts/gop-census --sweep`
replays the policy over a floor × ceiling grid and reports dirty cuts per
minute — the rate of the artifact, since a clean cut costs nothing — against
what ffmpeg's own muxer would do at the same floor:

| policy | dropped frames / min on *Wicked* |
|---|---|
| ffmpeg's HLS muxer, 6 s | 8.0 |
| segmenter, floor 6 s · ceiling 48 MB (as first shipped) | 5.9 |
| segmenter, floor 6 s · ceiling **64 MB** (shipped now) | **1.9** |
| segmenter, floor 4 s · ceiling 64 MB | 1.7 |
| segmenter, floor 2 s · ceiling 80 MB | 0.6 |

The ceiling dominates the floor across the whole grid, which is the opposite
of the intuition the six-second floor came from — under the old regime every
boundary cost a frame, so the goal was fewer of them; under this one most
boundaries are free, so the goal is a wide enough window to find them in.

**How far the ceiling can go is a SourceBuffer measurement, not a budget.** A
real hls.js in headless Chromium, fed a 59.6 Mb/s stream cut at each size with
the shipped `bufferTargets()` numbers (13 s forward / 4 s back at that
bitrate): zero quota events at 48 MB and 64 MB, `bufferFullError` at 80 MB and
at 96 MB, repeatable across runs. Peak buffered bytes were **identical**
(155 MB, 20.9 s) in all four arms — so what fails at 80 MB is not the total,
it is the size of a single append, which no client-side byte budget can be
traded against. 64 MB is the last rung that measured clean.

> **Correction, 2026-07-30 — the second sentence of that paragraph is wrong,
> and it is the sentence the ceiling was chosen on.** A single append *is*
> traded against the client's byte budget, because during an `appendBuffer`
> the browser holds the resident buffer **and** the segment arriving. The
> right invariant is `forward + one segment + back <= quota` (PERF-PLAN
> §4.3quater), and the shipped budget did not have the segment term in it at
> all: 13 s + 4 s at 61 Mb/s is 128 MB resident, plus a 64 MB segment is
> ~190 MB against a ~150 MB quota.
>
> The experiment above did not catch it because **a headless Chromium in a
> container is not the client**. Its quota is a function of the device memory
> class, and it let all four arms park at 155 MB — above what a real desktop
> Chrome allowed on the same bitrate. Reported from the sofa on *Tron*, Chrome,
> the same day: 6 quota refusals in two and a half minutes and then a fatal
> append failure that froze playback. The lesson is the one this document keeps
> re-learning: a measurement taken on the machine that is not the one
> complaining answers a different question. The ceiling itself stands — 64 MB
> is still the right cut size — but the client's budget now reserves room for
> it instead of assuming appends are free.

**What it is worth on any other title is still a property of that source.**
Clean-cut density is decided by whoever encoded the disc, and no synthetic
fixture can answer it — a file full of x265 test patterns has whatever GOP the
test asked for. `scripts/gop-census <file>` runs the production pipe command
against a real library file and reports fragments, clean/dirty counts, the gaps
between clean points, and what percentage of cuts would be clean at the shipped
floor and ceiling. On the repeat-headers open-GOP fixture built to match a
remux it reports 18 fragments, 1 IDR, 17 dirty: a source that gains nothing,
correctly, and says so. **Run it against the real library before believing any
number in this section applies there.**

**The residual, restated honestly.** §5.3ter's one discard per ~6 s is now one
discard per *ceiling cut*, and a ceiling cut happens only where a stretch of
film offers no clean keyframe within 48 MB or 15 s. Where clean points exist,
the drop is gone. Where they do not, this is exactly the old behaviour with a
counter attached. There is no case where it is worse: a misclassification costs
a boundary that could have been cleaner, never a frame that would have
survived, and a stream this reader cannot follow falls back to ffmpeg's muxer
once, automatically, before anything is published.

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

### 7.6bis The evening pass (fresh eyes on this document)

Two commits after the doc was first written, from re-reading it cold.

**The §5.0 strip.** `hevc_copy_bsf` on both copy paths, with the fixture
evidence above. The commit message carries the census numbers and the
framemd5 proof.

**The `late` counter and per-hitch decode forensics (§5.3).** The detector
could not see a compositor hold — no new frame, no callback — which is
probably why 16 `held` events undercounted a stutter Paul describes as
constant. Every fault's `last` line now carries the decode cost at the
hitch against the session's typical, so the next screenshot discriminates
the decoder theory from the delivery theories on its own.

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
3. **Do not route 4K copies to a transcode blindly.** The measured,
   per-client, reversible rescue is the acceptable form: it keys on zero
   slack plus visible hitches and never overrides an explicit Original. A
   global "4K → transcode" rule would burn GPU for every client whose
   pipeline holds a reserve (the TVs do) and throw away the quality the
   library exists for.
4. **Do not trust a healthy stats panel.** Every aggregate on it read clean
   through the entire investigation.
5. **Do not treat a viewer's interval estimate as a measurement.** §2.1.
6. **Do not use `ffprobe -show_frames` to reason about decode order.** It
   outputs in presentation order and will tell you a reordered stream is not
   reordered. §6.1.
7. **Do not add absolute thresholds to per-frame browser metrics.** §7.5.
   Pipeline depth, decoder generation and buffer depth all move them.
8. **Do not read `processingDuration` as decoder capability.** A median
   pinned to the frame budget is a cadence-clocked pipeline delivering
   just-in-time; the number measures slack, and capability claims need
   context it does not carry. §5.3 shipped that misread for a few hours
   before an M3 Max corrected it.
9. **Do not let an empty instrument read as a clean bill.** Safari
   stuttered with a blank Hitches row because rVFC never fired. Every
   detector needs a way to say "I observed nothing", and the overlay now
   has one.

---

## 9. Open questions for a fresh pair of eyes

1. **Answered (2026-07-29 evening): yes.** hls.js 1.6.16's TS demuxer
   handles HEVC — `case 36` sets `segmentVideoCodec="hevc"` and an `hvc1()`
   box builder consumes the parsed VPS. See §5.1 for the constraints the
   test still carries (per-session container choice; Apple requires fMP4).
2. Is the `prefer_segmented` routing still needed at all, now that
   progressive reads ahead 10.8 s instead of 2.2 s? Nobody re-measured
   progressive at 69 Mb/s after the `Content-Length` + `Accept-Ranges` fix.
   If it holds, the whole segmented copy path — and this bug with it — can be
   deleted rather than debugged.
3. Should `-tag:v hvc1` be `dvh1`/`dvhe` for Dolby Vision, and does Chrome
   care? Every DV file in the library currently ships as plain HEVC — and
   since §5.0 it ships without RPUs at all, which makes `hvc1` the honest
   tag rather than a mislabel.
4. Is the held frame *lost* or *repeated*? Partially addressed: the `late`
   counter (§5.3) now sees the compositor holding a frame, which the
   original detector could not, and every event carries the decode cost at
   the hitch. `VideoPlaybackQuality.totalVideoFrames` against the source
   frame count over a known interval would still settle the lost-vs-repeated
   half exactly.
5. Does it reproduce in Firefox or Safari? Only Chrome has been tested, on
   one browser across four servers. A second engine would separate "Chrome's
   MSE" from "the stream".
