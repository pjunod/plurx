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
| Rung parameter | `POST /files/:id/hls/sessions` body `height` (clamped 144–2160); omitted = Auto, resolved server-side | done |
| Height → bitrate ladder | `bitrate_for_height()` in `plurxd/src/transcode.rs` (2160→20 Mb/s, 1080→8, 720→4, 480→2, else 1.2) | done |
| Segment-aligned keyframes | `-force_key_frames expr:gte(t,n_forced*SEGMENT_SECONDS)` + `hls_time SEGMENT_SECONDS` in `hls_args` (2 s since PERF-PLAN §4.4) | done |
| Mid-stream session restart | the seek and audio-switch paths already call `hls/start?start=…` and re-attach via `attachHls()` | done |
| Never upscale | `video_filters()` refuses to scale above source height | done |
| Session lifecycle | idle reaper (60 s), first-segment watchdog, software self-heal | done |
| Client stream health | `hls.bandwidthEstimate`, `waiting` events, stall self-diagnosis | available |
| Bounded rung bitrate | `-maxrate` (1.5×) / `-bufsize` (2×) in `Encoder::encode_args` | done — **every** family, hardware included (PERF-PLAN §4.6) |
| Probe runs production args | `validation_args()` calls `encode_args()` | done — a driver that refuses the real rate control now fails at boot, not at play |

The bound is what makes a rung mean anything to an adaptation controller: a
"4 Mb/s" QSV encode that bursts to 12 Mb/s on a grain-heavy scene defeats the
estimate it is supposed to inform. It is a *window*, not a per-segment cap —
`bufsize` says how long a peak has to be paid back over — so the ladder's
advertised bandwidth must cover the measured peak over that window rather than
the nominal target.

The ladder API and its first web consumer are now in place: session creation
snaps explicit heights, decision/session responses return the source-filtered
rungs, and the player builds both its menu and Auto policy from those rows.

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

## Phase 1 — the ladder made real (revised 2026-07-28: the menu already exists)

The first version of this phase said "add a manual Quality menu." That
menu shipped 2026-07-23 (`Auto · Original · 1080p · 720p · 480p`,
localStorage persistence, forced decision modes, restart-at-position on
change) and review R12 rightly called the drift. Phase 1 is now only the
work that *remains*:

*Server*: Phase 1 is complete. The rate-control half shipped 2026-07-28 — every family now
carries `-maxrate` (1.5×) / `-bufsize` (2×), and the startup probe encodes
with the production argument set so a driver that accepts the encoder and
refuses its rate control is caught at boot rather than on a viewer's first
press of play ([PERF-PLAN.md](PERF-PLAN.md) §4.6). Auto also became a real
policy rather than a constant: `min(source, 1080)` on hardware, 720p on
software, decided server-side because only the server knows which encoder won
(§4.7). Session creation now snaps stray heights, and decision/session
responses return the source-filtered ladder with nominal and advertised-peak
kb/s. The advertised per-rung bandwidth covers the *measured* peak, not the
nominal target.

*Client*: complete. The player consumes the server ladder instead of a
hardcoded quality list, so 360p and any later server rung appear without a
second client edit. The menu, persistence, restart machinery, active rung, and
switch reason all use that one response.

Effort: **small** — one sitting, still. Risk: low; every mechanism is
already exercised elsewhere.

## Network-prior groundwork — Auto can remember a coarse network

The server-side half landed 2026-08-14 behind the default-off
`playback.network_priors` setting. N0 client telemetry maintains one node-local
row per user · fixed client class · IPv4 `/24`; no full address is stored or
returned. Each row carries a 25% sustained-throughput EWMA and the lowest rung
that reported a supply/network stall, with the time of that stall. An unused
row is pruned after 30 days, the starvation verdict inside it stops being
believed after 7 (below), and each user/client class keeps at most 64 network
rows.

