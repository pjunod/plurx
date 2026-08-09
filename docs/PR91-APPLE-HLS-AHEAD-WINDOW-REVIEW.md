# PR #91 review — Apple HLS ahead-window + natural-end dismissal

**Verified against:** PR head `9dcdb1d` (`codex/fix-apple-hls-hysteresis`),
merge-base with `origin/main` = `5c6bfe5`. Scope is `git diff 5c6bfe5 pr91`:
two real commits — `23e76a9` "Prevent Apple HLS ahead-window deadlock"
(server flow control + Apple build 36→37 + docs) and `aade8ff` "Dismiss Apple
playback after natural end" (Apple client teardown rework) — plus `9dcdb1d`,
a merge of main that carries PR #88 and is not reviewed here.

**Baseline at the sha, all green:** `cargo build --workspace --all-targets`
exit 0 (7m48s) · `cargo test -p plurxd` **331 passed, 1 ignored** (matches the
PR body exactly) · `cargo test --workspace` 712 passed, 1 ignored · `make
check` exit 0 in 217 s. `make apple-test` could not run (no macOS here); CI
*does* run it (`.github/workflows/ci.yml:195-218`, `macos-15`, fired by
`clients/apple/**` via `validation/points.toml:457-462`), so a broken Swift
build would be caught.

---

## Verdict: changes requested — 3 blockers, 11 should-fix

Two independent problems, and they fail in opposite directions.

**The server half fixes something that was not broken, and the evidence for
the bug is in the PR's own documents.** The client cannot starve because the
producer is suspended: `ahead.seconds` counts media that is *published, in the
served playlist, and not yet downloaded*. A client sitting at 154 s ahead has
154 s of fetchable media in the playlist it is polling. Resuming the producer
adds more media the client is already declining to fetch.

**The client half is a genuine, well-shaped fix for a real bug** (autoplay-off
titles never dismissed) — but it promotes a write-once `finished` flag from
"inert unless autoplay is on" to "always ejects the viewer", and that flag can
be set spuriously on any file whose duration is unknown.

---

## §1 The server change — the diagnosis does not survive the code

`time_release_threshold` now returns the ceiling unchanged
(`crates/plurxd/src/transcode.rs:609-611`) and `should_suspend` no longer
branches on `currently_suspended` for time (`:619`). Byte limits keep their
half-window hysteresis.

### A (blocker) — the stated failure mode is not reachable

The PR says the iPad "polled the unchanged EVENT playlist for 84 seconds, ran
dry, waited 12.29 seconds, and reopened" while suspension held the producer.
Three facts, each executed against the real functions, make that causally
impossible:

1. **Nothing hides published media from the client.**
   `served_live_playlist` (`transcode.rs:515-576`) only drops the *pruned
   prefix*; it never truncates the tail. Swept over a real 400-segment index
   at every frontier: `pruned_beyond_frontier=0` and
   `tail_seg399_visible=true` in all cases. `gc_expired_segments` (`:4746`)
   computes `keep_from = fetched_end_ms - RETENTION_SECS*1000` (`:4756`) —
   strictly *behind* the download frontier. Starvation-by-pruning is
   impossible.

2. **The old release was one segment away, not unreachable.** Driving the real
   `should_suspend` from the reported state (held, ahead 154, ceiling 180,
   release 150): **one** 6 s copy segment or **two** 2 s transcode segments
   (`plurx-core/src/transcode/mod.rs:48,75`) clears it. The new test comment
   at `transcode.rs:5576-5580` — "did not fetch **enough** whole segments to
   cross the old 150 s release" — is arithmetically wrong; the shortfall was
   one segment.

3. **The PR's own record says the client had buffer.** `docs/PLAYBACK.md`
   before this commit: "AVPlayer stopped fetching with about 95 s buffered and
   left the producer 138 s ahead." A client with ~95 s buffered that then runs
   dry in ~84 s did not starve — it *stopped fetching* while 138–154 s of
   media sat available to it. The failing actor is the client's fetch loop.

