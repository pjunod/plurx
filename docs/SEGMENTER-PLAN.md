# GOP-aware segmenter — zero boundary drops on the copy path

**Status:** BUILT 2026-07-30, M0–M6 · CI gate green (`make check`: fmt +
clippy + 247 core / 188 daemon tests) · **awaiting morning validation on
real hardware by Paul** · **Executes:** the residual in
[STUTTER-4K.md](STUTTER-4K.md) §5.3ter — one discarded leading picture per
segment start · **Written:** 2026-07-30, against `e212c55` (v0.2.0-2) —
**re-verify every cited line at build time; the file is the truth, this doc
is the map**

## 0. What shipped, and what was flagged

Four commits on top of `b3bc94f`:

| Commit | Milestone | What |
|---|---|---|
| `3fde76d` | M0 + M1 | `plurx_core::fmp4` reader + classifier + `CutPolicy`; `scripts/gop-census`; the two new constants |
| `53d8813` | M2 | `fmp4::merge` and `fmp4::Segmenter`, with the framemd5 proof |
| `6913c20` | M3 | `plurxd::copyseg`, `copy_pipe_args`, manager integration + fallback ladder |
| `323569b` | M5 | perf-report, STUTTER-4K §5.6, PLAYBACK, FEATURES, CHANGELOG |

**First thing to run in the morning**, because it is the number this whole
design's value depends on and no fixture in this container can stand in for
it:

```bash
scripts/gop-census "/path/to/Wicked (2024) Remux-2160p.mkv"     # and a few others
```

It prints what percentage of cuts the shipped floor and ceiling would take
cleanly on that source. High → the boundary drop is gone on that title. Low →
the segmenter costs nothing and gains nothing there, and `docs/STUTTER-4K.md`
§5.6 says so rather than claiming otherwise. Then play the reference file in
Chrome and in Safari, forced Original, and read the Hitches row: `drop` should
fall by roughly the clean-cut percentage.

### 0.1 Deviations from this plan, each with its reason

1. **The floor now gates the ceilings — the one substantive change to §3's
   rule, and it is a correctness fix, not a preference.** As written, "cut
   when `bytes ≥ BYTE_CEILING` at any keyframe fragment" fires independently
   of the floor. `COPY_SEGMENT_MAX_BYTES` is 48 MB and the reference file
   runs at 69 Mb/s, which reaches 48 MB in **5.6 seconds** — so on precisely
   the file this plan exists for, the segmenter would have cut *below* the
   six-second floor and produced ~8% MORE boundaries than the muxer it
   replaces. Every boundary costs a frame, so that is strictly worse than
   doing nothing, and §3's own promise ("misclassification degrades to today's
   behavior, never worse") forbids it. The rule is now: nothing cuts below the
   floor, ever; past the floor a clean fragment wins; past the floor with no
   clean fragment, either ceiling cuts. §7.3 puts the floor and ceiling
   constants in code rather than in settings, so this is a code decision, but
   it is flagged here because it changes a rule §3 states explicitly.
   Asserted by `no_ceiling_may_cut_below_the_floor`, mirrored in
   `scripts/gop-census`'s simulator, and drawn in STUTTER-4K §5.6.
2. **M0 and M1 landed in one commit.** They are one file and one deliverable —
   the classifier is unusable without the reader, and `gop-census` is the
   classifier's field instrument. Every other milestone is its own commit.
3. **The merger keeps one `trun` per source run, not one merged `trun`
   (§3).** The plan offered both and said to pick whichever survives M2's
   tests; this is the one that does. ffmpeg writes each track's samples
   contiguously *within* a fragment but interleaves the tracks *between*
   fragments, so a single `trun` — which carries one data offset — would force
   the sample data to be de-interleaved and rewritten sample by sample. Per
   source run, each `mdat` payload is memcpy'd whole and every offset is
   arithmetic. The sample values are still fully materialized, so §3's other
   reason for the single trun (erasing the default/first-sample-flag edge
   cases) is delivered anyway.
4. **§8.6's bundle command names `v0.2.0`, which does not exist as a tag in
   the working repo** (`git tag -l` is empty there — the tag lives on the
   remote). The bundle was built as `946486b..main HEAD`, which is what makes
   a bare `git pull <bundle>` work.
5. **M4 drives `fmp4::Segmenter` directly rather than `copyseg::run`.** The
   only codecs headless Chromium decodes are VP9 and Opus, and `copyseg::run`
   correctly refuses a video track whose keyframes it cannot read — that
   refusal *is* the capability check the fallback ladder depends on, so
   loosening it to make the test easier would have deleted the thing under
   test. The harness reproduces copyseg's writing loop over the same shipped
   playlist helpers; copyseg's own file semantics (tmp+rename, no ENDLIST on
   a killed session) are covered by unit tests that drive the real `run()`
   from a byte slice. The M2 equality checks were re-run against that
   session's actual published output, as §5 M4 asks.
