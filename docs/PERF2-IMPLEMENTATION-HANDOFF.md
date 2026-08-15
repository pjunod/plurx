# Performance II implementation handoff — how to build it without breaking it

**Status:** ready to execute · **Executes:**
[PERF2-PLAN.md](PERF2-PLAN.md) v2 (`5b9a04ec`), as revised for
[PERF2-PLAN-REVIEW.md](PERF2-PLAN-REVIEW.md) per
[PERF2-REVIEW-RESPONSE.md](PERF2-REVIEW-RESPONSE.md) · **Written:**
2026-08-09 against `main` @ `e8a910f` · **Audience:** the implementing
agent.

Read, in order: PERF2-PLAN.md (the design — §2 principles and §12
guardrails bind every line you write), then the review and response (they
are the record of what v1 got wrong; do not re-introduce it), then this
document (how to execute in *this* repo). The plan owns the *what and
why*; this handoff owns the *how and in what order*, plus a full build
spec for N0, the first slice. File:line references were verified against
`e8a910f` — re-verify at build time; the tree moves under multiple
agents.

**The standing instruction:** work milestone by milestone in the §4/§5
order. If a step seems to require changing a contract in PERF2-PLAN §11,
violating a guardrail in §12, touching another agent's branch, or making
a schema/digest decision the plan left explicit — stop and flag it in
the PR instead of improvising. A flagged blocker is cheap; an improvised
contract is a review cycle.

---

## 1. Ground rules of this repo

Break one of these and the work will bounce in review regardless of how
good the feature is.

- **The gate is `make check`** = `cargo fmt --all --check` + `cargo
  clippy --workspace --all-targets -- -D warnings` + `cargo test
  --workspace` + `validation-lint` + `history-check` +
  `operations-check`. Run it before every push. Use `--no-fail-fast` on
  test runs or plurx-core failures hide behind plurxd ones; `plurxd` is
  a **bin** — `cargo test -p plurxd --bin plurxd`, not `--lib`.
- **Toolchain is pinned**: `rust-toolchain.toml` says 1.97.1 and CI uses
  exactly that. If your local cargo resolves differently (`which -a
  cargo` — a Homebrew cargo shadowing rustup has bitten this repo), lint
  on the pinned one or CI will disagree with you.
- **Branch discipline.** Multiple agents share this repo. Work on your
  own branch off `main` (`agent/perf2-n0`, `agent/perf2-n1`, …), one
  milestone per branch, PR into `main`. Never commit to, rebase, or
  check out another agent's branch. Check `git branch --show-current`
  before assuming anything about the worktree.
- **Docs change in the same commit as behavior** (PERF-PLAN §8.7 rule).
  Every milestone lists its doc touches below; a PR that changes what
  the system does without updating CHANGELOG.md `[Unreleased]`,
  `docs/STATUS.html`'s Performance row, and the named docs is
  incomplete. OPERATIONS.md gets a row for every new env var or
  operator-visible behavior; the settings table in PERF2-PLAN §11.1 is
  the key inventory.
- **Mobile versioning gate.** Any change under `clients/apple/**` or
  `clients/android/**` must bump the build number
  (`validation/mobile_versions.py` compares the whole diff range against
  base — one bump per PR suffices; bumping `MARKETING_VERSION` alone
  **fails** the gate). Server-only changes bump nothing.
- **Governed files.** New scripts or contract-bearing files may need
  registration in `validation/points.toml` — `make validation-lint`
  fails loudly if so; register, don't work around.
- **The web UI is `include_str!`-embedded.** A JS syntax error in
  `crates/plurxd/src/web/index.html` compiles fine and breaks the whole
  UI at runtime. After any edit: `scripts/js-check` (or extract the
  `<script>` and `node --check` it). UI changes need rebuild + restart
  to see.
- **macOS/Linux test drift.** On macOS `TMPDIR` is a symlink; tests
  comparing `tempfile` paths to canonicalized paths pass on Linux CI and
  fail locally (or vice versa). Reproduce CI-side behavior with
  `mkdir /tmp/realtmp && ln -sfn /tmp/realtmp /tmp/linktmp &&
  TMPDIR=/tmp/linktmp cargo test --workspace` before blaming the test.
