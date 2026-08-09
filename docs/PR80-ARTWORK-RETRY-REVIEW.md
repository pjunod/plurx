# PR #80 review — TV artwork retry eligibility

**Status:** changes requested · **Verified against:** PR head
`32f84710c45cc323c719a4a966efe63c1edab7f3`, base `origin/main`
`f9279945affd0066c82def4d9bbc9150d873f76c` (merge-base = base, so the PR is
one commit on top of main) · **Written:** 2026-08-06

Companion to [CLIENTS-CODE-REVIEW.md](CLIENTS-CODE-REVIEW.md) (clients) —
this is a single-PR review of the server-side artwork retry path.

Every claim below was read out of the pinned PR head, never the working
tree. Findings marked **repro** were reproduced by building the PR's own
tree with an added test and running it; that test's output is quoted inline.
`cargo test -p plurx-core -p plurxd` is green at the PR head: 316 + 316
passed, 0 failed.

## The verdict

The diagnosis is right and the SQL is genuinely well-built — seasons and
episodes never receive `metadata_at`, so gating on it stranded exactly the
blank cards the sweep exists to repair. The join rewrite is correct,
index-covered, correctly parenthesised, and correctly excludes anime and
home libraries. Six separate refutation attempts against it failed (§6).

But it must not merge as written. Widening the candidate set to seasons and
episodes admits rows that **four code paths select and then cannot stamp**.
`items_missing_artwork`'s backoff clause is
`artwork_attempted_at IS NULL OR artwork_attempted_at <= unixepoch() - ?2`,
so an unstamped row is eligible *unconditionally* — the 24h
`ARTWORK_RETRY_BACKOFF_SECS` never engages for it. The retry job is on by
default at `ARTWORK_RETRY_DEFAULT_MINS = 30`. The net effect is that a
self-limiting daily retry becomes a permanent half-hourly loop that also
re-downloads healthy show artwork every pass (§2).

Three blockers, three should-fixes, two nits. §7 has the fix shape.

---

## 1. Blocker — rows the sweep selects but nothing can stamp

Four paths reach `continue`/return **before** any `apply()`, so
`artwork_attempted_at` is never written for the row the sweep selected:

| Path | File:line | Trigger |
|---|---|---|
| Episode has no TMDB counterpart | `metadata/mod.rs:581-587` | `Specials`/`S00`, `S01E13` when TMDB lists 12, `S01E01E02` double-episode, mis-parsed filename, absolute numbering in a non-anime library, or `episode_number IS NULL` (the comparison `Some(r.episode_number) == None` is never true) |
| `season_detail` errors | `metadata/mod.rs:538-545` | 404 for a local season TMDB lacks — `send_with_retry` (`metadata/tmdb.rs:173-190`) deliberately does not retry 404. Also swallows an exhausted 429 and network errors |
| Show lookup misses | `metadata/mod.rs:225-230` | `enrich_episodes` is only called from the `Ok(Some(m))` arm. A renamed show, a cleared TMDB key, or a TMDB outage strands every child the sweep selected |
| Season row with `season_number IS NULL` | `metadata/mod.rs:508` | `filter_map(|s| s.season_number.map(..))` drops it from `season_items` *and* from the new `only` bucket loop, so it gets zero work — and the new SQL has no `season_number IS NOT NULL` guard |

**Repro** — one local `S01E99` under a show whose TMDB season 1 lists only
E1, run against the PR's own mock:

```text
REPRO sweep1 repaired=1 ghost.poster=None ghost.attempted=None
REPRO still due after sweep1 (24h backoff): [(3, "Episode")]
REPRO sweep2 repaired=1 season_hits=2
```

The row is still due immediately after a sweep even when the backoff is set
to the production 24 hours, because the stamp is NULL. It will be selected
on every sweep, forever.

**Why this is new.** Pre-PR the query returned only `movie`/`show`, both of
which always stamp. This is the same failure mode the PR's own comment at
`metadata/mod.rs:548-551` claims to close — "instead of being selected by
every half-hour sweep forever." That fix landed for the season *poster*
case (§6.6 confirms it works) and was not carried to the three sibling
exits.

**Cost per stuck show, per 30 minutes, indefinitely:** one `/tv/{id}`
detail + one poster download + one backdrop download at
`BACKDROP_SIZE = "original"` (`metadata/mod.rs:28`, commonly 1–4 MB) + one
`season_detail`. One `Specials` folder in one show is enough to start it.

