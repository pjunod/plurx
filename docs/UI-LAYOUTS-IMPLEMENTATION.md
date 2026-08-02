# UI layouts & themes — implementation plan for the accepted slice

**Status:** ready to build · **Executes:**
[UI-LAYOUTS-PLAN.md](UI-LAYOUTS-PLAN.md) as amended by
[UI-LAYOUTS-REVIEW.md](UI-LAYOUTS-REVIEW.md) · **Written:** 2026-08-02 ·
**Verified against:** branch `docs/ui-layouts-proposals` @ `7fc1796`
(whose web app is main's — blob `d541671`)

This is the build order for the implementing agent. Read
[UI-LAYOUTS-PLAN.md](UI-LAYOUTS-PLAN.md) for what everything looks like
(and open `docs/mockups/` in a browser), then
[UI-LAYOUTS-REVIEW.md](UI-LAYOUTS-REVIEW.md) for why this plan is narrower
than the proposal, then work THIS document gate by gate, in order. Every
review finding cited here was independently re-verified against the code
on 2026-08-02 — DTO shapes, session telemetry, genre storage, client theme
infrastructure, and every contrast ratio (the review's numbers reproduced
exactly). File/line anchors below are against the blobs named above;
re-verify anchors at build time, the structure moves.

**Standing instructions.** Work gate by gate; a gate is not done until its
acceptance list passes. If any step seems to require a server API change,
a new route, a player change, or touching another surface's files — stop
and flag it instead of building it; §5 lists everything that was
deliberately deferred for exactly that reason. When a mockup and this
document disagree, this document wins; mockups illustrate structure, they
are not pixel law.

---

## 1. The slice

Ships in this plan, in order: **G0** frozen classic baseline → **G1**
layout registry with zero visual change → **G2** the `plex` layout pilot
(web: home, library, item detail) with **Amber** dark + corrected light →
**T1** the theme catalogue (Giallo, Silver, Void, VHS — plus Paper and
Tide if §8 box 2 is ticked) → **G3** a written go/revise/stop decision on
the abstraction → **G4** one second layout (§8 box 1 decides which).

Explicitly NOT in this plan — designed, parked, with pointers in §5:
`deck` (blocked on an API/product proposal), `guide` (blocked on
metadata + schedule + input contracts), `archive` (candidate experiment),
all native clients (separate plans per platform), per-layout theme
memory, README changes before the pilot is accepted.

Branch: create `feat/ui-layouts` from `docs/ui-layouts-proposals` (it is
main `2e0435f` + docs-only commits, so it merges back clean). All work is
in `crates/plurxd/src/web/index.html` — `include_str!`-embedded, so every
change needs `cargo build` + restart to see, and the Rust gate cannot see
JS errors: extract each `<script>` and `node --check` it, every commit
(project memory Rule 4e).

## 2. Review checklist → decisions

The review left boxes unchecked; this section answers them. Items marked
**[PAUL]** are the four still open in §8 — everything else is decided.

| Review box | Decision |
|---|---|
| Correct Amber/Giallo/Silver light tokens | Adopted, reviewer's values, re-verified ≥4.5:1 (§3.4). One additional fix of ours: Amber-dark `--bad` `#e5534b` → `#ee7168` — the reviewer checked against `--bg` only; on `--panel`/`--panel2` cards the old value measured 4.25:1/3.89:1. Held to the stricter surface set. |
| One global theme for M1 | Yes. `plurx_theme` stays the single global key. A layout's signature theme renders as a *recommendation row* in the menu ("Plex looks best in Amber — switch?"), never an automatic mutation. The per-layout-memory JSON schema from the review is recorded in §3.3 as deferred, not built. |
| Deck: API proposal or reduced telemetry | Neither half-ship. Deck exits this plan entirely; §5.1 records the two honest options and the endpoint sketch so a future `DECK-OPERATOR-PLAN.md` starts from facts. The mockup's operator console is the point of the layout — a "denser skin" version isn't worth a registry slot. |
| Guide: metadata/schedule/D-pad contract | Deferred to §5.2. Verified worse than the review states: genres aren't just unexposed — TMDB enrichment never persists them (the only genre handling in the tree is NFO `<genre>`→tags for home media). A genre channel needs a migration + enrichment change, i.e. a real proposal. |
| Split web/Android/Apple milestones | Yes. This plan is web-only. §5.4 seeds the two client plans with verified starting states (`ViewerColors` on Android; static forced-dark `Palette` on Apple). |
| Second layout after the pilot | **[PAUL]** §8 box 1 — Theater (default recommendation) vs Spine. |
| Archive Home experiment | **[PAUL]** §8 box 3. Data-feasible today (`recorded_at`/`added_at` both live on `ItemDto`) — it is the cheapest of the three parked layouts. |
| Paper + Tide themes | **[PAUL]** §8 box 2. Recommendation: adopt in T1 — both tables verified all-pairs ≥4.5:1 in both modes, and they fill real gaps (a light-first theme; a calm color family). |

## 3. The contract

### 3.1 Registry, persistence, capability (G1)

Mirror the shipped theme machinery exactly — `THEMES` /
`applyTheme()` / `themeMenuHtml()` sit at [index.html:826/854/1294]
(main blob; re-verify):

```js
const LAYOUTS = {
  classic: { name: "Classic", surfaces: ["desktop","mobile","tv"] },
  plex:    { name: "Plex",    surfaces: ["desktop","mobile","tv"] },
  // later entries declare the surfaces they actually support (deck will
  // omit "tv"); the menu never offers a layout the surface can't run.
};
localStorage.plurx_layout      // absent or unknown id → "classic"
<html data-layout="plex">      // set pre-paint, same <head> script that
                               // runs applyTheme() — a layout applied
                               // after first paint flashes classic chrome
```

Rules: unknown/unsupported stored id falls back to `classic` silently
(never an error, never shown as selectable); the appearance popover gains
a "Layout" section above "Theme"; the choice is per-browser like themes.
`surfaces` is decided from viewport/input class, one function, no user
agent sniffing.

### 3.2 The page-model seam (G1–G2, route by route)

Today each view owns the whole sequence — fetch, assemble, replace
`#main`, wire — e.g. home builds hub rails inline
([index.html:1623]) inside its view function, and the router
(`render()`, [index.html:5987]) just dispatches. A layout registry bolted
around those functions would duplicate fetches or reach into globals.
Split each converted route at one seam:

```
hash route
   │
   ▼
loadPage(route)  ──▶  page model (plain object)  ──▶  LAYOUTS[cur].render(page)
     (fetches;              (no DOM, no HTML)            (chrome + body HTML)
      unchanged                                              │
      API calls)                                             ▼
                                                   shared behavior wiring
```

Page model sketch (grow it as routes convert; keep it JSON-plain):

```js
// home
{ kind:"home", hubs:{ continue:[ItemCard...], next_up:[...],
  recent:[{library, items:[...]}] }, scan: {...} }
// library
{ kind:"library", library:{id,name,kind,count}, items:[...], sort, filter }
// item
{ kind:"item", item:{...}, files:[FileDto...], origin }
```

Conversion order: Home (G1, proves the seam under `classic`), then
library, then item detail (G2). Activity, settings, search, auth, and
the player stay on their existing renderers behind the layout chrome —
do not convert them in this plan. The one shared card partial keeps
serving every layout, parameterized by CSS (`--w`, label visibility),
as the mockups already demonstrate.

**Invariant:** converting a route must not change its network behavior.
Acceptance uses request logs: same endpoints, same counts, before and
after, per route.

### 3.3 Theme contract additions

Token set unchanged from the proposal (§2.3 there), plus:

- `darkOnly: true` (Void, VHS): appearance=light keeps the theme dark and
  the menu says so inline. No silent fallback to another theme.
- **Reduced motion:** every decorative animation — cursor blink, skeleton
  shimmer, VHS tracking bars, hover zooms, theater fades — sits behind
  `@media (prefers-reduced-motion: no-preference)`. The information the
  motion carried (working/idle, loading) must remain visible statically.
- **Forced colors:** selection, focus, and progress use system colors
  under `forced-colors: active`; verify the player progress bar and the
  poster focus ring specifically.
- Theme choice stays ONE global key. If per-layout memory ever returns,
  it is the review's versioned map (`{"v":1,"classic":"noirr",...}`,
  unknown ids ignored independently) — recorded here so nobody invents a
  second format, not scheduled.

