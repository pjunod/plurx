# PR #88 review — clustering M1c, replicated media and local search

**Verdict:** changes requested · **Verified against:** PR head `95827e3`
(`codex/clustering-m1c-media-fts`), base branch `codex/clustering-m1b-auth-store`
@ `8003072`, `origin/main` at review time `27ac86c` · **Reviewed:** 2026-08-08

Companion to [PR82](PR82-CLUSTERING-M0-REVIEW.md) (M0 identity),
[PR83](PR83-CLUSTERING-M1A-REVIEW.md) (backend-neutral parity) and
[PR84](PR84-CLUSTERING-M1B-REVIEW.md) (the first hiqlite slice, on which this
is stacked). Scope reviewed here is the **one M1c commit only** —
`git diff 8003072 95827e3`, 16 files, +3,210/−43. #84's blockers are not
re-raised; they still apply because this branch contains that commit.

Everything below was verified by execution in a pinned cloud checkout, not by
reading the diff. Where a claim is unproven it says so.

## What landed

| Piece | Size | What it is |
|---|---:|---|
| `store/hiqlite_media.rs` | 2,044 (new) | Library/Media/Watch on the raft backend |
| `store/hiqlite_catalog.rs` | 436 (new) | catalogue DDL, node-local FTS5, dump digest |
| `plurx-cluster-check/src/main.rs` | +213 | catalogue/watch/FTS cases in the 3-process gate |
| `scan/mod.rs` | +181 | **root fingerprint + bounded reconcile — ships to production** |
| `store/sqlite/media.rs` | +141 | `ensure_library_root_fingerprint`, `reconcile_library` |
| `store/sqlite/mod.rs` | +26 | migration v15, three new tables |
| `tests/store_contract.rs` | +62 | inventory 114 → 116, reconcile scenarios |

Baseline at this sha in the sandbox, all green: `cargo test --workspace`
331 plurxd + full core suite, 0 failed · `cargo fmt --check` clean ·
`cargo clippy --workspace --all-targets -- -D warnings` exit 0 ·
`make cluster-check` exit 0 in ~43 s · `scripts/validate plan --changed-from
8003072` correctly selects `library.catalog`, `core.media`,
`persistence.upgrades` and `cluster.auth`.

## Credit where it is due

1. **The hiqlite port is unusually faithful.** An adversarial pass that
   re-executed all 45 statements against a real rusqlite connection with the
   same pragmas hiqlite uses found **zero** banned identifiers, zero clock/RNG
   reads, zero `last_insert_rowid`, no lost cascade, and no schema drift beyond
   the intentional `DEFAULT (unixepoch())` removals. `COLLATE NOCASE`, `year IS
   ?`, the `l.kind != 'home'` joins and every `LIMIT`/`OFFSET` binding match the
   SQLite original character for character. `put_progress_at` is *stronger* than
   SQLite's — it folds the separate `SELECT MAX(duration_ms)` into one statement.
2. **`reconcile_library`'s "one transaction" claim is real on hiqlite**, despite
   `txn()` being unable to read or branch. The guard `INSERT`
   (`hiqlite_media.rs:1536-1544`) re-evaluates both the root fingerprint and the
   prune budget as SQL predicates inside the transaction, and statements 1–5 are
   each gated on `EXISTS (SELECT 1 FROM scan_reconcile_guards …)`. I executed the
   full nine-statement sequence: correct final state, no leftover staging rows,
   counts matching SQLite. That is a genuinely clever answer to the M0 finding
   that `txn()` can't branch.
3. **Three of the four new gate assertions are non-vacuous**, proven by mutation
   and re-run: neutering the FTS rebuild, the root check, or the prune bound each
   turns `make cluster-check` red with a specific message. #82's and #83's
   "the test structurally cannot fail" pattern is mostly gone. Mostly — see D.

---

## Blockers

### A. The scanner half ships to every existing user today, and both of its safety mechanisms are inert there

This is the thesis of the review. `scan/mod.rs` is not cluster-only code — it
runs against `SqliteStore` on every plurxd install the moment this merges,
because the daemon's scan path (`plurxd/src/state.rs:1127`) goes straight
through it. So M1c's *risky* half activates now and its *protective* half does
not.

