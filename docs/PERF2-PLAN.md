# Performance II — better starts, steadier streams, smarter bits

**Status:** revised v2, re-review welcome · **Extends:** [PERF-PLAN.md](PERF-PLAN.md)
(M0–M3 shipped and accepted; M4 waits on Phase 4) and executes
[ADAPTIVE-QUALITY.md](ADAPTIVE-QUALITY.md) Phase 2 · **Written:** 2026-08-09
against `main` @ `e8a910f` · **Revised:** 2026-08-09 v2 — all eight findings
of [PERF2-PLAN-REVIEW.md](PERF2-PLAN-REVIEW.md) accepted; the deltas are
itemized in [PERF2-REVIEW-RESPONSE.md](PERF2-REVIEW-RESPONSE.md).

This is the second performance arc. The first one ([PERF-PLAN.md](PERF-PLAN.md))
took starts from ~10 s to a ~684 ms median and put the 4K tone-map on the GPU
at 4.89× the CPU chain (PERF-PLAN §10). What remains is different in kind:
the seconds left are structural (a publish gate, a cold NAS open, a session
restart), the bits are spent blind (every rung is a fixed bitrate no matter
what the content is), and the streams that still stumble do so silently
(three clients, one of which reports nothing at all). This plan spends its
milestones on those three fronts, and it adds the first deliberately
*intelligent* machinery — content analysis, prediction, client-side
enhancement, and operational assistants — under one hard rule: *every piece
of it is optional, and none of it may make plurx depend on anything outside
the box.*

How to work this plan: milestone by milestone, each section self-contained,
each ending in an acceptance check that is a runnable command or an
observable fact. File:line citations were verified against `e8a910f`;
re-verify at build time — the tree moves. If a step seems to require
changing a contract in §11, or violating a guardrail in §12, stop and flag
it instead of improvising.

---

## 1. Where the seconds and the bits go now

Numbered for citation. Everything here is measured or cited to code; the
milestones refer back to these as B-facts.

### 1.1 Starts — the floor is structural now

Measured on nynuc 2026-07-29 (PERF-PLAN §10): **197 ms direct play,
651–1536 ms remux, median ~684 ms**. The remaining TTFF costs, in path
order:

- **B10 — `/decision` still touches the NAS.** One availability `stat`
  per play (60 s TTL cache, success only — `playstart.rs:24`,
  `stream.rs:679`), and a one-time live `ffprobe -show_chapters` for any
  file probed before chapter capture (`stream.rs:638`). Plus five separate
  DB reads of the same file row across `/decision` → create → serve
  (`stream.rs:669`, `hls.rs:194`, `transcode.rs:3505`, `hls.rs:389`).
- **B11 — the copy-path publish gate is a *deliberate* 12 s of media.**
  `COPY_PUBLISH_GATE_SECS = 12` (`plurx-core/src/transcode/mod.rs:221`),
  filled at I/O speed only when ffmpeg has `-readrate_initial_burst`
  (6.1+, warned at `ffmpeg.rs:140-152`). The playlist request long-polls
  up to 30 s for it (`transcode.rs:4426`). With burst the gate fills at
  I/O speed ("costs a few seconds" — STUTTER-4K); without it, at the
  flat 2× pace, the fill alone is cushion ÷ 2 ≈ 6 s — `ffmpeg.rs:144-145`
  warns exactly this. The gate is the *fix* for the cushion-edge freeze
  — the cost to attack is the fill, not the gate.
- **B12 — HEVC/DV masters wait for `init.mp4`.** For any
  `hvc1|hev1|dvh1|dvhe` session, `exact_hls_context` awaits
  `segment(session, "init.mp4")` before the master playlist is served
  (`http/hls.rs:648-654`) — the cold-NFS open+probe+first-fragment cost
  lands *before* the first byte of playlist on Apple.
- **B13 — a transcode session start pays admission + spawn + first
  segment.** Cache lookup happens before admission (`transcode.rs:3550`)
  — a cache hit skips everything — but a miss waits for a slot
  (`QUEUE_WAIT = 5 s`, `admission.rs:46`), spawns ffmpeg, and produces
  `SEGMENT_SECONDS = 2` of media before the playlist exists.
- **B14 — seeks restart the world.** Every non-VOD seek tears down and
  re-creates the session (`index.html:6273-6318`), paying B13 again —
  and on the copy path, B11 again. Cached (VOD) items seek free; live
  sessions do not.
- **B15 — autoplay-next starts from zero.** All three clients discover
  the next episode only after the current one ends, then make 3–4 serial
  API calls before `play()` (web `index.html:6345-6366`, Android
  `PlayerScreen.kt:566-576`, Apple `PlayerView.swift:817`). The server's
  producer sees `next_up` only on a multi-hour job cadence
  (`state.rs:1491`, `jobs.cache_produce_mins`). Nothing anywhere says
  "this viewer is 90% through episode 3" to anything that could warm
  episode 4.

### 1.2 Sustained streams — the machinery is half-wired

- **B16 — the ladder ships and nothing *adapts* with it.** The server
  snaps heights and returns `Rung { height, total_kbps, peak_kbps }` in
  the session response and `/decision` (`http/hls.rs:280`,
  `http/stream.rs:765`); `total_kbps` is, per its own comment, "the
  number an adaptation controller compares its estimate to"
  (`transcode.rs:4877-4889`). The native clients render that ladder in
  their *manual* quality menus (Apple `PlayerController.swift:575`,
  Android `PlaybackPolicy.kt:101-110`); the web client still hardcodes
  its own `QUALITIES` list, 360p missing (`index.html:4723`). But no
  client *adjusts* anything, and on Auto — the default — no client
  sends a height and no controller exists anywhere. The
  ADAPTIVE-QUALITY Phase 2 controller (`decideRung`) lives only in that
  doc's test plan — grep returns no implementation.
- **B17 — stalls are classified and then ignored.** The web client
  correctly splits `supply` vs `decode` stalls (`index.html:2646-2648`)
  and connects the verdict to nothing. No client can *lower the rung* in
  response to anything: every rescue path is "same bytes, different
  container" or "re-encode at the same Auto rung" (web
  `index.html:4508-4530`; Apple's one-shot same-recipe reopen
  `PlayerController.swift:239-251`; Android's three one-shot codec
  fallbacks `PlaybackPolicy.kt:62-81`). A bandwidth-starved Apple
  session has exactly two outcomes: one reopen, then a terminal error.
- **B18 — the last unexplained symptom is a telemetry gap.** Five
  `supply` stalls on the same 4K HEVC remux at 0.1 s buffered
  (PERF-PLAN §10 "still open"). The delivery-rate telemetry that would
  settle it exists — and evaporates (B19).
- **B19 — telemetry is rich, ephemeral, and web-only.** Every beacon is
  stringified at the door (`system.rs:452`, `client_log_line`
  `system.rs:472-575`) into a 2000-entry in-memory ring
  (`logbuf.rs:79`) behind a global 30/min bucket (`system.rs:396`).
  No DB table is playback-shaped. Prometheus exports exactly one
  playback number (`plurx_transcode_sessions_active`,
  `system.rs:1713`). Apple sends two beacon kinds ever
  (`PlayerController.swift:1872,1893`); **Android sends zero**
  (`Controller.kt:299-306` logs to logcat only). The one learning loop
  that exists is the reaper feeding `recent_speed` EWMAs into admission
  (`transcode.rs:4746-4755`) — proof the shape works.
- **B20 — flow control is invisible.** Suspend/resume is
  `tracing::debug!` only (`transcode.rs:4628,4638`); no counter, no
  duration accumulator, no `ahead_seconds` histogram. `SessionInfo`
  reports `hold_reason` live (`transcode.rs:1607-1653`) but nothing
  records it.

### 1.3 Bits — every stream is the same stream

- **B21 — one codec, one rate mode, one audio.** All five encoder
  families emit H.264 8-bit (`encoder.rs:31-39`, pinned by the
  exhaustive test at `encoder.rs:566-597`) with the same
  `-b:v / -maxrate 1.5× / -bufsize 2×` VBR closure
  (`encoder.rs:138-152`). No `-crf`, no ICQ/QVBR, no quality target
  anywhere in the tree. Audio is AAC stereo 160k unconditionally on the
  transcode path (`mod.rs:933-938`, defaults `mod.rs:477-494`). The
  rung bitrate is a pure function of height (`bitrate_for_height`,
  `transcode.rs:4857-4865`) — a grain-storm 4K HDR film and a static
  cartoon get the same 8 Mb/s at 1080p.