- **Settings idioms** (`crates/plurx-core/src/store/mod.rs:46-206`):
  dotted lowercase keys owned by the writing module; jobs are
  `jobs.*_mins`, `0 = off`, off by default; numeric reads via
  `num_setting` with a compiled `_DEFAULT` beside the consumer; validate
  writes in `system::update_settings`; read back *effective* values in
  `settings_dto`; snapshot hot-path reads (the `AheadLimits` 2 s TTL
  pattern, `transcode.rs:2091-2099`).
- **Attribution is a requirement, not polish** (plan principle 3).
  Anything spending GPU/disk/hours renders a card in the UI saying what
  it is doing and why, with a stop control — the producer's
  `ProducingNow` + Stop (`state.rs:1562`, `http/system.rs:1600`) is the
  template. This is an explicit operator requirement of this project.
- **Background work yields to viewers.** New jobs run at
  `Priority::Background` and participate in the existing yield flags
  (`admission.rs:288-290` live-waiter refusal; `offline_waiting` +
  `background_producer` try-lock, `transcode.rs:2570-2576,2920-2935`).
  A job that doesn't participate fights the producer *and* the viewer.
- **Third-party claims get probes, not versions** (PERF-PLAN §9). New
  ffmpeg capabilities are detected behaviorally — the `OnceCell` +
  parse-the-help/list pattern beside `has_dovi_rpu`
  (`ffmpeg.rs:57-92,115-157`). Never gate on a version string.
- **Expect adversarial review.** Every PR in this repo gets one. Cite
  file:line for claims in the PR body, name what you measured and on
  what machine, and never claim an acceptance criterion you didn't run.
  Timestamp/muxer claims need a produced stream measured with ffprobe —
  argument-list assertions have shipped two green-for-weeks defects here.

---

## 2. Decisions in force

Paul ratified PERF2-PLAN §14's recommended runtime defaults on 2026-08-09 and
the N1 peak/storage contracts on 2026-08-12, and the N4.2 storage contract on
2026-08-14. D1–D6 remain runtime settings; D7–D9 are binding implementation
contracts. In force for implementation:

| Decision | Implement as |
|---|---|
| D1 telemetry default | `telemetry.retain_days` default **30** (on) |
| D2 auto ABR default | ship **off**; flipping on waits for the acceptance week |
| D3 prefix length | key default **0** (off); 90 s is the recommended value to *set*, not the shipped default |
| D4 HEVC auto | ship `transcode.output_codec = h264`; `auto` is opt-in |
| D5 quality targets | family-tuned defaults calibrated during N1 acceptance |
| D6 LLM backend | `none`, forever the default |
| D7 N1 peak gate | each mode's exact 10 s complete-served-segment peak ≤ unchanged advertised peak; derived bufsize-window and theoretical VBV remain diagnostics |
| D8 N1 offline identity | one non-null `vbr` / `qvbr:<q>` column; SQLite v18, replicated v5/protocol v4 fresh bootstrap/import, existing replicated v4 refused; its N3 SQLite v19 assignment was superseded by D9 |
| D9 N4.2 prior storage | node-local SQLite v19; N3 SQLite v20; replicated schema v5/protocol v4 unchanged; another voter starts cold |

If the operator changes D1–D6 mid-build, that lands as a settings-default
commit with a CHANGELOG line. Changing D7, D8, or D9 is a new contract decision
and must follow the stop-and-flag rule.

---

## 3. The execution loop — every milestone runs the same protocol

1. **Read the plan section** for the milestone (and its review finding,
   if one names it). The plan is the spec; this handoff never overrides
   it.
2. **Build the harness slice first** where the plan names one (N1 bench
   mode, N4 shaping, N3.0 spike, N5.1 validator). The harness merges
   before or with the feature — never after (review R7).
3. **Contracts**: every new key/field/endpoint matches PERF2-PLAN §11
   exactly; recipe-affecting values go through `effective_recipe()`
   (§11.4) and get hash fixtures in the same PR.