**The prune bound has no production caller and no config key.**
`scan_library_with_progress` hard-codes `prune_limit = u64::MAX`
(`scan/mod.rs:298`), and `state.rs:1127` is the only production entry point.
`grep -rn prune crates/plurxd/src/config.rs plurx.example.toml` is empty. So
`ReconcileOutcome::RefusedPrune` is unreachable outside tests and the harness —
`requested` is a `COUNT(*)`, which cannot exceed `i64::MAX`.

**And the root fence does not cover the case the bound exists for.** The doc
comment at `scan/mod.rs:506-514` says so itself: *"A stale view of the correct
root can still carry the same identity, which is why the independent prune bound
remains mandatory."* Executed — root correct, same inode, contents gone:

```
B: established: added=25 counts=(25, 25, 1) root_ino=1786213
B: root_ino still 1786213 (same=true) -> removed_files=25 pruned_items=25
   errors=0 counts=(0, 0, 1)
```

Base on the same fixture: `removed_files=25 pruned_items=25 errors=0`. Byte for
byte the same unbounded delete, `errors=0`, nothing logged. The mandatory guard
is not wired.

Fix: give `prune_limit` a real default (a fraction of `library_file_paths().len()`
is the obvious shape) and a documented config key, and have `state.rs:1127` pass
it. Until then the PR should not claim a prune bound in
[CLUSTERING-PLAN.md](CLUSTERING-PLAN.md) §6.4 or
[PHASE3-SPIKE.md](PHASE3-SPIKE.md).

### B. Trust-on-first-use records whatever is mounted, and nothing can ever reset it

`ensure_library_root_fingerprint` is `INSERT … ON CONFLICT DO NOTHING`
(`sqlite/media.rs:1137`). The only statements anywhere that touch
`library_roots` are that insert and three `SELECT`s (`:1137`, `:1145`, `:1171`,
and the hiqlite mirrors). `update_library` (`sqlite/library.rs:61-83`) does not
clear it. There is no reset — the sole escape is deleting the library, which
takes its items and watch state with it.

**Consequence 1 — the v14→v15 upgrade can canonise the wrong root.** Simulated
an existing install whose first post-upgrade scan hits a present-but-empty
mountpoint:

```
A: simulated v14->v15 upgrade state (files,items,roots) = (3, 3, 0)
A: FIRST post-upgrade scan vs EMPTY root: removed_files=3 pruned_items=3 errors=0
A: (files,items,roots) after = (0, 0, 1)   fingerprint now established
A: recovery scan after the mount returns: added=3 removed_files=0 errors=4
   problems=[… "does not match the library identity recorded by the cluster"]
```

The wipe itself matches base — no regression there. What *is* new is that base
recovers on the next scan and this does not, forever. Nothing ties establishment
to "the library already has rows" or "the walk found ≥1 file", which is exactly
the signal in hand at `scan/mod.rs:404`.

**Consequence 2 — a legitimate path edit wedges cleanup silently.** Re-pointing
a library, or **adding a second valid folder to a working one**, flips it to
`Mismatch` on every scan from then on:

```
C: re-point scan #1: added=1 removed_files=0 pruned_items=0 errors=2
C: re-point scan #2: added=0 removed_files=0 pruned_items=0 errors=2
C: re-point scan #3: added=0 removed_files=0 pruned_items=0 errors=2
C: add-second-folder [a,b]: added=0 removed_files=0 errors=3
```

Base, same fixture: `removed_files=1 pruned_items=1 errors=1` — the stale row
is cleaned up normally. Restoring the *exact* original path set does recover.

And the operator never learns. `report.errors` has exactly one consumer in
`crates/plurxd/`: `web/index.html:7398`, which renders a red count and the
problem lines. No retry, no alert, no scheduling change, and
`mark_library_scanned` still runs on the `Ok` path — so a permanently-refusing
library looks like a healthy library with a red number next to it.

The PR's own test `a_different_present_root_commits_no_delete`
(`scan/mod.rs:1394`) bakes this in as intended behaviour by calling
`update_library` to a new root and asserting refusal. That conflates "the user
re-pointed this library" with "the wrong disk is mounted". They need different
answers: the first should re-establish, the second should refuse.

Fix: clear `library_roots` in `update_library` when `paths` changes, and expose
a "re-establish root identity" action for the disaster case.

### C. `reconcile_library` is O(items × gone) with the process-wide writer mutex held

