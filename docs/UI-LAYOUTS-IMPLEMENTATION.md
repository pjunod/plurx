# UI layouts & themes — implementation plan for the accepted slice

**Status:** G0 · G1 · T1 built and proven, G2 partial (home) — see
[UI-LAYOUTS-STATUS.md](UI-LAYOUTS-STATUS.md) for ground truth ·
**Executes:** [UI-LAYOUTS-PLAN.md](UI-LAYOUTS-PLAN.md) as amended by
[UI-LAYOUTS-REVIEW.md](UI-LAYOUTS-REVIEW.md) · **Written:** 2026-08-02;
v2 same day (server changes permitted, §3.6); v3 same day (§9 execution
model + the three-surface contrast bar); **v4 same day — folds the
build's status findings
(STATUS.md §3.1–3.8): migration slot corrected to v13, the piecewise
library seam, S2 rebuilt around the existing `playstart` hook, S3's
backfill cost named, `--accent2-ink`, and the completed §7 inventory** ·
**Verified against:** `feat/ui-layouts` @ `4ada24c` (code) + `3530ee6`
(status doc)

This is the build order for the implementing agent. Reading order:
[UI-LAYOUTS-PLAN.md](UI-LAYOUTS-PLAN.md) for what everything looks like
(open `docs/mockups/` in a browser) → [UI-LAYOUTS-REVIEW.md](UI-LAYOUTS-REVIEW.md)
for why the web track is narrower than the proposal →
[UI-LAYOUTS-STATUS.md](UI-LAYOUTS-STATUS.md) for what is already built
and proven → THIS document, gate by gate. Claims in earlier versions
were re-verified against code; v4 additionally verified the status
doc's §3 findings (migration count and assert, `playstart` seam,
`SessionKind::Copy`, `progressive::Streams`, every contrast number).
File/line anchors move; re-verify at build time.

**Standing instructions.** Work gate by gate; a gate is not done until
its acceptance list passes. Server work is allowed only inside the S
milestones and only under §3.6's rules — if a web gate seems to need a
server change that isn't an S milestone, stop and flag it rather than
building it. §9 says what may run in parallel and what must not — above
all: ONE agent owns `index.html` at a time. When a mockup and this
document disagree, this document wins; when this document and
STATUS.md's measured findings disagree, the measurement wins and this
document gets amended, as v4 just did.

---

## 1. The slice, and where it stands

```
web track (serial):    G0 ✔ → G1 ✔ → G2 ◐(home) → G2b → T1 ✔* → G3 → G4 → G5 → G6
server track (parallel
after G1, any order):  S1 · S2 · S3            (none started)

entry conditions:      G3 needs G2b            G5 needs G3-continue + S1 + S2
                       G6 needs S3 + §8 box 8
```

\* T1 ran ahead of G2's completion (themes are layout-independent) and
shipped Paper + Tide on the box-2 recommendation — §8 ratifies.
G2's landed half proved the thesis: plex adds **zero** new
card/rail/grid builders and zero extra requests; 136 CSS rules scoped
under `[data-layout=plex]`. Defend that property at every later gate.

Still NOT in this plan: `archive` (candidate post-G6 gate, §8 box 3),
native clients (separate plans, §5.2), per-layout theme memory, README
changes before the pilot is accepted.

Branch: `feat/ui-layouts` (exists, @ `4ada24c`+). Web work lives in
`crates/plurxd/src/web/index.html` — `include_str!`-embedded, so every
change needs `cargo build` + restart. Tooling now exists and is the
law: `make web-check` (`scripts/js-check` = node --check every embedded
script · `scripts/contrast-check` with `scripts/contrast-allow.txt`,
where an allowlisted pair that starts passing is a BUILD ERROR — the
file can only shrink) and `make ui-baseline` (`--self-host` is the
provable mode; compare runs at the SAME `--out` path — the seeded
library path renders on Settings, so different paths false-positive).

## 2. Status findings → what v4 changed

