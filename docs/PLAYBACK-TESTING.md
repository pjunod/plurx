# Playback testing — turn playback failures into a reproducible matrix

Companion to [PLAYBACK.md](PLAYBACK.md) (how a file becomes a stream) and
[STUTTER-4K.md](STUTTER-4K.md) (the measured 4K investigation) — this is how
you exercise those paths automatically and read the result.

The playback lab generates a deterministic media corpus, boots a new isolated
plurx server, scans the corpus through the real library API, and drives the
shipped web player in a real browser. It does not touch your normal database or
media. One run produces a human summary, a detailed JSON report, and JUnit XML.

## The harness tests four layers, because "did it play?" is not a diagnosis

```text
 cases.json ──▶ ffmpeg corpus ──▶ isolated plurxd ──▶ shipped web player
      │                │                 │                    │
      │                ├─ ffprobe shape  ├─ decision          ├─ first frame
      │                └─ real codecs    ├─ remux/transcode   ├─ media clock
      │                                  └─ HLS/progressive   ├─ stalls/hitches
      └─ source × quality × operation                         └─ fallback/decode
```

The split is load-bearing. Pure Rust tests already prove the decision engine
and ffmpeg argument builders without a browser. The playback lab proves that
the resulting bytes make it through the selected transport and become frames.
When a case fails, its report keeps the API verdict, actual player method,
session health, browser capabilities, and server warnings together.

## Run it

Node 22+ and `ffmpeg`/`ffprobe` are lab prerequisites; they are not new plurxd
runtime dependencies. Chrome and Edge need no separate driver. Safari uses the
`safaridriver` included with macOS. Firefox needs `geckodriver` on `PATH` (or
passed with `--geckodriver`); it adds no package to plurx or the harness.

```bash
make playback-doctor       # check codecs, filters, browser, and the debug server
make playback-fixtures     # build + ffprobe the corpus; cached under target/
make playback-smoke        # 11 risk-weighted Chrome cases, about 2–4 minutes
make playback-full         # 44 Chrome cases: every fixture × quality + restarts

# Fault injection: drive a session through a bandwidth cliff (see below).
scripts/playback-lab run --suite stall-recovery \
  --network-profile 8mbps-to-1.5mbps --json out/stall-recovery.json

# Safari: first enable Develop → Allow Remote Automation in Safari.
# macOS may also require you to run `safaridriver --enable` once yourself.
scripts/playback-lab doctor --browser safari
make playback-smoke-safari

# Optional engine coverage. Missing browsers produce a clear diagnostic.
scripts/playback-lab doctor --browser edge
make playback-smoke-edge
scripts/playback-lab doctor --browser firefox
make playback-smoke-firefox

# Tight loop while fixing one path. Matching is case-insensitive substring.
scripts/playback-lab run --suite smoke --case "2160 :: original" --observe 8
```

The harness binds a loopback port and launches a browser. Sandboxed agent or CI
environments must explicitly allow those two local actions. It makes no network
request outside the machine. Each case begins on a freshly loaded player page;
a failed native-HLS or transcode session therefore cannot poison later results.

## The matrix is broad on purpose, but not a blind Cartesian product

[`tests/playback/cases.json`](../tests/playback/cases.json) is the reviewable
test contract. The smoke suite covers each verdict, the three costly 4K
transport choices, and every restart class. The full suite adds the complete
source × quality product, then targeted operations where they are meaningful.

| Axis | Values exercised |
|---|---|
| Verdict | direct play · remux · transcode |
| Transport | native file/range · progressive fMP4 · copy-video HLS · transcode HLS |
| Video | H.264 · HEVC Main 10 HDR · VP9 · unsupported MPEG-4 Part 2 |
| Audio | AAC · AC-3 · Opus · MP3 · multi-track switch |
| Container | MP4 · MKV · WebM · AVI |
| Resolution | 720p · 1080p · 2160p |
| Quality | Auto · Original · Original one-stream · 1080p · 720p · 480p |
| Operation | cold play · seek/restart · audio switch · text-subtitle toggle |
| Browser state | reported codecs · HDR display · MSE · native HLS |