`sqlite/media.rs:1187` (the `COUNT`) and `:1203` (the `DELETE`) inline the gone
ids into the SQL text as `f.id IN (1,2,…)`. SQLite drives from `items` and
re-walks the IN-list per row. Base's `delete_files` (`:1274`) used a prepared
`DELETE … WHERE id = ?1` loop. Both run inside `with_conn`, which holds the
single `Arc<Mutex<Connection>>` writer for the whole process
(`sqlite/mod.rs:836-850`).

```
plan (n=1000): SEARCH i USING COVERING INDEX idx_items_library_kind (library_id=?)
               SEARCH f USING COVERING INDEX idx_files_item (item_id=? AND rowid=?)

PR   reconcile_library(1000 ids,   3,893 B of SQL) -> Applied{1000,1000}     203 ms
BASE delete_files+prune_empty_items(1000 ids)      -> Ok(1000)/Ok(1000)       22 ms
PR   reconcile_library(10000 ids, 48,894 B)        -> Applied{10000,10000}  15.7 s
BASE delete_files+prune_empty_items(10000 ids)     -> Ok(10000)/Ok(10000)    250 ms
PR   reconcile_library(100000 ids, 588,895 B)      -> Applied{100k,100k}    1661 s
BASE delete_files+prune_empty_items(100000 ids)    -> Ok(100000)/Ok(100000)  2.9 s
```

Independently reproduced the plan and the scaling. Ten times the ids costs
roughly a hundred times the time; 100k ids is **575× slower than base** — 27.7
minutes with every write in the server blocked behind it. It does not error out
first: SQLite prepares 30 MB of inlined ids without complaint, so the failure
mode is a silent multi-hour writer stall, not a diagnosable exception.

100k gone files is precisely the "mount vanished in place, full re-scan" case
this feature exists for. The pathological input is the intended one.

Fix is cheap, because both SQL scopings are redundant: `gone` is derived from
`library_file_paths(library.id)` at `scan/mod.rs:396-400`, so every id already
belongs to this library. `requested` can be `gone.len()` in Rust, and the delete
can go back to the prepared-statement loop inside the same transaction.

### D. Two of three fields in the "three-voter parity" comparison are leader reads

`plurx-cluster-check/src/main.rs:795` builds `CatalogView { libraries, browse,
search }`. `list_libraries` (`hiqlite_catalog.rs:416`) and `list_top_items`
(`hiqlite_media.rs:836`) both use `client.query_consistent_map`, which hiqlite
documents as running on the leader only (`hiqlite-0.14.0/src/client/query.rs:11-13`).
Only `search_items` (`:885`) is a local read. So
`wait_for_equal_catalog_views` (`main.rs:414-436`) compares three copies of the
leader's answer plus one genuinely per-voter field.

Proven by wiping voter 2's entire `items` table from its own state-machine file
and then asking voter 2 for its view:

```
PROBE wiped=3
node2.browse = [(1, [1]), (2, [2]), (3, [3])]      (identical to baseline)
node2.libs   = ["Cluster Movies 1", …]             (identical to baseline)
node2.search = []
```

A voter with an empty `items` table reports the full browse listing. So
`main.rs:458` (`if empty.browse != baseline.browse`) can never fire, and
`verify_proof`'s `list_top_items` / `files_for_item` / `watch_state` checks
(`main.rs:857-868`) prove cluster durability rather than that the survivor holds
a local copy. Fix: use a local read, or a per-voter digest, for the parity fields.

### E. A desynced FTS index makes replicated `DELETE FROM items` fail on that voter only

`hiqlite_catalog.rs:3-5` asserts that "losing and rebuilding that index cannot
alter cluster truth." That is false. The `items_fts_ad` / `items_fts_au` triggers
run on the raft **apply** path, and FTS5's external-content `'delete'` command
raises `SQLITE_CORRUPT_VTAB` when the entry it is told to remove is absent:

```
Error: PROBE: delete voter-2 local items rows
Caused by: 0: database disk image is malformed
           1: Error code 267: database disk image is malformed
```

Reduced repro on the exact schema: after emptying `items_fts`,
`DELETE FROM items WHERE id=1` → *database disk image is malformed*, while
`UPDATE items SET title=…` still succeeds. hiqlite's apply loop
(`state_machine.rs:687-703`) returns the writer's `Result` as the entry response
and marks the entry applied regardless. So the deletion commits on the leader
and the healthy follower, errors on the drifted one, and that voter keeps rows
the cluster deleted — permanently, undetected (the digest does not cover
`items_fts`, see H) and unrepairable (there is no rebuild command, see F).