### 3.4 Final token values (validated)

Every value below was machine-checked ≥4.5:1 for text/muted/prose/accent/
good/warn/bad against BOTH `--bg` and `--panel`, and btn-ink against
accent, on 2026-08-02. Re-run after any edit — the check is ~20 lines of
python (WCAG relative luminance; keep it in `scripts/` once it exists,
`scripts/contrast-check docs/UI-LAYOUTS-IMPLEMENTATION.md` style).

**Amber** (plex signature) — dark unchanged from the proposal EXCEPT
`--bad`; light replaces the proposal's failed values:

| token | dark | light |
|---|---|---|
| bg / panel / panel2 | `#191a1d` / `#212327` / `#2a2d32` | `#f3f4f6` / `#ffffff` / `#e9ebee` |
| line | `#383c43` | `#d5d9de` |
| text / muted / prose | `#eceef0` / `#9aa0a7` / `#c9cdd2` | `#1e2124` / `#5f666d` / `#3a4046` |
| accent / accent2 | `#e5a00d` / `#cc7b19` | **`#8b5e00`** / `#a95f16` |
| good / bad / warn | `#52b788` / **`#ee7168`** / `#f2c14e` | **`#246b49`** / **`#bd332d`** / `#8a6116` |
| radius / btn-ink | `8px` / `#1c1303` | `8px` / `#ffffff` |

