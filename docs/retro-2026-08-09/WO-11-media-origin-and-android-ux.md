# WO-11 — Media origin hardening + Android/tvOS seek UX

**Repo:** `~/code/plurx` · **Baseline:** `origin/main` @ `e8a910f` · **Priority: P2**
Client tasks need Android Studio/Xcode; bump versionCode / CURRENT_PROJECT_VERSION on release-path commits.

## Context

The media-origin feature (#56–#58) is correct end-to-end and well-designed (additive wire contract, honest old-server fallbacks, a server integration test whose fixture is deliberately mid-GOP). The gaps are that the client-side consumption is untested, and #42's global seek controls shipped with UX regressions on the non-web surfaces.

## Tasks

1. **Pin the client consumption of `media_origin_ms`.**
   Only the pure helpers are tested. Reverting any of these leaves both client suites green, silently restoring the mid-GOP scrobble/PGS-cue drift the feature exists to fix: `clients/android/.../Controller.kt:543` (HLS baseMs), `:732` (`.setTransferListener` registration), `:368-375` (`realPosition()` regime switch); `clients/apple/Sources/PlayerController.swift:1266` (nextBaseMs assignment).
   Fix: a Robolectric/fake-`HlsStart` Controller test asserting `realPosition()` after `openSession`, and an Apple test through the `changeStream` seam (or extract the assignment into a tested function). Add routing-decisions anchor rows (WO-04 pattern).
   Acceptance: mutation-revert of each call site fails a test.

2. **Coalesce Android hidden-controls seeks + restore feedback.**
   Each D-pad press while controls are hidden calls `controller.seekTo(...)` immediately (`PlayerScreen.kt:736-742`): on remuxes that's a full stream re-request per press (plus the origin probe — see WO-05 task 5), on transcodes a new HLS session per press (last-wins guarded, but real encoder churn). Web deliberately coalesces via `nudge()`; Android doesn't — and #42 removed the `poke()` reveal the old arms had, so seeks now give zero visual feedback.
   Fix: accumulate deltas against a frozen base with a ~300 ms quiet timer (mirror web's `nudge()`), and show a transient position toast/HUD on seek.
   Acceptance: mashing FF 5× on a transcode plan opens ≤1 new session (instrument or log-count); seek shows position feedback.

3. **tvOS: blind swipe-seeks.**
   `.onMoveCommand` on the bare surface (`PlayerView.swift:218, 437-446`) turned Siri-Remote swipes from reveal-controls into an immediate ±10/30 s jump with no on-screen confirmation. Fix: show a transient position HUD (or `revealControlsFromRemote()`) from `seekFromRemote`. Acceptance: device check — a swipe visibly indicates the jump.

4. **Small hardenings.**
   - Apple doesn't clamp negative `mediaOriginMs` (`PlayerController.swift:2377`); Android does (`MediaOrigin.kt:17`). Add `max(0, …)`.
   - #44's init.mp4 tier-bit rewrite silently skips inits >1 MiB (`http/hls.rs:1415` area) — log the skip so a High-tier playback failure on a huge init is diagnosable. Optionally gate the rewrite to copy-method sessions.
   - `docs/ANDROID-CLIENT-PARITY.md:86` — update the timeline-anchor line for #58 (also listed in WO-09).

5. **Device verification batch (fold into WO-06's device session).**
   - AVPlayer actually plays High-tier HEVC HDR via #44's collapsed master + relabelled init on physical hardware; Main-tier/DV titles unaffected.
   - Media3 on-device: the `X-Plurx-Media-Origin-Ms` header lands via `onTransferStart`/`responseHeaders` before the first 10 s progress post.
   - Android TV: D-pad auto-repeat seek behavior under tunneled playback (task 2's fix), and ExoPlayer indifference to the cleared hvcC tier bit.

## Don't

- Don't change the wire contract (JSON field + progressive header) — additive and correct; old-server fallback verified on both clients.
- Don't re-review the server integration test (`http/mod.rs:5182-5236`) — its mid-GOP fixture design is exactly right and documented.