The index is not "derived, therefore harmless". It is load-bearing on the write
path.

---

## Should-fix

### F. There is no FTS rebuild command in the product

`grep -rn rebuild crates/ --include=*.rs` finds `INSERT INTO
items_fts(items_fts) VALUES('rebuild')` in exactly two places: the
non-replicated SQLite migration (`sqlite/mod.rs:247`) and the test harness
(`main.rs:462`). No store method, no CLI, no plurxd route. The property the gate
proves is unreachable by an operator. Worse, the natural implementation is
wrong: routed through `HiqliteAuthStore::execute` it becomes a raft write
applied on **every** voter — cluster-wide, not node-local — and `validate_sql`
would not object, because the statement contains no banned identifier. It is
also one unbounded statement over the whole index; the harness's three items
hide that.

### G. The gate has zero coverage of item deletion

Nothing in `exercise()` ever deletes an `items` row: both `reconcile_library`
calls (`main.rs:766-785`) are deliberate *refusals*, `ReconcileOutcome::Applied`
is never produced anywhere in the harness, and no library is deleted. Proven by
deleting the whole `items_fts_ad` trigger from `CATALOG_SCHEMA` and re-running —
**green**. Ship a build with a broken delete-trigger and every pruned item
leaves a permanent stale FTS entry on every voter; the inner `JOIN items i ON
i.id = f.rowid` (`hiqlite_media.rs:873`) keeps it from surfacing in results, so
the symptom is delayed rather than absent.

Relatedly, `cargo test -p plurx-core` runs exactly **one** test from
`hiqlite_media.rs`. `set_watched_tree`, `prune_empty_items`, `delete_files`,
`update_item_fields`, `next_up`, `continue_watching`, `watch_rollup(s)`,
`media_shape`, `item_media_facts`, `recently_added` and the `items_*` sweeps are
executed nowhere.

### H. `local_catalog_digest` cannot see a search-index divergence

Column coverage of the seven authoritative tables is complete and every
`ORDER BY` is on a primary key, so ordering is total — that part is right. The
miss is that `items_fts` is not in `CatalogDump` at all, and neither is
`sqlite_master`, so a voter missing a *trigger* also digests identically:

```
PROBE digests_equal_after_fts_wipe=true
["540a9739cf27af95…", "540a9739cf27af95…", "540a9739cf27af95…"]
```

All three digests equal while voter 2 returns zero search results for a query
the other two answer with three rows. (It also materialises the entire catalogue
in memory, which is fine while `local_dump_digest` has one test-only caller.)

### I. The root inode does not distinguish a replacement filesystem

`scan/mod.rs:512` claims *"the root inode distinguishes a replacement mounted at
the same path."* It does not — most filesystems give their root a constant inode
(tmpfs 1, ext4 2). Executed with real mounts:

```
D2: filesystem A mounted, added=2 counts=(2, 2, 1) ino=1 fp=6683de4c058df864
D2: filesystem B mounted (empty), ino=1 fp=6683de4c058df864 SAME_AS_A=true
D2: scan against the replacement: removed_files=2 pruned_items=2 errors=0
```

Inode reuse weakens it locally too: removing a directory and recreating it at
the same path handed back the same inode. Either drop the claim from the comment
or add something with more discriminating power (`statfs` fsid, or a sentinel
file written into the root at establishment).

### J. A root that vanishes mid-scan loses the whole report and starts a re-scan loop

`library_root_fingerprint` runs *after* the walk (`scan/mod.rs:404`), so a root
disappearing in between makes `canonicalize()` fail and `?` throws away a
completed scan:

```
E: warm scan added=400 in 31.009 s
E: TOCTOU scan returned Err (whole report lost): storage task failed:
   cannot fingerprint library root `…/mnt`: No such file or directory
```

Base completes with `Ok` (the vanished set is empty — the walk saw everything).
Downstream, `state.rs:1130` calls `error_status` and `return`s, discarding
`added/updated/problems` and skipping `mark_library_scanned` at `:1158`. So
`last_scan_at` stays stale, `schedule::due` (`plurxd/src/schedule.rs:80`) keeps
reporting the library due, and the 60-second tick (`state.rs:1179`) re-walks the
whole library every minute until the root returns. Compute the fingerprint
before the walk, or treat the error as a refusal note rather than a scan failure.

### K. `prune_empty_items` diverges between backends, and the contract test stopped pinning it