Note: light-mode gold buttons keep the gold FILL only at large/bold
sizes; normal-weight accent text uses `#8b5e00`. If the fill reads muddy
in G2 screenshots, darken the fill, never lighten the ink.

**Giallo** — dark unchanged; light: accent `#a8720c`→**`#7f4e00`**, good
`#35855c`→**`#2c734d`**; everything else per the proposal table.

**Silver** — dark unchanged; light: warn `#8a7442`→**`#765f28`**.

**Void, VHS** — unchanged from the proposal (dark-only; all pairs pass).

**Paper, Tide** — if adopted (§8 box 2), exactly the review's tables
(UI-LAYOUTS-REVIEW.md §"Reviewer additions"); both verified all-pairs in
both modes. Signature pairings recorded there (Paper↔Spine, Tide↔Archive)
are recommendations only until those layouts exist.

### 3.5 Boundary (who owns what)

Adopt the review's corrected table verbatim — it replaces the proposal's
"structure vs paint" slogan:

| Axis | Owns | Must not own |
|---|---|---|
| Layout | navigation scaffold · page composition · density · information hierarchy · supported surfaces | palette values · semantic status meaning · playback behavior |
| Theme | palette · type family · corner/shadow treatment · decorative texture · brand lockup | DOM order · route availability · hidden information · input behavior |
| Accessibility preference | reduced motion · increased contrast · text scaling · forced-colors | brand identity · saved layout selection |

Every layout × theme combination must stay legible and operable; it need
not be equally pretty.

## 4. Gates

### G0 — freeze the classic baseline

Build `scripts/ui-baseline`: drives the running server with headless
chromium (`/opt/pw-browsers/.../chrome` in the sandbox; local Playwright
on the Mac), captures Home · library · category · search · item detail ·
activity · settings · auth · player-open at 1280×860 and 390×780, dark
noirr, plus a tab-order dump (sequence of focused element descriptors)
per route. Output to `target/ui-baseline/<name>.png` + `focus.json` —
gitignored; the SCRIPT is committed, goldens are regenerated from the
pre-change commit whenever needed.

**Accept:** two consecutive runs on an unchanged tree are pixel-identical
(deterministic enough to trust); the script exits nonzero on any route
that fails to render.

### G1 — registry with zero pixel change

`LAYOUTS` registry + `plurx_layout` + `data-layout` pre-paint + menu
section + capability filter (§3.1); Home converted to the page-model seam
(§3.2) with `classic` as the only complete layout.

**Accept:** `ui-baseline` output byte-identical to G0 under `classic`
(menu screenshot excepted — capture it as a new golden); unknown stored
layout falls back to classic; Home request log unchanged; `node --check`
clean; `make check` green.

### G2 — the Plex pilot