Running every operation against every source adds minutes without adding a new
path: toggling a subtitle on a file with no subtitle cannot teach us anything.
The manifest therefore uses a full product for source × quality and explicit
cases for applicable operations. Add a case when a failure introduces a new
branch; do not multiply the suite merely because another axis exists.

## The corpus makes each intended branch true

| Fixture | Why it exists |
|---|---|
| `direct-h264-aac-1080.mp4` | Browser baseline: H.264 + AAC + MP4 should direct-play. |
| `remux-h264-multitrack-1080.mkv` | Container remux with two audio codecs and a text subtitle; owns seek/audio/subtitle restarts. |
| `transcode-mpeg4-mp3-720.avi` | Unsupported video codec forces a real video transcode. |
| `direct-vp9-opus-720.webm` | Non-MP4 direct path where this browser reports VP9 + Opus. |
| `hevc-hdr-open-gop-2160.mkv` | 3840×2160 · 10-bit PQ/BT.2020 · open GOP · approximately 48 Mb/s, preserving the high-bitrate copy/segmenter stress case. |
| `shaping-mpeg4-mp3-720.avi` | Opt-in, `stall-recovery` only: the same forced-transcode shape as the AVI above, but 120 s long so a cliff at 12 s still leaves a full recovery window. |

Generation is cached. Every invocation still runs `ffprobe` and rejects a file
whose codec, streams, dimensions, HDR tags, duration, or critical bitrate no
longer match. That guard prevents a fast-compressing synthetic 4K clip from
quietly turning a high-bitrate buffer/segmenter case into an ordinary one.

The 4K file is about 108 MB; the whole corpus is about 195 MB under
`target/playback-lab/fixtures/`, which Git ignores. `--rebuild` regenerates it.

## A pass means continuous presentation, not just an HTTP 200

After the first presented frame, the default observation window is 8 seconds.
A case fails on any of these:

- no first frame within 30 seconds;
- an `HTMLMediaElement` error;
- media-clock progress below 0.90× wall time;
- more than two visible hitches in the window;
- any supply or decode stall;
- a direct/remux path rejected by the browser and rescued by fallback;
- a quality rung decoding above its requested height;
- `Original · one stream` accidentally using copy-HLS.

**How to read it:** a fallback is a failure even when the rescue transcode
plays. The viewer got pixels, but the requested path broke — exactly the Safari
failure that once turned a 4K remux into a 720p transcode. TTFF, runway, dropped
frames, `MediaCapabilities`, encoder, and server-ahead health are recorded but
not all are hard gates yet; promote a number to a gate only after it is stable
across the target hardware.

## Network shaping — a bandwidth cliff you can reproduce and timestamp

The `stall-recovery` suite is fault injection, not a correctness matrix. It
holds a link at one rate until the session has proven itself, drops it to a
lower rate at a recorded moment, and then records what the session did about
it. It is the harness [PERF2-PLAN.md §7](PERF2-PLAN.md) requires *before* the
N4 Auto controller exists, so that the controller's acceptance can be measured
rather than asserted.

```bash
scripts/playback-lab run --suite stall-recovery \
  --network-profile 8mbps-to-1.5mbps --json out/stall-recovery.json
```

**How the shaping works.** A loopback reverse proxy sits between the browser
and the isolated server, metering every byte the browser pulls through one
shared token bucket. This process keeps talking to the server directly, so the
harness's own polling never competes with the shaped budget. Three properties
follow, and each is the reason it is not an OS-level shaper:

- it needs no root, no `pfctl`/`tc`, and no kernel state that could outlive a
  failed run or affect anything else on the machine;
- the cliff is a function call, so the report can timestamp it exactly rather
  than infer it; and