`hiqlite_media.rs:1649-1685` replaces SQLite's loop-until-stable childless-folder
delete (`sqlite/media.rs:1311-1322`) with the single recursive-CTE delete.
Final state is identical but `changes()` counts only directly deleted rows —
nested folders go via `ON DELETE CASCADE` and are never counted. On the fixture
from SQLite's own `empty_folder_chains_prune_all_the_way_up` test:

```
prune_empty_items -> hiqlite 2 (items left 0), sqlite 4 (items left 0)
```

This PR also changed `store_contract.rs:885` from `assert!(… >= 1)` to
`let _ = …`. The weakening is *justified* by the new ordering (reconcile prunes
first), but it means nothing pins the return value any more and the divergence is
invisible. `reconcile_library` itself is fine — it counts folders via the staging
insert and matches SQLite at 3 on the same tree.

### L. Three smaller parity/robustness gaps

- **`reconcile_library` on hiqlite returns `Err` where SQLite returns
  `Ok(RefusedRoot)`** (`hiqlite_media.rs:1620-1624`). If the fingerprint changes
  between the unreplicated pre-read and the txn applying, the guard inserts 0
  rows and the method errors out — aborting the entire scan, where SQLite logs a
  note and completes. No data risk; wrong classification.
- **Multi-read methods lost their snapshot.** `with_conn` held the writer mutex
  for the whole closure; every hiqlite read is an independent leader round-trip.
  Worst case is `list_top_items_in_genre` (`:800-828`), where the `COUNT` and the
  page are separate calls — the SQLite version deliberately shares the `GENRE`
  fragment (`sqlite/media.rs:319-325`) so a total and a page can't disagree.
  A grid can now show "100 results" over an empty page 4.
  `update_item_fields` (`:1101-1127`) is the read-modify-write case:
  `PATCH /items/:id` can return a concurrent enrichment's values as if they were
  the edit's result.
- **`main.rs:449` opens a live hiqlite state-machine DB from a second process
  with no `busy_timeout`.** Node 2's writer holds the same WAL; any concurrent
  write is an immediate `SQLITE_BUSY` and a red gate for an unrelated reason. Did
  not flake across six runs (the cluster is idle there), but it is a latent CI
  flake — and `Connection::open` *creates* the file, so a hiqlite layout change
  would plant an empty `auth.db` before failing.
- **A voter that panics on the apply path after the first kill is never
  noticed** (`main.rs:136-144`). After `cluster.kill(target_id)` only `survivor`
  is queried, `second_loss` is killed without a further request, and no child's
  exit status is ever inspected (`kill_all`, `:330-337`). An apply-time panic —
  the exact failure the whole clock/RNG ban exists to catch — leaves the gate
  green.

---

## Nits

- **`UNION ALL` in the `descendants` CTE** (`hiqlite_media.rs:1578`, `:1660`;
  `sqlite/media.rs:1219`, `:1233`). The sibling `playable_leaves` and
  `watch_rollup` CTEs use `UNION` on purpose — `sqlite/watch.rs:27-29` says
  "so a corrupt parent cycle terminates". I could not construct a reachable
  `parent_id` cycle, so this is not live; flagging only because on the raft apply
  path it would wedge the single writer thread on *every* node, not fail one query.
- **`set_watched_tree` returns changed ids in query-plan order**, not ascending
  (`hiqlite_media.rs:1831-1869` vs `sqlite/watch.rs:252-299`). Executed:
  hiqlite `[900, 901, 50, 51]` vs sqlite `[50, 51, 900, 901]`. Observable in the
  `on_watched` notification order at `plurxd/src/http/watch.rs:92-97`, and not
  stable over time — hiqlite runs `PRAGMA optimize` periodically.
- **Write timestamps come from the calling node's clock**, not the leader's
  (`hiqlite.rs:213`). Correct for replication (bound parameter), but a 5 s skew
  makes `continue_watching`'s `ORDER BY w.updated_at DESC` put an older card
  first, and both `put_progress_at` and `apply_remote_watch` clamp against the
  local `now`, so a skewed node truncates legitimate offline timestamps.
- **v15 creates two tables the SQLite backend never touches.**
  `scan_reconcile_guards` and `scan_reconcile_items` (`sqlite/mod.rs:510-518`)
  are used only by `hiqlite_media.rs:1536-1605`. They exist to keep the schemas
  aligned, which is a reasonable choice, but the migration comment doesn't say so.