4. **Tests locally**, gate green, **then the acceptance run on the
   machine the plan names** — nynuc for encoder/pacing behavior,
   physical devices for client behavior. CI asserts selection from
   logs; pixels and timing are proven on real hardware (plan
   principle 4). Record numbers in the PR body.
5. **PR into `main`**: one milestone (or named sub-slice) per PR, plan-
   section reference in the title (`perf2 N0: telemetry becomes data`),
   body lists: what changed, contracts touched, acceptance evidence,
   docs updated, and any stop-and-flag items. Wait for review; answer
   findings with evidence, not assertion.
6. **After merge**: deploy nynuc first, run the milestone's acceptance
   there, then the fleet (`scripts/ship` wraps the last mile;
   the mobile deploy path is documented in
   [PUBLISHING.md](PUBLISHING.md)). Client milestones ride the normal
   TestFlight/sideload cadence.
7. **Update** `docs/STATUS.html`'s Performance row (milestone state) and
   the plan's §1 B-facts if your work changed a measured number — dated,
   in place.

---

## 4. N0 build spec — telemetry becomes data

Everything below implements PERF2-PLAN §3 exactly; read that first.
This is the full-detail slice because it is first, unblocked by any
§14 decision beyond D1, and everything later measures itself against
it. Scope guard: N0 changes **no** playback policy — no flow-control
tuning, no rung logic, no priors. It records.

### 4.1 Migration v17 — `playback_events`

Append to `MIGRATIONS` in `crates/plurx-core/src/store/sqlite/mod.rs`
(style per v15: STRICT, commented rationale; the array-length test at
`mod.rs:1114-1118` forces a deliberate bump):

```sql
-- v17: playback telemetry becomes rows (PERF2-PLAN §3). Node-local
-- operational data — never a replicated class; the hiqlite backend
-- keeps it beside its derived-local tables (items_fts precedent).
CREATE TABLE playback_events (
    id             INTEGER PRIMARY KEY,
    at_unix_ms     INTEGER NOT NULL,
    user_id        INTEGER,          -- NULL for server-emitted events
    session_id     TEXT,
    file_id        INTEGER,          -- no FK: events outlive files
    event          TEXT NOT NULL,    -- ttff | stall | rate_chase | ...
    level          TEXT,
    method         TEXT,             -- direct_play | remux | transcode
    encoder        TEXT,
    height         INTEGER,
    ms             INTEGER,          -- ttff/stall duration
    runway_ds      INTEGER,          -- client runway, deciseconds
    bandwidth_kbps INTEGER,          -- client estimate
    speed_recent   REAL,             -- server join: recent_speed
    ahead_seconds  INTEGER,          -- server join
    suspended      INTEGER,          -- server join: 0/1
    hold_reason    TEXT,             -- server join: time|bytes|global
    delivered_bps  INTEGER,          -- server join
    readrate        REAL,             -- server join: effective input pace
    detail         TEXT,
    attempt        TEXT,
    reason         TEXT,
    ua             TEXT,
    extra          TEXT              -- JSON overflow, sparingly
) STRICT;
CREATE INDEX playback_events_by_event ON playback_events(event, at_unix_ms);
CREATE INDEX playback_events_by_file  ON playback_events(file_id, at_unix_ms);
```

No FK to `files`/`users` on purpose: telemetry must survive library
churn, and pruning is age-based, not cascade-based. Exact column
bikeshed is yours; the *join columns* (speed/ahead/suspended/
hold_reason/delivered_bps) are the point of the table (plan §3.1) and
are not negotiable.

### 4.2 Setting + retention

- `keys::TELEMETRY_RETAIN_DAYS = "telemetry.retain_days"` with a doc
  comment stating: default 30 (§14 D1 — the one deliberate exception to
  jobs-off-by-default, argued in the plan: bounded local bookkeeping,
  and the referee for every later milestone), `0` = feature off — no
  rows written, no pruning, ingest behaves exactly like today.