- the bucket is shared across connections, so the cliff constrains *the link*
  rather than whichever request happened to be open.

Pre-cliff credit is clamped when the rate drops. Without that, a bucket filled
at 8 Mb/s would fund several seconds of post-cliff burst and blur the very
edge the suite exists to observe.

The evidence window starts only after the first presented frame. Stage-zero
byte and time counters reset there. Each stage's whole-stage rate extends the
first-to-last sample interval by the first delivered slice's cap-time, because
the interval contains one fewer reservation wait than its byte count. That
keeps browser preparation and player idle out without biasing a short stage
high by `N/(N-1)`. A one-second rolling peak accompanies the whole-stage rate
so later slow delivery cannot dilute a qualifying mid-stage burst. Its
sensitivity is explicit: some window must carry more than
`1.25 × cap × 1 s` of bytes to fail the gate. Shorter or smaller bursts can
stay below that bound; the whole-stage rate and the post-cliff cap remain the
other independent checks. The bucket's deliberate 250 ms credit consumes most
of the 1.25 tolerance; slice quantization consumes the remainder. Scheduler
delay lengthens the observed window and lowers the measured peak instead of
spending additional headroom. The whole-stage estimator does not spend the
tolerance on first-slice bias.
Only token reservations share the global scheduler; downstream socket drain
remains per-connection, so one non-reading response cannot stop the rest of the
shaped link. The proxy freezes one telemetry snapshot when the observation ends
and reuses it at both result and report level. Browser-side request cancellation
during a rendition restart closes the matching upstream request, refunds an
undelivered reservation, and is not a link error.

**The profile grammar** is `<high>-to-<low>[@<seconds>]`, e.g.
`8mbps-to-1.5mbps` or `8mbps-to-1.5mbps@20`. The descent is mandatory: a flat
or rising "cliff" is refused, because accepting one would let a green
stall-recovery run mean nothing at all. The cliff defaults to 12 s after the
first presented frame.

**Why 8 → 1.5 Mb/s.** It is the ladder's own geometry. The 1080 rung costs
about 8.16 Mb/s and the 720 rung about 4.16 Mb/s, so 8 Mb/s comfortably feeds
the rung the corpus source starts on; 1.5 Mb/s leaves only the 360 rung
(≈1.36 Mb/s) sustainable. The cliff therefore demands a real way down rather
than a cosmetic one.

**What a failure means.** The checks run in a fixed order, because that order
is the diagnosis. An unapplied or leaked cliff invalidates everything after
it; a baseline that was never healthy makes a post-cliff failure meaningless.
Only when the link and the baseline are both trustworthy does the verdict
describe adaptation. Every artifact therefore carries an `outcome`:

| `outcome` | What it says |
|---|---|
| `passed` | The shaped link and baseline were trustworthy, and the session met every recovery criterion. |
| `shaping` | The cliff never applied, the proxy reported a transport error, or the shaper delivered more than its cap. The run proves nothing about playback. |
| `browser_playback` | The baseline was unhealthy, or the player reported a media failure. There is no trustworthy recovery verdict. |
| `server_supply` | The link still had headroom and the player starved anyway — a producer problem, not an adaptation problem. |
| `recovery` | The link was shaped, the baseline was healthy, and the session did not answer the cliff within its criteria. |
| `harness` | The observation was too short to outlast banked runway, or the run itself failed before it could produce a playback verdict. |

The criteria live in `tests/playback/cases.json` beside the case, not in the
script: restarts, upgrades per 60 s, the recovery deadline, and the sustained
window are review material. The player gives every attempt a globally
monotonic identity, while its raw stall counter carries across in-place
`newAttempt()` changes and resets only when `play()` creates a new player
object. The harness samples both identities. Attempt transitions therefore
count same-reason restarts directly; a player-object transition records a
`counter_rebase` and rebases the raw counter before deciding whether a window
was stall-free. The current `stall-recovery` case restarts the live object, so
its counter remains exact at any poll rate. A future case that can replace one
or more player objects between samples needs a player-owned monotonic total
before it can claim the same evidence; intermediate counters would otherwise
be unobservable.

