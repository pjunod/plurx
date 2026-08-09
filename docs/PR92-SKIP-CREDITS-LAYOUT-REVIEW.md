# PR #92 review — Skip Credits button layout

**Verdict:** CHANGES REQUESTED · **Verified against:** `origin/main` @
`91522c724b7fd5672a145b3468e43c3c69624164` · **PR head:** `6ce3ff0` on
`agent/fix-skip-credits-layout` (1 commit, 2 files, +72/−10) ·
**Reviewed:** 2026-08-09

The layout change itself is right, small, and brings Apple to parity with what
Android has always done. Two things stop it: it will red at CI `preflight`
before a single test runs, and the test it ships proves the wrong thing.

---

## What the PR actually does

Three edits, all in `clients/apple/Sources/PlayerView.swift`:

| Edit | Before | After |
|---|---|---|
| New `PlayerTrailingControlRow` (`:429-446`) | — | `HStack { Spacer(minLength: 0); control.fixedSize(horizontal: true) }.frame(maxWidth: .infinity)`, declared **outside** the `#if os(iOS)` block at `:363-427`, so it compiles on both targets |
| Marker row (`:1041-1045`) | `#if os(tvOS)` inline `HStack`+`Spacer` / `#else` bare `markerButton(marker)` | `PlayerTrailingControlRow { markerButton(marker) }` on both platforms |
| `markerButton` label (`:1390-1391`) | `Label(...).font(...)` + `#if os(iOS) .frame(maxWidth: .infinity)` | `.frame(maxWidth: .infinity)` deleted |

**The diagnosis in the PR body is correct.** `playbackControls` is a
`VStack(alignment: .leading)` that gets `.frame(maxWidth: .infinity)` at
`:1072`, so it proposes the full player width to each child. With
`.frame(maxWidth: .infinity)` on the `Label`, the `.borderedProminent` Button
accepted that whole proposal — a 1366-pt Skip Credits bar on iPad. Deleting it
is the fix; the `Spacer` is what puts the natural-width button on the trailing
edge.

**tvOS is genuinely unchanged.** The extracted struct is byte-equivalent to the
old inline `#if os(tvOS)` branch except for the added
`.frame(maxWidth: .infinity)`, which is a no-op there — an `HStack` containing a
`Spacer` already expands to its proposal.

**This matches Android.** `clients/android/.../player/PlayerScreen.kt:814-819`
already renders the marker as a natural-width `TvButton` at
`Alignment.BottomEnd`. The Apple client was the outlier.

---

## Blocking

### B1 — `CURRENT_PROJECT_VERSION` is not bumped, so CI fails at preflight

`clients/apple/project.yml:17` is still `37` on both `origin/main` and `pr92`;
the PR's whole diff is two Swift files. `clients/apple/Sources/**` is a release
path (`validation/mobile_versions.py:20-23`), so this is machine-enforced.
Reproduced on the PR head:

```bash
$ PLURX_VALIDATION_MODE=changed-from \
  PLURX_VALIDATION_BASE=91522c724b7fd5672a145b3468e43c3c69624164 \
  python3 -m validation.mobile_versions
mobile version validation failed:
  - Apple release inputs changed, so CURRENT_PROJECT_VERSION must increase above 37; found 37
exit=1
```

`ci_scope` on this diff emits `apple=true · mobile_version=true`, and
`ci.yml:53-67` runs that check in `preflight`, which every expensive job —
including `apple` — depends on. **The Swift tests never get to run.** Bump
`CURRENT_PROJECT_VERSION` 37 → 38.

Every one of the last six merged Apple PRs bumped it exactly once, strictly
monotonic: 32→33→34→35→36→37 (`d4977ee`, `27ac86c`, `d25905a`, `ec0673b`,
`23e76a9`, `122efb4`).

### B2 — the new test does not pin the fix; reverting the fix leaves CI green

`testSkipMarkerRowStaysCompactAndTrailingAtEveryTouchWidth`
(`AppleClientTests.swift:2795-2845`) hosts this as its root view:

```swift
PlayerTrailingControlRow {
    Button("Skip Credits") {}
        .buttonStyle(.borderedProminent)
        .reportLayoutFrame()
}
```

It never touches `PlayerView`, `playbackControls`, or `markerButton`. It
constructs a fresh `Button` with a hardcoded string and measures the wrapper in
isolation. Consequences, all textually verifiable from the diff:

- Revert `PlayerView.swift:1041-1045` to the old `#if os(tvOS)` / `#else
  markerButton(marker)` form, keep the struct — **test still compiles, still
  passes.** iPad regresses to the full-width bar; CI is green.
- Restore `.frame(maxWidth: .infinity)` on the `Label` at `:1391` — **test still
  passes**, because the test's Button uses a plain `Text` title, not
  `markerButton`.

So the test asserts that a 15-line struct written in this PR behaves the way its
own docstring says. The user-visible behavior — *the marker row in the real
player is compact and trailing* — is asserted by nothing. This is the same
shape as PR #81's three unpinned headline fixes.

The cheap fix is to make the test measure the production view. The neighbouring
`testIPadPlaybackWideRowExpandsTheTimelineAcrossThePlayer` (`:2749-2793`) has
the same weakness, so if reaching `playbackControls` is impractical, the minimum
acceptable pin is to route the test through `markerButton`'s actual content:

```swift
PlayerTrailingControlRow {
    Button {} label: {
        Label("Skip Credits", systemImage: "forward.end.fill")
            .font(.system(.caption, design: .monospaced))
    }
    .buttonStyle(.borderedProminent)
    .reportLayoutFrame()
}
```

That at least fails if `.frame(maxWidth: .infinity)` comes back on the label.
Pinning the call site needs a `PlayerView`-level host or an extracted
`markerRow` view the test can instantiate.

---

## Should fix

### S1 — `CHANGELOG.md` and `docs/STATUS.html` are untouched

`git diff --stat origin/main pr92 -- CHANGELOG.md docs/STATUS.html` is empty.
The written convention (`docs/APPLE-NATIVE-SUBTITLES-PLAN.md:460-462`) is to
update `[Unreleased]` and the status page "in the same commits as the behavior
they describe." 5 of the last 6 Apple PRs touched `CHANGELOG.md`; `ec0673b`
(the analogous iPad-chrome fix, #89) touched `docs/STATUS.html` too.

Neither is machine-enforced, but `docs/STATUS.html:101` currently reads "Apple
build 37 source, not yet uploaded" and `:269` carries a pending device
acceptance matrix "On Apple build 37" — both of which go stale the moment B1 is
fixed.

### S2 — `.fixedSize(horizontal: true)` removes the label's only wrapping affordance

On iOS the `fixedSize` is redundant for the stated goal: once
`.frame(maxWidth: .infinity)` is off the label, a `.borderedProminent` Button
already takes its natural width, and the `Spacer` already pins it trailing.
What `fixedSize` adds is a guarantee the button will *never* compress or wrap —
it proposes `nil` width, so the `Text` reports its single-line ideal width and
overflows rather than truncating.

Marker labels are a closed set today — `classify_chapter`
(`crates/plurxd/src/http/stream.rs:499-533`) returns `&'static str`, so the only
two values a plurx server can emit are `"Skip Intro"` (10 chars) and
`"Skip Credits"` (12). Chapter titles steer classification but can never become
the label. So this is not a live overflow.

It is still worth a `.lineLimit(1)` on the `Label`, for two reasons the repo
already agrees with: `Marker.label` is a plain decoded `String`
(`clients/apple/Sources/Models.swift:307`) with no client-side validation, and
the adjacent player chrome does exactly this at `PlayerView.swift:1121` and
`:1133` (25+ `.lineLimit(1)` sites across `Sources/`). The test also fixes the
content size category at default — `XCTAssertLessThan(buttonFrame.width, 180)`
is only checked at Large, so a `.caption` scaled to AX5 on a 320-pt phone
(280 pt of content width after the player's `.padding(20)` at `:658`) is
untested in both directions.

---

## Notes, not objections

**The `#if os(iOS)` placement is right.** The new test sits inside the
`:2747-2846` iOS block, and `PlayerTrailingControlRow` is declared outside
`#if os(iOS)`, so the tvOS target compiles. Precedent confirms the test target
can see internal `Sources` types (`PlayerTouchWideRow` at `:2755`).

**CI does run this, on iPhone only.** `ci.yml:195-218` creates an
`iPhone-16-Pro` / iOS 18.5 sim and an `Apple TV 4K` / tvOS 18.5 sim, and
`make apple-test` (`Makefile:280-288`) runs the suite on both. **There is still
no iPad destination** — the same gap as PR #89. The test synthesizes 744/1024/
1366-pt widths by setting `UIHostingController.view.frame`, so the assertions do
run, but on iPhone traits: no real iPad size classes, no Slide Over, no Stage
Manager. Reasonable, and worth saying out loud in the PR body rather than
implying "iPad Pro 13-inch testing" was CI.

**"`make check` was run" proves nothing here.** `make check` is
`validation-lint history-check operations-check rust-check`
(`Makefile:58-59`) — no `xcodebuild`, no `xcodegen`, no `swiftc`, and no
SwiftLint anywhere in the repo. On a Swift-only diff a green `make check` means
the Rust workspace and doc contracts are intact. It notably does **not** run
`mobile-version`, which is why B1 wasn't caught locally. `make check` on `pr92`
does pass: `catalog ok: 18 points · 21 checks · 408 audited files`,
`operations-check` 10/10, `history ok: 269 corrective commits`.

**Adjacent gap, out of scope.** Offline items always carry `markers: []`
(`OfflineDownloadManager.swift:77`; `DetailView.swift:1617-1624` never passes
them), so downloaded playback shows no skip button at all on Apple. Not this
PR's problem — worth a separate issue.

---

## What I could not verify

No macOS runner here, so nothing was compiled or run on a simulator. Every
claim above is from reading the pinned tree, plus three commands actually
executed on `pr92`: `validation.mobile_versions` (failed, quoted above),
`validation.ci_scope`, and `make validation-lint / operations-check /
history-check` (all green). The layout assertions themselves — that the button
lands at the trailing edge and stays under 180 pt at each width — are
unexecuted.

## Merge checklist

- [ ] `clients/apple/project.yml:17` → `CURRENT_PROJECT_VERSION: "38"` (B1)
- [ ] Test exercises `markerButton`'s real content, or `playbackControls` (B2)
- [ ] `CHANGELOG.md` `[Unreleased]` entry (S1)
- [ ] `docs/STATUS.html` build 37 → 38 at `:101` and `:269` (S1)
- [ ] `.lineLimit(1)` on the marker `Label` (S2)
- [ ] Device pass on a real iPad — CI cannot cover it