- **`RefusedPrune { limit }` is a tautological assertion** (`main.rs:775-785`) —
  `limit` is echoed straight back from the caller's own argument
  (`hiqlite_media.rs:1527-1530`). The `requested: 1` half is real.
- **`replicated_catalog_schema_binds_every_clock_value`**
  (`hiqlite_catalog.rs:432`) is near-tautological: `CATALOG_SCHEMA` is pure DDL
  with no SQL function call of any kind, so `validate_sql` has nothing to reject.
  It guards future edits; the name overstates it. (`first_forbidden_identifier`
  *is* a lexical scan of the whole string, so trigger bodies are genuinely covered.)
- **`library.catalog`'s contract line was not updated** (`points.toml:302`):
  "Scans find supported media, update existing items idempotently, and preserve
  user-owned files." A scan can now refuse to reconcile at all — that is a
  contract change and the point should say so.
- **Housekeeping per project convention:** no version bump, no
  [STATUS.html](STATUS.html) update, and `CLUSTERING-PLAN.md`'s
  `**Revised:** 2026-08-07` was not moved even though the doc gained a whole
  M1c section.

---

## Checked and fine — recorded so they are not re-raised

- **Ban list / latent apply-time panic.** Zero occurrences of any of the 17
  `FORBIDDEN_IDENTIFIERS` as a whole word in any SQL literal in the new files.
  Every `format!`-assembled statement — `page_sql` across all five `ItemSort`
  orderings, the reconcile statements with inlined id lists — satisfies
  `validate_parameter_order`'s first-appearance rule. No runtime path trips
  `validate_sql`.
- **Foreign keys and cascades.** hiqlite sets `PRAGMA foreign_keys = ON` on both
  writer and reader (`state_machine.rs:381`), matching `sqlite/mod.rs:722`.
  `delete_library` → `items` → `files` / `watch_state` / `library_roots` /
  `scan_reconcile_*` all cascade, and `AFTER DELETE` FTS triggers do fire for
  FK-cascaded rows three levels deep (verified empirically).
- **Every path that mutates `items` updates the FTS index.** Enumerated all
  eight (`insert_item`, `apply_metadata`, `update_item_fields`, `set_nfo_seeded`,
  reconcile prune, `prune_empty_items`, `delete_library` cascade, `parent_id`
  self-cascade). No hole. `install_schema` goes through `client.batch` → a
  replicated log entry, so late-joining voters replay it and snapshot-installing
  voters get it in the byte copy.
- **`files.item_id` is `NOT NULL`**, so `id NOT IN (SELECT item_id FROM files)`
  cannot be poisoned by a NULL. The sibling subqueries already carry
  `parent_id IS NOT NULL`. Identical to base.
- **Empty `gone` → `list = "NULL"`** still prunes correctly, and normal-scan
  parity against base is exact on both a TV fixture (`removed_files=2
  pruned_items=4`, kinds `episode=5 season=2 show=1`) and a 3-deep home-library
  tree (`removed_files=3 pruned_items=7`).
- **Fingerprint stability** absorbs everything it should: symlink, trailing
  slash, `./` prefix, `parent/..` detour, relative path, and path order all
  produce the same digest. (`fp([a]) != fp([a,a])` — no de-duplication — but
  that only matters when `paths` changes, which already trips B.)
- **`put_progress_at` semantics** match SQLite across all five scenarios its unit
  tests cover: probe duration beating client duration, clamping, `MAX` across
  files, the 0.95 watched threshold, and the stale-offline-replay no-row case.
- **Gate plumbing is honest.** `main() -> Result<()>` yields
  `ExitCode::FAILURE`; verified exit 1 on three separate red mutations. EOF from
  a dead child becomes `bail!`, and every convergence-loop request is
  `?`-propagated, so a timeout aborts rather than spinning to the deadline —
  timeouts cannot mask a failure as a pass.

---

## Merge condition

Fix **A**, **B** and **C** before this lands, because all three are live on the
SQLite path that every current install runs — they are not deferred cluster
concerns. **D** and **E** are cluster-only but should be fixed here rather than
in M1d: D means the milestone's headline evidence is weaker than it reads, and E
contradicts a claim the module's own doc header makes.

The hiqlite port underneath (credit 1 and 2) is good work and I would not hold
it up on the should-fix list. The problem is the 181 lines in `scan/mod.rs`.