Against the actual observed behaviour (client polls, does not fetch), the new
code buys exactly one production burst and then re-holds identically:

```
OLD  non-fetching client, 2x:  SIGCONT=0 SIGSTOP=0 final_ahead=154s HELD
NEW  non-fetching client, 2x:  SIGCONT=1 SIGSTOP=1 final_ahead=182s HELD
NEW  non-fetching client, 10x: SIGCONT=1 SIGSTOP=1 final_ahead=214s HELD
```

Twenty-eight to sixty extra seconds of media on disk, zero client-visible
change. **This is the second time the release point has been chased upward to
clear a single device trace** — #87 moved it 90 → 150 to clear an observed
138, #91 moves it 150 → 180 to clear an observed 154. It is now pinned to the
ceiling, so the next trace at 190 has nowhere left to go. That is the signal
that the lever is the wrong one.

**Leading alternative cause, untested here (no iPad), code-confirmed.**
`served_live_playlist` *removes* `#EXT-X-PLAYLIST-TYPE:EVENT` (`:559`) and
starts advancing `MEDIA-SEQUENCE` (`:562-571`) the moment the first segment is
pruned — roughly three minutes into playback:

```
frontier_seg=10   EVENT_present=true   #EXT-X-MEDIA-SEQUENCE:0
frontier_seg=100  EVENT_present=false  #EXT-X-MEDIA-SEQUENCE:11
frontier_seg=399  EVENT_present=false  #EXT-X-MEDIA-SEQUENCE:310
```

The same URL silently mutates from an EVENT playlist into a sliding live
playlist mid-session. "AVPlayer polls but stops fetching, then runs dry and
reopens" is the classic symptom of a client that has re-derived a live-edge
model. The PR does not touch this. See §5 for the device capture that would
settle it.

### B (blocker) — a byte or global hold presents identically, and is unchanged

`web/index.html:7024-7026` renders `held · time release ≤{resume_below_seconds} s`
whenever `suspended`, regardless of which limit fired. Executed:
`ahead=154s but 3 GiB → suspend=true, stay_suspended=true`, and the operator
is shown `resume_below_seconds = Some(180)` — a **time** number for a **byte**
hold. Only a `tracing::debug!` (`:4565-4571`) distinguishes them. If the
incident was diagnosed from the activity page, the diagnosis was
unfalsifiable.

That matters because the byte path is a *better* quantitative fit and is
untouched. Real `bitrate_for_height`:

```
2160p 20 Mb/s: 180s reserve = 0.42 GiB — 2 GiB cap needs 851s of media
copy  69 Mb/s: 180s reserve = 1.45 GiB — 2 GiB cap at 249s, HALF-release at 124s
```

No transcode rung can hit the per-session cap before the time limit — so for a
transcode session the time hold is the only possible hold. But on the **copy /
remux** path (which is what "iPad remux playback" is), a source above
~95 Mb/s suspends on bytes at ~180 s ahead and requires `ahead_bytes ≤ 1 GiB`
≈ 90 s to release — the client must drain from 154 s to 90 s, **~64 s of
playback**, against the reported 84 s. Tighter fit than the time story, and
this PR changes nothing about it.

Worse, the **global** cap is the one true deadlock in this machine.
`global_live_bytes` sums *total* on-disk bytes (retention + reserve), so at
69 Mb/s each copy session is 2.89 GiB and **2.8 sessions reach the 8 GiB
cap**; release requires the *sum* below 4 GiB, which no amount of fetching by
the held client can achieve. Confirmed: `ahead=10s/1KiB but global=9 GiB →
suspend=true`; `stay_suspended at 5 GiB = true`.

Minimum ask: report the hold **reason** (`time` / `bytes` / `global`) and the
matching release value in `SessionInfo`, so the next trace is diagnosable.