## 2. Blocker — `force = true` rewrites artwork of shows that are fine

`state.rs:1444` feeds `enrich_targets` output — the blank child *and all its
ancestors* — into `enrich(&library, true, ..)`. The Show branch
(`metadata/mod.rs:177-194`) calls `cache_image` unconditionally for both
poster and backdrop. It has no `needs_art` guard — the very guard this PR
adds one level down at `metadata/mod.rs:552`.

From the same repro, on a show seeded with a perfectly good poster:

```text
REPRO show.poster after sweep1 = Some("1-poster.jpg")   (was existing-show.jpg)
```

Standing alone this would be a one-time cost. Chained to §1 it is
permanent: one un-stampable episode pins its show in `targets` forever.
Roughly 50–200 MB/day of TMDB CDN traffic and artwork-dir rewrites *per
stuck show*.

Secondary: the show patch also carries `title`, `year`, `overview`,
`genres`, and `apply_metadata`'s `COALESCE(?10, poster_path)` shape
(`store/sqlite/media.rs:452-469`) writes them over a manual rename made
through `update_item_fields`. Pre-existing mechanism, newly reachable for
any show with one blank child.

Refuted sub-concern: clients are unaffected. Artwork filenames are
deterministic (`{item_id}-{kind}.jpg`) and `serve_artwork` sends
`Cache-Control: public, max-age=604800` with no ETag
(`http/images.rs:49`), and `updated_at` is not a sync cursor anywhere. The
damage is server-side bandwidth and IO only.

## 3. Blocker — the whole change runs inline on the 60-second scheduler task

`items_missing_artwork` has **no `LIMIT`** — only an `ORDER BY`
(`store/sqlite/media.rs:651-670`). `sweep_artwork` materialises every row,
and `state.rs:1176-1179` awaits it inline in the scheduler loop whose own
comment at `state.rs:1163-1165` justifies that with "both are short". That
was true when the candidate set was bounded by title count. It is not true
now.

First run after this ships selects every posterless season and episode in
every non-anime TV library at once. A library with 5,000 blank episodes
means ~5,000 still downloads at `STILL_SIZE`, ~250 season details and ~50
show details in one uninterruptible call — hours, multiple GB. During that
window `DueJob::Scan`, `DueJob::Refresh` and `DueJob::CleanupTranscode`
(which includes `cachekeep::sweep`, the disk-reclaim path) do not run at
all. `ProduceCache` is `tokio::spawn`ed at `state.rs:1197-1204` for exactly
this reason; the now-long sweep is not.

Self-overlap is safely prevented — single sequential loop, stamp before
run, no HTTP trigger. That is the only part of this that is fine.

## 4. Should-fix — `enrich_targets` is 3N round-trips with an O(n²) dedup

`state.rs:758-782` walks ancestors per id with no memoization, so every
episode re-fetches its season and its show: 5,000 blank episodes across 50
shows and 250 seasons is **~15,000 `get_item` calls to reach 5,300 distinct
rows**. Each is a `spawn_blocking` + read-pool acquire
(`store/sqlite/mod.rs:820`), fully serialised. Measured on in-memory SQLite
with zero contention:

```text
5000 episodes -> 5300 targets in 2.677s
```

That is a floor; a file-backed DB on a NAS under live playback is
materially worse, and per §3 it blocks the scheduler before a single TMDB
call is made.

The `targets.contains(&current)` linear scan (`state.rs:766`) is real but
is *not* the bottleneck at this size — ~40M `i64` compares, tens of
milliseconds, ~1% of the measurement above. It only bites past ~50k ids.
Fix both together: `HashSet` for the dedup, and skip the ancestor walk
entirely for ids already in the set.

Related, benign: `id_filter` (`store/sqlite/media.rs:68-77`) inlines ids as
SQL literals, so the `items_needing_metadata` statement grows to ~37 KB at
5,300 targets. Measured at 10 ms, no SQLite limit hit. Untidy, not broken.

## 5. Should-fix — `repaired` counts show matches, not repairs

`EnrichReport.matched` is incremented only in the Movie and Show arms
(`metadata/mod.rs:159`, `:213`), on `apply()` success, regardless of whether
artwork landed. Episodes go to a separate `episodes_matched`
(`metadata/mod.rs:600`); seasons are counted **nowhere** —
`metadata/mod.rs:575` discards `apply`'s bool. So at `state.rs:1445`:

- 1 show + 40 blank episodes repaired → `attempted = 41`, `repaired = 1`.
- The §1 repro → `attempted = 1`, `repaired = 1`, actual repairs **zero**.
  The operator log reports progress on a run that made none and never will.

The log line at `state.rs:1447-1451` also mixes denominators: `attempted` is
the pre-expansion count, `repaired` is post-expansion.

And `assert_eq!(jobs.sweep_artwork().await.expect("sweep"), 1)` in the new
test (`state.rs:1890`) asserts this number, locking in the wrong semantics.
An implementation that repaired zero children still returns 1.

## 6. Should-fix — the tests do not cover the two things the diff does

`the_artwork_sweep_repairs_blank_tv_children` is not worthless — the two
`poster_path.is_some()` assertions genuinely prove that entering through the
show repairs children, which is the headline claim. But neither hunk in
`metadata/mod.rs` is exercised:

- **The empty-bucket insert (`metadata/mod.rs:529-535`) is a no-op in the
  test.** The test makes *both* the season and the episode blank, so the
  episode is in `only`, so bucket 1 is already non-empty and `or_default()`
  does nothing. The motivating case — a blank season whose episode stills
  are all healthy — has no test.
- **The `needs_art` widening (`metadata/mod.rs:552`) is a no-op in the
  test.** The mock serves `"poster_path": "/season-1.jpg"`
  (`state.rs:1569`), so `remote.poster_path.is_some()` is true and the
  removed conjunct never mattered. The test passes verbatim against base
  `metadata/mod.rs`.
- **Nothing asserts convergence.** The sweep is never re-run and
  `artwork_attempted_at` is never inspected. That gap is precisely why §1
  shipped.
- **Nothing asserts the show's poster is left alone** — and per §2 it is
  silently overwritten inside this very test. `season_hits == 1` guards
  season re-fetch; there is no equivalent guard on show images.
- The mock's `.fallback(get(|| async { vec![0,1,2,3] }))` answers *every*
  image path with bytes, so `Artwork::failed` / `Artwork::unavailable` are
  never exercised at the sweep level.
- The new store test is positive-only: no case for anime-library children
  being excluded, a child of an *unenriched* show being excluded, a home
  library being excluded, or a selected child ending up stamped — the last
  of which would have caught §1.

### Nits

- **`artwork_error` clobber.** With the widened `needs_art`, a season TMDB
  has no poster for now carries `Failed("the provider has no image for
  this")`, and `artwork_error = CASE WHEN ?15 = 1 THEN ?16 ..`
  (`store/sqlite/media.rs:482`) overwrites a previously recorded
  `"download: 429"` with the less actionable message. Movies already make
  this trade, so it is consistent — just lossy for the operator.
- **A show whose own poster is also NULL** is selected alongside its
  children. Harmless: `enrich_targets` dedupes, so it is one pass.

## 7. What is correct — refutations that succeeded

Recorded so the next reader does not re-litigate them.

1. **`libraries.anime` is `INTEGER NOT NULL DEFAULT 0`**
   (`store/sqlite/mod.rs:143`, v6 rebuild at `:188`). No NULLs, so
   `l.anime = 0` drops nothing silently. Excluding anime children is right
   — `enrich_anime_library` skips every non-`Show` kind
   (`metadata/mod.rs:342-345`).
2. **Episodes really do store their still in `poster_path`**
   (`metadata/mod.rs:588-604` writes `cache_image(.., meta.still_path, ..)`
   into `patch.poster_path`). There is no separate still column.
   `poster_path IS NULL` is the correct predicate.
3. **The new test's `[season, episode]` ordering is guaranteed**, not
   incidental: `ORDER BY i.artwork_attempted_at IS NOT NULL,
   i.artwork_attempted_at, i.id` — both NULL, tie-break on `i.id`.
   Verified by executing the statement against SQLite 3.45.
4. **No table scan.** `EXPLAIN QUERY PLAN` gives
   `SCAN i USING INDEX idx_items_missing_artwork` — the partial index
   `ON items(artwork_attempted_at) WHERE poster_path IS NULL`
   (`store/sqlite/mod.rs:381`) — then rowid `SEARCH` for `l`, `parent`,
   `grandparent`. Both `LEFT JOIN`s are on the PK, so they cannot multiply
   rows.
