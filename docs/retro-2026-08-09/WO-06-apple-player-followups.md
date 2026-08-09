# WO-06 — Apple player follow-ups (Swift — needs Xcode; run `make apple-test` locally)

**Repo:** `~/code/plurx` · **Baseline:** `origin/main` @ `e8a910f` (build 38) · **Priority: P1**
Bump `CURRENT_PROJECT_VERSION` (38→39) in the release-path commit or preflight fails.

## Context

The recovery ladder after #78+#85+#86+#87+#91 is now a real state machine (value-type detectors, ~15 focused tests, clean leg separation). The #91/#92 review asks were verified landed on main. What's left: the telemetry blind spot that keeps the iPad mystery unsolvable, and two flag-lifecycle bugs of exactly the class the ladder's scattered booleans invite.

## Tasks

1. **Sub-threshold stall telemetry (the operationally important one).**
   #86's stall beacon fires only when the ladder acts (~12-14 s of stagnation: 6 × 2 s samples). The motivating iPad trace showed AVPlayer running dry and self-recovering in **12.29 s** — a coin flip against the threshold. Shorter stalls (8-12 s, the plausible real cadence) still produce **zero** server-side evidence. `accessLog().numberOfStalls` is already read for the in-app stats panel (`PlayerController.swift:612-614`) and never transmitted.
   Fix: piggyback deltas of `numberOfStalls` (plus `stagnantDurationMs` when a stall self-recovers before the ladder fires) on the existing 10 s progress cadence to `/api/v1/client-log`; keep the bounded-payload discipline of `ApplePlaybackStallLog`.
   Acceptance: throttle a stream so AVPlayer stalls ~8 s repeatedly → server log shows stall evidence with zero reopens. Add a routing-decisions row.

2. **`establishedPlayback` is never reset in `open()` (probe-confirmed: set false only at `start()` :643, true at :1758).**
   The monitor's comment says recovery arms "only after **this item** has advanced five seconds", but after any mid-film stream change (quality/audio/burn) or first stall recovery, the flag carries over — the buffering detector monitors the successor session's publish-gate fill. A gate fill ≳14 s (4K HDR seek on a busy encoder) triggers a same-delivery reopen (second cold start), spends the one-shot, and a second slow fill lands a healthy-but-slow server on the terminal "Check the connection" screen.
   Fix: clear `establishedPlayback` in `open()` (restoring the per-item semantics the comment claims).
   Acceptance: unit test — post-open buffering samples don't count until 5 s of that item's progress.

3. **`endAction` treats every session as a growing playlist** (`PlayerController.swift:1796` passes `isGrowingPlaylist: self.sessionId != nil`). A cached/VOD session (server `vod: true` → `#EXT-X-ENDLIST`) is not growing; if both catalog duration and `hls.durationMs` are absent (NULL-duration file), the true end loops reopen→create→end forever (server session churn, no UI, no bound).
   Fix: pass `sessionId != nil && !isVOD`, and add a second-consecutive-end-with-no-positional-progress bound that finishes instead of reopening.
   Acceptance: unit tests on `endAction` + the progress bound; device test with a duration-less MKV if one exists.

4. **Harden the completion coordinator** (`PlayerView.swift:349`): `guard !didFinish` silently discards a late `finishPlayback(then:)` completion — every current call site pre-checks `isTearingDown`, but the next one won't. Run (or at least assert on) completions arriving after `didFinish`.

5. **Anchor the marker-row fix.** #92's rebuilt test pins the shared label/row components, but nothing hosts PlayerView's call site — re-adding `.frame(maxWidth:.infinity)` at the `markerButton` site stays green, and the fix has no routing-decisions row. Add the row (see WO-04 task 3) and, if cheap, a UIHostingController-based test through `playbackControls`.

6. **Doc sync (fold into WO-09 if doing both):** `clients/apple/README.md:15` and `docs/APPLE-CLIENT-PARITY.md:12,20` still say build 37 (project.yml is 38); PARITY:55 still lists stall telemetry as absent — #86 shipped the beacon; credit it and narrow the remaining-work line to sub-threshold classification (task 1).

## Device checklist (things only hardware settles — batch into one session)

- tvOS: teardown focus placeholder actually suppresses focus retargeting; an open tvOS `Menu` keeps focus on its label (else auto-hide can dismiss the presenting hierarchy after 4 s).
- Real publish-gate fill times after mid-film quality changes vs the ~14 s threshold (task 2's real-world exposure).
- `statusBarHidden`/`persistentSystemOverlays` through `fullScreenCover` on physical iPad; Stage Manager/Split View behavior (the CHANGELOG now correctly scopes the claim to full-screen).
- Autoplay hand-off: no other-app audio blip between episodes; lock-screen/Now-Playing continuity (the `deactivateAudioSession: false` path still cycles remote commands).
- Marker label at max accessibility sizes on narrow iPhones (lineLimit(1) compresses — check readability).

## Don't (verified fixed or refuted — re-raising wastes a session)

Verified fixed on main: eject-on-unknown-duration (corroborated-duration gate), `finished` write-once (reset at :634/:695/:988/:1115), `finishTask` leak (deleted; completion-survival test exists), `setActive` autoplay blip (deactivate flag), auto-hide restart in teardown (guarded by `isTearingDown`), chrome-resolve superset (`showStats || failed || isChangingStream || findingNext || banner`), hosting-controller chrome test + PLAYBACK.md row.
Refuted, do not re-raise: notice-banner ~1 s jump; failure-screen chrome lockout; #85 cold-start publish-gate eject (isChangingStream covers the gated window); `realPositionMs` at natural end.