**What it deliberately does not do.** It injects no probe onto the play path,
changes no rate control, and does not steer the player. Choosing a rung in
response to the cliff is the N4 controller's job; this harness only creates the
condition and records the answer. Until that controller lands, a
`stall-recovery` run is *expected* to fail with `outcome: recovery` — that
recorded failure is the baseline the controller has to move, which is why the
suite is not part of `make validate`.

**Comparing runs.** `scripts/playback-lab normalize --json <artifact>` reduces
a report to its behavioral shape with UUIDs, ports, wall-clock, temporary
paths, and durations removed. Run the same unshaped smoke suite against the
base commit and the candidate on the same machine, then compare their traces.
Equality is the acceptance evidence that the inactive harness did not alter
existing playback behavior; unit tests or two candidate-only runs do not prove
that claim:

```bash
# In a full clone at the base commit:
scripts/playback-lab run --suite smoke --json out/base.json
scripts/playback-lab normalize --json out/base.json --out out/base.trace.json

# In a separate full clone at the candidate commit, on the same machine:
scripts/playback-lab run --suite smoke --json out/head.json
scripts/playback-lab normalize --json out/head.json --out out/head.trace.json
diff -u out/base.trace.json out/head.trace.json
```

The shaping fixture is opt-in: it is built only for the suite that plays it,
and it is excluded from `full`'s source × quality product, so the existing
suites keep exactly their previous cases and corpus.

## Reports keep enough evidence to reproduce the failing layer

Reports land in `target/playback-lab/reports/`:

- `*.json` contains the corpus probes · browser capabilities · server build and
  ffmpeg · full decision and player snapshots · case-local server logs;
- `*.xml` is JUnit, ready for CI artifact and test-result ingestion.

The console is the shortest reading path:

```text
PASS  remux -> remux / copy-HLS · TTFF 563 ms · clock 0.974x · hitches 0 · stalls 0
FAIL  timed out waiting for first presented frame
SERVER WARN plurxd::transcode: transcode ffmpeg: No such filter: 'zscale'
```

The first line proves the expensive 4K video stayed on the copy path. The next
two isolate a missing first frame to the server's tone-map command rather than
to browser decode. Use the JSON when the one-line cause is not enough.

## Browser and device coverage is a pool, not one pretend-universal browser

**Chrome and Edge:** these are separate explicit targets, both driven through
the browser's built-in DevTools protocol. They run headless unless `--headed`
is passed and need no downloaded driver. Chrome never silently substitutes
Edge, or vice versa, so a green browser name means that browser actually ran.

**Safari:** `--browser safari` uses macOS's built-in WebDriver and performs a
real element click to satisfy autoplay policy. It is headed because Safari has
no production-equivalent headless mode. Run this on the Mac whose HEVC/HDR
pipeline you care about.

**Firefox:** `--browser firefox` uses Mozilla's small `geckodriver` bridge and
runs headless unless `--headed` is passed. Like Safari, it starts playback with
a real element click instead of weakening the browser's autoplay policy. This
target is optional: if Firefox or `geckodriver` is absent, only the Firefox run
is unavailable; the required Chrome and Safari commands are unaffected.

**Native Apple/Android clients:** not covered by this first slice. Their player
engines and passthrough rules are not the web player's, so pretending a browser
test covers them would be worse than an honest gap. Reuse the same corpus and
result schema when device adapters land; keep the browser-reported capability
block as a device-reported capability block.