The client class is derived from the request's own `User-Agent` and from
nothing else. That is a correctness requirement, not a style choice: the class
is part of the row's primary key, so a class the reporting path derives
differently from the consulting path writes priors that are maintained forever
and never read. Every path uses the one derivation.

`/decision` and session-create responses expose only
`prior_kbps: Option<u32>`. Without a matching prior, `auto_height` returns the
same encoder/source-capped height it returned before this feature. With one,
both signals apply and the lower rung wins: a known-starved client starts below
its lowest failed rung, and Auto then picks the highest rung whose advertised
peak fits the EWMA, including the existing cap on a proven-fast LAN. Rows stay
off Raft by design, so another cluster voter starts cold. The web controller
now seeds the first hls.js instance and picks its opening rung from this field;
without it, hls.js retains its existing cold default.

**A starvation verdict recovers.** It is the stronger signal — a known supply
failure beats an EWMA collected during healthy playback — but it is not
permanent. It stops binding 7 days after the stall that recorded it, and the
first observation past that horizon retires it from the row: a healthy sample
clears it, and a fresh stall replaces it outright rather than being combined
with a rung the link has since outgrown. Any starvation re-stamps the horizon,
so a link that genuinely still starves stays capped.

Without that horizon the verdict was a monotonic minimum nothing could raise.
One transient stall — a roommate's download, a brief Wi-Fi dropout — pinned
that user/class//24 one rung below its stall for as long as the row lived, and
because every observation refreshes `updated_at_ms`, an actively used row never
reached the 30-day retention either. The EWMA became dead code for that tuple
in both directions, so "a proven-fast LAN client starts at the cap" was
unreachable once a tuple had any starvation history. Applying both signals and
taking the lower rung restores the downward half; the horizon restores the
upward half.

## Phase 2 — Auto (the actual feature; controller revised per review R3)

**Implementation status (2026-08-14):** the web controller is implemented in
the pure playback-policy module and sampled by the embedded player every 5 s.
The constants below are the live defaults. The browser shaping run remains the
acceptance evidence for a host on which Chrome can start; unit tests do not
stand in for that run.

A client-side controller, roughly 120 lines, extracted as a pure function so
it unit-tests without a video element:

- **Sample** every 5 s: `hls.bandwidthEstimate` (hls.js's EWMA over real
  segment downloads), stalls (a `waiting` event after playback started, or
  hls.js `bufferStalledError`), buffer runway
  (`buffered.end − currentTime`), and — new — the server's `recent_speed`
  from the session status endpoint: on a JIT server the download estimate
  measures `min(link, encode)`, and a producer below 1× with shrinking
  runway is actionable *before* the client ever stalls.
- **Severe pressure** (an active supply stall, ≤1.5 s of runway, three
  supply stalls in 60 s, or an estimate below 0.7× the current rung) selects
  the highest rung whose `total_kbps` is ≤0.95× the estimate **in one move**.
  If no rung fits, the lowest rung is the only actionable choice. It never
  walks one rung at a time through a
  bandwidth cliff, which leaves the player above the sustainable rate for
  two full cooldowns (the review's 8 → 1.5 Mb/s example: 40 s of
  guaranteed stalling).
- **Mild pressure** (estimate below 1.3× the current rung's total bitrate
  for two consecutive samples) steps down one rung.
- **Cooldown** (20 s) governs *upgrades and mild steps only* — it exists
  to stop oscillation, and it must never block an emergency downgrade
  while the buffer is still draining.
- **Up** one rung when the estimate exceeds 1.8× the *next* rung's bitrate
  for 45 s with no stall in the last 60 s — and never above the player's
  actual backing-pixel height (the CSS layout height multiplied by the
  browser's device-pixel ratio; a 1080p encode into a 700-pixel target is
  waste). The cap governs starts and upgrades only: it never removes the
  ladder floor or prevents pressure from downgrading a session already above
  the cap.