5. **Parenthesisation and binding are correct.** The OR chain is properly
   grouped; `?1` = library id, `?2` = backoff. Executed with both a
   concrete id and NULL, identical correct results. No unintended kinds:
   `folder`/`video`/`photo` are not in the chain, `l.kind != 'home'` still
   holds, and a flat show→episode tree with no season rows is correctly
   excluded by `parent.kind = 'season'`.
6. **The `needs_art` comment's claim is TRUE for seasons.** `cache_image`
   (`metadata/mod.rs:640`) returns `Artwork::unavailable()` →
   `Artwork::failed(..)` → `attempt: Some(ArtworkAttempt::Failed(..))`;
   `MetadataPatch::is_empty` ends with `&& self.artwork.is_none()`
   (`domain.rs:261`), so the `if !patch.is_empty()` guard never suppresses
   it; `apply_metadata` stamps unconditionally via
   `CASE WHEN ?15 = 1 THEN unixepoch() ..`. A semantically no-op patch
   still stamps. And it costs zero extra HTTP — `cache_image` returns
   before `download_image`.
7. **The empty bucket is safe and correctly ordered.** In the final file the
   map is built at `:503-510`, episodes grouped at `:512-524`, empty buckets
   inserted at `:529-535` — *after* grouping, so `or_default()` cannot clear
   an existing bucket and `BTreeMap` dedupes. `for ep in locals` over an
   empty `Vec` is a no-op with no indexing or `unwrap`, and
   `season_items.get(&season_number)` is guaranteed `Some` because the key
   came from `season_items` itself. `only == None` (full scan) is entirely
   unchanged — the whole block is inside `if let Some(ids) = only`.
8. **Home and anime libraries at the sweep level are fine.** Home never
   reaches `by_library` (`l.kind != 'home'`). An anime *show* is still
   selectable exactly as pre-PR; `enrich_targets` on a show yields `[show]`
   and `enrich_anime_library` filters non-Show kinds anyway.

## 8. The fix shape

Ordered by what unblocks the merge.

1. **Stamp on every exit** (fixes §1). The cheapest correct version is a
   backstop rather than four separate patches: after `enrich` returns in
   `sweep_artwork`, write `ArtworkAttempt::Failed` for every target id that
   still has `poster_path IS NULL` and whose `artwork_attempted_at` did not
   move. That closes all four paths at once and cannot be re-opened by a
   fifth. If you prefer per-path stamps, all four in the §1 table need one,
   and the `season_number IS NULL` season needs either a stamp or a
   `season_number IS NOT NULL` guard in the SQL.
2. **Do not force-re-enrich healthy ancestors** (fixes §2). Either add the
   `needs_art` guard to the Show branch that the Season branch just got, or
   — cleaner — distinguish *entry point* from *repair target*: an ancestor
   pulled in by `enrich_targets` should route the provider call, not be
   re-fetched itself.
3. **`LIMIT` the sweep and get it off the scheduler task** (fixes §3). A
   few hundred rows per pass plus `tokio::spawn`, matching `ProduceCache`.
   With §1 fixed, the backoff drains the backlog over successive passes
   instead of one unbounded run.
4. **`HashSet` in `enrich_targets`, and skip walks for known ids** (§4).
5. **Count seasons and episodes in the return value, or rename it** (§5),
   and drop the `assert_eq!(.., 1)` in favour of asserting the children.
6. **Tests** (§6), each one a case that currently passes vacuously:
   - two consecutive sweeps, assert `items_missing_artwork` is empty after
     the second — the convergence property nothing currently asserts;
   - a season TMDB has no poster for (mock omits `poster_path`), assert
     `artwork_attempted_at` is set;
   - a blank season whose episodes are all healthy, assert the empty
     bucket actually fetches the season;
   - an episode number TMDB does not list, assert it converges;
   - anime-library children excluded; child of an unenriched show
     excluded; home library excluded;
   - the show's existing poster is untouched by a sweep.

## 9. Non-goals of this review

- Client-side behaviour. Nothing in this PR touches the apps; the blank
  cards they render are a consequence of the server rows, not of client
  code.
- The pre-existing decision to enrich TV through the show. That is right
  and this PR does not change it.
- Re-litigating §7. Those were checked against the pinned sha with
  executed SQL and read code; treat them as settled unless the code moves.