6. **The M4 harness lives in `/tmp/segtest-e2e/` and is not committed**, per
   §5 M4's instruction to write it fresh. Its result is recorded in
   STUTTER-4K §5.6.
7. **The M0 timing test compares pts, dts and size against ffprobe, not
   `packet=duration`.** ffprobe derives that field from the stream's frame
   rate, not from the sample table: on 24 fps at timescale 16000 it reads a
   flat 666 against real durations that cycle 672/672/656 to sum exactly.
   Per-packet dts *is* read from the container and every dts here is generated
   by summing our resolved durations, so dts agreement is the duration check
   from the other end; the stream's `duration_ts` closes the last sample.
8. **`-progress` moved to `pipe:2` for the pipe path**, since stdout now
   carries media. The stderr drain sorts progress blocks from log lines by
   key, so `speed`, `out_time` and the stall watchdog keep working — §4.4 said
   to keep the stderr wiring, and this is what keeping it costs.
9. **Fixtures are 12 s of 640x360, not §8.1's 30 s of 1280x720**, and live in
   `target/fixtures/` shared by both crates (`plurx_core::testfixtures`,
   behind a dev-only feature). CI pays for every x265 encode on every run, and
   no property under test cares how big the picture is. The four GOP shapes
   §8.1 asks for are all there, plus VP9+Opus for M4.

Nothing in §7 was violated: `SEGMENT_SECONDS` is untouched, hlsenc is still
the transcode muxer, there is no settings knob, no hls.js or web-app change,
no change to rescue thresholds or `prefer_segmented`,
`#EXT-X-INDEPENDENT-SEGMENTS` stays withdrawn, and `Cargo.lock` did not
change. `SEGMENT_SECONDS`, `Recipe` and `cachekeep` appear nowhere in the
diff, and the only `produce::` reference is the test that feeds our playlist
to `produce::Part::from_playlist` — which §4.3 explicitly allows, and which is
the cheapest possible proof that the playlist this writes is one the rest of
the daemon already reads.

This is a handoff plan for an implementing agent. Read
[STUTTER-4K.md](STUTTER-4K.md) first — §5.0–§5.3ter is the evidence chain
this design rests on. Work milestone by milestone, **in order**; each ends
with an acceptance check that is a runnable command. Run the full gate
(`make check`) before every commit, one commit per milestone, messages in
the repo's style (the claim, the why, the proof). If a step seems to
require changing something §7 forbids, **stop and flag it** in the status
header of this file rather than improvising. Two daemon tests
(`a_preempted_producer_resumes_without_losing_picture`,
`an_unfinished_run_keeps_its_place_but_is_never_serveable`) are known
load-flaky — they race a real ffmpeg encode; a failure there alone means
re-run before diagnosing.

At the end: build the delivery bundle exactly as
`git bundle create /tmp/plurx-segmenter.bundle 946486b..main HEAD v0.2.0`,
verify it with the clone-reset-pull procedure in §8.6, and leave a short
summary of what shipped and what (if anything) was flagged at the top of
this file.

---

## 1. Objective, and what the morning needs to contain

Copy-video HLS sessions currently cut segments wherever ffmpeg's `hlsenc`
finds a keyframe past the 6 s floor. On open-GOP sources — every disc remux
— most keyframes are CRAs carrying one RASL leading picture, and both
Chrome (MSE) and Safari (native HLS) treat a segment's first keyframe as a
random-access point and **discard the leading picture**: exactly one
dropped frame per segment boundary, measured at 11/12 boundary-attributed
in Safari and 15/23 in Chrome, and **proven** segment-triggered by the
one-stream experiment (1781 frames, zero drops, same bitstream).

The fix: plurx takes over the cutting. ffmpeg produces one continuous
fragmented stream (one fragment per GOP); plurx classifies each fragment's
opening keyframe as clean (IDR, or CRA with no leading pictures) or dirty
(CRA with leading pictures), merges consecutive fragments, and publishes a
segment boundary **only in front of a clean fragment** — past the duration
floor, under a byte ceiling. Every published segment is a single fragment
whose first frame is a true random-access point, so there is nothing for
any player to discard. Where a stretch of film offers no clean point
before the byte ceiling, the cut is taken dirty and **counted** — honesty
over silence.

Deliverables Paul should find in the morning:

1. The bundle, pullable with a bare `git pull ~/Downloads/<name>.bundle`
   (build it with the HEAD ref as in §8.6 — that is what makes a bare pull
   work).
2. `scripts/gop-census` — a standalone analyzer he can run against his real
   library files to measure clean-cut-point density (the go/no-go number
   this container cannot measure, §5 M1).
3. The segmenter live behind an automatic capability check with a legacy
   fallback, so the worst case of any surprise is today's behavior.
4. `docs/STUTTER-4K.md` §5.6 and `CHANGELOG.md` updated in the same commits
   as the behavior they describe.
5. This file's status header updated with what shipped and any flags.

---

## 2. Evidence this design rests on (do not re-derive)

All measured 2026-07-29/30, on real hardware or this container's fixtures;
see [STUTTER-4K.md](STUTTER-4K.md) §3, §5 and its §6 harnesses.

- Continuous decode keeps leading pictures: the same open-GOP 4K stream
  that drops one frame per segment when fragmented played **1781 frames
  with zero drops** as one continuous fMP4 (which itself contains a moof
  per GOP — through the progressive `src=` demuxer those interior moofs
  cost nothing).
- Discards happen at **segment** starts in both transports: Chrome MSE
  15/23 within 150 ms of a boundary; Safari native 11/12.
- Drop count scales with boundary count: floor 2 s → 6 s moved the cadence
  1.8 s → 6.1 s, exactly proportional.
- The copied bitstream is already clean (in-band parameter sets stripped,
  DV RPU/EL and `dvcC` signaling stripped, `hvc1` honest, frame-grid
  timestamps, zero join overlap). The remaining fault is GOP **structure**,
  which `-c copy` cannot change — only cut placement can.
- A 69 Mb/s segment must stay well inside the browser's ~150 MB SourceBuffer
  quota and the byte-budgeted forward buffer (`bufferTargets`, 96 MB
  forward): segment bytes are the binding ceiling, not seconds.

---

## 3. Design

```
 ffmpeg (one process per session, today's copy args MINUS hlsenc):
   -ss <resume> -readrate … -i file.mkv
   -map 0:v:0 -map 0:a:0? -sn -c:v copy -tag:v hvc1
   -bsf:v <hevc_copy_bsf(...)>          ← §4.3, reuse verbatim
   -c:a aac|copy … -avoid_negative_ts make_zero
   -movflags frag_keyframe+empty_moov+default_base_moof+delay_moov
   -f mp4 pipe:1
        │  ftyp moov (moof mdat)×N mfra
        ▼
 plurxd reader task (new: crates/plurxd/src/copyseg.rs)
   ├─ buffer until moov complete (delay_moov: it arrives after startup)
   ├─ write init.mp4 = ftyp+moov verbatim (byte-identical to hlsenc's)
   ├─ per fragment (moof+mdat): classify first video sample
   │      IDR (NAL 19/20)                      → CLEAN
   │      CRA (NAL 21), no earlier-pts sample  → CLEAN
   │      CRA with leading picture(s)          → DIRTY
   │      h264: IDR (NAL 5) → CLEAN, else DIRTY
   │      unparseable → DIRTY (never cut here) + count
   ├─ accumulate fragments; publish a segment when
   │      duration ≥ FLOOR  AND next fragment CLEAN     (clean cut)
   │   or bytes ≥ BYTE_CEILING at any keyframe fragment  (dirty cut, count)
   ├─ publish = merge accumulated fragments → ONE moof+mdat,
   │      write styp+moof+mdat to segNNNNN.m4s.tmp, rename, append EXTINF
   │      to index.m3u8 (rewrite playlist atomically: tmp+rename)
   └─ EOF → drop mfra, final segment, append #EXT-X-ENDLIST
```