- **Start** at the persisted last-good rung when one exists; a fresh browser
  with no network prior omits height and preserves the server's existing
  encoder-aware Auto choice. **Seed the replacement Hls instance's
  `bandwidthEstimate`** from the outgoing one — hls.js exposes the setter for
  exactly this, and without it every restart forgets everything and re-learns
  from the 500 kb/s default.

Asymmetric *selection* (down in one move, up one rung slowly) is the whole
trick of ABR; the constants are starting points to tune on real use.

The switch itself is the honest cost of the JIT model: a restart, not a
seamless splice — and the restart machinery **destroys the old stream
before the new one is ready** (`teardownHls` runs first), so the claim
that "the buffer covers it" is not true as built and this plan no longer
makes it. The interruption is a measured product property with an SLO
([PERF-PLAN.md](PERF-PLAN.md) §8.6, decision 4: p95 ≤ 2.5 s on LAN,
operator-confirmed): the loading overlay says "Adjusting quality…", the
toast names the move (`Quality → 480p — bandwidth`), and every switch is
logged to the Stats overlay with its reason, so "why did it get blurry"
always has an answer. A prepared-handoff variant (start the replacement,
switch at a boundary) is Option B in the review record — build it only if
the measured p95 misses the SLO.

Voluntary moves make that restart cost explicit. Over the 60 s dwell horizon,
a mild downgrade's estimated saved pressure is
`(1.3 × current_total / estimate − 1) × 60 s`; server-pressure samples use
`(1 / recent_speed − 1) × 60 s`; and an upgrade's estimated quality gain is
`(next_total − current_total) / current_total × 60 s`. The move happens only
when that value exceeds the 2.5 s restart-cost SLO. Emergencies are exempt,
because preserving a draining stream outranks avoiding the interruption.

Server prerequisites are Phase 1, the status endpoint, and the default-off
network-prior groundwork above. The web controller and its restart/recovery
contract are now wired; Phase 3 remains optional.

Effort: **medium** — one focused session including tests. Risk: low-medium;
the failure mode of a mistuned controller is a visible switch, not a broken
player, and Manual remains one menu tap away.

**Acceptance (from the review):** a scripted 8 → 1.5 Mb/s cliff reaches a
sustainable rung with at most one automatic restart; recovery causes at
most one upgrade per 60 s; switch-interruption p95 is measured and
documented; recreating hls.js demonstrably preserves the bandwidth
estimate.

## Phase 3 — seamless switching (optional, the majors' UX)

True multivariant HLS, adapted to JIT: `master.m3u8` advertises every rung
(`EXT-X-STREAM-INF` with `BANDWIDTH`/`RESOLUTION`); each variant's playlist
and segments live at `/hls/:session/:rung/…` and its encoder starts *lazily*
on first request. Because `-force_key_frames` already cuts every variant at
`t = n × SEGMENT_SECONDS`, segment N is the same time window in every rung; a
variant joining mid-timeline starts at that offset with `-start_number n` and
an `-output_ts_offset` for PTS continuity. Read the length from the constant —
it was 4 s, it is 2 s, and a hardcoded number here is a desync waiting for the
next tuning pass. hls.js's native ABR then does all
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
| 2 — Auto | Bandwidth-adaptive streaming (Plex-class), implemented | Medium | Low-med |
| 3 — Seamless | Blip-free switching (Netflix-class UX) | Large | High |

Recommended: build 1 + 2 together; hold 3 behind the decision gate.

## Test plan

Offline, `tests/playback/web-policy.test.js` drives the pure `decideRung`
policy through cliff, mild-pressure, supply/decode, dwell, restart-cost, and
recovery cases. End to end, the `stall-recovery` playback-lab suite applies an
8 → 1.5 Mb/s cliff and requires one restart to the sustainable 360p rung, then
no more than one upgrade per 60 s after the throttle lifts. On nynuc: play a
4K HDR title, clamp the client with browser DevTools network throttling to
3 Mb/s, and watch the Stats overlay move down to 480p and back.