[UI-LAYOUTS-STATUS.md](UI-LAYOUTS-STATUS.md) §3 raised eight issues;
every one is resolved here. Cite by status number.

| Status § | Finding | v4 resolution |
|---|---|---|
| 3.1 | plan said migration v8; repo is at v12 with an assert | §3.6 rule 3 corrected: **v13**, bump the `assert_eq!(version, 12…)` literal, and the `ITEM_COLS`/`ITEM_COL_COUNT = 23` positional-offset trap recorded for S3 |
| 3.2 | genre backfill can't recompute from stored data — it re-hits TMDB | S3 rewritten: forced re-enrich, settings-gated, rate-paced, resumable; movies ride the existing details call, shows map `genre_ids` via one cached genre-list call per media type |
| 3.3 | white badge label on `--accent2` fails in 3 new themes | §3.4 adds optional **`--accent2-ink`** (default `--btn-ink`); values below. `--accent2` itself is a gradient end, not a text token — do not "fix" it |
| 3.4 | `--panel2` is a text surface; two proposal values failed there | v3 had already moved §3.4 to the three-surface bar (the status doc quotes v2's two-surface text); v4 syncs the one value the build derived differently — shipped paper-light `--good` `#26724c` |
| 3.5 | §7 was incomplete (9 unlisted failures) and one claim false | §7 rewritten as the complete 16-pair inventory with classification + all-surface fix values; "dark variants pass" retracted (noirr-dark button ink measures 3.91:1) |
| 3.6 | library route is an incremental renderer; whole-body seam would regress scroll | §3.2 now defines TWO seam shapes; library uses the piecewise contract in G2b |
| 3.7 | +60–90 KB estimate already consumed by one layout + themes | G3 re-baselines the envelope and must answer the single-file question deliberately |
| 3.8 | activity copy is wrong today; S2 was speced against a misreading | S2 rebuilt on `playstart::note_playback_started`, client playback id + idle expiry for direct play, listing the existing `progressive::Streams` and `SessionKind::Copy`; fixes the copy at the same time |

Assumptions the build made without ticked boxes (status §6) are
ratified or reversed in §8 — including the build's two self-judged
status-token hardenings (silver-light and paper-light `--good`), which
§3.4 adopts and box 2's ratification covers. Nothing else in this
table is open.

## 3. The contract

### 3.1 Registry, persistence, capability (shipped in G1)

As built: `LAYOUTS` sits beside `THEMES` in the `<head>` pre-paint
script carrying `{name, surfaces}` only; the body script attaches
`.chrome` and `.views` to the same objects. **A new layout MUST add its
`{name, surfaces}` stub to the `<head>` object** — without it,
`applyLayout()` can't resolve the stored id, `data-layout` stays
`classic`, and every scoped CSS rule silently fails to match (it
debugs like a stylesheet that didn't load; this bit the plex port).
`surfaceClass()` decides desktop · mobile · tv from viewport/input,
never UA strings. Unknown/unsupported ids fall back to classic
silently. Deck declares desktop + tablet only.

### 3.2 The seam — two shapes, not one

**Whole-body routes** (home — shipped; item detail — G2b):

```
hash route ─▶ loadHome() ─▶ page model ─▶ LAYOUTS[cur].views.home(page)
               (fetches)    (plain data)        (body HTML)
                                                     │
                                                     ▼
                                             shared wiring
```

**Incremental routes** (library — G2b). `libraryView()` fetches pages
in a loop and redraws `#libbody` / `#libpager` / `#librail` /
`#libcount` piecewise so a large library never resets scroll position.
A `body(model) → html` contract would replace the whole body per batch
and reintroduce that regression. The library contract is therefore
region-shaped:

```js
LAYOUTS[cur].views.library = {
  shell(model),   // rendered once: chrome-fitting frame + region mounts
  items(model),   // re-rendered per arriving batch into its region
  count(model),   // cheap header/pager region
}
```

The loader drives regions as batches land; scroll position is owned by
the region container and never rebuilt. Any future incremental route
(search, if converted) uses the same shape.

Two ported disciplines from the build, now contract: (1) loaders
reproduce the original fetch sequencing exactly — tidying sequencing
is a performance change and ships as its own commit with its own
justification, never inside a seam refactor (the request-log invariant
enforces this); (2) plex introduced zero new card/rail/grid builders —
G4–G6 layouts hold the same line; a second card function is how five
layouts become five applications.

### 3.3 Theme contract additions

Unchanged from v3 (darkOnly · reduced-motion · forced-colors · one
global theme key; per-layout memory schema recorded-not-built), plus
one addition in §3.4: `--accent2-ink`.

### 3.4 Final token values (validated, synced to shipped code)

The contract bar: every text-bearing token pair ≥4.5:1 against **all
three surfaces** (`--bg`, `--panel`, `--panel2`), btn-ink against
accent, and — new — `--accent2-ink` against `--accent2` where a theme
sets it. `scripts/contrast-check` enforces exactly this;
`--suggest` prints nearest passing values, so nobody hand-computes.
`--accent2` alone is NOT a text token (two uses, both gradient ends —
status §3.3); auditing it as ink is the wrong test and stays out of
the check.

Shipped and correct (T1): Amber (dark `--bad` `#ee7168`; light
`#8b5e00`/`#246b49`/`#bd332d`), Giallo light `#7f4e00`/`#2c734d`,
Silver light `#765f28` warn + `#467353` good, Void, VHS, Paper (light
`--good` **`#26724c`** — the shipped value; the build hardened the
review's `#287a51` independently and landed one channel-step from v3's
`#25724c`; the shipped value stands), Tide. Full tables: proposal §4.2 + review §"Reviewer additions" +
`scripts/themes-proposed.json` (machine-readable, now the canonical
value source for future ports).

**`--accent2-ink`** — label ink for accent2-filled badges. Default:
the theme's `--btn-ink`. Themes whose accent2 is light set a dark ink;
the theme's own `--bg` measures excellently and keeps the badge
on-brand:

| theme | accent2 | accent2-ink | ratio |
|---|---|---|---:|
| VHS dark | `#26e2ff` | `#140d22` | 12.07 |
| Tide dark | `#d6b56d` | `#071412` | 9.56 |
| Paper dark | `#e58b84` | `#171715` | 7.14 |

The check measures every theme's *effective* badge ink — the token if
set, else `--btn-ink` — against that theme's `--accent2`; a theme
whose default ink fails must set `--accent2-ink`. That is how "all
other themes pass" stays a checked fact rather than a claim.

### 3.5 Boundary (who owns what)

Unchanged from v3 — the review's corrected table stands: layouts own
scaffold/composition/density/surfaces; themes own
palette/type/texture/lockup; accessibility preferences own
motion/contrast/scaling. Every combination stays legible and operable.

### 3.6 Server-work contract

Rules 1, 2, 4, 5 unchanged from v3 (≥2 consumers with a non-deck
consumer check · additive DTOs/params only · routes/player/façade/auth
untouched, admin gating never widened as a side effect · full
`make check --no-fail-fast` + TMPDIR-symlink run for scan-adjacent
code). Rule 3 corrected per status §3.1:

3. **Storage conventions:** logic in `plurx-core` behind the `Store`
   trait; SQLite STRICT; migrations append-only — **the next slot is
   v13**, and the version assert
   (`store/sqlite/mod.rs`, `assert_eq!(version, 12, …)`) is bumped to
   13 in the same commit, deliberately. If a migration adds a column to
   `items`, `ITEM_COLS` / `ITEM_COL_COUNT` (= 23 today) are positional
   offsets used by `continue_watching` / `next_up` / `recently_added` /
   `search_items` — add the column AND bump the count or those queries
   silently misread. Pure decision functions live beside their loops
   and carry the tests.

## 4. Gates

### G0 — baseline harness ✔ (done)

Shipped as `scripts/ui-baseline` (`make ui-baseline`): self-hosted
throwaway `plurxd`, seeded deterministic library, frozen page clock,
timestamps rewritten backwards, server restarted after seeding, motion
off, 9 routes × 2 viewports + tab-order dump. Proven: 7 consecutive
runs byte-identical. Operational rule inherited by every later gate:
compare runs at the same `--out`; `--base-url` mode is smoke-only, not
byte-exact.

### G1 — registry with zero pixel change ✔ (done)

All 18 captures byte-identical pre/post. The registry, seam (home),
menu section, and fallbacks are live per §3.1/§3.2.

### G2 — plex pilot, home ✔ · G2b — complete the pilot (next web work)

G2's landed half: plex chrome + Home through the seam, Amber both
modes, zero new components, zero request-log drift, classic untouched.

**G2b scope:** convert library (piecewise contract, §3.2) and item
detail (whole-body) under plex — A–Z scrubber, `Library/Collections/
Categories` tabs (Categories greys out until S3), backdrop detail —
plus the four defects the build logged (status §5):

- library List/Grid toggle ships greyed like Categories — it goes
  live via S1's consumer check, never via faked columns.
- corner unwatched flag: scope by item kind (`card()` gains a kind
  class); home-video folders and photos never flag.
- A–Z scroll-spy anchors to the layout's own header, not `header.top`
  (plex has none).
- TV surface hides Settings/admin affordances (the proposal's
  no-admin-on-TV rule).
- `PLEX_NAV` re-derives after `invalidateLibs()` — sidebar counts must
  not go stale until the next Home render.

**Accept:** everything from v3's G2 list (route matrix under plex ·
0/1/200+ item libraries · missing-art/long-title/no-year items ·
non-admin user · keyboard-only pass with visible non-color focus ·
hover affordances have keyboard/touch equivalents · light-mode sweep,
no hardcoded-dark boxes · reduced-motion + forced-colors ·
request-log invariant · classic byte-identical) — PLUS: library scroll
position survives batch arrival AND layout switching; the four defects
above have regression captures; one manual smoke of home/library/
detail in Safari and Firefox on the Mac (the harness is
Chromium-only — status §5 — so cross-engine stays a human check for
now) and one VoiceOver spot pass of the plex sidebar and cards.

### T1 — theme catalogue ✔ (done, pending §8 ratification)

Seven themes shipped including Paper + Tide (box 2 assumed yes —
ratify or delete two `THEMES` entries). Contrast enforcement live with
the 36-entry allowlist; shrink-only semantics. §7 debt intentionally
NOT applied (brand calls — box 4).

### S1 — media facts on the library list (server) — unchanged from v3

Aggregated optional `media` block, one join/group query, `?facts=1`
opt-in, `delivered_dynamic_range` vocabulary shared with
[MEDIA-BADGES-PLAN.md](MEDIA-BADGES-PLAN.md). Consumers: plex List
mode (G2b's toggle) · deck (G5) · card badges. Acceptance as v3
(no N+1 · ≤15% latency · byte-identical without the param · tolerant
Android decoder verified · consumer check closes against G2b's
library).

### S2 — playback attribution registry (server) — rebuilt per status §3.8

The ground truth is better than v3 assumed: HLS **copy** remuxes
already hold real sessions (`SessionKind::Copy`,
[transcode.rs:1237], enum at :1212; re-verify); progressive remux
already has a registry
(`progressive::Streams`, [progressive.rs:109]) that is simply never
listed; only true direct play has no server record. And all three
delivery methods already pass through ONE existing seam on the way to
starting: **`playstart::note_playback_started`**
([playstart.rs:124]). Build S2 there:

- Give the seam an end: registry entries keyed by a client-supplied
  playback id — `/stream.mp4` already accepts one
  ([stream.rs:918]) — with idle expiry driven by the existing
  progress-beacon cadence. NEVER one-request-one-session: direct play
  is a storm of ranged 206s ([stream.rs:1105]) and would show phantom
  viewers.
- List what exists: fold `progressive::Streams` (promote `len()` and
  iteration out of `#[cfg(test)]`) and `SessionKind::Copy` into one
  activity array with `method: direct | remux | hls-copy | transcode`.
- **Fix the lie at the merge, not in the worktree:** the activity
  page's "Only transcodes hold a server session — direct play and
  remux flow straight through" ([index.html:5192]) is half-wrong
  today and fully wrong after S2. The one-line copy replacement is
  part of S2's *consumer-check commit*, made by the `index.html`
  owner when S2 merges — never edited from the S2 worktree (§9's
  single-owner rule).

Consumers: Activity page in every layout · deck footer/spec sheets
(G5) · optional plex "now playing" later. **Accept (server-side):**
direct-play start visible within one beacon interval, expiry within
the chosen idle timeout; progressive and copy entries appear/disappear
with their streams; transcode entries unchanged; expiry logic is a
pure tested function. **Consumer check:** Activity shows all four
method labels under classic and plex; the old copy string is gone.

### S3 — genres (server, migration v13) — cost named per status §3.2

No stored TMDB payload exists to recompute from (`tmdb::Match` carries
nine fields, none genres), and enriched items are `metadata_at`-stamped
— invisible to `items_needing_metadata`. **The backfill is therefore a
forced re-enrich that re-hits TMDB once per title.** Spec:

- Migration v13 (append-only; bump the assert; mind
  `ITEM_COLS`/`ITEM_COL_COUNT` if touching `items` — §3.6).
- Forward path: persist genres at enrich time. Movies: the existing
  details call already returns them — zero extra requests. Shows: the
  search result carries `genre_ids`; resolve via `/genre/tv/list`
  (and `/genre/movie/list` for completeness) fetched ONCE per run and
  cached — never a per-title second round-trip. AniList genres map in
  for anime.
- Backfill: settings-gated (the `backfill_hdr_format` precedent for
  the gate, NOT for the mechanism — that one recomputed from stored
  probe_json; this one calls out), paced to TMDB's public rate
  ceiling, resumable (stamp progress; a crash or 429 resumes, never
  restarts), and reported in the refresh report per the
  errors-and-problems convention (per-title failures leave genres
  empty and NOTE it — no invisible errors).
- Additive `genres` on item DTOs; optional server-side `?genre=`
  filter on library items.

**Accept (server-side):** enrich of a new movie/show persists genres
with zero/one extra API calls respectively (the cached list call);
backfill over a seeded catalogue is resumable mid-run and its report
counts backfilled/failed/skipped; `?genre=` filters with a test.
**Consumer check:** plex Genre pill + Categories tab work end-to-end
(G2b greyed → live), theater chips at G4 if Theater is chosen.

### G3 — did the abstraction pay for itself? (entry: G2b)

Four of five numbers are already collected (status §4): zero
duplicated components, zero extra requests, +65.9 KB for one layout +
seven themes, zero real regressions (one false positive, documented).
Remaining: price one real shared-card change under both layouts.

Two deliberate calls the record must make, not bury:

1. **Re-baseline the size envelope.** v3's +60–90 KB total is spent.
   New estimate: G2b's library/detail conversions +15–25 KB; theater
   +35–55 KB; deck and guide +60–90 KB each (own body builders per
   route). Trajectory from the shipped 646.6 KB: **~820–910 KB by
   G6**.
2. **Single file: continue or split?** If split, the shape is several
   `include_str!` assets served by `plurxd` (per-layout JS/CSS chunks)
   — still zero build step, still one binary. Deciding at G3 is cheap;
   reopening at G6 is not.

**Accept:** `docs/UI-LAYOUTS-G3-DECISION.md` names its evidence and
answers both calls; one independent fresh-context assessment
accompanies the implementer's metrics (§9.3); if "revise", the
revision lands before G4/G5 start.

### G4 — one second layout (entry: G3 continue)

Unchanged from v3: §8 box 1 chooses (Theater default); Theater's
hero-selection rules and artwork budget as written (hero reuses the
page-model backdrop, progressive over accent wash, interactive ≤110%
of classic home's G0 timing); Spine has no blockers beyond the seam.
Theater's library view uses the piecewise contract from §3.2.

**Accept:** v3's list + the timing bound measured by `ui-baseline`.

### G5 — deck (entry: G3 continue + S1 + S2) — unchanged from v3

Operator console as designed; table fed by S1 only; footer fed by S2 +
scan poll; admin-gated tiles degrade for non-admins (box 6); history
from existing data (box 7); desktop + tablet only; keyboard map; a11y
re-run with real table semantics.

### G6 — guide (entry: S3 + §8 box 8) — unchanged from v3

Schedule/input defaults as specified (device-local date seed ·
slots-are-presentation · 45m/2h fallbacks · ≤1 library page per
channel · fixed per device-day · D-pad path + label for everything);
launch channels including `MOVIES · <genre>` from S3; home-replacement
only. Acceptance as v3.

## 5. Still deferred

### 5.1 Archive — post-G6 gate if §8 box 3 says yes (no server work).

### 5.2 Native clients — separate plans; starting states verified
(Android `ViewerColors` 3×2; Apple static forced-dark `Palette`).
`scripts/themes-proposed.json` now exists and becomes the seed of the
canonical token source those plans require. S1/S2/S3 stay additive, so
current clients keep working untouched.

## 6. Guardrails

Unchanged from v3, with one addition and one sharpening:

- Server changes only inside S milestones, under §3.6 (multi-consumer
  rule; Paul's call, 2026-08-02).
- No route, player, or façade changes; classic byte-frozen; theme ids
  permanent; migrations append-only, DTOs additive-only.
- Two `index.html` lineages in flight (codex branch, +75/−22) — §8
  box 5; keep layout diffs surgical.
- `${APP_NAME}` everywhere; mockups are reference, not law; regenerate
  stills when shipped values change.
- **NEW — the allowlist is shrink-only.** `scripts/contrast-allow.txt`
  entries that start passing fail the build until deleted. Nobody adds
  an entry without a §8-box-4-class decision behind it.
- **Sharpened — "classic byte-frozen" binds the LAYOUT track.**
  S-milestone consumer changes (the activity method column and copy
  fix, S1's List-mode columns) legitimately alter classic's pixels;
  their consumer-check commits regenerate the affected `ui-baseline`
  goldens and say so. Any classic diff outside an S consumer-check
  commit remains a regression.
- **Sharpened:** "byte-identical" comparisons only at the same
  `--out` path (status §1's trap), and `--self-host` is the only
  provable mode.

## 7. Shipped-theme contrast debt — the complete inventory

Status §3.5 proved v3's table was less than half the story (7 of 16
`--bg`-only failures listed, and a false "dark variants pass" claim —
noirr-dark's white button ink on crimson measures **3.91:1**). This
table replaces it. All 36 current failures live in
`scripts/contrast-allow.txt`; fixing = changing the value AND deleting
the line. Fix values below pass ALL THREE surfaces (recomputed for v4;
`contrast-check --suggest` reproduces them).

| Shipped pair | Now (bg) | Class | All-surface fix |
|---|---:|---|---|
| Classic-l accent `#2f6fe0` | 4.38 | brand accent | `#2d69d6` |
| Classic-l good `#1a9d5a` | 3.26 | **status** | `#147d47` |
| Classic-l bad `#d64550` | 4.06 | **status** | `#c23e48` |
| Terminal-l accent `#6e7f00` | 3.65 | brand accent (Solarized) | `#5b6900` ¹ |
| Terminal-l good `#859900` | 2.62 | **status** | `#5c6900` ¹ |
| Terminal-l warn `#b58900` | 2.62 | **status** | `#7d5e00` |
| Terminal-l bad `#dc322f` | 3.77 | **status** | `#ba2a27` |
| Terminal-l muted `#657b83` | 3.64 | text (Solarized-faithful) | via `--suggest` if fixed |
| Terminal-l prose `#586e75` | 4.39 | text (Solarized-faithful) | via `--suggest` if fixed |
| Terminal-l btn-ink on accent | 4.14 | button ink | darkens with accent fix |
| matinee warn `#a3742b` | 3.59 | **status** (kit-exact) | `#8d6425` ² |
| matinee good `#35855c` | 3.92 | **status** (kit-exact) | `#307a54` ² |
| matinee bad `#c96442` | 3.40 | **status** (kit-exact) | `#aa5438` ² |
| noirr-d btn-ink `#fff` on `#e5484d` | 3.91 | button ink (brand) | brand call ³ |
| noirr-d / terminal-l `--accent2` pairs | 2.2–3.0 | not a text token | none — wrong test (§3.4) |

¹ Solarized's accent and green sit at near-identical luminance; after
darkening they land one channel-step apart. Keeping them tellable
apart is a HUE decision — nobody invents it silently (box 4 note).
² Kit-exact: `brand/tokens.css` changes in the same commit — brand
call. ³ Button labels are 14–15 px semibold — under WCAG that is not
"large text", but a 3:1 large-text waiver is the defensible brand
position if box 4 keeps the crimson; record it in the allowlist either
way.

**Box 4's "statuses only" option now means exactly:** apply the eight
**status** rows (classic 2, terminal 3, matinee 3), matinee being a
knowing brand-kit edit; waive the rest into the allowlist with this
section as the recorded reason.

## 8. Open boxes for Paul — now mostly ratifications

The build ran ahead on recommended defaults (status §6). Boxes 1–5
ratify or reverse; 6–8 still gate G5/G6.

1. **Second layout at G4:** ☐ Theater (assumed, recommended) · ☐ Spine
   — nothing built yet; free to change.
2. **Paper + Tide:** ☐ ratify (SHIPPED in T1, recommended) · ☐ reverse
   (delete two `THEMES` entries).
3. **Archive as a post-G6 gate:** ☐ yes · ☐ park (assumed parked).
4. **Shipped-theme contrast (§7):** ☐ statuses only (recommended — the
   eight bold rows; includes knowingly editing matinee kit values) ·
   ☐ all incl. brand accents · ☐ leave as allowlisted debt.
   Sub-call if terminal is touched: accept accent≈green luminance or
   shift hue — ☐ accept · ☐ shift.
5. **Merge order vs codex branch:** ☐ ratify started-now (assumed;
   recommended) · ☐ pause until codex lands to main.
6. **Deck telemetry for non-admins:** ☐ admin-only disk/uptime
   (recommended) · ☐ reduced household status endpoint in S2.
7. **Play-history log:** ☐ existing data only (recommended) · ☐ S2
   extension table.
8. **G6 schedule defaults:** ☐ as specified (recommended) · edits: ___

## 9. Execution model — agents and parallelism

Unchanged from v3, and now proven in practice (the build's own
verification shape produced STATUS.md §3 — exactly the fresh-eyes
yield §9.3 predicts). Three rules: (1) one owner of `index.html` at a
time — web gates serial; side agents only on disjoint files
(`scripts/*`, docs, mockup regeneration). (2) S1/S2/S3 run as parallel
agents in isolated worktrees, each passing the full §3.6 gate before a
serial merge; `dto.rs` is the only shared file (additive on all sides,
separate commits). (3) One fresh-context verifier per gate boundary
reading only the acceptance list + artifacts; two perspectives at G3
only; scripts over agents for anything mechanical. G2b's library
conversion belongs to the `index.html` owner — the piecewise seam is
not a parallelizable edit.