- Pruning: `DueJob::PruneTelemetry`, driven by the same key (no
  separate `jobs.*_mins` — retain_days > 0 *is* the arming), scheduled
  in the trailing tier (`schedule.rs:97-99`), deleting by `at_unix_ms`
  age in bounded batches. Stamp `jobs.last_telemetry_prune`.
- Validate the key in `system::update_settings` (integer ≥ 0); surface
  the effective value in `settings_dto`; add a row to the Settings page
  near the Cache block, `0` rendered as "Off".

### 4.3 Store surface — node-local, but contract-covered

- New `Store` methods (names indicative): `record_playback_event(ev)`,
  `prune_playback_events(before_ms, limit) -> u64`,
  `playback_events(filter) -> Vec<PlaybackEvent>`.
- **Class: node-local operational data.** The hiqlite backend stores it
  the way it stores derived-local FTS (`hiqlite_catalog.rs:1-3` — the
  precedent that replicated and local tables coexist in that backend);
  telemetry rows never travel through raft.
- The backend-parity harness (`plurx-core/tests/store_contract.rs`, the
  118-method contract from clustering M1a) must cover the new methods —
  both backends, same observable behavior. **Stop-and-flag** if the
  harness's structure fights node-local semantics rather than bending
  to it; that is a contract conversation, not a workaround.

### 4.4 Ingest — keep the string, add the row

`client_log` (`crates/plurxd/src/http/system.rs:441-462`):

- `ClientLog` gains `session_id: Option<String>` — the struct is
  `#[serde(default)]` so old clients keep working unchanged.