Container Chromium without proprietary decoders remains useful as a negative
control, not as 4K proof. [STUTTER-4K.md](STUTTER-4K.md#6-the-harnesses) records
the scar: that browser cannot validate H.264, HEVC, or AAC.

## Native Apple exits — prove teardown on the device that owns the chrome

The simulator suite pins end classification, dismiss-vs-autoplay routing,
idempotent teardown order, and completion delivery. Run both Apple targets
before a source change lands:

```bash
xcodebuild test -project clients/apple/plurx.xcodeproj -scheme plurx-iOS \
  -destination 'platform=iOS Simulator,name=iPad Pro 13-inch (M5)'
xcodebuild test -project clients/apple/plurx.xcodeproj -scheme plurx-tvOS \
  -destination 'platform=tvOS Simulator,name=Apple TV 4K (3rd generation)'
```

The simulator cannot prove system chrome, PiP, audio-session handoff, or tvOS
focus. Build 37 therefore keeps this physical matrix as a release gate:

| Scenario | Expected result | Evidence to record |
|---|---|---|
| Known duration · autoplay off | Natural end restores iOS chrome, releases the item/session, and dismisses once | End timestamp · detail screen visible · no player cover |
| Unknown catalog duration · finite direct item | The item duration corroborates the end and dismissal follows the same path | Catalog duration is NULL · item duration · dismissal |
| Unknown catalog and item duration · growing HLS | A temporary end reopens at the same film position; it never silently ejects the viewer | `knownDurationMs` · item duration · reopen position |
| Autoplay on · successor exists | Episode N tears down once, keeps the audio session active, and episode N+1 attaches without another app sounding between them | Audio route log · successor first frame |
| Autoplay on · no successor | The cover dismisses after teardown; “Up next” does not remain | Next lookup result · dismissal |
| Manual Close/Menu and failure Close | Every exit restores chrome and releases observers, timers, PiP, overlays, player item, and HLS resources before dismissal | One stop callback · no live session |
| PiP active at natural end | PiP stops and stays detached; SwiftUI does not construct a replacement controller during the dismissal turn | `attach`/`detach` breakpoint order |
| tvOS controls hidden at natural end | The cover keeps a focusable sink until dismissal; the console has no “no focusable views” diagnostic | Focus log · DetailView focus target |

Run the iPad cases in full screen and Split View, and the iPhone cases in
portrait and landscape. A green simulator result is source evidence; this
table is the platform evidence the release claim depends on.

## Add a regression without turning the suite into a junk drawer

1. Reproduce the failure on a corpus fixture. If no fixture has the relevant
   codec/container/GOP/subtitle shape, add one builder in
   [`scripts/playback-lab`](../scripts/playback-lab) and its exact probe
   contract beside the existing builders.
2. Add the smallest case to `smoke.cases` that would have failed before the
   fix. Add broader combinations to `full` only when they exercise distinct
   delivery or restart behavior.
3. Run the case by substring, then the smoke suite. Keep its JSON report with
   the investigation until the failure is understood.
4. Run `make check`; the browser suite supplements the Rust gate and does not
   replace it.

Real commercial-media clips should stay outside Git. A sanitized or synthetic
reproducer belongs in the corpus only when redistribution is safe and its
probe contract is stable.

## Non-goals — what this first harness deliberately does not claim

- **No exhaustive codec-profile proof.** H.264 and HEVC each contain more
  profiles, levels, reference-frame patterns, and vendor quirks than a small
  generated corpus can represent.
- **No WAN or congested-Wi-Fi *fidelity*.** The shaping layer above injects a
  bandwidth cliff, and that is all it claims: one shared rate limit, changed at
  a known moment. It models no latency, jitter, loss, reordering, competing
  traffic, or radio behavior, so it can prove that a session answers a
  bandwidth collapse — never that it behaves like a particular real network.
- **No visual-quality oracle yet.** It detects presentation failure, cadence,
  size, and fallback; it does not score banding, color accuracy, or subtitle
  placement from screenshots.
- **No live-library mutation.** Every run owns a new database and scratch
  directory, because a QC tool must not alter watch state or caches in the
  server you actually use.
