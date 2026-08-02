# UI layouts & themes — implementation plan for the accepted slice

**Status:** ready to build · **Executes:**
[UI-LAYOUTS-PLAN.md](UI-LAYOUTS-PLAN.md) as amended by
[UI-LAYOUTS-REVIEW.md](UI-LAYOUTS-REVIEW.md) · **Written:** 2026-08-02,
amended same day — server changes are now permitted under the
multi-consumer rule in §3.6 (Paul's direction; supersedes the review's
client-only stance and this plan's earlier "no server changes" guardrail)
· **Verified against:** branch `docs/ui-layouts-proposals` @ `7fc1796`
(whose web app is main's — blob `d541671`)

This is the build order for the implementing agent. Read
[UI-LAYOUTS-PLAN.md](UI-LAYOUTS-PLAN.md) for what everything looks like
(and open `docs/mockups/` in a browser), then
[UI-LAYOUTS-REVIEW.md](UI-LAYOUTS-REVIEW.md) for why the web track is
narrower than the proposal, then work THIS document gate by gate. Every
review finding cited here was independently re-verified against the code
on 2026-08-02 — DTO shapes, session telemetry, genre storage, client
theme infrastructure, and every contrast ratio (the review's numbers
reproduced exactly). File/line anchors are against the blobs named above;
re-verify anchors at build time, the structure moves.

**Standing instructions.** Work gate by gate; a gate is not done until
its acceptance list passes. Server work is allowed only inside the S
milestones and only under §3.6's rules — if a web gate seems to need a
server change that isn't an S milestone, stop and flag it rather than
building it. When a mockup and this document disagree, this document
wins; mockups illustrate structure, they are not pixel law.

---

## 1. The slice

Two tracks. The **web track** proves the layout system; the **server
track** adds the small set of server capabilities that more than one
layout needs. S milestones are Rust-side and independent of the web
pilot — they may start any time after G1 and interleave freely.

```
web track:     G0 ──▶ G1 ──▶ G2 ──▶ T1 ──▶ G3 ──▶ G4 ──▶ G5 ──▶ G6
               baseline registry plex  themes decide 2nd    deck   guide
                                pilot               layout
server track:              S1 ────────┐      S2 ──┐          │      │
                           list media facts  playback        │      │
                                      │      registry        │      │
                           S3 ────────┼───────────┼──────────┘      │
                           genres     └───────────┴── G5 needs S1+S2┘
                                                      G6 needs S3
```

Still NOT in this plan: `archive` (candidate experiment, §8 box 3),
native clients (separate plans, §5.2), per-layout theme memory, README
changes before the pilot is accepted.

Branch: create `feat/ui-layouts` from `docs/ui-layouts-proposals` (it is
main `2e0435f` + docs-only commits, so it merges back clean). Web work
lives in `crates/plurxd/src/web/index.html` — `include_str!`-embedded,
so every change needs `cargo build` + restart, and the Rust gate cannot
see JS errors: extract each `<script>` and `node --check` it, every
commit (project memory Rule 4e). Server work follows the repo's own
conventions (§3.6) and the full `make check` gate.

## 2. Review checklist → decisions

The review left boxes unchecked; this section answers them. Items marked
**[PAUL]** are collected in §8 — everything else is decided.

| Review box | Decision |
|---|---|
| Correct Amber/Giallo/Silver light tokens | Adopted, reviewer's values, re-verified ≥4.5:1 (§3.4). One additional fix of ours: Amber-dark `--bad` `#e5534b` → `#ee7168` — the reviewer checked against `--bg` only; on `--panel`/`--panel2` cards the old value measured 4.25:1/3.89:1. Held to the stricter surface set. |
| One global theme for M1 | Yes. `plurx_theme` stays the single global key. A layout's signature theme renders as a *recommendation row* in the menu ("Plex looks best in Amber — switch?"), never an automatic mutation. The per-layout-memory JSON schema from the review is recorded in §3.3 as deferred, not built. |
| Deck: API proposal or reduced telemetry | **The review's option 1, now that server work is allowed:** S1 (list media facts) + S2 (playback registry) give Deck real data, and Deck ships as designed at G5. The review's core demand — no faked telemetry, no N+1 — is preserved in the S acceptance lists. |
| Guide: metadata/schedule/D-pad contract | S3 supplies genres (verified: they aren't just unexposed — TMDB enrichment never persists them; the only genre handling in the tree is NFO `<genre>`→tags for home media). The schedule and input contracts are now *specified* in G6 with proposed defaults instead of parked as open questions. |
| Split web/Android/Apple milestones | Yes. This plan is web + server only. §5.2 seeds the two client plans with verified starting states (`ViewerColors` on Android; static forced-dark `Palette` on Apple). |
| Second layout after the pilot | **[PAUL]** §8 box 1 — Theater (default recommendation) vs Spine. |
| Archive Home experiment | **[PAUL]** §8 box 3. Data-feasible today (`recorded_at`/`added_at` both live on `ItemDto`) — no server work needed. |
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
  // later entries declare the surfaces they actually support (deck
  // omits "tv"); the menu never offers a layout the surface can't run.
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
`#main`, wire — e.g. home builds hub rails inline ([index.html:1623])
inside its view function, and the router (`render()`,
[index.html:5987]) just dispatches. A layout registry bolted around
those functions would duplicate fetches or reach into globals. Split
each converted route at one seam:

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
the player stay on their existing renderers behind the layout chrome
until a gate explicitly converts them (G5 converts activity's body for
the deck footer's benefit — the DATA moves in S2; the page stays where
it is). The one shared card partial keeps serving every layout,
parameterized by CSS (`--w`, label visibility), as the mockups already
demonstrate.

**Invariant:** converting a route must not change its network behavior.
Acceptance uses request logs: same endpoints, same counts, before and
after, per route. (S milestones change this invariant deliberately and
say so in their own acceptance lists; web-only gates may not.)

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
python (WCAG relative luminance); land it as `scripts/contrast-check` in
G2 so it stays runnable.

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

### 3.6 Server-work contract (the rule that replaced "no server changes")

Server changes are welcome when they are sensible and serve more than
one layout's function (Paul, 2026-08-02). Concretely, every S milestone:

1. **Names at least two consumers, and lands at least one non-deck
   consumer in the same gate** — the rule is enforced by acceptance, not
   by intention. A change that only one layout would ever read does not
   belong in the S track.
2. **Is additive at the API surface.** New DTO fields are optional
   (`skip_serializing_if` / defaulted), new query params are optional,
   no existing field changes meaning or type. The native clients parse
   these DTOs today; verify Android's decoder ignores unknown fields
   before shipping the first additive change (one test, then trust it).
3. **Follows the repo's storage conventions:** logic in `plurx-core`
   behind the `Store` trait; SQLite STRICT; migrations append-only
   (next slot is **v8**; v6/v7 precedents show the shape, including the
   per-migration `foreign_keys` handling); pure decision functions live
   beside their loops and carry the tests.
4. **Does not touch:** hash routes, the player/projection pipeline, the
   Plex-compat façade (it must never see new fields it could trip on —
   the façade builds its own responses; verify, don't assume), or auth
   semantics. Admin-gated data stays admin-gated unless §8 box 6 says
   otherwise — a layout never widens visibility as a side effect.
5. **Passes the full gate:** `make check` with `--no-fail-fast`, plus
   the TMPDIR-symlink run for anything touching scan/paths (project
   memory Rule 4c).

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
scrubber, backdrop detail — per mockup structure. (The library List
toggle may ship disabled or grid-only here; it becomes real when S1
lands — do not fake its columns from detail fetches.)

**Accept:**
- Full route matrix operable under plex — unconverted routes (activity,
  settings, search) render their existing bodies inside plex chrome.
- Libraries with 0, 1, and 200+ items; items with no art, long titles,
  missing year; non-admin user (no Settings in sidebar).
- Keyboard-only pass of home → library → detail → play with visible
  non-color focus; hover-only affordances (play circle, captions) have
  keyboard/touch equivalents.
- Light mode screenshot sweep: no hardcoded-dark boxes (the 2026-07-22
  scar); `scripts/contrast-check` passes on §3.4 values.
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
errors; reduced-motion kills VHS tracking animation;
`docs/mockups/themes.html` regenerated to match shipped values (mockups
must not drift from code).

### S1 — media facts on the library list (server)

The facts already exist — `files` table columns feed `FileDto`
([media.rs FILE_COLS selects], [dto.rs:182]); the browse list just never
aggregates them. Add an optional aggregated block per item on the
library list response, computed in one query (join/group on `files`, no
per-item fan-out), requested via an optional query param so unchanged
clients pay nothing:

```jsonc
// ItemDto, additive, only when ?facts=1 (name at build time)
"media": { "files": 2, "bytes": 75800000000, "video": "HEVC",
           "height": 2160, "dr": "DV", "audio": "TrueHD 7.1",
           "container": "MKV" }
```

`dr` MUST use the same source-vs-delivered vocabulary as
[MEDIA-BADGES-PLAN.md](MEDIA-BADGES-PLAN.md) (`delivered_dynamic_range`
contract) — one HDR labeling system in this codebase, not two. Photos
and folder items skip the block.

**Consumers (≥2, at least one lands with S1):** plex library List mode
(real columns, G2's disabled toggle turns on) · deck's table (G5) ·
richer card badges for every layout per the badges plan.

**Accept:** one SQL query serves a 210-item library page (log proves no
N+1); list latency within 15% of pre-S1 measurement on the same library;
response without `?facts=1` is byte-identical to pre-S1; Android decoder
verified tolerant of the new field; plex List mode renders codec/DR/size
columns from the list response alone; badge vocabulary cross-checked
against MEDIA-BADGES-PLAN.md.

### S2 — playback attribution registry (server)

An in-memory registry in `plurxd` state of every active delivery —
direct play, remux (`stream.mp4` pipes), and HLS transcode — each entry
`{user, client, item, method, started_at, progress?}`. Today only
transcodes are visible (`activity_detail` →
`transcode.list_sessions()`, [system.rs:1121]); direct/remux are
invisible, which is how a GPU-class resource question became a design
rule in the first place. Register on `/decision` + stream open; expire
on the existing progress-beacon heartbeat going quiet (pick the timeout
from the beacon interval, not a guess). Exposed as an additive array on
the activity response. No schema change; a persistent play-history LOG
is explicitly out of scope unless §8 box 7 opts in.

**Consumers (≥2, landing together):** the Activity page in every layout
(method column: direct · remux · transcode) · deck's status footer and
spec sheets (G5) · optional later: plex sidebar "now playing".

**Accept:** a direct-play start appears in activity within one beacon
interval and expires within the chosen timeout after stop; a remux pipe
appears and disappears with the request; transcode entries unchanged;
the Activity page shows all three methods under classic AND plex; pure
expiry/decision logic is a tested function beside its loop (repo
convention).

### S3 — genres for catalog media (server, migration v8)

Verified: no genre data exists server-side for movies/TV — TMDB
enrichment does not persist genres; NFO `<genre>` folds into home-media
tags only (`scan/nfo.rs`). Add: migration **v8** (append-only, STRICT)
storing genres per item (JSON-array column or join table — decide at
build against query needs; stop-and-flag if it grows past this); TMDB
enrichment persists genre names on match (AniList genres map in for
anime); metadata refresh backfills; additive `genres` on item DTOs;
optional `genre` filter param on the library-items query, filtered
server-side.

**Consumers (≥2):** plex library Genre filter pill + Categories tab
(drawn in the accepted mockups, currently impossible) · theater's chip
bar (same) · guide's `MOVIES · <genre>` channels (G6) · search later.

**Accept:** refresh backfills genres for TMDB-matched items (report
count in the scan/refresh report per the errors-and-problems
convention); `?genre=` filters server-side with a test; plex filter
pill works end-to-end; `EnrichReport` notes items that matched but
returned no genres (log-only is fine — do not resurrect the UI-invisible
errors gap knowingly).

### G3 — did the abstraction pay for itself?

Write `docs/UI-LAYOUTS-G3-DECISION.md` (one page): duplicated markup
count, extra requests (must be zero on web-only routes), index.html size
delta vs the +60–90 KB estimate, route regressions caught by
`ui-baseline`, and the cost in files/lines of one shared card fix
applied under both layouts. Verdict: continue · revise the contract ·
stop.

**Accept:** the verdict names its evidence; if "revise", the revision
lands before G4/G5 start.

### G4 — one second layout

Per §8 box 1 (Theater default). Entry conditions: G3 said continue; for
Theater — hero-selection rules (newest in-progress → newest added
fallback, playable video only) and an artwork-loading budget; Spine has
no blockers beyond the seam. Deck and Guide are not choices here — they
have their own gates.

**Accept:** no business-logic fork; no layout-specific fetch outside its
registered loader; the G2 acceptance list re-run for the new layout.

### G5 — deck, as designed (entry: G3 continue + S1 + S2)

The operator console per the proposal §3.4 and mockups: mono rail,
stat-tile home, **table library view** fed entirely by S1 (client-side
sort), spec-sheet detail (FileDto is already rich enough), persistent
status footer fed by S2 + the scan poll, keyboard map (`/` `j/k`
`enter` `w`). Disk/uptime tiles read `/system` and therefore render for
admins only unless §8 box 6 opens a reduced household view — the footer
degrades gracefully for non-admins (scan + sessions only). The detail
history section renders from existing watch/progress data; a device+
method play-log exists only if §8 box 7 says so (via S2's extension).

**Accept:** 210-item table from ONE list request (request log); footer
live-updates during a scan and shows all three delivery methods; a
non-admin sees no admin-gated numbers and no broken tiles; desktop +
tablet only (capability filter hides deck on TV/phone-class surfaces —
phone list variant may follow later); keyboard map works; G2's a11y
list re-run (tables get real semantics — sortable headers announced).

### G6 — guide (entry: S3 + the defaults below confirmed)

The review asked for a schedule/input contract; here it is — defaults
chosen so this gate is decidable, overridable in §8 box 8:

- **Seed:** the viewing device's local calendar date (`YYYY-MM-DD`).
  Two devices may show different schedules; the guide is presentation,
  not shared state.
- **Slots are presentation.** Enter always plays from the user's saved
  position (or 0:00). No faux-live mid-item starts.
- **Missing runtime:** 45-minute block for episodes, 2-hour for movies.
- **Episode order:** the next-up ordering; season gaps skipped silently.
- **Data budget:** channels derive from already-fetched hubs plus at
  most one library page (≤100 items) per channel; never a full-library
  crawl.
- **Stability:** the schedule is fixed per device-day (seeded PRNG);
  only the CONTINUE channel re-flows with watch progress.
- **Input:** every action has a D-pad path and an on-screen label;
  number keys and color buttons are enhancements only.
- Channels at launch: CONTINUE · `MOVIES · <genre>` (S3) · deepest-show
  marathon · RECENTLY ADDED · per-library rows where present · SHUFFLE.
  Home-replacement only; browse/detail/settings stay classic-shelled.

**Accept:** same-day determinism test (two renders, identical schedule);
every cell plays on enter/click; D-pad-only TV pass; genre channels
sourced from S3 (no client-side genre guessing); non-home routes visibly
classic-shelled; VHS pairing screenshotted for the docs.

## 5. Still deferred

### 5.1 Archive (reviewer's candidate)

No server work needed (`recorded_at`/`added_at` ship on `ItemDto`, v6).
If §8 box 3 is yes, it slots in as a G4-style gate after G6, using the
review's Captured/Added and "Date unknown" rules as written.

### 5.2 Native clients (separate plans, later)

Verified starting states: Android has `ViewerColors` (3 themes × 2
modes) composed with Material `ColorScheme`
([clients/android/.../ui/theme/Theme.kt:22-84]) — extend by GENERATING
`ViewerColors` from a canonical token file, then navigation scaffolds
one form factor at a time. Apple has a static noirr `Palette` and
forces dark ([clients/apple/Sources/Theme.swift:5],
[PlurxApp.swift:11]) — theme infrastructure (selection, persistence,
environment injection, light appearance) is foundation work before any
catalogue. One canonical token source generating web/Kotlin/Swift is
required before either port; extract it as the first task of the
Android plan. S1/S2/S3 responses are additive, so current clients keep
working untouched.

## 6. Guardrails

- **Server changes only inside S milestones, under §3.6.** The
  multi-consumer rule is the law: if only one layout would read it, it
  doesn't ship. (This replaces the earlier blanket "no server changes"
  — Paul's call, 2026-08-02.)
- **No route changes, no player changes, no façade changes.** Hash
  routes, projection mode, player internals, and the Plex-compat façade
  stay untouched by both tracks.
- **Classic's pixels are frozen** through G1 and byte-checked at every
  web gate after.
- **Theme ids are permanent.** `classic`/`terminal`/`noirr` keep
  working; additions are additive; no renames.
- **Migrations append-only, DTOs additive-only** (§3.6). Old clients
  must never notice the S track happened.
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

1. **Second layout at G4:** ☐ Theater (recommended — TV-facing, and it
   exercises the hero/immersion path the clients need) · ☐ Spine
   (recommended if desktop browsing is the next surface)
2. **Adopt Paper + Tide in T1:** ☐ yes (recommended) · ☐ later
3. **Archive experiment after G6:** ☐ yes · ☐ park
4. **Fix shipped light-theme contrast (§7):** ☐ all · ☐ statuses only,
   waive brand accents (recommended) · ☐ leave shipped themes alone
5. **Merge order:** ☐ land `codex/fix-dolby-vision-quality` to main
   before starting (cleanest index.html story) · ☐ start
   `feat/ui-layouts` now and merge carefully later (recommended if the
   codex branch is still weeks out)
6. **Deck telemetry for non-admins:** ☐ admin-only disk/uptime,
   household sees scan + sessions (recommended — matches the current
   admin gating) · ☐ add a reduced household status endpoint in S2
7. **Play-history log** (who played what, when, how — deck's history
   table wants it): ☐ existing watch/progress data only (recommended
   for now) · ☐ add a persistent play log as an S2 extension (new
   table; its own retention decision)
8. **G6 schedule defaults** (device-local date seed · slots-as-
   presentation · 45m/2h fallbacks · fixed per device-day): ☐ as
   specified (recommended) · edits: ____________

Boxes 1–5 unblock the web track; 6–8 only gate G5/G6. Tick 1–5, hand
the branch to the implementing agent, and G0 starts the same day; the S
track can begin right after G1 in parallel.