- **B22 — the fleet's decoders outrun its encoders' output.** Clients
  already declare decode caps on every decision request (the `Caps`
  struct, `http/stream.rs:125`).
  HEVC decode is effectively universal across the fleet's clients in
  2026 (~92% of browsers incl. hardware paths —
  [caniuse](https://caniuse.com/hevc)); Plex made HEVC output mainstream
  in 2025. plurx spends ~1.5× the bits H.264 needs for the same picture
  on every transcode.
- **B23 — much of the 4K DV library may still miss the GPU tone-map.**
  At the 2026-07-29 census (PERF-PLAN §5, `docs/PERF-PLAN.md:1119-1131`),
  all 614 DV files of 1359 measured 4K files took the CPU chain. Since
  `074fa5b`, HDR10-compatible DV routes to the GPU graph
  (`routing_hdr`, `mod.rs:687-699`) — and the compatible share of those
  614 is exactly the census question PERF-PLAN §5 left open. The AMD
  boxes' tone-map/encode speeds are likewise unmeasured. N0's fleet
  pass (§3.5) owns both numbers.

The through-line: the machinery from the first arc is sound, and most of
what this plan does is *finish wiring it to information* — about the
content (B21), the client (B16, B22), the network (B17), and the future
(B15).

---

## 2. Principles — the constraint set every milestone obeys

Numbered because milestones cite them.

1. **Optional, always.** Every feature in this plan lands behind a
   settings key following the house idioms: jobs are `jobs.*_mins`
   with `0 = off` and **off is the default** ("upgrading a server
   changes nothing about what it does at 3am until someone asks for
   it", `schedule.rs:11-15`); toggles follow the documented
   off-unless-on / on-unless-off idioms (`store/mod.rs:46-206`);
   staged features that must ship dark get a `PLURX_*` gate first,
   like `PLURX_PGS_OVERLAY` (`state.rs:218`). §11.1 is the complete
   key table.
2. **Nothing external is load-bearing.** Anything that *can* call out
   (an LLM digest, a hosted API) is an adapter with `none` as a valid
   and default backend, a local option as a peer, and a documented
   degraded mode. plurx with every adapter set to `none` must behave
   exactly like plurx today. This is a design invariant, not a
   preference.
3. **Real hardware is attributable and stoppable.** Anything that
   spends GPU, disk, or hours of encode time must say *what it is
   doing and why* in the product UI and offer a stop — the producer's
   `ProducingNow` card + Stop button (`state.rs:1562`,
   `http/system.rs:1600`) is the required precedent. `ps auxwww` on
   the box is not an acceptable answer.
4. **Acceptance runs where the bug can occur.** The §4.5bis scar
   (PERF-PLAN §10): a milestone about encoder behavior, validated in
   CI on software, shipped broken on hardware. Encoder- and
   client-facing milestones here name their acceptance machine (nynuc,
   or a physical device); CI asserts *selection* from logs, not
   pixels.
5. **Cache identity is sacred.** Every parameter that changes output
   bytes goes into `Recipe::hash` in the same commit that introduces
   it (`recipe.rs:11-15`: "a field added to the recipe and forgotten
   here is a cache that serves stale output forever"). §11.4 lists
   every hash change this plan makes.
6. **Measure, then model.** Heuristics ship first; a learned model is
   admitted only where stored telemetry shows the heuristic missing,
   and it must run locally. This is the Puffer lesson
   ([NSDI '20](https://puffer.stanford.edu/static/puffer/documents/puffer-paper.pdf)):
   simple buffer-based control beat RL controllers over 13
   stream-years; the ML that won (Fugu) was a small supervised
   predictor trained in situ on the deployment's own data. plurx's
   version of that loop already exists in miniature — the reaper
   feeding measured speeds into admission (`transcode.rs:4746-4755`).
7. **Nothing new on the play path.** Analysis runs at scan time or in
   idle jobs, never between press-play and first frame — "a play-path
   that quietly re-probes on the NAS is this plan's original sin
   coming back" (PERF-PLAN §9). The play path may *read* stored
   verdicts only.

---

## 3. N0 — Telemetry becomes data

**Implementation status (2026-08-09):** server storage, ingest, lifecycle
events, metrics, readers, census fields, the web parity slice, and Android
native telemetry parity are landed in source on their milestone branches.
Apple parity and the named-machine acceptance runs remain and must not be
inferred from these source milestones.

**Objective:** the beacons plurx already emits become queryable rows that
survive restart, get their server-side context attached at ingest, and
exist for all three clients — because every later milestone either feeds
on this data (N2 calibration, N4 priors, N7 digests) or is judged by it
(N1, N3, N6 acceptance). This is the plan's M0.

### 3.1 Persist structured beacons

`client_log` currently discards the typed struct at the door (B19). Keep
the log line exactly as is — it is the human debugging surface — and
*additionally* insert the structured record:

- New table `playback_events` (migration v17): `id, at_unix_ms, user_id,
  session_id NULL, file_id NULL, event, level, method, encoder, height,
  ms NULL, runway NULL, bandwidth_kbps NULL, detail, attempt, reason,
  ua, extra JSON`. Indexed `(event, at_unix_ms)` and `(file_id,
  at_unix_ms)`.
- **Join server truth at ingest.** When the beacon names a live session,
  attach the server's view *at that moment* — `recent_speed`,
  `ahead_seconds`, `suspended`, `hold_reason`, `delivered_bps`, and the
  effective `readrate` (all in `session_info()`). Today the
  client sends `bandwidth` and `runway`, the server knows
  `recent_speed` and `ahead_seconds`, and **no record ever contains
  both** — this join is the single change that makes B18 answerable.
- Retention: `telemetry.retain_days`, default **30**, `0 = off`
  (rows still requires the feature on; see §14 D1 for the
  default-state decision). A nightly `DueJob` prunes by age. At ~a few
  hundred beacons per viewing hour this is megabytes, not gigabytes —
  but the budget is enforced, not assumed.
- Rate limit becomes per-user token buckets sized for a data pipeline
  (e.g. 240/min per user), replacing the global 30/min bucket whose
  size was chosen to protect a log ring (`system.rs:396`).

### 3.2 Server-side event records

Client beacons only see what clients see. Record the server's own
events into the same table (`user_id NULL`): session start/end with
reason (idle / superseded / client-released / killed — today free text
at `transcode.rs:4386,4740`), suspend/resume with `hold_reason` and
held-duration (B20), producer pass summaries, cache hit/miss per
session start, EVENT→sliding playlist transition (today invisible,
`transcode.rs:558-560`).

### 3.3 Prometheus grows a playback surface

Histograms/counters derived from the same ingest path: `plurx_ttff_ms`
histogram by method · `plurx_stalls_total{kind}` ·
`plurx_suspends_total{reason}` + `plurx_suspended_seconds_total` ·
`plurx_cache_serves_total{result}` · `plurx_sessions_total{encoder}`.
The scrape endpoint already exists (`system.rs:1621`); this is additive.

### 3.4 Client parity — the floor, not the ceiling

- **Android: from zero to the minimal set.** `ttff`, `stall`, and an
  `hls_fatal`-equivalent on `onPlayerError` — built new; today's only
  output is the logcat line at `Controller.kt:299-306`, which carries
  delivery/action/caps context and no timing at all. Android also gets
  the stall *watchdog* it currently lacks in N4; here it only reports.
- **Apple: add `ttff`.** The client already stamps open-time and
  position; one beacon at first real progress. (Apple's stall beacon
  shipped with PR #86; failure beacon exists.)
- **Web:** already complete; add `quality_switch` (currently a switch
  is only inferable from a later `ttff` reason, `index.html:6611`).
- `SessionInfo` gains `readrate` parity with `StreamInfo`
  (`progressive.rs:90`) so the overlay's paced/fed classification works
  on HLS sessions too.

### 3.5 The fleet census rides along

The boot probes already detect everything (`detect_encoders`
`encoder.rs:450-492`, pipeline probe, pacing caps); N1/N5 need the
*fleet* answer: which nodes have which QSV generation, which rate-control
modes their driver accepts, HEVC/AV1 encode presence. Add the new
capability fields to `EncoderCaps` (§4.3), surface per-node on
`/api/v1/system` (already `Serialize`, `transcode.rs:2046`), and record
one census table in this doc's review record after the first fleet
deploy. The AMD boxes' unmeasured tone-map speeds (B23) get their
number from the same pass.

**Effort:** medium — one focused weekend. **Risk:** low; additive
everywhere. **Toggle:** `telemetry.retain_days` (rows), individual
beacons ride existing client versions.

**Acceptance:** restart the server; TTFF and stall distributions for the
prior week still render (`scripts/perf-report` reads the table, not the
ring). Play the known 4K remux stall title on nynuc; the stored rows
answer B18's question — `delivered_bps` vs `readrate` vs `runway` at
each stall instant, joined in one record. An Android play produces a
`ttff` row.

---

## 4. N1 — Quality-bounded rate control

**Objective:** stop paying fixed bitrates for content that doesn't need
them. Same encoders, same ladder heights, same caps — but the target
becomes *quality*, and the bitrate becomes the *ceiling*. This is the
cheapest real win in the plan: a pure argument change, boot-validated
for free, worth 10–35% of the bits on typical content at flat perceptual
quality (per the capped-CRF record:
[Ozer's measurements](https://streaminglearningcenter.com/encoding/saving-encoding-streaming-deploy-capped-crf.html);
Jellyfin moved its QSV defaults the same direction in
[PR #14079](https://github.com/jellyfin/jellyfin/pull/14079)).

### 4.1 Per-family quality modes

One new arm in the `rate_control` closure (`encoder.rs:138-152`), keyed
by a `RateMode` in `TranscodeOptions`:

| Family | Quality mode | Flags (caps retained) |
|---|---|---|
| Software | capped CRF | `-crf {q} -maxrate {kbps*3/2}k -bufsize {kbps*2}k` |
| QSV | QVBR | `-global_quality {q} -b:v {kbps}k -maxrate {kbps*3/2}k -bufsize {kbps*2}k` |
| VA-API | QVBR | `-rc_mode QVBR -global_quality {q}` + caps (AMD needs Mesa ≥ 24.3 — [relnotes](https://docs.mesa3d.org/relnotes/24.3.0.html)) |
| NVENC | capped CQ | `-rc vbr -cq {q}` + caps |
| VideoToolbox | constant quality | `-q:v {q}` + caps (Apple Silicon only) |

The bitrate ladder's numbers stop being targets and become the ceilings
they already advertise: `Rung.peak_kbps` is unchanged, `EXT-X-STREAM-INF`
BANDWIDTH stays honest, and the client-facing contract does not move.

### 4.2 Guarded by boot validation, not hope

`validation_args()` runs the production argument set (`encoder.rs:328`;
pinned by the window-comparison test at `encoder.rs:797-828`), so every
new flag is exercised against the real driver at boot. ICQ/QVBR
availability genuinely varies by generation and driver (Intel's own
tracker has holes — [media-driver #1597](https://github.com/intel/media-driver/issues/1597));
the failure mode is therefore *a boot log line and a per-family
fallback to today's VBR*, never a viewer-facing error. A family whose
quality mode fails validation records that in the new `EncoderCaps`
fields (§3.5) and keeps bitrate mode.

### 4.3 Contract changes — one effective identity, everywhere (review R4)

- **`EffectiveRateControl` is resolved before anything is hashed.**
  Requested mode (`transcode.rate_mode`) + target quality pass through
  validation/fallback first; what comes out — `Vbr` or `Qvbr { q }`
  per family — is the *effective* value, and only the effective value
  reaches ffmpeg args or a recipe. A runtime mode flip re-runs
  validation and atomically publishes the new effective mode; a
  session never hashes a requested-but-unvalidated value, so a
  fallback can never cache one encoding under another's identity.
- **One `effective_recipe()` builder feeds every path.** The tree has
  four recipe constructors today (live lookup, speculative producer,
  offline, tests — `transcode.rs:2430,2607,2694`); changing their
  digest strings independently is a drift trap. They collapse into one
  builder whose input is the normalized, validated `TranscodeOptions`
  (rate control included; output codec joins it in N5). `rate_control`
  moves from the manager-level `PipelineDigest` constant
  (`recipe.rs:72`) into the per-recipe fields, since N2 makes quality
  per-title.
- **Legacy digest bytes are preserved exactly.** Bitrate mode keeps
  `vbr:maxrate1.5x:bufsize2x` byte-for-byte (golden-hash fixtures pin
  it); every effective QVBR value hashes differently (inequality
  fixtures); the same options through live, producer, and offline
  paths hash identically (cross-path fixture).
- **Offline packages pin their effective recipe at creation.** A
  package can yield and requeue mid-preparation, and
  `set_offline_package_recipe` accepts only the original hash
  (`store/sqlite/offline.rs:423-438`) — a hot setting change
  mid-package would strand it. The package row persists its effective
  recipe inputs at creation; every resume rebuilds from that snapshot,
  never from current settings.
- `EncoderCaps` gains per-family `quality_rc: bool` from validation.
- Settings: `transcode.rate_mode` = `bitrate` (default) | `quality`;
  `transcode.quality` = an optional single override — unset means the
  family-tuned defaults the N1 acceptance run calibrates (§14 D5).
  Height-independent to start; the per-title layer is N2's job, not
  this one's.

**Effort:** medium (review R4/R8) — the flags are a sitting; the
effective-identity plumbing, offline snapshot, golden-hash fixtures,
and the VMAF harness slice below are the real budget. **Risk:**
low-medium; driver variance is exactly what boot validation catches.
**Toggle:** `transcode.rate_mode`, default off (= `bitrate`).

**Acceptance (nynuc, per principle 4):** `scripts/bench` grows a
rate-control mode first — today it deliberately does not decode pixels
(`scripts/bench:18`), so the harness slice precedes the feature
(review R7). Shape: `scripts/bench rate-control --corpus <fixtures>
--modes vbr,qvbr --vmaf-model vmaf_v0.6.1 --json out/rate-control.json`,
failing nonzero when quality mode regresses VMAF or exceeds the
advertised peak over any `bufsize` window. On that harness: quality
mode produces ≤ the bytes of bitrate mode on the easy half of the
corpus at equal-or-better VMAF (VMAF offline only, `n_subsample` as
needed — never in the live path, principle 7); encode speed within 10%
of bitrate mode; a session on a family whose driver refuses the mode
starts and logs the fallback. Identity: golden-hash fixtures hold the
legacy VBR digest constant; cross-path fixtures prove
live/producer/offline hash agreement.

---

## 5. N2 — Per-title intelligence

**Objective:** plurx learns what each file *is* — how hard it is to
encode, where its scenes fall — once, at scan/idle time, and every
consumer of a bitrate or a schedule gets to read the verdict. This is
the "smarter encoding" milestone: Netflix-shaped per-title logic
([~17–20% measured savings](https://netflixtechblog.com/dynamic-optimizer-a-perceptual-video-encoding-optimization-framework-e19f1e3a277f))
built from parts the tree already has, with no ML dependency in v1.

### 5.1 The analysis job

New `DueJob::AnalyzeMedia` + `jobs.media_analysis_mins` (default 0 =
off), scheduled in the same tier as `ProduceCache` (last — it competes
for the box, `schedule.rs:97-99`). Shape: `RetryProbes`' backlog sweep
(`scan/mod.rs:193-233` — a queryable predicate and a writer) with the
artwork retry's bounded-batch discipline (`store/mod.rs:411`), because
an unbounded first pass over a full library would run for days.
Runs at `Priority::Background` under the same yield rules as the
producer (live waiter → stand down, `admission.rs:288-290`), shows an
attribution card with a stop button (principle 3).

**Pass 1 — cheap, ffmpeg-only (v1):** sampled windows (e.g. 6 × 20 s
spread across the runtime) through `-vf siti,scdet` on the existing
ffmpeg (both filters ship in distro/jellyfin builds; verify with a
behavioral probe beside `has_dovi_rpu`, `ffmpeg.rs:57-92` — ~20 lines,
principle: probe behavior, never parse versions). Yields spatial/temporal
complexity stats, scene-cut density, plus bits-per-pixel from the probe
row. Cost: a few seconds of decode per title, NAS-read bounded, sampled
not full-pass.

**Pass 2 — calibrated, optional:** for titles the producer will cache
(N3/N5), short probe encodes at 2–3 quality values on sample windows +
`libvmaf` scoring — an in-tree, ffmpeg-CLI version of what
[ab-av1's crf-search](https://github.com/alexheretic/ab-av1) does —
producing a per-title quality/ceiling pair that hits a VMAF target.
Offline only, minutes per title, `jobs.deep_analysis_mins` separately
gated. (External tools like [VCA](https://github.com/cd-athena/VCA)
(GPL-3, ~370 fps at 2160p) are noted as an *optional* accelerator, not
a dependency; ML predictors of the LiteVPNet kind are research-grade in
2026 and stay out per principle 6.)

### 5.2 Where the verdict lives

`probe_json` via `json_set` — the chapters precedent exactly
(`merge_file_probe_chapters`, `store/sqlite/media.rs:915-936`): atomic
against concurrent scans, guarded on `probe_json IS NOT NULL`, and
**correctly destroyed by a rescan of changed bytes** (`upsert_file`
overwrites the blob, `media.rs:792` — new bytes, stale analysis, right
answer). Key: `plurx_analysis` `{ v, siti: {...}, scene_cut_density,
bpp, complexity_class, quality_target?, calibrated_at? }`. No
migration, no new table, hiqlite mirror already carries the column.

### 5.3 Consumers

- **Bitrate/quality bias (the point):** at the single injection line
  `video_bitrate_kbps: bitrate_for_height(...)` (`transcode.rs:2342`),
  a clamped per-title multiplier (bounds ±30%, hard-clamped so a bad
  analysis can never halve or double a rung beyond intent) in bitrate
  mode, or a per-title quality target in N1's quality mode. All three
  call sites of `options_for` share it automatically, and the applied
  value rides N1's `effective_recipe()` into the hash (§11.4,
  principle 5).
- **Producer ranking:** `worth_producing` / `rank`
  (`produce.rs:195-233`) prefer high-complexity titles — the ones
  whose live encode is riskiest and whose cache entry buys most.
- **Admission (later, flagged):** a complexity class could inform
  `hopeless_in_software` (`admission.rs:420-425`) for cold classes;
  noted as a follow-on, not v1 — admission is safety machinery and
  changes there need their own acceptance pass.

**Effort:** medium — the job + pass 1 + the bias is a weekend; pass 2
calibration is a second one. **Risk:** low-medium; the bias clamp is
the safety rail, and off means byte-identical behavior. **Toggles:**
`jobs.media_analysis_mins`, `jobs.deep_analysis_mins`,
`transcode.per_title` = off (default) | on.

**Acceptance (nynuc):** corpus A/B — per-title mode spends measurably
fewer bytes on the easy half at equal VMAF and *no fewer* on the hard
half than N1 alone (the hard half is allowed to cost more: that is the
feature working); the analysis sweep over the fixture library completes
inside its pass budget with live playback untouched (start a play
mid-sweep; the sweep yields within `PRODUCER_POLL`-class latency).
Verdicts visible per-file in the UI (principle 3: what was decided and
why).

---

## 6. N3 — Warm starts

**Objective:** the seconds that remain in TTFF are almost all "the
encoder isn't running yet" and "the NAS hasn't answered yet." Attack
both: cache the *front* of likely titles so pressing play meets bytes
that already exist, warm the next episode while this one plays, and
shave the remaining cold-open serial costs. This is B11–B15's
milestone, and PERF-PLAN §6.3 already named its centerpiece as the
deliberate deferral: "prefix-only warm caches (first N minutes at the
top rung to bridge live-session spin-up)" (`docs/PERF-PLAN.md:1416-1420`).

### 6.1 Prefix entries in the M3 cache — the seam is a protocol, not a trick

The machinery is already prefix-shaped: the producer emits numbered
parts with exact resume boundaries (`transcode.rs:2969-3073`,
`produce::resume_at_ms` summing real EXTINFs `produce.rs:109-111`) and
`produce::assemble` builds a playlist from any part slice
(`produce.rs:118-155`). What v1 of this plan got wrong — and review R1
proved with a live probe — is the seam: a second ffmpeg started at the
boundary *rewrites* the playlist rather than appending
(`-start_number` renames files; it does not touch packet timestamps),
and both tracks' PTS restart near zero. Phase 3 said exactly this
(seeked restarts reset PTS and need `EXT-X-DISCONTINUITY`), and the M4
takeover contract (PERF-PLAN §7.3) already carries the full answer:
overlap, sequence and discontinuity accounting, and an owner. A
prefix-hydrated session is a *planned, same-node instance of that same
takeover* — v2 specifies it as one instead of pretending it is an
ordinary session.

**N3.0 — the seam spike comes first (review R8).** Before any schema
or producer change: prove the continuation recipe end to end on
fixtures, against every enabled encoder family. The spike owns the
four choices below, and the §6.4 packet gate is its exit criterion.

The contract, per review R1–R2:

- **The server owns the served playlist for hydrated sessions.** The
  ffmpeg writer file stays ffmpeg's own; `Manager::playlist` composes
  what clients see: the prefix's entries (from the artifact manifest,
  immutable) + `EXT-X-DISCONTINUITY` + the writer's entries, with
  server-managed `MEDIA-SEQUENCE` and `EXT-X-DISCONTINUITY-SEQUENCE`
  (Apple authoring rules 8.13/8.17). The raw writer playlist is never
  served for a hydrated session — extending the standing precedent
  that the served view is already a rewrite (`served_live_playlist`,
  `transcode.rs:515`). Publication is atomic: hydrate into the session
  dir, validate against the manifest, start the continuation encoder,
  then publish one playlist generation — the gate opens only on the
  published generation, so no client can observe two histories.
- **Timestamps are made monotonic, and the seam is still marked.** The
  continuation encoder gets `-output_ts_offset covered_ms` alongside
  `-start_number K` — the pairing ADAPTIVE-QUALITY Phase 3 specifies,
  because they solve different problems: one offsets packet time, the
  other only names files. The `EXT-X-DISCONTINUITY` stays regardless: a
  fresh encoder is a new priming boundary and possibly new parameter
  sets.
- **Source identity is proven, not assumed (R2).** The recipe hashes
  the DB row's (size, mtime); a file replaced in place would otherwise
  splice old prefix bytes onto new source bytes mid-stream. Before
  anything publishes, a fresh `stat` of the source must equal the
  recipe's identity, and the continuation ffmpeg is spawned against
  that verified identity. Mismatch anywhere → the prefix is discarded
  (row invalidated) and the session proceeds as a plain live encode.
- **A prefix is its own artifact kind (R2).** Location rows gain
  `kind = full | prefix` — explicit, never inferred from `covered_ms`
  at use time — plus a boundary manifest: the segment list with exact
  per-segment EXTINF and byte sizes, summed covered duration, and the
  container generation. The manifest is authoritative for serving; the
  artifact's own playlist file (VOD-shaped, ENDLIST and all) is never
  served. `cache_hit`'s `complete = 1` contract keeps meaning "safe to
  serve *as its kind*."
- **Hydration is all-or-nothing.** Hard-link segments into the session
  dir when same-filesystem, else copy; the `CacheReadGuard` is held
  from lookup to session end exactly as full hits hold it
  (`transcode.rs:2478-2516`). Any missing segment, manifest mismatch,
  identity failure, or continuation-spawn failure before publication →
  fresh session, no prefix URI ever advertised (every cache read path
  must degrade to a live transcode, PERF-PLAN §9).
- **Production:** the producer's per-pass deadline machinery already
  stops cleanly at a boundary; a prefix job is a produce with a small
  target (`cache.prefix_secs`; off by default, §14 D3 recommends 90 s)
  that *publishes* — kind = prefix, manifest included — instead of
  checkpointing. `prefix_secs` is in the recipe (§11.4) so a prefix
  and a full asset are different names; `start_seconds` stays rightly
  excluded (`recipe.rs:141-144`).

Prefixes obey the same LRU/budget/sweep rules (`cachekeep.rs` phases
1–3) and the same reader guards.

### 6.2 Predictive production — event-driven rails

The producer's rails (`continue_watching` → `next_up` →
`recently_added`, `state.rs:1487-1505`) are the right predictor —
next-episode-during-binge is near-deterministic and continue-watching
covers most of the rest; a Markov model would add little (principle 6,
and the prior-art record agrees). What's wrong is the *cadence* (B15):
hours, when the signal is minutes old. Add event triggers, each its own
toggle, all funneling into the existing single-flight producer
(`background_producer` try-lock + offline yield, `transcode.rs:2570-2576`
— new triggers must participate or fight it):

- **Progress trigger:** a `POST /items/:id/progress` crossing ~85% of
  an episode enqueues a prefix produce for that user's next-up at that
  user's predicted recipe (`auto_height` + their declared caps + N2
  verdict). `jobs.prewarm_on_progress` toggle.
- **Browse-intent trigger (cheap tier only):** opening a detail view
  warms the *decision facts* (B10: the availability stat + the
  file-row reads) — no encode, just the memo. An optional aggressive
  tier may prefix-produce on detail view; default off.
- **Session-create fallback:** a play that misses everything records
  the miss (N0) — the hit-rate number this milestone is judged by.

### 6.3 Cold-open shaves

- **Init-segment warm cache (B12):** persist `init.mp4` per
  `(file_id, size, mtime, track-selection)` beside the subtitle VTT
  cache (`subtitles.rs` `warm_vtt` is the pattern — detached,
  deduplicated, negative-memoed). `exact_hls_context` serves the
  cached init for HEVC/DV masters instead of awaiting the session's;
  produced at scan/analysis time for eligible files.
- **Decision-facts memo (B10):** one in-process
  `(file_id, mtime) → PlaybackFacts` memo collapsing the repeated
  `get_file`/`get_file_probe_json` reads across
  `/decision`→create→serve. Internal machinery in the
  `AvailabilityCache` / `AheadLimits`-snapshot mold — bounded, TTL'd,
  no settings key. Invalidation by mtime, same as the recipe.
- **Client next-episode pre-resolve (B15, client half):** at ~95%
  progress, clients resolve the next episode's ids and `/decision`
  *(not* the session*)* so the end-of-episode path is one call, not
  four. Pairs with the server's progress trigger having already
  warmed the prefix.

### 6.4 Seam gates — packet truth before player truth (review R3)

Zero stall beacons cannot prove the seam: every play could carry a
click, a silence gap, or a duplicated frame and still beacon nothing.
The seam is gated at the media layer first — the rule that already
cost this repo two green-for-weeks defects: for anything about
timestamps or muxer output, produce a stream and *measure* it.

- **Packet gate**, per stream across the seam: monotonic DTS/PTS under
  the `-output_ts_offset` contract; audio gap/overlap bounded
  (starting bound: ≤ one AAC frame, 1024 samples — the spike may
  tighten it); video gap/overlap bounded (starting bound: ± one frame
  duration); EXTINF sums equal to packet reality within muxer
  rounding.
- **Decode gate:** decoded frames and audio around the boundary
  compared against a single-encoder reference of the same recipe — a
  new AAC encoder instance is a priming boundary, which is a media
  contract, not an implementation detail.
- **Fixture matrix:** 24000/1001 · 30000/1001 · integer rates · VFR ·
  audio-leading-video · 44.1 kHz · 5.1-source-to-stereo. Run per
  enabled encoder family — timestamp behavior is encoder-specific.
- Player-level integration (stall/error-free plays) is a *separate,
  later* gate — necessary, never sufficient.

Command shapes (names may change; existence, nonzero failure on the
stated invariants, and a stable output artifact may not):

```text
cargo test -p plurxd prefix_hydration_seam -- --nocapture
scripts/perf2-seam-probe --all-encoders --fixtures testdata/perf2-seams
```

**Effort:** the largest item in this plan (review R8) — a two-writer
playlist-ownership protocol plus a media-boundary correctness gate;
the triggers and shaves stay small. Sequence *after* N1/N2 so prefixes
are produced under final recipes (a rate-mode change orphans every
prefix by design — principle 5 — so fill the cache once, not twice);
N3.0's spike sequences before any N3 schema. **Risk:** medium-high,
held by the all-or-nothing rule and the packet gates. **Toggles:**
`cache.prefix_secs` (0 = off), `jobs.prewarm_on_progress`.

**Acceptance (after the §6.4 gates pass; nynuc + one physical Apple
device):** press play on a prefix-cached 4K HDR transcode title — TTFF
lands in the cached-VOD class (measured by the N0 beacon), the served
playlist carries exactly one discontinuity at the boundary, and 10
consecutive plays produce zero stall beacons *and* pass the packet
gate on the session's actual output. Autoplay-next with the progress
trigger: episode N+1 starts ≤ 2 s after N ends. An Apple HEVC copy
session's master playlist is served without awaiting a fresh init.mp4
(log line proves the warm path; TTFF delta measured). Failure-mode
matrix, each case distinct (review R2): source file replaced in place
→ prefix discarded, plain live session, no corruption; cache dir
corrupted mid-hydration → plain live session; ordinary LRU pressure →
the held reader guard pins the entry for the session's lifetime.
Prefix hit-rate per trigger visible in the digest (N7) or system page.

---

## 7. N4 — Sustained smoothness

**Objective:** finish the adaptation story the server already built its
half of (B16), connect the stall classification that dangles (B17), and
give the two native clients a way down that isn't "same recipe, then
terminal error." This executes [ADAPTIVE-QUALITY.md](ADAPTIVE-QUALITY.md)
Phase 2 — that doc remains the design record; the deltas below are what
two more years of evidence changed.

### 7.1 The web Auto controller — Phase 2, with restart-aware constants

As designed in ADAPTIVE-QUALITY Phase 2 (pure `decideRung` function,
5 s sampling of `hls.bandwidthEstimate` + stalls + runway + server
`recent_speed`, severe-pressure one-move drop, cooldown on upgrades
only, estimate seeding across `Hls` instances) — plus what the record
since adds:

- **The web client finally consumes the server ladder** (B16):
  `QUALITIES` hardcoding is replaced by `/decision`'s `ladder`,
  comparing the estimate against `total_kbps` — the field the code
  built for exactly this comparison (`transcode.rs:4880-4882`) — with
  `peak_kbps` staying the advertised BANDWIDTH bound; the missing 360p
  rung appears.
- **Switch-cost awareness.** plurx's switch is a session restart with a
  measured interruption SLO (p95 ≤ 2.5 s, PERF-PLAN §8.6 d4), not a
  free segment hop — so the controller gets a *dwell time* (no
  voluntary switch within 60 s of the last; emergencies exempt) and an
  MPC-style one-liner: switch only when the estimated QoE gain over
  the dwell horizon exceeds the restart cost. Asymmetric thresholds
  mirror hls.js's own 0.95-stay/0.7-up factors, widened for the
  restart cost.
- **Supply-stall rescue** (B17, the ADAPTIVE-QUALITY "adjacent win"):
  ≥3 `supply`-classified stalls in 60 s (A-Q's threshold; tune with N0
  data) on a ladder session → severe pressure (one-move drop). On
  remux/direct — outside the ladder —
  the same trigger drops into transcode Auto, closing the "69 Mb/s
  remux over hotel Wi-Fi buffers forever" hole. `decode`-classified
  stalls never touch the rung (they already have their own rescue,
  `index.html:5831-5904`).

### 7.2 Network priors — the CS2P-lite memory

The evidence for remembering networks is strong
([CS2P](https://users.ece.cmu.edu/~vsekar/assets/pdf/sigcomm16_cs2p.pdf):
sessions sharing network features have similar throughput; 40–50%
prediction-error cuts) and the cost here is a table:

- From N0 data, the server maintains per `(user, client-class,
  network-fingerprint)` a sustained-throughput EWMA and a worst-rung
  verdict (fingerprint = UA class + /24, coarse on purpose — this is
  a prior, not tracking).
- `/decision` and the session response gain `prior_kbps`; the web
  controller seeds `hls.bandwidthEstimate` with it (replacing the
  500 kb/s cold default) and picks its starting rung under it.
- **Apple and Android benefit without a controller:** on Auto they
  send no height, so `auto_height` (`transcode.rs:3379-3394`) — already the
  server-side Auto policy — consults the prior and starts a
  historically-starved client one rung down, or a proven-fast LAN
  client at the cap. Server-side, zero client changes.

### 7.3 Native clients get a way down — bound, normalized, replayable (review R5)

A bare `reopen_reason=stall` flag is not enough to make "one rung
down" deterministic: the create body carries no prior-session binding,
a transport retry re-normalizes against different state once the first
attempt supersedes the old session, and Apple's reopen queue collapses
concurrent causes last-writer-wins. The contract:

- **Bound to the previous session.** The stall reopen carries
  `previous_session_id` + a typed `reopen_reason`. The server
  validates that session belongs to the same user/playback/file, reads
  its *resolved* rung exactly once, and computes the target one rung
  below. Floor: the lowest ladder rung — at the floor, one same-rung
  retry, then the existing terminal surface. Manual-height sessions
  are never auto-stepped — a manual pick is sticky; stepping is for
  Auto.
- **Normalized once, under the existing idempotency key.** The
  resolved target is persisted under `request_id` *before* the
  previous session is superseded — extending `claim_request`
  (`transcode.rs:3204`), not adding a parallel retry store (the
  review's clean-list endorses exactly this). A transport replay of
  the same `request_id` returns the same session and target; it can
  never step down twice or 409 on a re-normalized fingerprint.
- **Apple carries the cause, typed, with precedence.** The reopen
  queue (`PlayerController.swift:92-125`) gains a cause field with
  defined merging: a user-initiated seek or track change *clears* a
  pending stall cause (the user acted; recover normally); the
  HDR-compatibility ladder keeps its own untouched lane; and the
  recovery budget's scope is now stated as its definition — per
  source-session, reset by 5 s of established playback (today's
  mechanism, named).
- **Android:** gains the minimal stall watchdog it lacks (position
  stagnant ≥ 6 s while `STATE_BUFFERING` → one reopen with the same
  typed cause) — and the N0 beacon so its stalls exist at all.
- **Tests:** transport replay of a stall reopen; a queued seek racing
  a stall; an audio/subtitle change during a stall; a stall at the
  floor rung; two devices sharing a `playback_id`.

### 7.4 Flow control becomes observable

Rides N0: suspend/resume counters + held-duration by `hold_reason`
(B20), the EVENT→sliding transition logged as an event (the PR #91
review's leading alternative hypothesis for the iPad hold — currently
invisible), and `SessionInfo.readrate` parity. No policy change to the
ahead-window in this plan — with N0's data, tuning it becomes an
evidence question instead of a lever-guessing one (the #87→#91
release-threshold arc is the cautionary tale).

**Effort:** medium-large (review R8) — the controller is ~120 lines
plus tests (ADAPTIVE-QUALITY's estimate holds) and priors are a table
+ two read sites, but the reopen normalization, the Apple queue
semantics, and the shaping harness below are real work; native deltas
ride each client's normal release.
**Risk:** low-medium; a mistuned controller is a visible switch, Manual
stays one tap away, and `off` restores today exactly. **Toggles:**
`playback.auto_abr` (web controller, default off until the acceptance
week), `playback.network_priors` (default off first release),
per-client reopen behavior rides client versions.

**Acceptance:** playback-lab first grows the shaping layer it
deliberately lacks today (PLAYBACK-TESTING.md's non-goals exclude
WAN/Wi-Fi simulation — the harness slice precedes the feature, review
R7). Shape: `scripts/playback-lab run --suite stall-recovery
--network-profile 8mbps-to-1.5mbps --json out/stall-recovery.json`,
failing nonzero when recovery misses the criteria. On it: the 8 →
1.5 Mb/s cliff reaches a sustainable rung with ≤ 1 automatic
restart and recovers with ≤ 1 upgrade per 60 s (ADAPTIVE-QUALITY's own
criteria); switch-interruption p95 measured and ≤ 2.5 s on LAN; on a
repeat session behind a throttled link, the *first* rung is already
sustainable (priors working — `ttff` row shows no immediate downgrade);
an Apple session stalled by throttling reopens one rung down and
survives (device test); controller off ⇒ playback-lab's normalized
trace (UUIDs, ports, wall-clock times, temp paths scrubbed) is
identical to today's.

---

## 8. N5 — Codec modernization: HEVC now, AV1 where it's free

**Objective:** spend B22. The fleet's clients decode more than the
fleet's encoders emit; shipping the same picture in fewer bits is a
sustained-streaming fix (the Wi-Fi-bridged NAS hop, hotel links) and a
capacity fix (smaller ahead-windows, more concurrent streams per byte
budget).

### 8.1 HEVC as a JIT output rung — gated per client, opt-in

- **Eligibility travels with the create, not just the decision
  (review R6).** `Caps` is a `GET /decision` query object today
  (`http/stream.rs:111-149`); nothing on `POST hls/sessions` carries
  it. The create body gains the same `Caps` (additive, optional —
  absent means H.264), and `OutputCodec` becomes its own typed value,
  separate from encoder *family* (`Encoder::video_codec` hard-wires
  H.264 per family today, `encoder.rs:31-39`). The chosen codec rides
  the request fingerprint, the effective recipe (N1's builder), the
  `Session`, and the response.
- Encoder eligibility from boot caps (`hevc_qsv` Gen9.5+,
  `hevc_vaapi`; validated the same way as N1).
- Same ladder heights; per-rung bits ×~0.65, or the same N1 quality
  target (which then simply *finds* the savings).
- **HEVC JIT output is fMP4 — decided, not open (review R6).** Apple's
  authoring spec (item 1.5) *requires* HEVC in fragmented MP4, and
  Apple is the gating client, so v1's "TS or fMP4?" question is
  answered. That is a `muxer`/`segment_policy` digest change, and it
  reaches the producer: `produce::assemble` moves segments and writes
  a playlist with no init-segment concept today (`produce.rs:113-155`)
  — it gains `EXT-X-MAP`/init ownership, and every resume path proves
  init compatibility across parts or publishes a discontinuity + new
  map. Init/`.m4s` serving and exact `hvc1` codec-string derivation
  already exist on the copy path (`hls.rs:634-671`) — that derivation
  becomes the signaling contract.
- **`CODECS` is derived from produced media, fail-closed.** Live and
  cached transcode sessions hardcode `avc1.640034,mp4a.40.2` today
  (`transcode.rs:2496,3739`) — HEVC bytes advertised as AVC would be
  the bug. The master's `CODECS` comes from the actual init segment; a
  mismatch fails the session rather than mis-signaling it.
- **The tripwire is load-bearing:** the exhaustive
  every-encoder-is-H.264 test (`encoder.rs:566-597`) and
  `delivered_dynamic_range`'s hardcoded `"sdr"` badge contract
  ([MEDIA-BADGES-PLAN.md](MEDIA-BADGES-PLAN.md) §2.1) must be updated
  *deliberately* in the same change — output stays 8-bit SDR BT.709 in
  v1 (`pixfmt`/`colour` digest fields unchanged), so the badge answer
  is the same; codec and dynamic range remain separate fields with
  independent assertions ("HEVC" is not a dynamic range).
- **Predictive production without a requesting client (review R6):**
  the producer makes H.264 entries by default; HEVC prefix/cache
  entries are made only for `playback_id`s whose recent decisions (N0
  data) declared HEVC decode.
- Settings: `transcode.output_codec` = `h264` (default) | `auto`
  (HEVC when client ∧ encoder agree, else H.264).

### 8.2 AV1 in the cache lane only — the grain dividend

No box in the fleet encodes AV1 in hardware (Intel needs Arc/Meteor
Lake+; the AMD boxes need VCN4+ —
[Jellyfin's matrices](https://jellyfin.org/docs/general/post-install/transcoding/hardware-acceleration/intel/));
JIT AV1 is out (§12). But the *producer* is unpaced background CPU
(`transcode.rs:2982-2994`), and SVT-AV1 in 2026 is fast enough there:
v4.2's mid presets reach realtime-class 1080p on 8-core NUC CPUs
(scaled from
[published 24-core numbers](https://openbenchmarking.org/result/2502218-NE-SVTAV130739&export=html)
— treat as an estimate to verify on nynuc, not a fact), and **film
grain synthesis is the one technique in this plan that can halve the
bits on exactly the titles that hurt most** — heavy-grain film (the
[AV1 FGS reference](https://norkin.org/pdf/DCC_2018_AV1_film_grain.pdf)
measured up to ~50% on grainy sources; savings on clean sources are
modest, and honesty about that is part of the design).

- Cache-producer-only encode lane: SVT-AV1 (behavioral probe for
  `libsvtav1` in the ffmpeg build), mid preset, N2's per-title quality
  target, `--film-grain` on titles whose N2 analysis flags grain.
- Served only to sessions whose client declared AV1 decode (dav1d-backed
  browsers and Android 14+ broadly; Apple only A17 Pro/M3+ — and the
  current Apple TV 4K has **no** AV1, so the Apple TV keeps HEVC/H.264;
  [caniuse](https://caniuse.com/av1), [Android 14 CDD
  §5.1/H-1-14](https://source.android.com/docs/compatibility/14/android-14-cdd)).
  Eligibility is a recipe input, so an AV1 entry and an H.264 entry for
  the same title coexist under different hashes.
- fMP4 segments (`av01` has no TS mapping); hls.js has handled AV1
  fMP4 since 1.5.0.
- Quality measurement trap, named so it isn't relearned: VMAF mis-scores
  synthesized grain — score against the pre-grain encode
  ([Netflix/vmaf #1192](https://github.com/Netflix/vmaf/issues/1192)).

**Effort:** HEVC large (review R8) — the encode is trivial; the typed
codec model, the create-API carry, fMP4 across live/cache/offline
paths, signaling, and the device matrix are the work. AV1 lane medium,
after N2 (it consumes the analysis) *and* after the container work —
prefix hydration must be container-aware before any HEVC/AV1 prefix
exists. **Risk:** medium — client-matrix risk, held by per-client
gating and by defaults staying off. **Toggles:**
`transcode.output_codec` (default `h264`), `cache.av1_lane` (default
off).

**Acceptance (nynuc + physical devices, per principle 4):** a
manifest/media validation command exists first and gates the rollout
(review R7) — shape: `scripts/perf2-hevc-validate --fixture
testdata/perf2/hevc-sdr --device-matrix
docs/fixtures/perf2-apple-devices.json` — checking fMP4 boxes,
`EXT-X-MAP`, `CODECS` derived-vs-produced agreement, and
profile/level, failing nonzero on any mismatch. On top of it: an HEVC
transcode session plays on Chrome, Safari, the Apple TV, and Android
with badges and stats truthful, at measured ≥30% byte reduction for
equal VMAF on the corpus; a declaring-client session serves the AV1
cache entry and a non-declaring client the H.264 one for the same
title (log-proven); a grain-flagged title's AV1 entry hits its VMAF
target (scored pre-grain) at ≤60% of the H.264 entry's bytes; producer
AV1 encode never runs while a live session waits (admission yield
exercised); the feature leaves default-off only after the full device
matrix passes.

---

## 9. N6 — Client-side enhancement (honest, per platform)

**Objective:** when the pipe, not the library, is the constraint — a
480p/720p rung on a 1080p+ screen — spend *client* GPU cycles to make
the rung look better. This is strictly additive polish: it changes no
bytes, no server state, and it must never cost smoothness (the N0 hitch
beacons are the referee).

- **Web (the real target):** a canvas pipeline over the existing
  `<video>` (same-origin, no DRM — the path is clean): CAS-style
  sharpening + deband-with-grain fragment shaders — WebGL2 baseline so
  it runs everywhere, WebGPU where present. These are cheap enough for
  any iGPU and they work on live-action, which is where NN upscalers
  honestly do not ([Anime4K's own docs](https://github.com/bloc97/Anime4K)
  scope it to anime). Anime4K-WebGPU
  ([MIT](https://github.com/Anime4KWebBoost/Anime4K-WebGPU)) ships as
  an *experimental* toggle for anime libraries — its published numbers
  are dGPU-only, so it earns default-off until benched on real
  laptops. Player-menu toggle, auto-suggested when rung height <
  viewport height; enhancement applies to transcode rungs (always SDR
  — convenient: the HDR question never arises), never to direct-play.
  Auto-disables itself if the hitch rate rises while on.
- **Apple:** one narrow, legitimate path exists as of the 26-cycle
  OSes — [`VTLowLatencySuperResolutionScaler`](https://developer.apple.com/documentation/videotoolbox/vtlowlatencysuperresolutionscalerconfiguration)
  (VideoToolbox, includes tvOS 26) via
  `AVPlayerItemVideoOutput` → Metal. Runtime `isSupported` gating,
  SDR sessions only, and the prior art is thin (unanswered forum
  threads on ABR resolution changes mid-stream). Ships dark behind a
  Labs toggle; device acceptance decides if it stays. MetalFX is not
  the answer (no tvOS); generic CoreML SR models are ~100–200×
  too slow for realtime.
- **Android/TV: deliberately nothing.** Mid-tier TV SoCs have no
  headroom, vendor SR is not app-accessible, the efficient path is
  zero-copy MediaCodec→Surface that a GPU effect would break, and the
  panel's own scaler already upscales everything. Documented as a
  non-goal so it isn't re-litigated; a sharpening effect on capable
  *phones* via Media3's effects pipeline is a someday-note, not a
  milestone.

**Effort:** web small-medium; Apple medium and gated on device tests.
**Risk:** low (web — worst case is a toggle that looks worse and gets
turned off) / medium (Apple — owns the render path). **Toggles:**
player-menu per-device (web, localStorage like the quality menu),
`Labs` flag (Apple).

**Acceptance:** web A/B screenshot pairs on a 480p rung (subjective
check in review), hitch-rate delta ≈ 0 while enhanced on a mid-tier
laptop, auto-off proven by forcing hitches; Apple path renders SDR
HLS through the scaler on one physical device ≥ one full film with
zero recovery-ladder trips, or the toggle stays dark.

---

## 10. N7 — Assistants: ops intelligence and content intelligence

**Objective:** the two places an *assistant* (statistical or LLM)
genuinely earns its keep here, both strictly adapter-shaped
(principle 2): explaining the QoE record, and enriching the library.
Everything in this section is a consumer of N0/N2 machinery — nothing
here touches the play path.

### 10.1 The QoE digest — analytics first, narration optional

A nightly `DueJob` over `playback_events`:

- **Stage 1, pure Rust, no dependencies:** per-metric baselines and
  deltas (TTFF p50/p95 by method, stall rate by title/client/kind,
  suspend time by reason, cache and prewarm hit rates, prior
  accuracy), top regressions vs the trailing window, rendered as a
  system-page card. This stage *is* the feature; it must be useful
  with no LLM configured.
- **Stage 2, optional narration/diagnosis:** an `llm` adapter —
  `none` (default) | `command` (any local CLI: llama.cpp-class 8B
  models run ~10 tok/s on these CPUs, adequate for summarize-and-
  triage) | `http` (any OpenAI/Anthropic-compatible endpoint; a
  Haiku-class daily digest costs cents). The adapter receives stage
  1's structured findings + sampled raw rows, returns prose + ranked
  hypotheses. Diagnosis quality degrades gracefully to stage 1's
  tables — the box never *needs* the narrator.

### 10.2 Subtitles on demand — the whisper queue

Optional `jobs.subtitle_gen_mins` sweep over files lacking subtitles
in wanted languages: `command`-adapter transcription
(whisper.cpp — Vulkan iGPU builds measured 3–4× realtime-factor gains
on exactly this class of chip
([Phoronix](https://www.phoronix.com/news/Whisper-cpp-1.8.3-12x-Perf))
— or faster-whisper), writing `.srt` sidecars the scanner already
knows how to pick up. Fully offline, attribution card + stop button
(principle 3), off by default. The ecosystem precedent (Bazarr/subgen)
says this is the single most-asked-for "AI" feature in self-hosted
media.

### 10.3 Intro/credits markers that are measured, not guessed

Today's credits marker is a heuristic when chapters don't say
(last `dur/12` clamped 45–150 s, `stream.rs:570-585`). The proven
recipe ([intro-skipper](https://github.com/intro-skipper/intro-skipper),
GPL-3 — *reimplement the approach, don't vendor the code*) is
chromaprint audio fingerprint cross-correlation across a season's
episodes for intros/recaps, plus `blackdetect`/`silencedetect` (both
in ffmpeg already) for credits boundaries. Runs in the analysis job
(N2's sweep, same yield rules), writes into `probe_json` beside
chapters, and `markers_for` prefers measured markers over the
heuristic. Chromaprint presence is a behavioral boot probe
(jellyfin-ffmpeg ships it; absence just disables the feature).

**Effort:** each of the three is small-medium and independent.
**Risk:** low — all off by default, none on the play path.
**Toggles:** `jobs.qoe_digest_mins`, `ops.llm_backend` (`none`
default), `jobs.subtitle_gen_mins`, marker detection rides
`jobs.media_analysis_mins`.

**Acceptance:** digest renders useful stage-1 output with zero
configuration beyond the interval; with a `command` LLM configured,
narration appears and with it removed everything still works;
whisper queue produces a playable-in-UI subtitle for a fixture file;
on a show with chaptered episodes, measured credits markers agree
with chapter ground truth within ±5 s on ≥80% of episodes (the shows
*with* chapters are the free validation set for the shows without).

---

## 11. Contracts — re-verify against the tree at build time

### 11.1 Settings keys introduced by this plan

All follow `store/mod.rs` idioms: jobs in minutes with `0 = off`
defaulting off; every key read through the `num_setting`/documented
toggle patterns; every key surfaced in `settings_dto` with its
effective default; hot-path reads snapshotted like `AheadLimits`
(TTL 2 s, `transcode.rs:2091-2099`).

| Key | Default | Milestone | Meaning of off |
|---|---|---|---|
| `telemetry.retain_days` | 30 (§14 D1) | N0 | no rows stored; ring/log behavior = today |
| `transcode.rate_mode` | `bitrate` | N1 | today's VBR exactly |
| `transcode.quality` | unset → family-tuned | N1 | n/a (used only in quality mode) |
| `jobs.media_analysis_mins` | 0 | N2 | no analysis, no markers detection |
| `jobs.deep_analysis_mins` | 0 | N2 | no probe-encode calibration |
| `transcode.per_title` | off | N2 | rungs stay pure height functions |
| `cache.prefix_secs` | 0 (§14 D3) | N3 | producer makes whole-title entries only |
| `jobs.prewarm_on_progress` | off | N3 | producer cadence = scheduled passes only |
| `playback.auto_abr` | off (§14 D2) | N4 | manual quality menu only, today's behavior |
| `playback.network_priors` | off | N4 | cold-start estimates, today's behavior |
| `transcode.output_codec` | `h264` | N5 | H.264 everywhere |
| `cache.av1_lane` | off | N5 | producer encodes H.264/HEVC only |
| `jobs.qoe_digest_mins` | 0 | N7 | no digest |
| `ops.llm_backend` | `none` | N7 | stage-1 analytics only |
| `jobs.subtitle_gen_mins` | 0 | N7 | no transcription |

### 11.2 New job variants

`DueJob::{AnalyzeMedia, PruneTelemetry, QoeDigest, SubtitleGen}` —
all in `ProduceCache`'s tier (last; they compete with playback,
`schedule.rs:97-99`), all with `jobs.last_*` stamps, all yielding to
live waiters via the existing admission flags, all rendering an
attribution card with a stop control (principle 3).

### 11.3 Capability surface

`EncoderCaps` gains per-family `quality_rc: bool` (N1), `hevc: bool`
(N5); ffmpeg behavioral probes (`OnceCell` pattern beside
`has_dovi_rpu`, `ffmpeg.rs:57-92`) for `siti`/`scdet` filters (N2),
`libsvtav1` (N5), `chromaprint` muxer (N7). All surfaced through
`SystemInfo` on `/api/v1/system` for the fleet census (§3.5).

### 11.4 Recipe identity (principle 5 — the complete list, review R4)

All changes flow through the single `effective_recipe()` builder
(§4.3); nothing edits a digest string at a call site.

- N1: `rate_control` moves from the manager-level `PipelineDigest`
  constant to a per-recipe field carrying the *effective* mode —
  legacy bytes preserved exactly for bitrate mode (`vbr:…`), distinct
  per effective QVBR value; golden-hash + cross-path fixtures pin it.
  Offline packages persist their effective recipe inputs at creation.
- N2: applied per-title bias/quality value, when `transcode.per_title`
  is on (absent = no field change, so old entries stay valid).
- N3: artifact `kind = full | prefix`, `covered_ms`, and the boundary
  manifest on location rows (schema **v18**; N0's telemetry table is
  **v17** — unique versions per slice, review R8, renumbered after the
  pre-existing offline-package v16 migration was discovered); `prefix_secs` in
  the recipe.
- N5: `OutputCodec` joins the effective recipe; the fMP4 muxer for
  HEVC changes the `muxer`/`segment_policy` fields — its own
  deliberate digest change, called out in its PR.

### 11.5 API deltas

`DecisionResponse`/`StartResponse` gain `prior_kbps: Option<u32>`
(N4); session create accepts `previous_session_id` + a typed
`reopen_reason`, with the resolved target persisted under `request_id`
before supersede (N4, review R5), and optionally the same `Caps`
object the decision request carries (N5, review R6); `SessionInfo`
gains `readrate` (N0); `POST /api/v1/client-log` accepts the existing
field set unchanged (N0 is storage-side). All additive; no client is
required to change to keep working.

---

## 12. Non-goals & guardrails

Carried forward from PERF-PLAN §9 unchanged: no LL-HLS ·
no simultaneous multi-rung live encodes (ADAPTIVE-QUALITY Phase 3
stays behind its decision gate) · no per-segment distributed encodes ·
no SIGSTOP as capacity release · no play-path probing · frontier
discipline · retention-window discipline.

New to this plan:

- **No reinforcement-learned ABR, no learned bandwidth model in v1.**
  The strongest field evidence (Puffer) says simple control wins and
  the ML worth having is a small in-situ predictor — which requires
  N0's data to exist first. Revisit only with stored evidence of the
  heuristic missing, and only as a local model.
- **No VMAF, analysis, or model inference on the play path — ever**
  (principle 7). Scan-time and idle jobs only.
- **No neural codecs, no LCEVC, no VVC, no AV2.** Respectively:
  research-grade with no client decoders; effectively proprietary
  (patent-excluded license); zero HLS client decode; spec-only.
  One line each so they aren't re-researched quarterly.
- **No JIT AV1.** No hardware encoder in the fleet; SVT-AV1 realtime
  budgets belong to the cache lane where preemption rules already
  exist.
- **No NN upscaling on Android/TV** (§9's reasons, recorded once).
- **No hard external dependency anywhere** (principle 2). If a feature
  cannot ship with `none` as its backend, it does not ship.
- **The clamp is the contract.** Per-title bias never exceeds its
  bounds; a wrong analysis may waste bits inside the clamp, never
  break playback outside it.
- **Every background worker yields to a viewer** via the existing
  admission machinery (`live_is_waiting`, offline flags) — a new job
  that doesn't participate will fight the producer *and* the viewer.
  Both existing flags are load-bearing precedent
  (`transcode.rs:2920-2935`, `admission.rs:288-290`).

---

## 13. Order of work, and what each slice buys

| Slice | Contents | Expected result |
|---|---|---|
| Weekend 1 | N0 (telemetry + census) | B18 answerable; distributions survive restart; Android exists in the data; fleet caps known |
| Weekend 2 | N1 harness (bench rate-control + VMAF + golden hashes), then N1 | measurement exists before the change it judges; 10–35% fewer bytes, boot-validated |
| Weekend 3 | N4 harness (playback-lab shaping) + N4.2 priors groundwork | the cliff is reproducible on demand; the priors table starts filling |
| Focused week | N4 (controller + normalized reopens + native way-down) | the cliff test passes; supply stalls act; Apple/Android stop dying at the same rung |
| Focused week | N2 (analysis job + per-title) | hard titles stop starving, easy titles stop wasting; markers groundwork in place |
| Spike | N3.0 (seam spike: ownership, timestamps, identity, atomic publish; §6.4 gates are the exit) | the continuation protocol is proven on fixtures before any schema ships |
| Focused week+ | N3 (prefix cache + prewarm + shaves) | prefix-hit TTFF in the cached class; next-episode ≤ 2 s; Apple master unblocked |
| Focused week | N5.1 (OutputCodec model + fMP4 + HEVC rung) | ~30%+ byte cut for declaring clients; the container abstraction lands |
| Background | N5.2 (AV1 lane, container-aware), N6 (enhancement), N7 (assistants) | grain dividend in the cache; low-rung polish; digests/subs/markers — each optional |

Dependencies that matter: N0 before everything that learns or is
judged (N2 calibration, N4 priors, all acceptance measurement). N1
*and* N2's per-title values before N3's mass prefix fill — both are
recipe-hash inputs, and recipes must be final or the cache is filled
twice (principle 5; the prefix *plumbing* can land earlier, the fill
waits). N2 before N5.2 (the AV1 lane consumes per-title targets and
grain flags), and N7's marker detection rides N2's sweep. Harness
slices precede the milestones they judge (review R7/R8): bench
rate-control before N1's flags, playback-lab shaping before N4's
controller, N3.0 + the §6.4 gates before N3's schema, and the
OutputCodec/fMP4 abstraction before N5.1 — and before any HEVC/AV1
*prefix* exists, since hydration must be container-aware first. The
rest is independent by construction; every slice leaves the tree
releasable.

---

## 14. Decisions for the operator — ratified implementation defaults

Paul ratified the recommended defaults on 2026-08-09. They remain runtime
settings and may be amended without changing the contracts below.

1. **D1 — telemetry default.** Recommended: **on**, 30-day retention.
   It is bounded megabytes, it is the referee for everything else,
   and principle 3 argues *for* the box keeping records of what it
   did. The strict jobs-default-off convention is for work that
   spends hardware; a bounded local event table is bookkeeping. If
   ratified off instead, every later acceptance check gains a "turn
   it on first" step.
2. **D2 — Auto ABR default once accepted.** Recommended: on for the
   web client after the acceptance week (it is the product's own
   stated goal in ADAPTIVE-QUALITY), off permanently for native
   clients until each has its own device acceptance.
3. **D3 — prefix length.** Recommended `cache.prefix_secs = 90`:
   long enough to cover spawn + gate + a slow NAS open with margin,
   short enough that a full rail of prefixes costs less than one
   whole-title entry. Tune with N0's prefix-hit/underrun data.
4. **D4 — HEVC `auto` default.** Recommended: stay `h264` for one
   release after N5 lands, then flip to `auto` once the badge/muxer
   acceptance has survived a fleet cycle. The efficiency is free;
   the compatibility matrix is the only risk and it is per-client
   gated.
5. **D5 — quality-mode targets.** N1 ships family-tuned defaults
   calibrated on the corpus (one number per family); N2's per-title
   targets supersede them where analysis exists. The corpus
   calibration run is part of N1 acceptance, so this decision is
   "accept the calibrated numbers," not "invent numbers."
6. **D6 — LLM backend.** `none` is the shipped default forever
   (principle 2). Recommendation beyond that is personal preference:
   the `command` adapter with a local 8B model is the
   no-strings option; the `http` adapter is better prose for cents a
   day. Nothing in the product will ever notice the difference.

---

*Review record:* this plan follows the PERF-PLAN review convention —
reviews land beside it as `PERF2-PLAN-REVIEW.md` →
`PERF2-REVIEW-RESPONSE.md`, decisions migrate into §14, and §1's
B-facts get corrected in place with dated notes rather than silently.