**The merger is the core.** Merging fragments A,B,…: one new `moof` with a
`traf` per track. Do not try to preserve ffmpeg's compact encoding —
**normalize**: `tfhd` carries only `default-base-is-moof` (0x020000) +
track id; `tfdt` v1 = A's base decode time per track; a single `trun` per
track, version 0, flags `0x000701` (data-offset + duration + size + flags
present) **plus** `0x000800` (composition offsets) for video, every value
written per sample. Materialize per-sample values by resolving each source
fragment's own defaults (§4.4: ffmpeg uses tfhd defaults + first-sample
flags, per-sample cto on video only). Sample data: mdat payloads
concatenated in fragment order, per-track interleaving preserved exactly as
the source trun data offsets describe — the simplest correct approach is to
copy each source fragment's mdat payload whole and set each track's trun
data offsets to (new moof size + preceding payload bytes + source offset
within its fragment's payload). Multiple `trun` boxes per traf are legal
and an acceptable alternative to one merged trun (keep source truns,
re-base their data offsets); choose whichever survives M2's equality tests,
but normalized-single-trun is the recommendation because it also erases
the default/first-sample-flag edge cases.

**Fallback ladder.** (1) At session start: if the pipe's `moov` can't be
parsed, or the video track has no `hvcC`/`avcC`, or the first fragment
can't be classified → kill the pipe, log one warning with the reason,
respawn via today's `hls_copy_args` path. One respawn, ever. (2)
Mid-session: an unparseable fragment is DIRTY (cut never lands in front of
it); publishing continues. (3) The byte ceiling guarantees forward
progress when no clean point appears. There is no mode in which playback
depends on the classifier being right — misclassification degrades to
today's behavior, never to a broken stream.

**Constants** (new, in `plurx-core`, beside `COPY_SEGMENT_SECONDS`):

| Constant | Value | Why |
|---|---|---|
| `COPY_SEGMENT_SECONDS` | 6 (exists) | floor before a clean cut is taken |
| `COPY_SEGMENT_MAX_BYTES` | 48 MB | hard ceiling; ~½ the 96 MB forward budget so hls.js holds ≥2 segments |
| `COPY_SEGMENT_MAX_SECS` | 15 | secondary ceiling for low-bitrate sources so native players' TARGETDURATION stays sane |

`EXT-X-TARGETDURATION` = ceil(max published EXTINF) and must never
decrease mid-session: write it as `ceil(COPY_SEGMENT_MAX_SECS)` from the
start (§4.5's template) rather than growing it.

---

## 4. Contract — verified facts (re-verify at build time)

### 4.1 Where the code lives today

- Copy args builder: `hls_copy_args(...)` in
  `crates/plurx-core/src/transcode/mod.rs` (~line 640). Its audio/pacing/
  bsf logic is the spec for the pipe args builder — factor, don't fork:
  extract the shared input/map/codec/bsf section into a helper both use.
- Daemon spawn site: `crates/plurxd/src/transcode.rs` ~2398
  (`let args = transcode::hls_copy_args(` — the `have_dovi` block sits just
  above). The session dir is `self.work_dir.join(&session_id)`; ffmpeg
  currently writes `seg%05d.m4s`, `init.mp4`, `index.m3u8` there and
  plurxd serves them as files. **Keep those exact filenames** and the
  serving layer never changes.
- Suspend: the manager pauses sessions with `libc::SIGSTOP`/`SIGCONT`
  (~line 2682). A pipe-fed ffmpeg suspends the same way — and additionally
  blocks on the pipe when the reader stops reading, which is free
  backpressure. Keep SIGSTOP behavior untouched; the reader task must
  simply tolerate long gaps between reads.
- Session accounting reads the **playlist**, not the filesystem:
  `SegmentMeta` (~line 270) accumulates real `EXTINF`s — comment there
  explicitly says never index × constant. Our playlist is the interface;
  match the format and everything downstream (ahead-window suspend, GC,
  status frontier, perf-report's segment section) keeps working.

### 4.2 Byte-level facts, measured on this container's fixtures

Continuous pipe output (`-movflags frag_keyframe+empty_moov+
default_base_moof+delay_moov -f mp4 pipe:`):

```
ftyp(28) moov(3700) [moof mdat]×18 mfra(756)     ← 18 = one per keyframe
```

- The pipe's `ftyp`+`moov` were **byte-identical in size** to hlsenc's
  `init.mp4` (28+3700) on the same input: init.mp4 = first two boxes,
  verbatim. Strip `mfra` (it arrives at EOF; never publish it).
- hlsenc's segment layout is `styp(24) sidx sidx moof mdat`. **Emit
  `styp` (copy hlsenc's 24 bytes: `styp` major `msdh`, compatible
  `msdh msix`) and OMIT the two `sidx`** — sidx is optional in HLS fMP4,
  hls.js's passthrough ignores it, and synthesizing it is risk for zero
  measured benefit. This is the one deliberate byte-level divergence from
  hlsenc; it is on the M5 morning-validation checklist, and if Safari
  balks the follow-up is a per-track single-reference sidx.
- ffmpeg's fragment encoding (what the merger must resolve):
  `tfhd` flags `0x020038` (default-base-is-moof + default duration/size/
  flags), `tfdt` **v1** (64-bit), video `trun` v0 flags `0x000a05`
  (data-offset + first-sample-flags + size + composition offsets,
  durations from tfhd default), audio `trun` v0 flags `0x000201`
  (data-offset + size). Composition offsets are v0 **unsigned** — ffmpeg
  shifts dts so pts−dts ≥ 0; tfdt base ≠ first pts on video. The merged
  output normalizes all of this (§3).
- NAL framing inside samples is length-prefixed; the length size is
  `(hvcC byte at offset 21 & 3)+1` (HEVC) / `(avcC byte 4 & 3)+1`
  (H.264), hvcC/avcC found inside `moov→trak(vide)→…→stsd`. HEVC NAL type
  = `(first byte >> 1) & 0x3F`: 19/20 IDR, 21 CRA, 32–34 VPS/SPS/PPS
  (already stripped upstream — a fragment whose first NAL is not VCL is a
  classification error; treat DIRTY and count). H.264 NAL type =
  `first byte & 0x1F`: 5 = IDR.
- Leading-picture test for a CRA-opening fragment: with per-sample pts =
  `tfdt + Σ durations(0..i-1) + cto(i)`, DIRTY iff any sample i>0 in the
  fragment has pts < pts(0). (On the fixture: every CRA carries exactly
  one RASL; the reference implementation is §8.3.)

### 4.3 Reuse, verbatim

- `hevc_copy_bsf(hdr, have_dovi_bsf)` and `ffmpeg_has_dovi_bsf(...)` —
  the bitstream hygiene is settled; the pipe builder takes the same
  filters.
- `Pacing` / `pacing.push(&mut args)` — same burst-then-hold pacing.
- `-avoid_negative_ts make_zero` — from the progressive builder
  (`crates/plurxd/src/http/stream.rs` ~1040), with its comment's reasons.
- The playlist **template** is §4.5; `parse_playlist` in
  `scripts/perf-report` and `Part::from_playlist` in
  `crates/plurxd/src/produce.rs` show exactly what downstream parsers
  tolerate.

### 4.4 What the daemon expects of a session (mirror, don't modify)

- Files appear via **tmp + rename** (`temp_file` semantics) — a reader
  must never see a partial segment or playlist. Rewrite `index.m3u8` by
  writing `index.m3u8.tmp` then renaming over.
- The playlist grows append-only; `#EXT-X-ENDLIST` exactly once, at EOF.
- Segment numbering `seg00000.m4s` from `-start_number 0` — keep
  five-digit zero-padded from 0.
- ffmpeg stderr is already piped and logged by the manager; keep that
  wiring for the pipe process (stdout becomes the data pipe; stderr stays
  the log).
- `kill_on_drop(true)`, supersede, and idle-timeout paths in the manager
  must work unchanged — the reader task must exit promptly when the child
  is killed (read returns 0/err → clean shutdown, finalize nothing).

### 4.5 The playlist, exactly

```
#EXTM3U
#EXT-X-VERSION:7
#EXT-X-TARGETDURATION:15
#EXT-X-MEDIA-SEQUENCE:0
#EXT-X-PLAYLIST-TYPE:EVENT
#EXT-X-MAP:URI="init.mp4"
#EXTINF:6.208333,
seg00000.m4s
…
#EXT-X-ENDLIST
```

Six-decimal EXTINF from summed **video** fragment durations in the video
timescale (never wall clock, never audio). No
`#EXT-X-INDEPENDENT-SEGMENTS` — with clean cuts the tag would actually be
TRUE for clean segments, but dirty ceiling cuts make it a lie again;
leave it off and note that restoring it becomes possible if census data
shows ceiling cuts are rare (§7.6).

---

## 5. Milestones

Every `cargo test` name below is a real test to write, named in the
repo's sentence style. Fixture generation commands are in §8.1 — generate
into `target/fixtures/` or tempdirs from `#[test]` helpers (the producer
tests at `crates/plurxd/src/transcode.rs` show the pattern for shelling to
real ffmpeg inside tests; skip gracefully when ffmpeg lacks libx265, but
this container's ffmpeg has it).

### M0 — fMP4 reader: boxes, fragments, tracks

New module `crates/plurx-core/src/fmp4.rs` (pure, no I/O): top-level box
walker tolerant of 64-bit sizes, `Init` (track id/handler/timescale per
track, hvcC/avcC + NAL length size, byte range of ftyp+moov),
`Fragment` (moof+mdat byte ranges, per-track: tfdt, resolved per-sample
duration/size/flags/cto, data offsets), streaming split of a byte feed
into init + fragments + trailing mfra. Resolve tfhd defaults and
first-sample-flags into per-sample values at parse time.

Acceptance:
```bash
cargo test -p plurx-core fmp4::   # includes, at minimum:
# parses_the_pipes_init_and_every_fragment (18 fragments on the fixture)
# resolved_samples_reproduce_ffprobe_timing (spot-check pts/dur vs ffprobe csv)
# a_truncated_feed_yields_no_fragment_and_no_panic
```

### M1 — classifier + the census tool Paul runs at breakfast

`fmp4::classify(fragment, init) -> CutClass {CleanIdr, CleanCra, Dirty,
Unparseable}` per §4.2's rules. Then `scripts/gop-census` (Python,
standalone, stdlib-only like `scripts/perf-report`): takes a media file
path, runs the production pipe command via the local ffmpeg, classifies
every fragment, prints: total fragments, clean/dirty/unparseable counts,
histogram of gaps between clean points, and the verdict line "at FLOOR=6s
/ CEILING=48MB, N% of cuts would be clean". This is the go/no-go
instrument for real libraries — the container's x265 fixtures cannot
answer how many clean points a real Blu-ray remux has.

Acceptance:
```bash
cargo test -p plurx-core fmp4::classif
# every_cra_on_the_open_gop_fixture_is_dirty (x265 open-gop: all CRAs carry RASL)
# every_keyframe_on_the_closed_gop_fixture_is_clean (repeat-headers=0? no — x265 open-gop=0)
# h264_only_idr_is_clean
scripts/gop-census /tmp/fixtures/og.mkv     # runs, prints counts, exits 0
```

### M2 — the merger, proven harmless

`fmp4::merge(&[Fragment], &Init) -> Vec<u8>` producing
`styp+moof+mdat` per §3's normalization. The proof obligations, all as
tests that shell to ffmpeg/ffprobe:

Acceptance:
```bash
cargo test -p plurx-core fmp4::merge
# merged_stream_decodes_bit_identical: framemd5 of init+merged segments ==
#   framemd5 of the original continuous pipe file (both via ffmpeg -f framemd5)
# every_sample_keeps_its_pts_dur_size: re-parse own output, compare to inputs
# joins_are_gapless_and_overlap_free: port of joins.py — consecutive
#   published segments: next.start == prev.end exactly, both tracks
# audio_and_video_survive_interleaved: two-track fixture, ffprobe stream
#   counts and durations match the source
```

### M3 — the session: copyseg.rs wired into the manager

`crates/plurxd/src/copyseg.rs`: the reader task per §3's diagram; a
`copy_pipe_args(...)` builder in plurx-core (shared helper with
`hls_copy_args`, §4.1); manager integration at the §4.1 spawn site:
attempt the segmenter for `hevc|h265|h264` sources, fall back per §3's
ladder. Log one info line per session: mode (segmenter/legacy), and at
end: segments published, clean cuts, dirty (ceiling) cuts, unparseable
fragments. That line is what perf-report will grep.

Acceptance:
```bash
cargo test -p plurxd copyseg
# a_session_on_an_open_gop_source_publishes_only_clean_cuts (fixture has
#   clean points: build one with x265 scenecut enabled so IDR/clean CRAs exist)
# a_source_with_no_clean_points_cuts_at_the_ceiling_and_says_so
# the_playlist_matches_the_template_and_produce_can_parse_it
#   (feed it to produce::Part::from_playlist)
# a_killed_session_leaves_no_tmp_files_and_no_endlist
# an_unparseable_moov_falls_back_to_the_legacy_path (corrupt the pipe head)
make check
```

### M4 — e2e in this container, through a real browser

VP9 has no leading pictures, so the *discard* can't reproduce here — this
milestone proves the **serving contract**: build a VP9+Opus fixture, run a
real session through the manager (or serve the produced dir statically),
play it with the repo's own `hls.min.js` in headless Chromium
(`/opt/pw-browsers/chromium`, Playwright preinstalled — see STUTTER-4K §6
for the harness shape; write `/tmp/segtest-e2e/test.js` fresh, extracting
`armHitchDetector` from `index.html` exactly as the §6 harnesses do).

Acceptance: 40 s of playback, `back=held=drop=gap=0`, no hls.js fatal, and
the detector's `at`/`near` maps empty. Also run the M2 equality checks
against the session's actual published output, not just the library
function.

### M5 — reporting, docs, changelog

- perf-report: in the copy-session block, grep the M3 end-of-session line
  and print `segments N · clean cuts X · ceiling cuts Y`; a Y > 0 note
  explains what a ceiling cut is in one sentence.
- `docs/STUTTER-4K.md`: add §5.6 (the segmenter: what shipped, the M2
  proof, what a ceiling cut is, and that §5.3ter's residual is now
  bounded by clean-point density — with `scripts/gop-census` named as the
  measuring tool). Update the status header.
- `CHANGELOG.md` Unreleased: one operator-facing entry.
- `docs/PLAYBACK.md`: two-sentence note in the copy-video section.
- Do **not** cut a release; Paul cuts releases.

Acceptance: `make check`; the doc lint rules from the house style (one H1,
no H4+, blank line after headings, ~80 col prose).

### M6 — the bundle

§8.6 procedure, verified by clone-reset-pull. Update this file's status
header. Stop.

---

## 6. Traps, from tonight's scars

1. `delay_moov` means the pipe starts with `ftyp` then **silence** until
   the first packets commit; the reader must buffer patiently and must not
   write `init.mp4` until `moov` is complete. Started-in time budget: the
   burst makes the first 6 s segment near-instant *after* moov; do not add
   waits.
2. The first fragment's classification is irrelevant to cutting (segment 0
   starts wherever the stream starts — a resume lands on whatever `-ss`
   chose) but its parse SUCCESS is the capability check.
3. EXTINF from the **video** track timescale; audio fragment durations
   differ by up to one AAC frame (21.3 ms) and using them drifts the
   playlist against the media (§3.7 of STUTTER-4K proved audio is
   continuous — leave it alone).
4. tfdt across a dirty cut: nothing special — decode time is continuous
   because the stream is; the merger never rewrites tfdt except to copy
   the first source fragment's.
5. Unsigned v0 composition offsets: the dts-shift means video pts(0) ≠
   tfdt. The classifier compares **pts**, not cto, and never assumes
   pts(0) is the minimum (on a dirty fragment it isn't — that's the
   point).
6. `PLAYER`-side nothing changes: the web app, `bufferTargets`, the hitch
   detector, the native boundary marks (they parse the playlist — which is
   why the playlist template must hold) all stay untouched.
7. SIGSTOP'd ffmpeg + full pipe: the reader must use bounded reads and
   survive minutes of nothing; never buffer more than ~2 segments of
   bytes in memory (spill: since the ceiling is 48 MB, cap reader memory
   at ~128 MB and treat exceeding it as a bug to log, cut, and continue).
8. Segment 0 of a *resumed* session starts at the seek keyframe, which
   `-ss` picks — it may be DIRTY (a CRA whose RASL reference the seek
   discarded). ffmpeg drops those RASL packets itself on input seek;
   whatever arrives in fragment 0 is what there is. Publish it as-is.
9. The producer/cache and Phase-3 failover contracts are transcode-only.
   The words `SEGMENT_SECONDS`, `Recipe`, `produce`, `cachekeep` should
   not appear in the diff (reading `produce::Part::from_playlist` in a
   test is fine).
10. `Cargo.lock` will not change; if it does, something added a
    dependency — this plan needs none beyond what the workspace has
    (`libc` is already there; box parsing is `std` byte work).

---

## 7. Non-goals and guardrails

1. **No re-encoding, ever.** The merger moves bytes; framemd5 equality is
   the license to ship.
2. **No transcode-path changes** — `SEGMENT_SECONDS` stays 2, hlsenc stays
   the transcode muxer, `independent_segments` stays on that path.
3. **No settings/UI knobs.** The segmenter is automatic-with-fallback; a
   knob for it is scope creep. (The floor/ceiling constants are code.)
4. **No hls.js or web-app changes.** If M4 seems to need one, the server
   output is wrong — fix the output.
5. **No changes to rescue thresholds, routing (`prefer_segmented`), or the
   Safari native path** beyond serving the same files it gets today.
6. Do not restore `#EXT-X-INDEPENDENT-SEGMENTS` — revisit only with real
   census data showing ceiling cuts ≈ 0 (§4.5).
7. Do not chase the two load-flaky producer tests; re-run them.
8. If the manager integration (M3) turns out to require restructuring
   session lifecycle code shared with transcode sessions, **stop at M2 +
   M4-as-static-serving + census tool**, flag it here, and leave M3 for a
   session with Paul awake — a half-integrated session manager is worse
   than a missing feature.

---

## 8. Appendix — fixtures, reference algorithms, procedures

### 8.1 Fixtures (generate; do not commit binaries)

Open-GOP HEVC + TrueHD, the disc-remux shape (every CRA carries one RASL;
repeat-headers exercises the strip):

```bash
ffmpeg -y -f lavfi -i "testsrc2=size=1280x720:rate=24:duration=30" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=30" \
  -c:v libx265 -preset veryfast \
  -x265-params "keyint=42:min-keyint=42:open-gop=1:bframes=4:b-pyramid=2:scenecut=0:repeat-headers=1:log-level=none" \
  -pix_fmt yuv420p -strict -2 -c:a truehd -ac 2 -shortest og.mkv
```

A source **with** clean points (scenecut IDRs among open-GOP CRAs) for
M3's clean-cut test — x265 `scenecut=40` + a source with real cuts:

```bash
ffmpeg -y -f lavfi -i "testsrc2=size=1280x720:rate=24:duration=30" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=30" \
  -vf "select=1,setpts=N/24/TB" \
  -c:v libx265 -preset veryfast \
  -x265-params "keyint=48:min-keyint=12:open-gop=1:bframes=4:scenecut=40:repeat-headers=1:log-level=none" \
  -pix_fmt yuv420p -strict -2 -c:a truehd -ac 2 -shortest og-cuts.mkv
# testsrc2 has hard scene-like transitions; verify ≥2 IDR/clean-CRA appear:
ffprobe -select_streams v:0 -show_packets -show_entries packet=pts_time,flags -of csv og-cuts.mkv | grep -c K__
# if x265 emits only CRAs, force periodic IDR instead:
#   x265-params ...:no-open-gop=0 + a second fixture with open-gop=0 whose
#   every keyframe is clean — the test needs both classes present across
#   the two fixtures, not necessarily within one.
```

Closed-GOP control (every keyframe clean): same command with
`open-gop=0:bframes=4`. H.264 fixture: `-c:v libx264 -x264-params
"keyint=42:min-keyint=42:open_gop=1:bframes=3"` (x264 open-GOP uses
recovery points; only NAL 5 is CLEAN under our rule). VP9+Opus for M4:
see STUTTER-4K §6.1's mux commands.

The production pipe command for fixtures = §3's diagram args with
`-bsf:v "filter_units=remove_types=32-34"` (this container's ffmpeg 6.1
has no `dovi_rpu`; the gate function handles that — pass
`have_dovi_bsf=false` in fixture generation).

### 8.2 Verifying decode equality (the M2 license)

```bash
# reference: the continuous pipe file itself
ffmpeg -v error -i pipe.mp4 -map 0:v:0 -f framemd5 - > ref.md5
# candidate: init + published segments, concatenated in order
cat sess/init.mp4 sess/seg*.m4s > cand.mp4
ffmpeg -v error -i cand.mp4 -map 0:v:0 -f framemd5 - > cand.md5
diff ref.md5 cand.md5     # empty = bit-identical decode, timing included
# repeat with -map 0:a:0
```

### 8.3 Reference classifier (port to Rust; this exact logic ran tonight)

```python
# first video sample's NAL walk (lsz = NAL length size from hvcC):
q, end = sample_start, sample_start + sample_size
while q + lsz <= end:
    nlen = int.from_bytes(buf[q:q+lsz], 'big')
    ntype = (buf[q+lsz] >> 1) & 0x3F          # HEVC
    if ntype <= 31: break                      # first VCL NAL decides
    q += lsz + nlen                            # skip SEI etc.
# clean: ntype in (19, 20) or (ntype == 21 and not has_leading)
# has_leading: any sample i>0 in fragment with pts[i] < pts[0], where
#   pts[i] = tfdt + sum(dur[0..i-1]) + cto[i]
```

The moof/traf/tfhd/tfdt/trun walking reference is
`fmp4-join-overlap.py` / `census.py` in the investigation handoff tarball;
their algorithms are restated in §4.2 and were validated against ffprobe
on 2026-07-29. Trust the restatement; re-derive nothing.

### 8.4 Golden-structure comparison (M3)

Run hlsenc (today's `hls_copy_args`) and the segmenter on the same
fixture; assert both playlists parse with `produce::Part::from_playlist`,
EXTINF sums agree with ffprobe duration ±1 frame, and box sequences per
segment are `styp moof mdat` (ours) vs `styp sidx sidx moof mdat`
(hlsenc) — the sidx delta is the only allowed difference.

### 8.5 Census output shape (M1)

```
gop-census: og.mkv
  fragments 18 · clean 1 (IDR 1, CRA 0) · dirty 17 · unparseable 0
  gaps between clean points: p50 —  p90 —  max 28.6s (only one clean point)
  at floor 6s / ceiling 48MB: 0% of cuts clean — ceiling cuts every ~5.6s
  verdict: this source gains nothing; a scene-cut source will differ
```

### 8.6 Delivery procedure (verbatim)

```bash
cd /root/plurx && make check
git bundle create /tmp/plurx-segmenter.bundle 946486b..main HEAD v0.2.0
rm -rf /tmp/pulltest && git clone -q /root/plurx /tmp/pulltest
cd /tmp/pulltest && git reset -q --hard 946486b && git config pull.rebase false
git pull -q /tmp/plurx-segmenter.bundle && git log --oneline -3 && git describe --tags
# then SendUserFile the bundle with a one-line caption:
#   git pull ~/Downloads/plurx-segmenter.bundle — <one sentence of what's in it>
```
