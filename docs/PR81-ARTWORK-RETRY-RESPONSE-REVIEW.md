# PR #81 review — the right five fixes, two of which introduce new regressions

**Status:** review complete · **Reviews:** [PR #81][pr81] "Address TV artwork
retry review findings", head `791ce78e` on `agent/address-pr80-review`
(draft) · **Verified against:** `origin/main` @ `854d575b`, which is also the
merge-base · **Written:** 2026-08-06 · **Outcome:** **changes requested** ·
**Follows:** the review of [PR #80][pr80], whose three blockers this PR
answers

[pr81]: https://github.com/pjunod/plurx/pull/81
[pr80]: https://github.com/pjunod/plurx/pull/80

Companion to the PR #80 review (what was broken) — this is *whether the fixes
hold*. Every finding below cites `file:line` at `791ce78e`.

**How it was reviewed.** The PR head and `origin/main` were checked out as
pinned detached worktrees from a fresh clone; nothing was read from a working
tree. Four scoped passes ran independently: prior-blocker verification, fresh
adversarial correctness, test-quality audit, and a tiebreak pass that settled
the one place where two passes disagreed. Findings that two passes reached
independently are marked as such. Every CONFIRMED finding below was
reproduced by executing code, not by reading it.

**Build state at head:** `cargo test -p plurx-core -p plurxd` is green —
322 + 320 passed, 1 pre-existing ignored storage test.

**Severity legend:** **BLOCKING** = a regression against `main`, or a claimed
fix that does not hold · **SHOULD-FIX** = real cost or a contract that
permits wrong behavior · **NIT** = worth doing, not worth blocking.

---

## 1. Verdict

Five of the six claimed fixes are real, and — unlike PR #80 — **the tests
genuinely pin them**. Every substantive new test fails when run against base
production code. That is the single biggest improvement in this PR and it
should be said first.

But two of the fixes introduce regressions against `main` that are worse than
what they replace, and both were reproduced by execution:

1. The convergence backstop **launders transient provider failures into a
   24-hour backoff** with a fabricated error string. `main` retried in 30
   minutes; this PR blanks the cards for a day.
2. The `enrich_targets` rewrite **permanently drops ancestor folders** in home
   libraries nested nine deep or more, and nothing ever repairs them because
   the artwork sweep excludes home libraries by design.

Neither is hard to fix — finding 1 needs the outcome value that line 1531
already has and throws away; finding 2 is a two-line edit. But merging as-is
trades a self-healing 30-minute retry for a stuck 24-hour one, which is the
opposite of the PR's purpose.

**Prior-blocker scorecard:**

| PR #80 finding | Verdict |
|---|---|
| 1 · Unstampable rows → forever 30-min loop | Fixed **by backstop**, not at the four sites; three escape hatches remain, and the backstop is finding **A** below |
| 2 · `force=true` rewrites healthy ancestor art | **Fixed.** Route-only shows make zero HTTP requests and get no `apply_metadata` |
| 3 · Unbounded sweep inline in the 60s loop | **Fixed.** Real SQL `LIMIT`, correct RAII single-flight guard |
| S1 · `enrich_targets` 3N reads + O(n²) | Fixed for reads; the early stop is finding **B** below |
| S2 · `repaired` counted show matches | **Fixed.** Re-reads each row rather than inferring |

---

## 2. Findings

### A · BLOCKING — the backstop cannot tell "no image exists" from "we never asked", and backs off 24h either way

`crates/plurxd/src/state.rs:1531` discards the `EnrichOutcome`, which carries
`report.errors` and `report.notes`:

```rust
self.enrich(&library, true, Some(&targets), Some(&ids))
    .await;
```

Then `:1548-1560` stamps *any* row whose `artwork_attempted_at` is unchanged
as `ArtworkAttempt::Failed(...)`, which `apply_metadata`
(`crates/plurx-core/src/store/sqlite/media.rs:480-482`) turns into
`artwork_attempted_at = unixepoch()` — a 24-hour backoff
(`ARTWORK_RETRY_BACKOFF_SECS`, `store/mod.rs:98`).

The information needed to distinguish the cases was available on line 1531.

**CONFIRMED — reproduced twice, by two independent passes:**

```
# transient provider failure: /tv/42/season/1 returns 503
after one sweep_artwork():
  attempted_at = Some(1786058549)
  artwork_error = Some("artwork retry found no provider image for this item")
  items_missing_artwork(None, ARTWORK_RETRY_BACKOFF_SECS, 100) == []

# no provider configured: TMDB_API_KEY = "" (key rotated out post-enrich)
zero HTTP calls made, yet:
  attempted_at = Some(...)   same fabricated error
```

**Failure scenario.** TMDB returns 503 for 90 seconds during a sweep. All 200
rows in the batch take the `season_detail` error path
(`crates/plurx-core/src/metadata/mod.rs:601-608`), the backstop stamps every
one of them, and they are invisible to the self-healing job for 24 hours with
a false operator-facing cause. Re-adding a rotated API key does not un-stick
them either. On `main`, nothing was stamped and the next 30-minute tick
retried.

**Fix.** Thread the per-item outcome — or at minimum
`EnrichReport.errors == 0` for that library — into the backstop, and stamp
only when the provider was actually reached and answered. If an errored
attempt must be stamped at all, give it a short backoff, not the 24-hour one
reserved for "this item has no art upstream".

### B · BLOCKING — the `enrich_targets` early stop permanently drops ancestor folders in deep home libraries

`crates/plurxd/src/state.rs:807-840`:

```rust
for id in item_ids {
    if seen.contains(id) { continue; }          // <-- skips the walk entirely
    let mut current = *id;
    for _ in 0..8 {
        if !seen.insert(current) { break; }     // <-- stops the walk
        targets.push(current);
```

`main` skipped only the *push*; the walk continued, so every start id got a
fresh 8-step budget. The PR lets the first walk's position permanently cap
everything downstream of it.

**CONFIRMED by exhaustive search.** Both algorithms were transcribed verbatim
into a standalone program over an in-memory `parent_id` map and run over
every functional graph on ≤ 7 nodes (2,097,152 graphs, self-loops and cycles
included) crossed with every ordered start list:

```
exhaustive n≤6, all permutations:      85,655,804 cases,     0 diffs
exhaustive n=7, all 1/2/3-start lists: 2,157,969,408 cases,  0 diffs
deep chains (depth 9..15):             14,224 cases,     3,920 diffs
randomized 8..15 nodes w/ cycles:      5,000,000 cases,  11,189 diffs
```

Zero differences below depth 9 — for n ≤ 8 any walk covers its whole
reachable set within budget, so the cap never truncates. **A difference
requires an ancestor chain of ≥ 9 items.** The PR's target set is a subset of
base's in 100% of differing cases, never a superset.

Smallest differing input (9 nodes, provably minimal):

```
  0←1←2←3←4←5←6←7←8        item_ids: [8, 1]
  base: [8,7,6,5,4,3,2,1,0]
  pr81: [8,7,6,5,4,3,2,1]      dropped: [0]
```

**Reachable in production, home libraries only.** Of the three callers,
`refresh_item_artwork` (`:937`) passes a single id and can never differ, and
`sweep_artwork` (`:1530`) is fed by SQL that excludes home libraries
(`media.rs:655`) and tops out at three levels. The targeted scan (`:746`) is
the reachable one. Home nesting is unbounded — `home::place`
(`crates/plurx-core/src/scan/home.rs:66-69`) creates one folder item per path
component with no depth limit, and the repo says so itself:
`docs/HOMEVIDEO-PLAN.md:408` calls for bumping the guard 8 → 16 because "deep
folder trees are legitimate here", and `http/browse.rs:203` already uses 16.
`Photos/2019/Trips/Japan/Day1/Camera/DCIM/100MEDIA/` is nine deep.
`scan_path` uses `WalkDir` with no sort, so the triggering order occurs
roughly half the time and is not under anyone's control.

**Observable consequence — minimal production repro**, monarr scanning
`ROOT/F1/F2`:

```
  ROOT/F1/folder.jpg                                 <- sidecar poster
  ROOT/F1/top.mp4                       item 9       (outside the scanned dir)
  ROOT/F1/F2/F3/F4/F5/F6/F7/F8/deep.mp4 item 10
  ROOT/F1/F2/mid.mp4                    item 11

  placed (WalkDir order): [10, 11]
  dropped by PR #81: [1] = folder F1
  reversed order [11, 10]: both algorithms agree
```

`F1` is a top-level folder with a media child and a `folder.jpg` beside it.
Base adopts the sidecar; this PR renders the most visible card in the home
library blank. **And it never heals** — `items_missing_artwork` excludes home
libraries, so the sweep will never retry it. Only a full library scan
(`only = None`) recovers it. Base was self-correcting across successive
imports *precisely because* of the fresh per-start budget; this PR makes the
miss sticky.

Simulation quantifies the difference between full and partial scans: over
2.9M simulated *full* home imports, 90,577 had differing targets and **zero**
lost a poster (a folder with a media child always gets pushed at step 2 by
that child's own walk). Over 2.19M *partial* imports — monarr's actual usage —
68,381 differed and **2,900 lost a real poster**.

**Fix.** Keep the `HashSet` (the O(1) dedupe is the real win over
`Vec::contains`); drop the early stop. Delete line 811 and change the insert
to a push-guard:

```rust
if seen.insert(current) { targets.push(current); }
```

Verified identical to base over 88,695,116 inputs. If the read savings matter,
a depth-aware variant (store remaining budget, break only when this walk
cannot get further than the one that passed through) also verified at zero
differences. Separately and independently: the `0..8` guard should be `0..16`
to match `browse.rs:203` and `HOMEVIDEO-PLAN.md:408`.

### C · SHOULD-FIX — three escape hatches still leave selected rows unstamped

None of the four `continue` sites from the PR #80 review were touched. That is
a defensible design — defend at the point that matters — but the whole
convergence guarantee now rests on the backstop, and the backstop has holes:

```rust
1521  let Ok(Some(library)) = self.store.get_library(library_id).await else {
1522      continue;                     // whole library's candidates unstamped
1523  };
...
1540  let Some(after) = self.store.get_item(before.id).await? else {   // `?`
1560      .await?;                                                     // `?`
```

`:1521-1523` is the exact shape of the original blocker: a `get_library` error,
or a library deleted between the two queries, skips that library's candidates
with no stamp. `:1540` and `:1560` abort `sweep_artwork` entirely on a single
`StoreError`, leaving every remaining candidate in this library *and all later
libraries* unstamped; `artwork_retry_pass` (`:1301`) only logs a warning.

**Failure scenario.** SQLite returns `SQLITE_BUSY` on candidate 7 of 200.
Candidates 7–200 keep `artwork_attempted_at IS NULL`. The `ORDER BY` sorts
NULLs first (`media.rs:671`), so the next sweep selects the same head rows —
and consumes the whole 200-row budget doing it, starving everything behind
them. This is where blocker 1's residual and blocker 3's new cap interact.
Prefer logging and continuing, as the surrounding code does elsewhere.

Related, same file: `:765-772` uses
`.await.ok().flatten().is_some_and(|item| item.poster_path.is_none())`, so a
`StoreError` silently reclassifies a genuinely blank ancestor as healthy. On a
*first* import that leaves a brand-new show permanently blank while the import
reports success. `Err` and `Ok(None)` deserve different handling.

### D · SHOULD-FIX — a route-only show re-searches TMDB on every pass and discards the result

`crates/plurx-core/src/metadata/mod.rs:143-159` resolves a `tmdb_id` in the
route-only branch, uses it, and never calls `apply_metadata`. Base took the
full path, which persisted it, so the search happened once.

**CONFIRMED.** A show with an adopted sidecar poster, `enriched = true`,
`tmdb_id = NULL`, plus one blank season: after `sweep_artwork()`,
`tmdb_id = None`, `searches = 1`. Every subsequent sweep and every targeted
import pays a fresh `/search/tv` (plus `/find/{imdb}` when an imdb id exists)
forever — on the job whose stated purpose is to not be a rate-limit
generator. A title search per pass is also the path most likely to *mis*match.

### E · SHOULD-FIX — the season guard was added, the matching episode guard was not

`crates/plurx-core/src/store/sqlite/media.rs:660-666`. The season arm gained
`AND i.season_number IS NOT NULL`; the episode arm gained no equivalent guard
on `i.season_number`. `enrich_episodes` buckets by
`ep.season_number.unwrap_or(0)` (`metadata/mod.rs:584`).

**CONFIRMED (selection).** An episode with `season_number = NULL` under an
enriched show is returned by `items_missing_artwork(None, 0, 100)`. It is then
routed to `season_detail(show, 0)` — TMDB's *Specials*. If the show has a
Specials season, the episode is matched by `episode_number` against Specials
and gets a Specials still, title, overview, air date and runtime written to
it, and the local Specials season row is patched too. If the show has no
season 0, the request errors and finding **A** kicks in. Largely pre-existing,
but the PR hardened exactly this class for seasons and left the sibling open.

### F · SHOULD-FIX — three of the fixes have no test that fails when reverted

The test audit reverted each production change one at a time against the full
suite. These are unpinned:

| Reverted change | Suite result |
|---|---|
| `ARTWORK_RETRY_BATCH` → `i64::MAX` in `sweep_artwork` (`state.rs:1509`) | **all green** |
| `tokio::spawn` → inline `self.sweep_artwork().await?` (`state.rs:1249`) | **all green** |
| Delete `retrying_artwork` + `ArtworkRetryGuard` entirely | **all green** |
| `enrich_episodes(.., repairs)` → `routes` (`metadata/mod.rs:284`) | **all green** |
| Invert never-attempted-first ordering (`media.rs:671`) | **all green** |

No test ever calls `artwork_retry_pass` — the tests call `sweep_artwork()`
directly, which bypasses both the spawn and the flag. The SQL `LIMIT` is
pinned at `limit = 1` by a store unit test, but nothing checks that the sweep
bounds itself. So the three things the PR describes as the fix for blocker 3
could all be deleted and CI would stay green.

The starvation case is untested entirely: no test seeds more than `LIMIT`
rows, and no test asserts a second pass picks up what the first left behind.

Required before merge, as concrete tests:

1. Seed `ARTWORK_RETRY_BATCH + N` eligible rows; one `sweep_artwork()`;
   assert exactly `BATCH` rows changed `artwork_attempted_at` and ≥ `N`
   remain due.
2. From the same fixture with `retry_after_secs = 0`, run passes until drained;
   assert no row is claimed twice before every row is claimed once.
3. Call `artwork_retry_pass()` twice concurrently against a TMDB stub blocking
   on a oneshot; assert one pass's worth of requests. (Forces a test through
   the real entry point.)
4. Drive `run_due_jobs` with a blocking stub; assert another due job still
   completes while the artwork pass is in flight.
5. In `a_targeted_scan_enriches_new_children_of_an_existing_show`, assert the
   show row's `poster_path`/`backdrop_path` after the first scan. Currently
   `show_hits == 1` is load-bearing by accident — dropping the blank-ancestor
   promotion leaves the show card permanently blank and only the request
   counter catches it.

### G · NIT — a show missing only its backdrop is now unrepairable

`items_missing_artwork` gates on `i.poster_path IS NULL` (`media.rs:657`) and
`run_targeted` classifies an ancestor as a repair target the same way
(`state.rs:770`). A show whose poster downloaded fine but whose
`original`-size backdrop 429'd is never selected by the sweep and is route-only
on every subsequent import. Base's `force = true` eventually refilled it. Only
a full forced refresh or an explicit `refresh_item_artwork` fixes it now. The
same applies to empty `genres` or a missing `overview`. Their own test asserts
the new behavior (`state.rs:1941-1945`), so the scoping is intentional — the
backdrop gap is the unconsidered consequence.

### H · NIT — smaller items

- **`limit.max(0)`** (`media.rs:677`) — `LIMIT -1` means *unbounded* in
  SQLite; `.max(0)` converts it to `LIMIT 0`, an empty result. No caller
  passes a negative today, and the trait doc (`store/mod.rs:397-399`) doesn't
  forbid it.
- **`ORDER BY` defeats its own index.** The leading term is the *expression*
  `i.artwork_attempted_at IS NOT NULL`, which SQLite cannot satisfy from
  `idx_items_missing_artwork`. Plain `ORDER BY artwork_attempted_at, i.id` is
  semantically identical (SQLite sorts NULLs first) and is index-satisfiable.
  The `LIMIT` bounds provider work, which is the real concern, but not DB work.
- **Drain rate silently changed** from unbounded to 9,600 rows/day
  (200 × 48). The v12 migration comment (`store/sqlite/mod.rs:388-391`) says
  draining an upgrade's inherited backlog "is the point of shipping this"; a
  100k-item catalogue now takes ~10 days. Bounding is right; 200 may be low,
  and it isn't operator-configurable.
- **A store error aborts the rest of the sweep** and emits no summary line.
  Libraries iterate in `HashMap` order, so which ones get skipped is
  nondeterministic.
- **No CHANGELOG entry.** This changes user-visible self-healing behavior —
  per-item backoff stamping, a batch cap, targeted-import scope.

---

## 3. Checked and clean

Stated so the absence of a finding is meaningful.

- **No migration, no schema change.** `store/sqlite/mod.rs` is untouched — no
  table, column, index, or `user_version` step. No downgrade story needed.
- **No HTTP or metric surface change.** `EnrichReport`, `EnrichOutcome`,
  `RefreshArtworkReport` unchanged; `sweep_artwork` has no HTTP caller.
  Values shift; no field is added, removed, or renamed.
- **Trait change is consistent.** `SqliteStore` is the sole `impl MediaStore`
  in the workspace; all three call sites updated; no mock store to drift.
- **Single-flight guard is correct.** `retrying_artwork.swap(true, Relaxed)`
  is a proper test-and-set; the early return happens *before* the guard is
  constructed, so a loser cannot clear the winner's flag. `ArtworkRetryGuard`
  is RAII and bound to `_guard`, so the flag clears on `Err`, on early return,
  on panic-unwind, and on future-drop. Mirrors the existing
  `ProduceGuard`/`GenreBackfillGuard` pattern. (Untested — see finding **F** —
  but correct.)
- **No lock held across `.await`** in any new code. The only lock is inside
  `SqliteStore::with_conn`, acquired *inside* `spawn_blocking`.
- **Shutdown is safe.** `write_artwork` writes the file fully before
  `apply_metadata`, so a kill mid-write leaves an orphan under a deterministic
  name that the row's own re-selection overwrites; the row keeps
  `poster_path IS NULL` and is re-picked next boot.
- **Moving the sweep off the tick improves other jobs.** Base's
  `self.sweep_artwork().await?` aborted the whole `for job in due_jobs` loop
  on error, skipping `CleanupTranscode`/`ProduceCache` in that tick.
- **SQL read character by character.** No `NOT IN`/`NOT EXISTS`, so the
  classic `NOT IN (…NULL…)` silent-empty trap does not apply. `LEFT JOIN`s are
  on primary keys and cannot multiply rows. `l.anime = 0` is safe. The new
  `season_number IS NOT NULL` sits inside an existing `OR` arm and does not
  disturb the partial index. The `?1 IS NULL OR i.library_id = ?1` idiom is
  correct.
- **Ordinary-case convergence has no starvation.** NULL-stamped rows sort
  first, get stamped, drop out for 24h; successive passes strictly advance.
  Steady state with 20,000 permanently art-less rows is degraded (each row
  retried ~every 2.1 days) but not starving. Pinned by
  `the_artwork_sweep_repairs_and_converges_blank_tv_children`, which is the
  strongest test in the PR — two sweeps, empty due set in between,
  `show_hits == 0`, `season_hits == 1`.
- **`Some(&[])` handling.** `id_filter` returns `None` for an empty slice, so
  an empty `repairs` cannot make every item route-only.
- **`enrich_library` back-compat wrapper** passes `only, only`, so
  `route_only` is always false and `main.rs:167` is bit-identical to base.
- **`artwork_attempted_at` equality check** (`:1548`) cannot false-positive: a
  row is only selected when its stamp is ≥ 24h old.
- **No duplicate `season_detail` fetches.** The empty-bucket loop uses
  `.or_default()`; their test asserts `season_hits == 1`.
- **Tests are not a repeat of #80.** Every substantive new test fails against
  base production code. Verified by splicing the PR's `mod tests` blocks onto
  base with a signature-only shim:

  ```
  plurx-core --lib:  320 passed; 2 failed
  plurxd  state::tests: 3 passed; 4 failed
  ```

  The one partially-vacuous case is
  `items_missing_artwork_excludes_children_the_provider_cannot_repair` — its
  anime/unenriched/home assertions hold on base (pre-existing behavior); only
  the unnumbered-season assertion is new.

---

## 4. What must change before merge

1. **A** — stop stamping a 24-hour backoff for attempts that errored or never
   happened. Use the outcome value line 1531 already receives.
2. **B** — remove the `enrich_targets` early stop (two-line fix above);
   optionally bump the depth guard 8 → 16 to match the rest of the tree.
3. **C** — make the backstop's three escape hatches log-and-continue rather
   than skip-or-abort.
4. **F** — add tests 1–5 in §2F. Three of the PR's own headline fixes can be
   deleted today without turning CI red.

**D**, **E**, **G**, **H** are worth doing but should not hold the merge.

Everything else in this PR is correct, and the test discipline is a marked
improvement over #80. The two blockers are both narrow, both have known
one-to-two-line fixes, and both were reproduced rather than argued.