Plex chrome (sidebar per the proposal §3.2 + mockups), Home, one library
route, one item-detail route through the seam; library and detail loaders
extracted in the process (classic consumes the same loaders). Amber
dark + light with §3.4 values. Corner unwatched flags, hub rows, A–Z
scrubber, backdrop detail — per mockup structure.

**Accept:**
- Full route matrix operable under plex — unconverted routes (activity,
  settings, search) render their existing bodies inside plex chrome.
- Libraries with 0, 1, and 200+ items; items with no art, long titles,
  missing year; non-admin user (no Settings in sidebar).
- Keyboard-only pass of home → library → detail → play with visible
  non-color focus; hover-only affordances (play circle, captions) have
  keyboard/touch equivalents.
- Light mode screenshot sweep: no hardcoded-dark boxes (the 2026-07-22
  scar), contrast script passes on §3.4 values.
- Reduced-motion and forced-colors behaviors per §3.3.
- Request-log invariant per route; `ui-baseline` under classic still
  byte-identical (plex must not leak into classic).
- Screenshots regenerated for `docs/mockups`-style states → G3 evidence.

### T1 — the theme catalogue

After G2. Add Giallo, Silver, Void, VHS (+ Paper, Tide per §8 box 2) with
§3.4 values; `darkOnly` plumbed; midnight-only menu note; VHS tracking
skeletons + chromatic wordmark; Silver's quiet status tints; grain where
specified. If §8 box 4 says fix shipped themes, apply §7's values in the
same commit set.

**Accept:** contrast script passes every published pair on bg AND panel;
theme × {classic, plex} × mode matrix screenshotted with zero console
errors; reduced-motion kills VHS tracking animation; `docs/mockups/themes.html`
regenerated to match shipped values (mockups must not drift from code).

### G3 — did the abstraction pay for itself?

Write `docs/UI-LAYOUTS-G3-DECISION.md` (one page): duplicated markup
count, extra requests (must be zero), index.html size delta vs the
+60–90 KB estimate, route regressions caught by `ui-baseline`, and the
cost in files/lines of one shared card fix applied under both layouts.
Verdict: continue · revise the contract · stop.

**Accept:** the verdict names its evidence; if "revise", the revision
lands before G4 starts.

### G4 — one second layout

Per §8 box 1 (Theater default). Entry conditions: G3 said continue;
the chosen layout's §5 blockers (if any) are resolved — for Theater that
is hero-selection rules (newest in-progress → newest added fallback,
playable video only) and an artwork-loading budget; Spine has none beyond
the seam. Deck and Guide cannot be chosen here; they have their own
proposals to clear first.

**Accept:** no business-logic fork; no layout-specific fetch outside its
registered loader; the G2 acceptance list re-run for the new layout.

## 5. Deferred tracks — facts for the next proposals

### 5.1 Deck (operator console)