### C (blocker) — `apply_ahead_window`'s state guard is the mechanism the new
docs rely on, and removing it is invisible to 331 tests

Mutation: delete `if want_suspend == suspended { return; }` (`:4532-4534`) →
`cargo test -p plurxd` = **331 passed, 0 failed**. The new doc comment
(`:4512-4515`) explicitly leans on this line ("never per poll:
`want_suspend == suspended` below suppresses a signal"), and with time
hysteresis now zero it is the *only* thing standing between a playlist poll
and a signal storm.

The failure mode is not just signal spam. With the early return gone, every
poll of a healthy running session takes the `!want_suspend` path and
`session.progress.touch()` (`:4557`) fires — resetting the stall clock on
every reap tick, silently disabling the `PROGRESS_STALL` watchdog for every
session on the box. Nothing notices.

Test that would catch it: hold a real session, re-apply the same limits with
unchanged state, and assert the motion clock was **not** touched and
`suspended` did not change.

### Should-fix — server

- **Flapping is 17× at the shipping default.** Measured over 900 s with the
  real `should_suspend` and the real trigger sites: 2 s segments at the
  default `-readrate 2.0` go from **0.7 → 12.2 SIGSTOP/SIGCONT pairs per
  minute**; the copy path 0.6 → 4.0. The doc's "one pair per client segment"
  is a true upper bound (max measured 0.36/segment) but never states the
  per-minute figure, which is the number an operator sees.
- **Overshoot past the ceiling scales with encoder speed:** +4 s at 2×/2 s,
  **+120 s at 20×/6 s** — about 1 GiB past the intended reserve at the 69 Mb/s
  figure the repo itself quotes. With no client polling, the only evaluator is
  the 15 s reap pass.
- **`recent_speed` goes stale on short bursts.** `recent_rate_step` (`:170`)
  rejects samples with `d_wall > 5000 ms`, and ffmpeg emits ~2 progress blocks
  per second, so a running burst under ~0.9 s can never produce an in-burst
  pair. Burst census: OLD has **0** sub-0.9 s bursts at every speed; NEW has
  72/155 at 5×, 53/97 at 10×, 46/67 at 20× (only 1/183 at the default 2×). So
  the regression is confined to encoders faster than the configured readrate —
  the 90 s initial burst and the copy path. Note also that a hold *shorter*
  than 5 s is silently folded into the rate as a slowdown, and short holds are
  now the common case.
- **`resume_below_seconds` is now vacuous.** It is
  `time_release_threshold(max_secs)` (`:4289`, `:4305`), now identically
  `max_secs`. A time-held session is by construction *above* `max_secs`, so
  the UI always prints a release point below the ahead figure beside it. The
  PR's own test (`:5215`, `Some(150)` → `Some(180)`) locks the redundancy in,
  while `docs/PERF-PLAN.md:476` still claims it lets operators "distinguish
  this state from an encoder stall". Fold this into the hold-reason field
  asked for in §1B.
- **The "154 s regression coverage" claim is not coverage.**
  `assert!(!should_suspend(secs(154), 0, limits, true))` (`:5582`) is strictly
  dominated by `:5581` (`!should_suspend(secs(180), …, true)`) — proven by
  setting the threshold to `Some(limit - 1)`: the suite failed at `:5581` and
  `:5582` was never reached. Across every mutation run, `:5582` was never the
  discriminating assertion. 154 is an anecdote encoded as a constant.
- **No test proves a client fetch resumes a held session** — the actual bug
  being fixed. `flow_control` has zero test callers; `mgr.segment(...)` appears
  in tests only against a *missing* session (`:7477-7478`); the one
  `apply_ahead_window` test (`:5930`) toggles suspension by swapping the whole
  limits struct (`max_secs: 1` vs all-zero = every limit disabled) and never
  exercises a release threshold or moves the client frontier. The missing test
  is `a_client_fetch_releases_a_held_session`: hold at a realistic
  `max_secs: 180`, `mgr.segment(...)` the next unfetched segment, assert
  `suspended == false` and `out_time_ms` advances. It would fail under the old
  code and pass under the new — the only test in this PR that would.
- Credit where due: **`max_secs = 1` was genuinely broken and is now fixed.**
  Old: `time_release_threshold(1) = max(1-30, 0) = 0`, and `over(v, 0)` is
  false because of the `limit > 0` guard (`:625`), so the time limit
  *vanished while suspended* and the session flapped on every evaluation.
- The second production hunk (restoring/removing the `currently_suspended`
  branch, `:620`) is a semantic no-op once `time_release_threshold` is
  identity — dead-code cleanup, unpinnable by construction. Not a coverage
  hole.

---

## §2 The Apple client change — one blocker, otherwise sound

The bug is real and the shape of the fix (`PlayerLifecycleCoordinator`, one
idempotent exit for natural end / Close / Menu / failure screen) is right.
Several of the incidental hardening changes are genuine improvements —
`openGeneration &+= 1` and `reopenQueue.clear()` in `stop()`, and the
`guard started` / `guard player.currentItem === item` additions, which stop a
superseded open from re-publishing state after teardown.

**Not a regression** (checked, since it would have been): `started` is set
*synchronously* at `PlayerController.swift:602` and `:661`, before the
`Task { await load… }` / `loadOffline` dispatch at `:651` and `:685`. Every
new `guard started` runs strictly after. Offline playback and first open are
unaffected.

### D (blocker) — a spurious `finished` now ejects the viewer mid-film

`PlayerController.swift:1736-1766`:

```swift
1752:  if self.knownDurationMs > 0 && endedAt < self.knownDurationMs - 15_000 {
1755:      await self.reopen(at: endedAt); return
1757:  }
1764:  self.finished = true
```

The comment at `:1753-1754` names the exact hazard — *"A growing EVENT
playlist can momentarily end before the title does"* — but gates the
protection on `knownDurationMs > 0`. That is reachably zero:

- `AppModel.swift:661`, `:722` — `durationMs: file.durationMs ?? playable.runtimeMs ?? 0`
- `PlayerController.swift:996` — `knownDurationMs = decision.source?.durationMs ?? 0`
- `:1234` — `knownDurationMs = hls.durationMs ?? 0`, fed by
  `crates/plurxd/src/http/stream.rs:458 duration_ms: file.duration_ms`, an
  `Option<i64>` that is NULL for an unprobed or ffprobe-failed file. This is a
  known-live state in this repo (the same NULL-duration hole was recorded in
  the PR #90 review). A **direct play** never gets the `:1234` second chance
  at all.

With `knownDurationMs == 0` the early-end branch is skipped and *any*
`AVPlayerItemDidPlayToEndTime` sets `finished = true`. Before this commit that
was inert unless autoplay was on. After it, `PlayerView.swift:717-725` routes
it to `finishPlayback()` → full teardown → `dismiss()`, with **no error, no
banner, no failure view** (`if !lifecycle.isTearingDown` at `:574` hides
`failureView` and `playbackBannerMessage` too). The viewer is thrown back to
the detail screen mid-film with no explanation.

`finished` is also **write-once**: declared `private(set)` at
`PlayerController.swift:443`, written only at `:1764`, and never reset — not
on `reopen`, not on `open()`'s `replaceCurrentItem`, not on seek, stream
change, or `stop()`. Today that is masked only by `PlayerView` being recreated
per `.id(ctx.id)`.

This is exactly the growing-EVENT-playlist regime that §1 of this same PR is
about, so the risk is not theoretical on this branch.

Fix: require corroboration before `finished = true` — treat unknown duration
as "reopen at `endedAt`, do not finish" — and reset `finished = false` in
`open()`/`reopen()` when a new item attaches.

### Should-fix — Apple client

- **Nothing pins the headline fix.** Mutate `PlayerView.swift:724-725` to
  `case .dismiss: break` and no test notices. The three new tests
  (`AppleClientTests.swift:2544`, `:2568`, `:2595`) cover the pure
  `naturalEndAction` table, coordinator ordering/idempotency, and
  `stop()`-when-never-started. Also GREEN under mutation: deleting the
  `if !lifecycle.isTearingDown` wrapper (`:574`); deleting the
  `restoredAfterPlayback` *use* (`:770-771`, the constant is asserted, its use
  is not); reverting `detach()` → `stop()` (`:846`); deleting
  `onPlaybackStopped?(stoppedAt)` (`:848`); reverting `finishPlayback()` →
  `dismiss()` at `:751`, `:952`, `:966`; deleting all `nextEpisodeTask`
  cancellation; deleting every new `guard started`; deleting
  `openGeneration &+= 1` / `reopenQueue.clear()` / `AVAudioSession.setActive(false)`.
- **`finishTask`'s cancellation guard is dead code.** `PlayerView.swift:327`,
  `:343-350`: `finishTask` is assigned and never read, cancelled, or awaited,
  so `Task.isCancelled` at `:348` is always false. The live half is
  `guard self != nil`, which can silently drop `completion()` — harmless for
  `dismiss()`, but on the `.findNext` path it means `onPlayNext(next)` never
  fires and **autoplay silently stalls**. Either cancel `finishTask` somewhere
  and let that be the mechanism, or drop the guard and the `[weak self]`.
- **`finish` swallows the completion, not just the teardown.**
  `guard !didFinish else { return }` (`:341`) means a second
  `finishPlayback { onPlayNext(next) }` after a first `finishPlayback()` will
  never advance the episode. Currently unreachable via the
  `!lifecycle.isTearingDown` check at `:731`, but it is a load-bearing
  coupling with no test.
- **Teardown re-creates the PiP controller it just detached.** `:846`
  `detach()` nils `controller`/`playerLayer` (`PlayerSurface.swift:86-97`);
  `controller.stop()` at `:847` publishes `pgsOverlayWindow = nil`, and any
  resulting body re-evaluation re-runs `PlayerSurface.updateUIView`, whose last
  line is `attach(to: view.playerLayer)` (`PlayerSurface.swift:153`) — whose
  guard (`:39`) is now true, so it constructs a fresh
  `AVPictureInPictureController` with
  `canStartPictureInPictureAutomaticallyFromInline = true` on a layer whose
  player has a nil item, one turn before dismissal. Inert but wrong.
  (`detach()` *is* a strict superset of `stop()` — it calls `stop()` first at
  `:87` — so no PiP session is orphaned. That concern resolves favourably.)
- **`AVAudioSession.setActive(false, options: .notifyOthersOnDeactivation)`
  fires on the autoplay-next path** (`PlayerController.swift:972-977`). Between
  episodes the app relinquishes the audio session and explicitly tells other
  apps to resume, then the successor calls `setActive(true)` at `:629` a turn
  later. On a device that is a paused Music/podcast app resuming for a beat
  between episodes plus a route re-negotiation. Suppress the deactivation when
  a successor is about to attach.
- **`autoHideGeneration &+= 1` in teardown restarts the timer rather than
  cancelling it** (`PlayerView.swift:839`, identical to
  `restartAutoHideTimer()` at `:880-882`). The restarted
  `.task(id: autoHideGeneration)` sleeps 4 s and calls `hideControls()`, which
  on tvOS sets `focusedControl = .reveal` (`:894`) — a `@FocusState` whose
  `.focused(...)` view has just been removed by the `!isTearingDown` wrapper.
  Masked because dismissal wins the race; still the wrong primitive.
- **tvOS: teardown removes every focusable view while the cover is still
  presented** (`:574-661` wraps the reveal layer at `:576-590` and
  `playbackControls` at `:613`). For one main-loop turn the presented view has
  zero focusable elements — only `Color.black` and a non-interactive
  `PlayerSurface`. Needs a device check (§5).
- **`sessionStatus` can be resurrected after `stop()`.** `statusTask?.cancel()`
  (`:947`) does not abort the in-flight `await model.hlsStatus(sessionId)`
  inside `startStatusPolling` (`:1607`); that assignment can land after
  `sessionStatus = nil` at `:961`. Cosmetic, but the "clear everything" intent
  isn't achieved.
- **`stop()` on a never-started controller now mutates process-global state.**
  `removeRemoteCommands()`, `MPNowPlayingInfoCenter.default().nowPlayingInfo = nil`,
  `playbackState = .stopped` (`:969-971`) run outside the `wasStarted` guards
  used at `:962` and `:972`. Reachable when `startOffline` bails at `:660`. The
  new test at `AppleClientTests.swift:2595` also runs this against the real
  `MPNowPlayingInfoCenter` singleton twice inside the test process — test-order
  pollution.

**Checked and clean, do not re-raise:** `onPlaybackStopped` ordering is benign
— its only call sites are `DetailView.swift:662` (local snapshot, guarded by
`detail.item.id == itemId` at `:686`) and `DownloadsView.swift:76` (not
supplied); no server write, no resume-position write, guaranteed single by
`didTeardown`. `realPositionMs()` does **not** return 0 after natural end —
`stop()` reads it at `:955` *before* `replaceCurrentItem(with: nil)` at `:957`.
Offline + autoplay → `.dismiss` is a behaviour-preserving rename of the old
`guard finished, model.autoplay, offlineItem == nil`. The double progress
report at natural end (`:1760` then `:962`) is pre-existing; only the window
tightened.

---

## §3 Docs, CHANGELOG, versioning

### E (blocker) — the CHANGELOG heading says the opposite of what shipped

`CHANGELOG.md:453`: **"iPad remux playback gets a larger server-side recovery
margin."** The time recovery margin went from 30 s (150 s release under a
180 s ceiling) to **zero**. The heading is a leftover from the #87/#90 version
of the entry that `23e76a9` rewrote in place. The body at `:464` states the
truth. Rewriting is permitted here — the entry is inside `## [Unreleased]` and
no released section was touched — but the heading has to change.

Related: the entry now folds #87's retention change and #91's threshold change
into one bullet scoped to "iPad remux playback", while `should_suspend` is
global HLS flow control affecting every session on every client. Per
`docs/RELEASING.md` step 3, a web user hitting the same held-session behaviour
will not find it under an iPad heading.

### F (should-fix) — the honest caveat was deleted while its test was kept

`23e76a9` removed "This is not a structural deadlock escape: without another
client fetch the producer can reach 186 seconds and become held again, leaving
the existing same-delivery reopen as the terminal recovery" from `CHANGELOG.md`,
`docs/PLAYBACK.md`, `docs/PERF-PLAN.md:471-473`, and the doc comment at
`transcode.rs:598-608`. `git grep "same-delivery|terminal recovery|structural
escape"` now returns zero hits in `docs/` and `CHANGELOG.md`. But the same
commit keeps the assertion that proves it (`transcode.rs:5583-5586`, *"without
a client fetch, the producer stays held above the ceiling"*). A commit titled
"Prevent … deadlock" that deletes the "this does not eliminate the deadlock"
caveat while keeping the test is a documentation regression.

### G (should-fix) — a claimed flow-control mechanism does not exist

`transcode.rs:4629` ("a segment completed, or the client's frontier
advanced"), `:4689` ("Flow control proper runs on segment completion and
frontier advance"), and `docs/PERF-PLAN.md:459` all assert a producer-side
evaluation. `flow_control` has exactly three callers, all client-driven:
`:4367` (playlist read), `:4389` (subtitle-timing lookup), `:4434` (segment
fetch, guarded by `if i > previous`). `apply_ahead_window` has two non-test
callers: `:4640` and the 15 s `reap_loop` at `:4698`. Nothing observes ffmpeg
finishing a segment.

This is load-bearing for this PR: with time hysteresis removed, nothing
re-evaluates the producer after a SIGCONT until the next client HTTP request
or the 15 s sweep — which is exactly the +120 s overshoot measured above. Fix
the docs or add the caller.

### Stale after this PR

- `docs/OPERATIONS.md:361` — "resumes it with `SIGCONT` once you are within
  half the limit". Half of 180 is 90; the release is now 180. The only
  operator-facing description of this mechanism, and it has been wrong since
  #87. (Same section, `:355`: "roughly two minutes behind" —
  `RETENTION_SECS` is 120+30+30 = **three** minutes, `transcode.rs:76-80`.)
- `docs/PERF-REVIEW-RESPONSE.md:270` — "Media-time window 180 s high / 90 s
  low". Reads as current design.
- `clients/apple/README.md:15` — "Status: **v0.2.7**, build `36` in
  [`project.yml`](project.yml)", explicitly cross-referencing the file this PR
  changed to 37. `tests/validation/test_mobile_versions.py:106-119`
  deliberately excludes this path, so no gate catches it.
- `docs/APPLE-CLIENT-PARITY.md:12`, `:20` and
  `docs/APPLE-NATIVE-SUBTITLES-PLAN.md:361`, `:365`, `:471` — "current source
  is build 36".
- **`docs/STATUS.html` untouched — the same gap flagged on #88 and #90.**
  `:101` "Apple build 36 source, not yet uploaded"; `:269` and `:270` are
  build-36 operator tasks; the stamp at `:277` reads "reviewed against `main`
  @ `d25905ae` … page refreshed 2026-08-08" — today's date against a basis
  three merges behind (`origin/main` is `5c6bfe5`). Neither the ahead-window
  change nor the natural-end dismissal appears anywhere on the page. Nothing
  in `points.toml` or `tests/validation/` checks `STATUS.html` content, so
  this is invisible to CI by construction.
- `docs/PLAYBACK-TESTING.md` has no natural-end / player-exit scenario at all;
  this PR introduces four exit paths worth a manual matrix there.

### Versioning — correct, but attached to the wrong commit

37 is right and 38 is not required: `validation/mobile_versions.py:186-193`
compares the whole diff range against the base, not per commit, so one bump
covers both client-affecting commits. No Android bump (no
`ANDROID_RELEASE_PATHS` touched), no `Cargo.toml` bump (`MARKETING_VERSION`
must equal the workspace version, `mobile_versions.py:157-161`, so bumping one
alone would *fail* the gate). Note the asymmetry: `23e76a9` bumps the build
for a server-only change while `aade8ff` makes the actual Apple source change
with no bump — anyone bisecting by build number will land wrong.

### Nits

- `transcode.rs:4513` / `CHANGELOG.md:467` "never per poll" — `flow_control`
  *is* invoked on every playlist poll (`:4367`); measured, **44 % of holds at
  2× and 85–95 % at 5–20× are issued during a poll**. What
  `want_suspend == suspended` guarantees is no *repeat* signal.
  (`docs/PERF-PLAN.md:477` phrases it correctly.)
- `docs/PERF-PLAN.md:469` "each below half their limit" — the code releases at
  exactly `≤ half` (`:620-628`), pinned by `:5589`. Likewise `CHANGELOG.md:460`
  "suspended at its 180-second ceiling" — suspension fires at `> 180`.
- `docs/PLAYBACK.md:473` "the reaper SIGSTOPs its ffmpeg" — the reaper is the
  repair loop; the suspend normally comes from `flow_control` on a client
  request. Pre-existing, but this hunk was rewritten and the fix was missed.
- `CHANGELOG.md:434` says "iPadOS" for an `#if os(iOS)` path — iPhone restores
  chrome too.
- `transcode.rs:5577-5579` ("did not fetch **enough** whole segments") vs
  `docs/PLAYBACK.md:476-478` and `CHANGELOG.md:462` ("polled the unchanged
  EVENT playlist for 84 s") describe two different failure modes implying two
  different fixes. No trace artifact is committed either way.
- `PictureInPictureController.stop()` is now dead as an external API (only
  reachable via `detach()` and `toggle()`) — consider making it private.

---

## §4 What I would do

1. **Do not ship §1 as a fix for the reported incident.** Land the hold-reason
   field (§1B) and the `a_client_fetch_releases_a_held_session` test first,
   then capture one more trace. The threshold change itself is harmless-ish
   and does fix the `max_secs = 1` degeneracy — but it costs 17× the signal
   rate and up to +120 s of overshoot for a benefit nobody has demonstrated.
2. **Fix D before merge.** It converts a latent flag into a user-visible
   ejection on every NULL-duration file.
3. **Add the two missing tests** — the server fetch-resume test and a
   `controller.finished → dismiss` view test. Both are the only tests in this
   PR that would fail against the pre-PR code.
4. Fix the CHANGELOG heading (E), the flow-control doc claim (G), and
   `OPERATIONS.md:361`; refresh `STATUS.html` and the build-36 references.

---

## §5 Device / Xcode checks I could not run

No macOS or iPad here. These are the exact captures that would settle the open
questions:

1. **Settle §1A.** On the iPad, reproduce a held session and capture, at the
   moment of the freeze: the server's `ahead_seconds` **and** the client's
   `loadedTimeRanges` / `AVPlayerItemAccessLog` `numberOfSegmentsDownloaded`.
   If `ahead_seconds` is large while the client's buffer is draining, the
   client is declining to fetch available media and the server threshold is
   irrelevant.
2. **Settle the EVENT→live hypothesis.** Capture the served `index.m3u8` at
   T-90 s, T-0 and T+30 s around a freeze and diff the header. If
   `#EXT-X-PLAYLIST-TYPE:EVENT` disappears and `MEDIA-SEQUENCE` starts moving
   shortly before the stall, that is the cause.
3. **Reproduce D.** `UPDATE files SET duration_ms = NULL WHERE id = <id>`, play
   that file to its end on an iPhone with autoplay **off**. Expected under this
   PR: the cover dismisses at the first `AVPlayerItemDidPlayToEndTime`. Add
   `os_log` at `PlayerController.swift:1764` printing `knownDurationMs`,
   `endedAt`, `sessionId != nil`.
4. **PiP re-attach (§2).** Breakpoint `PictureInPictureController.attach`
   (`PlayerSurface.swift:37`) and `detach` (`:86`); play a title to natural end
   with autoplay off and record the call order.
5. **Live PiP.** Start PiP, background the app, let the episode end with
   autoplay **on**. Does the PiP window close and stay closed, or does episode
   N+1 resume in PiP?
6. **Audio session.** With Apple Music paused in the background, watch an
   episode to its end with autoplay on; confirm whether Music resumes for the
   turn between `setActive(false, .notifyOthersOnDeactivation)` and the
   successor's `setActive(true)`.
7. **tvOS focus.** Let a title end with autoplay off while the transport is
   hidden; watch the Xcode console for "no focusable views" diagnostics during
   the `isTearingDown` turn, and check where focus lands on the underlying
   DetailView.
8. **SwiftUI diagnostics.** `finishPlayback` mutates `@Published` and four
   `@State` vars synchronously from inside `.onChange(of: controller.finished)`
   — run with Main Thread Checker and check for "Publishing changes from within
   view updates is not allowed".
9. **`make apple-test` on tvOS.** The two new coordinator/controller tests sit
   *outside* the `#if os(iOS)` block that starts at
   `AppleClientTests.swift:2622`, so they run on Apple TV too. Confirm they
   compile and pass there.