- The log-line path stays byte-identical (it is the human surface and
  `scripts/perf-report`'s fallback parser).
- After the line: build the row; when `session_id` names a live
  session, snapshot the server's view into the join columns —
  `session_info()` already assembles every needed field
  (`transcode.rs:1299-1345`). Insert is fire-and-forget (spawned, never
  blocking the 204).
- **Rate limits become per-user**: replace the global 30/min bucket
  (`system.rs:396`) with per-user token buckets (240/min each) plus a
  global ceiling (1000/min) as abuse insurance. Drops still count and
  still print `(+N suppressed)`.
- When `telemetry.retain_days = 0`: skip the insert, keep everything
  else — today's behavior exactly.

### 4.5 Server-emitted events — the sessions' own diary

Same table, `user_id NULL`. Emit points (each a few lines at an
existing seam):

| Event | Where | Payload notes |
|---|---|---|
| `session_start` | end of `start_with_audio_offset` / copy start / `serve_cached` | method, encoder, height, `cache=hit\|miss\|prefix-later` |
| `session_end` | `stop_session` (`transcode.rs:4386`) + reaper kill (`:4740`) | reason: idle / superseded / client_released / killed / failed |
| `suspend` / `resume` | `apply_ahead_window` (`transcode.rs:4601-4638`) | hold_reason, ahead_seconds; on resume: held-duration in `ms` |
| `playlist_slide` | first prune in `served_live_playlist` (`transcode.rs:558-560`) | once per session (flag on the session) — the EVENT→sliding transition, currently invisible |
| `producer_pass` | end of `produce_pass` (`state.rs:1447`) | produced/skipped counts, reasons |

### 4.6 Prometheus — a playback surface

In the metrics handler (`http/system.rs:1621-1731`), fed by cheap
atomics/histograms updated at the same emit points (not by querying
SQLite per scrape): `plurx_ttff_ms` histogram by method (buckets 100,
250, 500, 1000, 2500, 5000, 10000, 30000) · `plurx_stalls_total{kind}`
· `plurx_suspends_total{reason}` + `plurx_suspended_seconds_total` ·
`plurx_cache_serves_total{result}` · `plurx_sessions_total{encoder}`.
The endpoint is unauthenticated — labels carry no titles, users, or
paths.

### 4.7 `SessionInfo.readrate`

Add `readrate` to `SessionInfo` (`transcode.rs:1607-1653`) mirroring
`StreamInfo.readrate` (`progressive.rs:90`), from the pacing values the
session was started with. This un-breaks the web overlay's paced/fed
classification on HLS sessions (plan B19/§3.4).

### 4.8 Clients — the parity floor

- **Web** (`crates/plurxd/src/web/index.html`): `playbackContext()`
  (`:2545`) adds `session: p.sessionId || null` (`PLAYER.sessionId`
  already exists, `:4573`); `setQuality` (`:6611`) emits a
  `quality_switch` beacon (from, to, reason=manual). `scripts/js-check`
  after; rebuild to see.
- **Apple**: one `ttff` beacon at first established progress —
  open-time is already stamped; post via the existing `postClientLog`
  plumbing (`PlayerController.swift:1872` region), carrying
  `session_id`. Suppressed offline like the other two beacons. **Bump
  the build number** (project.yml) — the gate checks the whole PR
  range.
- **Android**: build the client-log plumbing that does not exist —
  a small OkHttp POST mirroring the web field set — and send `ttff`
  (first `onRenderedFirstFrame` after prepare, `Controller.kt:340`
  region), `stall` (position stagnant ≥ 6 s while `STATE_BUFFERING` —
  measurement only in N0; the reopen behavior is N4's), and
  `playback_error` on `onPlayerError` (`Controller.kt:299-306` already
  assembles the context fields). Bump `versionCode`.
- Client PRs are separate from the server PR; server first (the field
  is optional, order-safe in both directions).

### 4.9 Readers

- New admin endpoint `GET /api/v1/system/playback-events?since=&event=&limit=`
  (admin auth, capped like `/system/logs`).
- `scripts/perf-report` prefers the endpoint, falls back to the log
  ring on older servers (its regex parser stays).
- The System page gets a small "Playback (7 days)" card: TTFF p50/p95
  by method, stall count by kind, suspend seconds — read from the
  endpoint. Keep it modest; N7 owns the real digest.

### 4.10 Census

`/api/v1/system` already serializes `EncoderCaps`. Add the pipeline
probe result and pacing caps beside it if not already exposed, so one
fleet sweep answers: per node — encoder families validated, tone-map
pipeline selected, ffmpeg build, readrate/burst support. Record the
table for all nodes in the N0 PR (this is the fleet census PERF2-PLAN
§3.5 requires; the AMD tone-map numbers ride the nynuc acceptance
visit).

### 4.11 Tests

Migration up-from-v16 (and the `MIGRATIONS.len()` bump); ingest inserts
with and without a live session (join columns present/absent); ingest
with `retain_days = 0` inserts nothing and logs identically;
per-user bucket isolation (user A's flood doesn't silence user B);
prune deletes only aged rows, bounded batches, stamps the clock;
store-contract parity for the new methods on both backends;
`SessionInfo` carries `readrate`; metrics render and count (unit-level
against the atomics); endpoint auth + caps.

### 4.12 N0 acceptance (run before calling it done)

- Restart plurxd: last week's TTFF/stall distributions still render
  from `perf-report` (table path, not ring).
- Play the known 4K-remux-stall title on nynuc: the stored rows carry
  `delivered_bps`, `readrate`, `runway`, and stall instants in the same
  records — PERF-PLAN §10's open question is now answerable from data
  (answering it is analysis, not N0 scope — but the rows must suffice).
- One Android play on a real device produces a `ttff` row with a
  session join.
- A suspended session (small ahead-window on a test box) produces
  `suspend`/`resume` rows with held-duration and `hold_reason`.
- `curl /metrics` shows the new series moving during a play.
- Docs in the same PR: CHANGELOG `[Unreleased]`, OPERATIONS.md
  (`telemetry.retain_days` row + endpoint), PERF2-PLAN §3 marked
  in-progress→landed in STATUS.html's row.

---

## 5. N1–N7 — execution notes per milestone

Full specs live in PERF2-PLAN; these are the repo-specific execution
facts and the per-milestone stop-and-flag list. Do them in this order
(§13 of the plan; dependencies are stated there and are real).

### N1 — quality-bounded rate control (plan §4; review R4)

*Order inside the milestone:* the `scripts/bench rate-control` harness
(production Jellyfin FFmpeg creates real, uncached plurxd HLS sessions; an
explicit separate FFmpeg with a behavior-probed `libvmaf` filter may score the
captured bytes offline after each encode, but never encodes production
playback; no scorer is installed or needed on nynuc or in compose) and the
golden-hash fixtures land **before** any flag changes. Full comparison uses a
pinned, balanced SDR corpus with unique filename/path/hash identities plus an
operator-captured nynuc `sha256sum` manifest, with the node reserved for
maintenance. Full acceptance also requires the exact VMAF model,
`n_subsample=1`, 10-second window, 0.25-second poll, 3-second settle delay,
recorded timing, usable server video facts, identical cross-mode ladder facts,
and session/status identity equal to the server-selected encoder. Idle checks
also reject producer/offline/other-delivery activity before mutation or
capture, and every capture poll requires exactly the owned HLS session. They
are immediate observations,
not GET-to-PUT locks; bounded polling may still include one in-flight 30-second
HTTP timeout and otherwise reports the safe two-field manual rollback body.
Harness settings-DTO and stable-encoder evidence does **not** prove effective
flags or fallback: N1's boot/production tests and the separate forced-fallback
acceptance run must do that. Then:
`EffectiveRateControl` resolved post-validation; the single
`effective_recipe()` builder replacing the four constructor sites
(`transcode.rs:2473,2755,2941,3014` + the cross-path fixture at `:5980`);
the per-family flag arms in `Encoder::encode_args`
(`encoder.rs:215-306` — boot validation covers the same production arguments
via `validation_args`, `encoder.rs:485`, pinned at `:1338-1352`); offline
packages persisting effective recipe inputs
at creation. Owner decisions recorded 2026-08-12: the full harness binds the
10-second complete-served-segment peak to the advertised peak; the derived
bufsize-window observation and theoretical VBV allowance stay diagnostic.
Offline packages use one non-null `effective_rate_control` column (`vbr` or
`qvbr:<q>`), SQLite v18 with existing rows defaulted/backfilled to `vbr`, and
replicated schema v5 with protocol v4; D9 later moves N3's SQLite migration to
v20 because N4.2's node-local priors take v19.
*Stop-and-flag if:* the legacy VBR digest bytes cannot be preserved
exactly; or a driver family needs flags that `validation_args`'s
15-frame probe cannot exercise; or offline snapshot storage needs anything
beyond that ratified single column; or implementation would change the
ratified 10-second binding peak. Such a change is a new §11 contract decision,
not an implementation detail.
*Acceptance:* plan §4's block, on nynuc, numbers in the PR.

### N2 — per-title analysis (plan §5)

`DueJob::AnalyzeMedia` + `jobs.media_analysis_mins` (0/off default);
behavioral probes for `siti`/`scdet`; verdict written via the
`json_set` chapters pattern (`store/sqlite/media.rs:915-936`) — the
rescan-overwrite of `probe_json` is correct behavior, don't fight it;
bias clamp ±30% applied at `transcode.rs:2342` through
`effective_recipe()`; attribution card + stop control (producer
template); bounded batches (artwork-retry style, `store/mod.rs:411`).
*Stop-and-flag if:* the sampled analysis wants > ~1 min/title on nynuc
(budget smell), or the clamp wants widening (that is a §14-class
decision).

### N3.0 then N3 — warm starts (plan §6; reviews R1–R3)

**The spike is not optional.** N3.0 proves the continuation protocol on
fixtures (every enabled encoder family) with the §6.4 packet gates as
its exit; only then does the v20 schema, producer `kind=prefix`, or any
serving change land. The serving contract is the plan's, verbatim:
server-owned served playlist, `-output_ts_offset` + `-start_number`,
fresh source `stat` equality before publish, artifact manifest
authoritative, all-or-nothing hydration with degrade-to-live. Event
triggers funnel into the single-flight producer
(`background_producer` + `offline_waiting` participation).
*Stop-and-flag if:* the packet gate needs per-family bound exceptions
(document per family, don't loosen globally); the atomic-publish
generation fights `Manager::playlist`'s current structure; or fMP4
prefixes are wanted before N5.1's container work (forbidden — plan
§13).

### N4 — sustained smoothness (plan §7; review R5)

N4.2's default-off server groundwork may land independently and is judged by
its store/API contracts. The shaping harness must land before the web
controller and reopen behavior it judges. Then the web controller (consuming
the served ladder; `decideRung` pure + table-tested) and the reopen contract:
`previous_session_id`
+ typed `reopen_reason`, target persisted under `request_id` via
`claim_request` **before** supersede; Apple queue carries the typed
cause; Android gets its first stall watchdog + reopen. The five R5 test
cases are required, not suggested.

The N4.2 server groundwork landed separately: the default-off setting, v19
node-local table, bounded 25% throughput EWMA, lowest-starved-rung verdict,
optional decision/session `prior_kbps`, and server-side Auto consult. A
starvation verdict binds until the conservative EWMA again covers the current
rung's advertised peak; that explicit recovery rule prevents one transient
stall from becoming a permanent cap. Do not mistake that groundwork for the
web controller, normalized reopen contract, or native way-down work named
above; those remain in the focused N4 slice.
*Stop-and-flag if:* anything wants to change ahead-window policy or
release thresholds (out of scope — evidence first, per plan §7.4); or
node-local prior isolation across deleted/recreated accounts needs a durable
user-generation field or other schema identity (the numeric id can be reused;
do not improvise that contract or activate priors without ratification); or
the fingerprint/idempotency extension wants to weaken the 409-on-
mismatch contract.

### N5.1 — OutputCodec + fMP4 + HEVC (plan §8.1; review R6)

The container abstraction is the milestone: typed `OutputCodec`
separate from encoder family; `Caps` optionally on create; fMP4 muxer
for HEVC with `EXT-X-MAP`/init ownership incl. `produce::assemble`;
`CODECS` derived from the produced init via the copy path's `hvc1`
derivation (`hls.rs:634-671`), fail-closed — retiring both hardcoded
`avc1.640034` sites (`transcode.rs:2496,3739`). Update the exhaustive
every-encoder-is-H.264 test and the badge contract *deliberately*, in
the same PR, with MEDIA-BADGES-PLAN cited. `scripts/perf2-hevc-validate`
before the toggle can leave `h264`.
*Stop-and-flag if:* TS-vs-fMP4 wants to differ per client (the plan
says fMP4 for HEVC, period); or dynamic range wants to change (v1 is
8-bit BT.709 SDR, unchanged digest colour fields).

### N5.2 / N6 / N7 — background lanes (plan §8.2, §9, §10)

Each independent, each optional, each default-off. N5.2 after N2 *and*
the container work; behavioral probe for `libsvtav1`; VMAF scored
pre-grain (the vmaf#1192 trap is named in the plan). N6 web shaders
behind a player-menu toggle with the hitch-rate auto-off; Apple 26-only
Labs toggle; Android deliberately nothing. N7's three assistants ride
the adapter rule — `none` must be a fully useful mode for the digest
(stage-1 analytics are the feature; the LLM is presentation).
*Stop-and-flag if:* any adapter wants a required credential, or any of
these wants onto the play path.

---

## 6. Standing guardrails (the short list you re-read before every PR)

PERF2-PLAN §12 in full, plus operationally:

- Nothing new on the play path — analysis and inference are scan/idle
  work; the play path only reads stored verdicts.
- Every output-changing parameter → `effective_recipe()` + hash
  fixtures, same PR.
- Every background worker: Background priority, yield flags,
  attribution card, stop control, `0 = off` default.
- Every external call: adapter with `none` default; plurx with every
  adapter at `none` behaves exactly like plurx today.
- Every acceptance number: measured on the machine where the bug can
  occur, quoted in the PR.
- Defaults ship as §2's table says; changing one is a settings-default
  commit, not a debate inside a feature PR.

## 7. Done means

A milestone is done when: gate green · plan-§ acceptance run and quoted
· docs updated in the same commits · STATUS.html row moved · deployed
to nynuc and then the fleet without incident · and the next agent could
pick up the following milestone from the plan + this handoff without
asking what state the tree is in.