Verified blockers: browse `ItemDto` carries best-file `resolution` only —
codec/HDR/audio/size live on `FileDto`, returned by item detail
([dto.rs:36/182]); painting the mockup's table from browse data would be
an N+1 fan-out. `activity_detail` lists transcode sessions only
([system.rs:1121]) — direct play and remux hold no server session, and
`/system` diagnostics are admin-only. The honest options (review's):
(1) a paginated operator-list DTO with aggregated file facts + a playback
registry covering all delivery methods → Deck as designed; (2) transcode-
and-scan-only telemetry → a denser skin. Recommendation stands: option 1
or nothing; write `DECK-OPERATOR-PLAN.md` when wanted.

### 5.2 Guide

Blockers: no genre data exists server-side for catalog media (TMDB
enrichment does not persist genres; NFO genres fold into home-media tags
— verified in `scan/nfo.rs`); schedule semantics undefined (timezone
seed, start-at-slot vs saved position, missing runtimes, season gaps,
pagination bound, same-day stability); number keys / color buttons cannot
be assumed on current remotes — every action needs a D-pad path and an
on-screen label. A v1 without new APIs could channel by library +
continue + next-up + shuffle only (no genre channels). Needs its own
product note before any gate.

### 5.3 Archive (reviewer's candidate)

Cheapest parked layout: `recorded_at` and `added_at` already ship on
`ItemDto` (v6 migration), the Captured/Added switch resolves the
two-clocks honesty problem, and "Date unknown" grouping is specified.
Home-replacement-only like Guide. If §8 box 3 is yes, it slots into a
later G4-style gate with no server work.

### 5.4 Native clients (separate plans, later)

Verified starting states: Android has `ViewerColors` (3 themes × 2 modes)
composed with Material `ColorScheme`
([clients/android/.../ui/theme/Theme.kt:22-84]) — extend by GENERATING
`ViewerColors` from a canonical token file, then navigation scaffolds one
form factor at a time. Apple has a static noirr `Palette` and forces dark
([clients/apple/Sources/Theme.swift:5], [PlurxApp.swift:11]) — theme
infrastructure (selection, persistence, environment injection, light
appearance) is foundation work before any catalogue. One canonical token
source generating web/Kotlin/Swift is required before either port —
hand-copying 8+ themes × 2 modes × 3 platforms guarantees drift; extract
it as the first task of the Android plan, not during this one.

## 6. Guardrails

- **No server changes of any kind.** No new endpoints, DTO fields,
  settings keys, or migrations. Everything in G0–G4/T1 is
  `index.html`-only (§5 items are parked precisely because they violate
  this).
- **No route or player changes.** Hash routes, the Plex-compat façade,
  projection mode, and the player internals are untouched by layouts.
- **Classic's pixels are frozen** through G1 and byte-checked at every
  gate after.
- **Theme ids are permanent.** `classic`/`terminal`/`noirr` keep working;
  additions are additive; no renames.
- **Two index.html lineages are in flight** — the Codex agent's branch
  (`codex/fix-dolby-vision-quality`) carries its own +75/−22 to the same
  file. Coordinate merge order per §8 box 5; keep layout diffs surgical
  (new functions + scoped CSS blocks, minimal edits inside existing
  functions) so the eventual merge is mechanical.
- **Strings render `${APP_NAME}`** (`"cinemarr"`, [index.html:819]) —
  never the literal "plurx".
- **Mockups are reference, not law**; when shipped values change (T1),
  regenerate the mockup/theme-sheet stills so docs don't drift.

## 7. Shipped-theme contrast findings (out of review scope, ours)

Extending the reviewer's audit to the three SHIPPED light themes found
failures they politely left alone. Minimal darkened values that pass
4.5:1 (computed the same way, against each theme's `--bg`):

| Shipped pair | Now | Passing fix |
|---|---:|---|
| Classic light accent `#2f6fe0` | 4.38:1 | `#2e6cdb` (4.57) |
| noirr matinee warn `#a3742b` | 3.59:1 | `#8c6324` (4.66) |
| noirr matinee good `#35855c` | 3.92:1 | `#307a54` (4.53) |
| noirr matinee bad `#c96442` | 3.40:1 | `#a85337` (4.62) |
| Terminal light accent `#6e7f00` | 3.65:1 | `#606f00` (4.54) |
| Terminal light good `#859900` | 2.62:1 | `#5f6e00` (4.61) |
| Terminal light warn `#b58900` | 2.62:1 | `#826200` (4.64) |

Caveats that make this §8 box 4 rather than an automatic fix: the matinee
values are kit-exact (`brand/tokens.css`) — changing them is a brand
decision and the kit file changes in the same commit; Terminal-light is
faithful Solarized, whose dim statuses are the aesthetic — fixing
statuses (which carry meaning) while waiving the accent is a defensible
middle. Dark variants of all three shipped themes pass.

## 8. Open boxes for Paul

1. **Second layout at G4:** ☐ Theater (recommended — it is the TV-facing
   design and Paul picked it; exercises the hero/immersion path clients
   need) · ☐ Spine (recommended if desktop browsing is the next surface)
2. **Adopt Paper + Tide in T1:** ☐ yes (recommended) · ☐ later
3. **Archive experiment after G4:** ☐ yes · ☐ park
4. **Fix shipped light-theme contrast (§7):** ☐ all · ☐ statuses only,
   waive brand accents (recommended) · ☐ leave shipped themes alone
5. **Merge order:** ☐ land `codex/fix-dolby-vision-quality` to main
   before starting (cleanest index.html story) · ☐ start `feat/ui-layouts`
   now and merge carefully later (recommended if the codex branch is
   still weeks out)

Everything else in this plan is decided. Tick the boxes, hand the branch
to the implementing agent, and G0 can start the same day.
