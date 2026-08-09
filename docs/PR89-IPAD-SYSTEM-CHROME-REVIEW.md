# PR #89 review — the right mechanism, coupled to one state too few

**Status:** review complete · **Reviews:**
[PR #89](https://github.com/pjunod/plurx/pull/89) "Hide iPad player system
chrome with playback controls", head `41fa3fbc` on
`codex/fix-ipad-player-system-overlays` (draft) · **Verified against:**
`origin/main` @ `d25905ae` · **Written:** 2026-08-08 · **Outcome:** review
only; no code was changed · **Verdict:** APPROVE WITH CHANGES

Companion to [APPLE-CLIENT-PARITY.md](APPLE-CLIENT-PARITY.md) (the matrix this
change belongs in) and [PLAYBACK.md](PLAYBACK.md) §"Native clients" (the
routing inventory that PRs #86 and #87 each extended). Read
[CHANGELOG.md](../CHANGELOG.md) `[Unreleased] → Fixed` for the entry under
review.

**How it was reviewed.** The PR was fetched as `refs/pull/89/head` into a
clean cloud clone and checked out read-only at `41fa3fbc` — never the working
tree, never local `main`. Three scoped passes ran in parallel (adversarial
Swift/SwiftUI correctness, docs/versioning/bookkeeping, presentation-context
tracing); every headline finding was then handed to a fourth pass whose
default posture was *refuted*. Two findings did not survive intact and are
recorded in §6 rather than deleted, because they are the ones a future reader
will re-derive. There is no Swift toolchain and no Apple device here, so every
finding is marked **CONFIRMED** (traced to `file:line`) or **SUSPECTED**
(needs a compiler or a device), and §7 is the device script that settles the
residual.

**Severity legend:** **BLOCKING** = a standing project rule is violated or
the change ships a false claim · **SHOULD-FIX** = a real user-visible defect
or an unpinned behaviour · **NIT** = polish.

## 1. Verdict — ship it after §2 and §3.1

The premise is correct and was verified, not assumed. Nothing in
`clients/apple/` before this commit set `statusBarHidden`,
`prefersHomeIndicatorAutoHidden`, `persistentSystemOverlays`,
`UIStatusBarHidden`, or `UIRequiresFullScreen` — a repo-wide grep at
`644a39b0` returns zero hits, and `clients/apple/Support/iOS-Info.plist`
does not exist (XcodeGen synthesises the plist from
`clients/apple/project.yml:38-75`, which sets none of those keys). So the iPad
really did keep its status bar and Home indicator up for the entire film, and
a custom `AVPlayerLayer` surface really does have no
`AVPlayerViewController` to retire them on its behalf.

The mechanism is also right. `.statusBarHidden` /
`.persistentSystemOverlays` are applied at the **outermost** modifier
position of `PlayerView.body`
(`clients/apple/Sources/PlayerView.swift:694-700`), both presentation sites
are full-screen covers (`DetailView.swift:650` → `:657`,
`DownloadsView.swift:75` → `:76`), and `UIViewControllerBasedStatusBarAppearance`
is unset so it defaults to `YES`. A `.fullScreen` modal owns status-bar
appearance, so the preference reaches a controller that can honour it. Tying
chrome to control visibility is what `AVPlayerViewController` itself does —
this is parity, not invention.

What is wrong is narrow: the preference is derived from `controlsVisible`
alone, and there are two other states in this view that mean "the viewer is
looking at something" (§3.1). Plus the bookkeeping the project's own rules
require (§2), and a CHANGELOG sentence that claims a platform outcome nothing
in the diff produces (§2.3).

**One process note before the findings.** The PR targets
`agent/fix-apple-periodic-stall`, but #87 is already merged as `d25905ae` and
`git diff d25905ae 644a39b0` is **empty** — the base branch and `main` are
identical in content. Retarget to `main` and #89 becomes what it actually is:
a single 57-line commit. Reviewed as-is it reads as a 262-line PR across 11
files, 10 of which are already shipped.

## 2. BLOCKING — the bookkeeping the project rules require

### 2.1 Four builds of version drift, in the two places that are read

`clients/apple/project.yml:18` is now `CURRENT_PROJECT_VERSION: "36"`.
`MARKETING_VERSION` correctly stays `"0.2.7"` and matches `Cargo.toml:14`, so
`validation/mobile_versions.py` passes — I ran it: *mobile versions and
changed-app build counters are valid*. That script is the only gate, and it
reads exactly three files (`validation/mobile_versions.py:80-200`); grepping
`validation/*.py` for `README|STATUS` returns nothing. So prose rots freely,
and it has:

| Where | Says | Should say |
|---|---|---|
| `clients/apple/README.md:15` | ``build `32` in project.yml — working`` | 36 |
| `clients/apple/README.md:18` | `Build 29 adds app-managed offline viewing` | (historical, but check) |
| `docs/STATUS.html:101` | `Apple build 32 source, not yet uploaded` | 36 |
| `docs/STATUS.html:269` | `upload Apple build 32 to TestFlight` | 36 |
| `docs/APPLE-CLIENT-PARITY.md:12` | `Status (2026-08-06): source is v0.2.7, Apple build 32.` | 36, 2026-08-08 |
| `docs/APPLE-NATIVE-SUBTITLES-PLAN.md:381` | `The next Apple upload is build 32 or higher` | ≥36 |

**This is a standing lapse, not a #89 regression, and that is the argument for
fixing it here.** `project.yml` was at `32` when `docs/STATUS.html` and
`clients/apple/README.md` were last touched (`bb3f6a1`, PR #79). #85
(`d4977ee`), #86 (`27ac86c`) and #87 (`d25905ae`) each bumped the build and
none of them touched either file. #89 makes it four builds stale. The
semantic claim on both pages — "not yet uploaded to TestFlight" — is still
true; only the number is wrong, which is exactly the failure mode that makes
a reader confident about something untrue.

### 2.2 The status page has no row for this, and CI cannot grow one

The repo states its own rule twice:
`docs/APPLE-NATIVE-SUBTITLES-PLAN.md:461-462` ("update `CHANGELOG.md`
`[Unreleased]` and the status page (`docs/STATUS.html`) in the same commits as
the behavior they describe") and `docs/OFFLINE-VIEWING-REVIEW.md:727-728`
("the pinned ledger, whose standing rule is that it updates in the same
commit as the behavior"). Nothing enforces it —
`validation/ci_scope.py:37` only routes `STATUS.html` to the docs-only fast
lane. Three things are now wrong or missing:

- `docs/STATUS.html:90` — the header stamp still reads `2026-08-06 · offline
  viewing and observability merged`, which predates #85, #86, #87 and this PR.
- `docs/STATUS.html:195` — the Apple viewer item enumerates shipped behaviour
  (cinematic details · native text subtitles · staged PGS overlays · truthful
  delivered-range badges · stable seeking/recovery · offline downloads) and has
  no entry for player system chrome.
- **No `👤 device` acceptance row exists for this change, and no automated
  surface can substitute.** The Apple CI job provisions an iPhone-16-Pro and
  an Apple TV simulator only — there is no iPad destination anywhere in
  `.github/workflows/ci.yml`. Everything this PR claims about iPadOS chrome is
  therefore unobservable in CI *in principle*, not just in practice. That is
  the strongest reason the operator checklist needs the row.

### 2.3 The CHANGELOG claims an outcome no API in the diff produces

`CHANGELOG.md:430-438` (correctly under `## [Unreleased]` → `### Fixed`) says
the change retires "the separate status bar, Home indicator, **or
multitasking/window controls**". Two problems:

- **Nothing in the diff addresses window controls.**
  `.persistentSystemOverlays(.hidden)` (`PlayerView.swift:333`) is the SwiftUI
  spelling of `prefersHomeIndicatorAutoHidden`. No repo doc, test, or device
  note backs the multitasking-control claim, and the new test
  (`AppleClientTests.swift:2545-2553`) asserts only the resolver's own return
  values — it cannot and does not prove what iPadOS renders.
- **In Split View / Slide Over / Stage Manager the fix silently no-ops.**
  `UIRequiresFullScreen` is unset, so multitasking is enabled, and in a
  non-full-screen iPad window the status bar is system-owned and
  `prefersStatusBarHidden` is ignored. The Home indicator still follows the
  controls; the status bar does not. Worth saying out loud, because Paul's
  freeze reports are presumably full-screen and the doc shouldn't promise more
  than the full-screen case.
- **Scope is misstated as iPad-only.** The gate is `#if os(iOS)`
  (`PlayerView.swift:311`, `:694`) with no `userInterfaceIdiom` check, and
  iPhone portrait playback exists (`project.yml:43-46` lists Portrait;
  `PlayerView.swift:1254-1268` has a two-row transport fallback "only when a
  portrait phone cannot fit it safely"). iPhone behaviour changes too. Either
  say so, or gate on `.pad` if iPhone was out of scope.

Minimal fix: soften to "the status bar and the Home indicator, in a
full-screen window", say iPhone as well as iPad, and hang the
multitasking-control question on the §2.2 device row.

## 3. SHOULD-FIX

### 3.1 The chrome leaves while the viewer is still reading — two deterministic cases

`PlayerSystemOverlayPreferences.resolve` takes `controlsVisible` and nothing
else (`PlayerView.swift:316-321`). But `controlsVisible` is not the only state
that means the viewer is engaged:

**The Playback info panel.** `overlayVisibility.playbackInfo` is `showStats`
alone (`PlayerView.swift:704-717`), `shouldAutoHideControls` deliberately
excludes it (`:720-727` — and there is a test pinning that independence,
`AppleClientTests.swift:2533-2541`), and `hideControls()` does not clear it
(`:762-778` touches only `activeOptionMenu` and `controlsVisible`). So: open
Playback info from the transport (`:1412` or `:1455`), wait 4 s, and the
status bar and Home indicator vanish while a diagnostics panel the viewer
explicitly opened stays on screen. This is the case the change gets wrong on
purpose-built input, not on a race.

**The terminal failure screen.** `failed = true` is always set alongside
`isChangingStream = false` (`PlayerController.swift:1656-1657`, `:1663-1664`,
`:1771-1772`, `:2026-2027`), so `shouldAutoHideControls(visible: true,
scrubbing: false, changingStream: false, optionMenuOpen: false)` returns
`true` and the auto-hide task fires 4 s into the "Couldn't start playback"
screen (`PlayerView.swift:819-841`) — which #85/#86/#87 made reachable. The
viewer is left on a black screen with an error message, no status bar, no Home
indicator, and no transport. The `Close` button (`:830-832`) still works, so
this is cosmetic, not a lockout (see §6.2).

Minimal fix, one line plus a parameter:

```swift
PlayerSystemOverlayPreferences.resolve(
    controlsVisible: controlsVisible || showStats || controller.failed
)
```

**Judgement call, not a finding:** the chrome also leaves 4 s after the viewer
*pauses* — `shouldAutoHideControls` has no pause term, and
`.onChange(of: controller.isPlaying)` (`:663`) is direction-agnostic, so
pausing reveals the controls and then lets them (and the chrome) go.
`AVPlayerViewController` keeps its controls up while paused. Decide
deliberately and say which you chose in the CHANGELOG; don't inherit it by
omission.

### 3.2 Toggling the status bar moves five overlays that don't ignore the safe area

`ZStack(alignment: .topLeading)` at `PlayerView.swift:512` applies
`.ignoresSafeArea()` per child, at exactly four sites — `:513` (`Color.black`),
`:520` (`PlayerSurface`), `:526` (tvOS reveal catcher), `:543` (iOS tap
catcher). These five children do **not** ignore it, and are therefore laid out
in a rect whose top inset collapses from ~20–24 pt to 0 when the status bar
hides:

| Child | Line | Anchor | Moves |
|---|---|---|---|
| notice / error banner | `:601-608` | top | full inset |
| `PlaybackStatsView` | `:567-573` | top-trailing | full inset |
| `streamChangeProgress` | `:578-582` | centre | ~half |
| `findingNext` "Up next…" | `:586-594` | centre | ~half |
| `failureView` | `:549` (def. `:819-841`) | centre | ~half |

The controls `VStack` (`:552-563`) is gated on `controlsVisible` itself, so it
leaves in the same transaction and cannot be seen moving — it is not part of
this finding. `.persistentSystemOverlays` does **not** change the bottom
inset, so the top edge is the only delta. And because every flip happens
inside `withAnimation(.easeInOut(duration: 0.2))` (`:746-748`, `:754`,
`:766-768`), this is an animated ~24 pt slide, not a frame-skip.

Two windows where it is observable rather than theoretical:

- **Playback info** — the §3.1 case: the panel the viewer is reading slides up
  ~24 pt as the clock disappears, then back down when they tap to restore.
- **Menu-driven notices** — every subtitle/quality/audio button calls the
  controller and then `revealControls()` (`:1668-1672`, `:1682-1686`,
  `:1698-1702`, `:1713-1717`). `selectSubtitle` raises the notice
  synchronously (`PlayerController.swift:827`, `:835`) *before* returning, so a
  5 s notice (`PlayerController.swift:862`) starts microseconds before the 4 s
  window (`PlayerView.swift:474`) restarts: the banner slides ~24 pt with
  roughly a second left to live, every time.

The `SUSPECTED` half: whether a `ZStack` mixing ignoring and non-ignoring
children reflows the non-ignoring ones on an inset change is framework
behaviour I cannot pin to a line in this repo. It is the standard reading of
the `ZStack { Color.black.ignoresSafeArea(); content }` idiom the author used
four times deliberately, and §7 settles it in one capture.

Minimal fix: make the non-transport overlay layer inset-independent — group
the banner, the two centred spinners and `PlaybackStatsView`, give the group
`.ignoresSafeArea()`, and re-establish their insets with explicit padding — so
no overlay position depends on status-bar state. Fixing §3.1 shrinks the
observable windows but does not remove the class.

### 3.3 Nothing pins the behaviour — deleting the modifier keeps CI green

`AppleClientTests.swift:2544-2553` calls
`PlayerSystemOverlayPreferences.resolve` directly and never constructs
`PlayerView` or `PlayerSystemOverlayModifier`. `resolve` has exactly two
references in the tree: that test, and the application site at
`PlayerView.swift:695-700`. **Delete `:695-700` and the entire suite passes.**
Confirmed further: no other test, snapshot, validation script or contract file
touches these symbols (`tests/ui-structure.golden` is web DOM only;
`tests/playback/routing-decisions.toml`, `tests/contracts/native-api.json` and
`tests/operations/test_contracts.py` have nothing on system overlays); there
is no `SWIFT_TREAT_WARNINGS_AS_ERRORS` / `-Werror` anywhere, and Swift emits
no unused-declaration warning for an unreferenced `private struct` anyway. The
assertions are also a literal restatement of the two-line function body at
`:317-320` — no threshold, no policy, no branch condition.

This file already has the tool for a real test: `UIHostingController` + a
`UIWindow`, used at `AppleClientTests.swift:2580-2607` and `:2674-2745`. Make
`PlayerSystemOverlayModifier` non-`private` (`:324`) and assert the preference
reaches the host:

```swift
let host = UIHostingController(rootView: Color.clear.modifier(
    PlayerSystemOverlayModifier(preferences: .resolve(controlsVisible: false))))
// window + layout exactly as at AppleClientTests.swift:2604-2608
XCTAssertTrue(host.prefersStatusBarHidden)
```

That fails if the modifier leaves the chain or if the preference stops
propagating — which is the thing the PR is actually claiming.

### 3.4 The doc rows PRs #86 and #87 both added, skipped here

`docs/PLAYBACK.md:110-120` is the native-client routing inventory, and
`tests/validation/test_playback_routing_inventory.py:22-60` forces
set-equality between its ids and `tests/playback/routing-decisions.toml`
(40 entries) plus existence of every `source_anchor` / `test_anchor`. #86
added `apple.buffering-recovery` (`PLAYBACK.md:115`) and #87 added
`apple.hls-buffer-window` (`:116`), each touching both files. This change has
the identical shape — a pure resolver plus an XCTest pin — and would slot in
as `apple.player-system-chrome` with `source_anchor`
`PlayerSystemOverlayPreferences.resolve` and `test_anchor` the new test name.
The gate only bites once a row exists, so adding neither passes silently. It
is arguable that chrome is not a *transport* fork; that argument was not made,
and the two-PR precedent is unbroken.

Also under-describing the iOS player now: `docs/APPLE-CLIENT-PARITY.md:50`
(the Transport row) and `clients/apple/README.md:52-56`, `:80-83` (the
on-demand player bullets).

### 3.5 Adjacent, pre-existing, in a file this PR should be editing anyway

`clients/apple/README.md:301-303` still states the retired retention
contract — "60 seconds ahead · 30 seconds behind · 30 seconds for a retry" —
which #87 replaced with 180 s (`docs/PLAYBACK.md:116`;
`PlayerController.swift:409-413`). Since §2.1 already sends you into this file
for the build number, fix this line in the same pass.

## 4. NIT

- **`PlayerView.swift:336-338`** — the new declarations close `#if os(iOS)` at
  `:336` and immediately reopen it at `:338`. Two adjacent identical gates;
  delete the pair and let the new types live in the existing block.
- **`PlayerView.swift:312`** — `PlayerSystemOverlayPreferences` is `internal`
  only so the test can reach it, while the type that carries the behaviour
  (`PlayerSystemOverlayModifier`, `:324`) is `private` and therefore
  untestable. The `Equatable` conformance has no consumer anywhere. §3.3's fix
  inverts this correctly.
- **Returning from PiP** — `canStartPictureInPictureAutomaticallyFromInline =
  true` (`PlayerSurface.swift:47`) auto-starts PiP on backgrounding,
  `PlayerView` stays presented, and
  `restoreUserInterfaceForPictureInPictureStop…` just completes
  (`PlayerSurface.swift:118-123`). So a viewer can return from PiP to a screen
  with no status bar, no Home indicator and no controls — indistinguishable
  from a hung app until they tap. Pre-existing for the controls; this change
  removes the last two system affordances. §3.1's fix does not cover it.
- **`docs/APPLE-NATIVE-SUBTITLES-PLAN.md:656`** asserts `STATUS.html` was
  "refreshed 2026-08-06"; git says it was last touched at `bb3f6a1`.

## 5. What is right and should not be relitigated

- **Compilation.** `import SwiftUI` present in both files
  (`PlayerView.swift:1`, `AppleClientTests.swift:4`); `Visibility` is
  `Hashable` so the synthesised `Equatable` and `XCTAssertEqual(...,
  .automatic)` both resolve; `persistentSystemOverlays` is iOS 16+ against an
  iOS 17 target (`project.yml:11`), so no availability guard is owed.
- **Platform gating.** `Tests/` is shared by both test targets
  (`project.yml:107-129`) and the new test's `#if os(iOS)` wrapper
  (`AppleClientTests.swift:2544`, `:2554`) matches the established pattern
  exactly. `.statusBarHidden` is unavailable on tvOS, so the gate is
  load-bearing and correct. **"tvOS is unchanged" verified.**
- **The `#if` inside the modifier chain** is not a new risk — the same body
  already does it for tvOS at `PlayerView.swift:653-666` in the base, and
  `playbackControls` does it at `:884-897`.
- **"Four-second control lifetime" verified.**
  `PlayerView.swift:474` — `controlAutoHideDelayNanoseconds: UInt64 =
  4_000_000_000`, consumed at `:653`.
- **The test-count claim is exactly right.** 116 `func test` declarations: 95
  unconditional, 11 `#if os(iOS)`, 10 `#if os(tvOS)` → 106 iOS / 105 tvOS. The
  new test is the 11th iOS-only case and accounts for the +1 on iOS alone.
- **The offline path is the calm case.** `startOffline`
  (`PlayerController.swift:659-684`) never starts the recovery monitor and
  offline is `isVOD`, so `isChangingStream` never toggles there — the new
  coupling sees only tap-toggle and play/pause.
- **The build bump was owed and is correct.**
  `validation/mobile_versions.py:187-192` forces a strictly increasing
  `CURRENT_PROJECT_VERSION` when Apple release inputs change; `35 → 36` is
  exactly that.

## 6. Findings that did not survive the refutation pass

Recorded so they are not re-derived.

### 6.1 "The notice banner always jumps one second before it disappears"

The stated mechanism was wrong. A notice arriving does **not** restart the
auto-hide clock: `autoHideGeneration` (`:499`) is bumped only by
`restartAutoHideTimer()` (`:758-760`), reached from `revealControls()`,
`toggleControls()` and the `.onChange` handlers at `:663-667` (plus tvOS
`:681-683`). `controller.playbackNotice` is not among them. The conclusion
survives only for the **menu-driven** notice sites, by the different route
recorded in §3.2. For the async sites — `PlayerController.swift:1425`, `:1539`
(PGS preparation/render failures) and `:2390` (`applySubtitleSelection`
fallback after a reopen) — the notice can fire while controls are already
hidden, and then nothing moves at all. Overlap is not guaranteed there.

### 6.2 "The failure screen's chrome can never come back"

`failureView` contains `Button("Close") { dismiss() }`
(`PlayerView.swift:830-832`) with no `.allowsHitTesting(false)`, above the tap
catcher in z-order, and dismissing the cover drops the whole preference. More
interestingly, the absorption argument was self-defeating: `failureView` ends
in `.frame(maxWidth: .infinity, maxHeight: .infinity).background(Color.black)`
(`:839-841`) with **no** `ignoresSafeArea`, while the tap catcher at `:541-545`
does ignore it — so under the same layout model §3.2 depends on, the ~24 pt
top strip still belongs to the tap catcher and a tap there restores the
chrome. §3.2 and a lockout cannot both be true. Downgraded to the cosmetic
case in §3.1.

### 6.3 Also checked and clear

The chrome does cycle once per stall-recovery episode —
`.onChange(of: controller.isChangingStream)` (`:664`) fires on both edges,
`shouldAutoHideControls` pins the chrome up while `changingStream` (`:723`),
and it fades 4 s after the reopen completes (`PlayerController.swift:1057`,
`:1293`). At the #87-reported cadence that is once every ~6 minutes: one
animated fade, not a strobe. Not worth a finding on its own; §3.2's
inset-independence fix removes the visible part.

## 7. The one device capture that settles everything left

Everything `SUSPECTED` above collapses into a single 30-second recording on a
physical iPad in a **full-screen** window (not Split View):

1. Start any film. Tap **Playback info** in the transport. Wait out the 4 s
   auto-hide **without touching the screen**, then tap once to restore.
   - Does the status bar leave and come back? → §1 mechanism.
   - Does the stats panel's top edge move ~24 pt each way? → §3.2, and with it
     the whole mixed-`ignoresSafeArea` reflow question.
   - Does the Home indicator leave? Does anything at the bottom move? →
     expected: indicator yes, layout no.
   - Does the ••• multitasking control leave? → §2.3. If it does not, soften
     the CHANGELOG.
2. Repeat step 1 with the app in **Split View**. Expected: the Home indicator
   follows the controls, the status bar does not. Confirms the §2.3 caveat.
3. Force a failure (point the client at a stopped server mid-film, or open a
   deleted file). Wait 4 s on the "Couldn't start playback" screen, then tap
   the top ~24 pt strip.
   - Status bar hides on an error screen? → §3.1.
   - Tapping the top strip brings it back? → jointly confirms §3.2's inset
     model and §6.2's refutation. If nothing happens, both fail together.
4. Pause and wait 4 s. Note whether losing the clock while paused is
   acceptable → the §3.1 judgement call.
5. iPhone, portrait and landscape: is the status bar even present while the
   transport is up? → §2.3's scope wording.
