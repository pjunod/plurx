# PR #90 review — clustering M1d, the complete replicated store

**Verdict:** changes requested ·
**Verified against:** `codex/clustering-m1d-remaining-store` @ `3901448`
(draft), diffed against its base branch `codex/clustering-m1c-media-fts`
@ `95827e3` (PR #88) ·
**Reviewed:** 2026-08-08 · **origin/main** at review time: `ec0673b1`
(merge-base with main: `d4977ee` — irrelevant, this PR's base is the M1c
branch)

Scope reviewed = the two M1d commits, `git diff 95827e3 3901448`:
13 files, +2,518 / −55. Baseline at the sha is green:
`cargo build --workspace --all-targets` exit 0 ·
`cargo test --workspace` 719 passed / 0 failed ·
`make cluster-check` exit 0, seven consecutive runs, 51–57 s, no flake.

---

## The thesis: one half of this PR ships today, and it is the unreviewed half

The PR's own framing is that "the production daemon continues using SQLite"
and that this milestone only *completes* the replicated backend behind a
gate. That is true of `hiqlite_durable.rs` — `cluster.rs:47-76` hard-wires
`SqliteStore::open`, there is no backend selector in `ClusterConfig`, and
`HiqliteAuthStore`'s only consumer is the `plurx-cluster-check` binary. An
operator cannot select it. Good.

But the same PR inserts `ProgressCoalescer` **above the trait object**
(`state.rs:160`: `ProgressCoalescer::new(Arc::clone(&store))`), so it sits
on the live request path of every SQLite install the moment this merges —
`http/watch.rs:46-59` and `http/plex.rs:327`. Every finding in §1 is a
production behavior change shipping to real users now.

So the review splits cleanly:

| Half | Ships when | State |
|---|---|---|
| Progress coalescer (`progress.rs` + wiring) | **on merge, to every install** | 3 blockers, 3 should-fix |
| Replicated store (`hiqlite_durable.rs`) | M2 activation | 3 blockers, 5 should-fix |
| The gate that is supposed to catch both | now | structurally blind to identity |

**Everything below was executed.** Every §1 claim was reproduced through the
real axum router against a real `SqliteStore`, not by calling the store
directly. Every §2 claim was reproduced against a real bootstrapped hiqlite
cluster or by differential execution against `sqlite/*.rs`. Every §3 claim
is a mutation applied to pristine PR source with the gate's exit code
recorded.

---

## 1. The progress coalescer — what ships to every install

### 1.1 Blocker A — the coalescer answers `watched: true` for an item the user just un-watched, and fires a real notification and a real Trakt scrobble-stop

`progress.rs:150` serves `watched: committed.watched` from the cached last
commit for the rest of the window. Nothing invalidates that cache when
another path writes: `unscrobble` (`http/watch.rs:120-134`), Plex
`/:/unscrobble` (`http/plex.rs:364-375`) and Trakt's `apply_remote_watch`
all go straight to `state.store.set_watched_tree(...)`.

The handler then compares the coalescer's stale `watch.watched` against
`was_watched` read fresh from the DB (`http/watch.rs:39-45, 87`). After an
un-watch those disagree, so `watch.watched && !was_watched` is **true** and
the handler does the full watched-transition work — on a beat at 2 % of the
runtime:

```
response at 2%   = {"position_ms":2000, ..., "watched":true}
durable row      = (0, Some(100000), false)
watched_outbox   = (1,0,0) -> (2,0,0)          # spurious monarr webhook
```

The same path calls `state.trakt.on_progress(user, id, 2.0, true)`, and
`trakt.rs:225` does `send = pct.max(95.0)` — so Trakt receives a **Stop at
95 %** for a film the viewer just restarted. `watched_outbox` has no dedupe.

This is user-visible corruption of two external systems from an ordinary
sequence (finish something, mark it unwatched, start it again).

### 1.2 Blocker B — the deferred flush is an unconditional overwrite, so it reverts `unscrobble` and clobbers dated offline sync

`flush_loop` (`progress.rs:196`) calls `Store::put_progress`, i.e.
`put_progress_at` with `recorded_at = None`, which selects the
`?7 = 1` unconditional-overwrite branch of
`WHERE ?7 = 1 OR excluded.updated_at >= watch_state.updated_at`
(`sqlite/watch.rs:154`). That was safe when the write happened at request
time. It is not safe when the value is up to ten seconds old and no
compare-and-set guards it.

Reverting an explicit un-watch (HTTP-level repro):

```
POST progress {1000}   -> durable 1000
POST progress {50000}  -> durable still 1000        (coalesced)
POST /unscrobble       -> durable (0, Some(100000), false)
... 11.5 s ...         -> position=50000 watched=false   # un-watch undone
```

It propagates: `plan_sync` (`plurx-core/src/trakt.rs:174-180`) decides
`push_remove` from `l.updated_at`, and the flush stamps a newer
`updated_at`, so the Trakt removal is dropped too.

Clobbering a dated import — `http/watch.rs:46-52` routes
`recorded_at.is_some()` *around* the coalescer, so the store's dated-replay
ordering guard is defeated from the other side:

```
POST progress {80000, recorded_at: now}  -> durable 80000
... 11.5 s ...                           -> position=50000
```

The offline-sync clobber needs two devices on the same title within ten
seconds and is rare. The `unscrobble` revert is not rare, because of §1.4.

**Fix shape for A and B:** the flush must be a compare-and-set — carry the
`updated_at` the entry last observed and refuse the write if the row moved —
or every non-coalescer write path must invalidate the entry. A blind replay
of a stale value has no safe form.

### 1.3 Blocker C — `200 OK` is returned for a write that is not durable, and nothing drains on shutdown

`ProgressUpdate { committed: false }` still answers `200 OK` with the new
position (`http/watch.rs:59`, `plex.rs:327`). The value lives only in
`slot.pending` behind a detached `tokio::spawn` (`progress.rs:161-163`), and
`main.rs:428-441` drains HTTP connections and returns — nothing touches
`state.progress`. `SHUTDOWN_DRAIN_TIMEOUT` is 5 s, `COMMIT_WINDOW` is 10 s,
so even a deliberate wait could not guarantee it.

```
client last reported 600000 ms; restart; durable position = 1000 ms
```

A two-hour film reported at 10:00 resumes at 00:01 after a deploy. Every
active stream loses its trailing beat on every restart.

This contradicts two of the repo's own written invariants —
`docs/ARCHITECTURE.md:88` ("Replicated-durable … loss tolerance: **None once
acked**") and `store/mod.rs:12` ("A write acknowledged ⇒ durable") — and no
doc in this PR records the exception. Either drain on shutdown, or write the
new contract down explicitly and stop returning the optimistic body.

### 1.4 Why A and B are reachable: the worker steals the window

The flush worker wakes at `last_commit + window` and resets `last_commit`,
so a five-second-beat client loses the race to the request path essentially
every time:

```
1 of 5 five-second beats were durable immediately
```

Durable state therefore trails the newest beat almost always, which is what
makes "close the player, click Mark unwatched" land inside the window.

### 1.5 Should-fix — the retry loop never gives up on a permanent error

`progress.rs:203-214` has no attempt counter, no backoff and no give-up, and
`StoreError` (`plurx-core/src/error.rs:9-22`) carries no transient/permanent
distinction. Foreign keys are ON, so a pruned item or a deleted user makes
the write fail forever:

```
after 3.5 s: worker_running=true pending=Some(Pending { position_ms: 5000, ... })
```

One leaked task per affected key, one `tracing::warn!` per second (86 400
lines/day/key), and the entry becomes permanently unsweepable because the
sweep predicate keeps anything with `worker_running || pending.is_some()`.

### 1.6 Should-fix — `ENTRY_SWEEP_AT` is a trigger, not a bound

`progress.rs:75-93`: `entries.retain` keeps every active entry, so
concurrently-active keys grow the map without limit; and once
`len >= sweep_at` the O(N) retain runs on **every** beat, holding the
map-wide mutex and `try_lock`ing every entry, on the hot path.

```
sweep_at=4, active keys=40, map len = 40
```

### 1.7 Should-fix — the wiring is completely unpinned

Reverting **both** call sites to `state.store.put_progress` leaves
`cargo test --workspace` at **719 passed / 0 failed**. Only
`clippy -D warnings` notices, and only because the module goes fully dead.
Reverting `plex.rs` alone: clippy clean, 334 passed. Turning the
`recorded_at` bypass into `if false` — which destroys the offline ordering
clock — clippy clean, 334 passed.

The three unit tests in `progress.rs` do pin the coalescing itself (a
passthrough and a missing trailing flush both go red, as does an off-by-more
threshold), but six of ten mutations stayed green, including: dropping
`|| slot.pending.is_some()` from the sweep predicate, deleting the whole
`Arc::strong_count` guard, dropping the pending value on flush error, and
removing `slot.pending = None` on the due path (which is load-bearing — a
due write is otherwise immediately followed by a stale flush that rewinds
it). Nothing covers shutdown, retries, concurrency, or interaction with
`set_watched_tree` / `put_progress_at`.

### 1.8 Nit — the optimistic DTO is internally inconsistent

`progress.rs:149-154` pairs a new `position_ms` with the *previous*
commit's `updated_at`, does not clamp position to runtime as
`sqlite/watch.rs:132-135` does (`position_ms=500000` returned for a
100 000 ms item), and can report `watched` stale-true. It reaches the wire
as `WatchDto`. It happens to be harmless today only because **no shipping
client reads the response body** — Apple `postNoContent`, Android returns
`Unit`, the web player discards it, Plex answers an empty container — and
because `applied` short-circuits on the branch the coalescer can't reach.
That is luck, not a design.

### 1.9 Refuted in §1 — do not re-raise

- **The direct-play heartbeat is unaffected.** `state.direct_plays.touch_item`
  (`watch.rs:71-73`) is in-memory and unconditional; `IDLE_TIMEOUT` derives
  from `BEACON_INTERVAL`, not from `watch.updated_at`. Coalescing makes
  liveness strictly no worse.
- **`applied` is still correct**, and no client consumes it.
- **A player simply closing does not lose its final beat** — the worker
  commits it within the window. Only process death does (§1.3).
- **First-beat resume accuracy is unchanged** — `last_commit == None` is
  always `due`, so "start and immediately quit" commits synchronously.
- **The 95 % threshold arithmetic agrees with the store exactly**
  (`pos*100 >= dur*95` vs `pos/dur >= 0.95`, identical at the boundary).
- **The ordinary watched transition is synchronous and correct.** After the
  first commit, `committed.duration_ms` holds the store's *probe-derived*
  value (`COALESCE(excluded.duration_ms, …)` where `excluded` is
  `known(probe).or(client)`, `sqlite/watch.rs:126,145`), so the two sides
  cannot disagree for a probed file. The narrow real divergence is below.
- **No lock-ordering deadlock, no lost update between two coalescer callers,
  no reachable "clock without committed state" error, no busy-spin** — the
  per-key `Mutex<Entry>` is held across the store await and every `continue`
  re-derives its deadline.
- **`plex.rs` did not lose a notification in this PR** — it discarded the
  returned `WatchState` before it too.

### 1.10 Should-fix — one genuine deferred-watched hole survives

Downgraded from a claimed blocker after adjudication, but real:
`scan/mod.rs:875-893` records a file with `ProbeResult::default()` (duration
NULL) when ffprobe fails, and both native clients then omit `duration_ms`
(`PlayerController.swift:1852`, web `index.html:7222-7225`). With
`committed.duration_ms = None`, `newly_complete` (`progress.rs:122-126`) can
never fire for that entry; a backfill (`scan::reprobe_files`,
`scan/mod.rs:192-231`) then lands and the flush marks the row watched with
no callback:

```
after first commit: (1000, None, false)
crossing response:  {"duration_ms":null,"position_ms":96000,"watched":false}
after flush:        (96000, Some(100000), true)   outbox (0,0,0) -> (0,0,0)
```

The monarr event is lost outright. Trakt is not: the next beat sees
`committed.watched == true` and `plan_sync` pushes it via `push_add` — only
the real-time scrobble-stop is missed. Needs a probe failure plus a repair
landing mid-session, so: should-fix.

---

## 2. The replicated store — `hiqlite_durable.rs`

Latent until M2 activation, but the PR's own claim is that M1d is "the last
backend-completeness milestone before … the daemon can safely select
one-voter Hiqlite". These are the reasons it is not.

### 2.1 Blocker D — `watched_outbox_counts()` panics on an empty outbox

`hiqlite_durable.rs:554-556` is the only bare `SUM(CASE …)` in the file (all
of `:848, :1137, :1223, :1366-1382` use `COALESCE`). Over zero rows SQLite
returns one row of three NULLs, and `OutboxCountsRow::from` (`:588`) reads
them as `i64` via `hiqlite::Row::get`, which **panics**:

```
thread 'main' panicked at hiqlite-0.14.0/src/query/rows.rs:26:17:
Cannot convert column index 'pending' to requested type
  at crates/plurx-core/src/store/hiqlite_durable.rs:588:26
```

SQLite's backend returns `(0,0,0)`. This is the *default state of every
fresh install*, and the callers are `/metrics` (`http/system.rs:1677`) and
the monarr status endpoint (`http/comingsoon.rs:194`) — the
`.unwrap_or((0,0,0))` cannot help because the panic is inside the call.
`cluster-check` never hits it because it only reads counts after enqueuing.

### 2.2 Blocker E — two background loops that were single-writer by accident are now multi-writer against shared rows

`main.rs:353` and `:358` spawn `trakt::sync_loop()` and
`WatchedNotifier::run()` in **every** process, with no leader gate
(`grep -rn "is_leader" crates/plurxd/src` → no hits). On one node that is
fine. On three voters sharing replicated tables it is not.

**Trakt refresh unlinks the account.** `hiqlite_durable.rs:403` is a blind
`UPDATE trakt_auth SET access_token=$1, refresh_token=$2, expires_at=$3
WHERE user_id=$4` — no compare-and-swap on the token being rotated, even
though this PR uses SQL guard predicates everywhere else. Trakt rotates
refresh tokens, so with two voters inside `REFRESH_MARGIN_SECS` both POST
the same token; the loser gets `AuthExpired` and takes `trakt.rs:130`:
`delete_trakt_auth(user_id)` — deleting, cluster-wide, the row the winner
just refreshed. The user is silently unlinked while holding a valid token.
Fix: `… WHERE user_id = $4 AND refresh_token = $5` and classify the row
count.

**The outbox has no claim.** `due_watched` (`:515`) is a pure read with no
state transition and no owner column; `settle_watched` (`:532`) is an
unconditional `UPDATE … WHERE id = $6`. `watched.rs:192` ticks every second
in every process. Three voters read the same pending row in the same second
and all three POST to monarr; then node A writes `status='ok'` and node B's
timeout writes `status='pending', attempts=1` over it — redelivered forever.
The schema has no `owner`/`claimed_at`/`lease` column to fix this with.

### 2.3 Blocker F — the global byte budget is summed cluster-wide while the bytes it protects are node-local

`hiqlite_durable.rs:1140-1142` sums `offline_packages` with **no** `node_id`
filter, while `offline_package_stats` (`:1359`) is correctly node-scoped and
`plurxd` enforces node ownership before serving a byte
(`http/offline.rs:643`). `offline_max_gb` is an instance-level setting
describing one server's local cache disk (`http/system.rs:633`).

Both backends agree textually today (`GlobalByteLimit{9500,9500}` on each),
which is exactly why no test catches it — but once the DB spans nodes, which
is the entire point of M1d, node A is refused admission because **node B's**
disk is full, and A's own empty disk is never consulted. This directly
contradicts the PR's headline claim to "keep cache and offline paths and
bytes node-local". Either scope the guard by `node_id`, or rename and
re-document the limit as a cluster-wide budget.

### 2.4 Should-fix — the refusal *reason* is re-derived non-atomically and can contradict the guard that refused

The admission decision itself is correct: one atomic
`INSERT … SELECT … WHERE NOT EXISTS(request_id) AND rows < $21 AND
per-user bytes + $18 <= $22 AND global bytes + $18 <= $23 … RETURNING`
(`:1125-1148`). But when it inserts zero rows, `:1255` re-derives *why* with
four separate `query_consistent_*` reads, and if all three re-checks pass it
returns `GlobalByteLimit` **with no predicate at all** (`:1255-1258`). The
reference evaluates the reason inside the deciding transaction
(`sqlite/offline.rs:148`).

```
used=500, reserved=i64::MAX, both limits i64::MAX:
  sqlite  = Ok("Created(big2)")
  hiqlite = Ok("GlobalByteLimit{500, 9223372036854775807}")   DIFF
```

The realistic form is the race: any concurrent delete or expiry between the
guarded INSERT and the fallback reads reports `global_bytes` to the user
(`plurxd/src/offline.rs:34`) for a refusal that was a row-limit hit, or that
no longer applies at all.

### 2.5 Should-fix — a permanently removed node strands its `preparing` packages and their reserved bytes

`reset_interrupted_offline_packages` (`:1401`) and
`claim_next_offline_package` are both scoped `WHERE node_id = $2`, and the
only caller passes its own id (`plurxd/src/offline.rs:376`). In the
single-node world the node always returns with the same id; in a cluster it
may not.

```
node-gone claimed "dead1"  -> 'preparing' forever
survivor claim_next        -> None
survivor reset_interrupted -> 0
global reserved bytes still charged (incl. dead node) = 700
```

The user's client polls a package that will never progress, and its
`reserved_bytes` count against both quotas until `expires_at` (7 days).
There is no operator path to re-home or purge it.

### 2.6 Should-fix — three replicated write statements bypass the non-determinism guard

Every other replicated write goes through `self.execute` (`hiqlite.rs:225`
calls `validate_sql`) or an explicit validate loop before `txn()`
(`:693, :832, :1303, :1556, :1648`). Three `execute_returning_map` sites
call `self.client` directly with no guard: `enqueue_watched` (`:500`),
`create_offline_package` (`:1124`), `claim_next_offline_package` (`:1419`).
M1b's equivalents *do* guard theirs (`hiqlite.rs:330, :487`), as does
`hiqlite_catalog.rs:343, :371`.

Proof, injecting `sqlite_version()` (a `FORBIDDEN_IDENTIFIERS` entry hiqlite
does not stub with a panicking scalar):

```
claim_next (execute_returning_map, unguarded) -> Ok(Some("p1"))
requeue    (self.execute, guarded)            -> Err("... `sqlite_version` is forbidden")
```

The unguarded path wrote a node-derived value into a replicated column and
returned success. The shipped SQL at all three sites is clean — this is a
missing net, on exactly the statements a reviewer is least likely to
re-audit.

### 2.7 Should-fix — Trakt bearer credentials now replicate in cleartext to every voter, and no doc says so

Everything M1b/M1c replicated was a *hash* (`users.password_hash`,
`tokens.token_hash`, `api_keys.key_hash`,
`offline_package_leases.token_hash`). `trakt_auth.access_token` and
`refresh_token` are live OAuth bearer credentials in cleartext — in both
backends (`grep -rn "encrypt\|cryptr\|enc_keys" crates/plurx-core/src/store/`
→ no hits) — but M1d puts them on every voter's disk **and in every voter's
raft WAL and snapshots**, where they survive `delete_trakt_auth` until log
compaction. `CLUSTERING-PLAN.md` §3.2 carefully keeps
`enc_keys`/`secret_raft`/`secret_api` node-local; the M1d section (`:452`)
says only that Trakt credentials "use quorum-consistent reads and
deterministic replicated writes". One voter's disk image is now every user's
Trakt account. This needs a line in the threat model and, before M2
activation, envelope encryption.

### 2.8 Nits

`limit.max(0)` (`:521, :761`) turns SQLite's `LIMIT -1` (unlimited) into zero
rows — contract drift, no current caller passes a negative ·
`same_request` (`:1003`) omits `expires_at`, `estimated_bytes`,
`reserved_bytes`, so a retry with a different TTL silently returns the old
package (faithful to `sqlite/offline.rs:65`, but the PR's headline is
"enforce offline idempotency") · `offline_package_for_lease` (`:1662`)
re-reads after writing the new expiry and so denies a renewal into the past
that the reference authorizes (unreachable from the only caller) · all five
cache reads (`:643, :745, :770, :794, :844`) use `query_consistent_map`
(leader-only, documented by hiqlite as "very expensive… pauses the raft") for
rows filtered `WHERE l.node_id = $1`, i.e. rows this node wrote itself.

### 2.9 Refuted in §2 — do not re-raise

The differential parity work here is genuinely good and should not be
re-derived. Across ~60 executed scenarios covering all 16 offline methods
and the trakt/outbox/cache surface, hiqlite returned exactly what
`sqlite/*.rs` returned except where noted above.

- **No schema drift.** Built both schemas and diffed `PRAGMA table_info` /
  `foreign_key_list` / `index_list` plus CHECK clauses for all six shared
  tables: every column, type, nullability, PK, UNIQUE, FK target +
  `ON DELETE`, CHECK and `STRICT` flag identical. The only differences are
  dropped `DEFAULT` clauses, which is required. No index or constraint lost.
- **No `items_fts`-class digest hole.** `local_durable_digest` covers all
  seven durable tables and every column, each with a deterministic
  `ORDER BY`. No table omitted; no node-local-only table wrongly included.
- **Zero forbidden identifiers and zero `$N` ordering violations** across
  every statement in the file, including `format!`-built SQL.
- **Cache node-scoping is correct throughout**: `cache_bytes("A")` returns
  30, not 129; `all_cache_rows("Z")` returns `[]`; `forget_cache_entry`
  reproduces the two-statement recipe GC exactly and is atomic under raft.
- **The PR #79 "empty `all_cache_rows` deletes the cache root" blocker is
  not inherited.** `cachekeep.rs:508-517` fails closed on an empty local
  inventory and `cluster.rs:59-70` refuses to start on an id mismatch.
- **Admission, claim and lease minting really are atomic.** 8 concurrent
  creates under `max_rows_per_user=3` admitted exactly 3; 8 concurrent
  4000-byte creates under a 10 000 budget admitted exactly 2; 8 concurrent
  `claim_next` handed out 6 distinct packages, none twice; 6 concurrent
  `put_offline_lease` on one package yielded exactly 1 `Created`, 0 errors.
- **`offline_lease_guards` is not a poisonable latch** — a forced PK
  violation inside the txn rolled the whole thing back and the next lease
  succeeded.
- **No server-side RNG in the lease path** — the 256-bit token is
  client-generated and validated at `http/offline.rs:499`; only its SHA-256
  reaches the DB. Cross-package reuse fails on a constraint.
- **No cross-user idempotency collision** — key is `UNIQUE (user_id,
  request_id)` and every guard, lookup and count is `user_id`-scoped.
- **No state-machine holes** — `mark_ready` on failed/ready → false,
  `requeue` on failed/missing → false, `fail` only from `queued|preparing`;
  all identical to the reference.
- **No aggregate drift** in `offline_package_stats` or
  `offline_activity_packages`, including `active_leases` and `pinned_bytes`,
  on a fixture spanning every state plus a foreign-node row.
- **`trakt_sync_candidates` is row-for-row and order-for-order identical**
  on a fixture with ties, NULLs, a fileless item and another user's row.
- **`enqueue_watched` ids are safe** — no `last_insert_rowid`; the id comes
  from raft-serialized `INSERT … RETURNING id` and the table is append-only.
- **All 116 `Store` methods are implemented** (114 explicit + 2 provided
  defaults that both delegate to implemented methods — `SqliteStore` relies
  on the same two), and **`Arc<dyn Store>` placement is now genuinely
  possible** via the blanket `impl<T> Store for T` at `mod.rs:977`. The
  rewritten M1b doc comment is accurate.

---

## 3. The gate — `make cluster-check` proves the cluster agrees, not what it agrees on

`plurx-cluster-check` is the only automated gate for the entire replicated
backend. It *is* wired correctly: `.github/workflows/ci.yml:147` runs on
`pull_request`, is in `pr_gate.needs`, and `validation/points.toml:270`'s
`cluster-auth` check literally shells `make cluster-check`. Deleting
`hiqlite_durable.rs` fails `validation.runner lint` with exit 2. That part
is real.

### 3.1 Blocker G — every `user_id` ownership binding in the offline surface can be deleted and the gate stays green

`exercise()` (`main.rs:927-1096`) uses exactly one user and one node per
ordinal, so no assertion can distinguish "scoped to owner" from "unscoped".
Applied simultaneously to pristine PR source:

| mutation | `cluster-check` |
|---|---|
| `offline_package_for_user`: drop `AND user_id = $2` | GREEN |
| `delete_offline_package`: drop `AND user_id = $2` | GREEN |
| `renew_offline_package_for_user`: drop `AND user_id = $4` | GREEN |
| `put_offline_lease`: drop `AND p.user_id = $3` | GREEN |
| `put_offline_lease`: drop `AND p.state = 'ready'` | GREEN |
| `claim_next_offline_package`: drop `WHERE node_id = $2` | GREEN |
| `reset_interrupted_offline_packages`: drop `node_id = $2` | GREEN |
| `offline_activity_packages`: drop `p.node_id = $3` | GREEN |
| `mark_offline_package_ready`: drop `state IN ('queued','preparing')` | GREEN |
| `requeue`/`fail`/`set_recipe`/`update_progress`: drop state guards | GREEN |
| `offline_package_for_lease`: drop `p.state='ready'`; neutralize `l.expires_at > $2` | GREEN |
| `create_offline_package`: drop `user_id` from the idempotency guard | GREEN |
| `create_offline_package`: row limit `< $21` → `<= $21` | GREEN |
| `expire_offline_packages`: `<=` → `<` | GREEN |
| **all 11 authorization/accounting mutations at once** | **GREEN, 52 s** |

I installed probes in the harness, confirmed them sound against pristine
code (exit 0) and firing against the mutants (exit 1), which proves these
are live defects and not equivalent mutants:

```
PROBE VIOLATIONS: N1 offline_package_for_user leaked another user's package |
N2 delete_offline_package deleted another user's package | N3 put_offline_lease
leased a package that is not ready | N3b put_offline_lease leased another user's
package | N10 renew_offline_package_for_user renewed for another user | N4
cache_bytes reported 0 | N5 update_trakt_tokens left refresh_token unchanged |
N5b expires_at unchanged | N6 set_trakt_sync left last_sync_at = 0 |
N7 watched_outbox_counts reported pending=4242 failed=-4242
```

The red pipeline works — `cache_hit` dropping `AND l.node_id = $2` exits 1 in
11 s. The problem is precisely characterized: **the gate asserts eight
negatives, and every one of them varies a quota, a token or a repeat. None
varies an identity — no second user, no wrong node, no refused transition
post-condition.** That is the axis every blocker above lives on.

`claim_next_offline_package` losing its node scope is its own live hazard:
node B claims and transcodes node A's package, writes the artifact to B's
disk, and A serves 404 forever — while `validation/points.toml:260`, newly
broadened by this PR, asserts "node-local paths stay owned".

### 3.2 Should-fix — six of the new M1d assertions are vacuous

- `main.rs:1021` — `cache_bytes(&node_id) != 0` is only evaluated where the
  recipe is pinned, so the true answer is 0 and `fn cache_bytes → Ok(0)`
  passes. Probe N4 shows the real consequence: cache accounting reports zero
  forever, so eviction never runs.
- `:1189` — `let (_, ok, _) = watched_outbox_counts()`; returning
  `(pending+4242, ok, failed-4242)` passes.
- `:808` — `set_trakt_sync` as a pure no-op passes (`last_sync_at` stays 0 →
  Trakt re-syncs from epoch forever).
- `:821` — only `access_token` is compared; never writing `refresh_token` or
  `expires_at` passes (token refresh permanently broken after first expiry).
- `:911` — `touch_cache_claim` as a no-op passes.
- `:913` — `stale_cache_claims` is called *only* with `i64::MAX`, so the
  `l.last_seen_at < $2` bound that separates a crashed producer from a
  working one is never exercised. `sqlite/cache.rs` has a dedicated test for
  exactly this.

Nothing in `cargo test` can help: `tests/store_contract.rs:175` iterates
`for_each_sqlite_backend` only, and `hiqlite_durable.rs` has no `#[cfg(test)]`
module. Stubbing six methods to constants leaves `cargo test -p plurx-core`
at 347 + 11 passed.

### 3.3 Should-fix — the "expanded state digest through follower and leader loss" claim is false as written

`Request::Dump` is issued from exactly one place, `wait_for_equal_dumps()`
(`main.rs:93`), which runs **before** `cluster.kill(target_id)` (`:106`). No
digest is taken after any kill, and none can be — `wait_for_equal_dumps`
iterates `1..=len` and `node_mut` errors on a taken slot. What survives the
loss is `verify_proof`, which is almost entirely `is_none()` existence
checks.

Separately, `:406` compares `dumps.windows(2).all(|pair| pair[0] == pair[1])`
— voters only against each other. All three run the same binary, so every
deterministic bug yields three identical wrong digests. The digest is also
wall-clock dependent, so a golden value is impossible without changing what
the digest is.

### 3.4 Should-fix — no child exit status is ever inspected, and one assertion checks the wrong process

`NodeProcess::kill` (`:278`) uses `try_wait()` for liveness and discards
`wait()`'s status. Replacing the intentional kill of the third voter with a
real `std::process::abort()` inside that voter: **exit 0**, "all M1b/M1c/M1d
failure contracts passed". The harness cannot distinguish "I killed it" from
"it crashed".

*Correction to the PR #88 review:* the earlier claim that "an apply-time
panic after the first kill leaves the gate green" is over-broad. The same
abort injected immediately after `cluster.kill(target_id)` exits 1
("surviving voter 1 did not regain quorum readiness") because quorum is 2 of
3. The blind window is only after `VerifyProof`/`PostLossWrite` complete.
M1d changed nothing here either way.

`:132` — `if current_leader == target_id { bail!("refused voter … became
leader") }`. `target_id` is the follower SIGKILLed at `:106`, not the refused
voter; the refused voter is a separate `preflight` process (`:1207`) that
only builds a `Client::remote` and never calls `start_node`, so it has no
raft node id and can never be leader under any bug. The assertion reads "a
killed process is not the leader" — unfalsifiable. So `points.toml`'s
"incompatible voters cannot participate" is proven only for a voter that
*voluntarily* self-checks.

### 3.5 Nits

`:144` — the quorum-loss assertion accepts *any* `Response::Error`,
including `"auth store has not been opened"` or a serde failure; and `ping()`
is bounded by `READY_TIMEOUT = 3 s` while the positive `wait_for_ready`
retries for 20 s, so host slowness makes the quorum test pass spuriously ·
`.github/workflows/ci.yml:147` has no `timeout-minutes` (the
`timeout_seconds = 600` in `points.toml:170` does not apply because CI
invokes the Make target directly); every internal wait has its own deadline
so a hang cannot masquerade as a pass, but the GitHub default is 6 h.

### 3.6 Refuted in §3

`make cluster-check` **does** run on PRs (`ci.yml:147`, `on: pull_request`,
in `pr_gate.needs`, skipped only when `docs_only`) · the `cluster-auth` check
is **not** a presence assertion — it shells the binary; the `assertIn` checks
in `tests/operations/test_contracts.py:141-145` guard the CI wiring text,
which is the right scope for them · the leader-loss path **does** kill the
reported leader (`reported_leader=1 kill_target=1`) · the gate is **not
flaky** (7/7 green, 51–57 s; reds fail in 11–15 s) · exit-code and hang
honesty are correct, with distinct messages for startup failure, stream
death and per-request timeout.

**If this gate is green, what have we proven?** That three separate hiqlite
processes elect a leader, converge byte-for-byte on whatever the code
writes, rebuild local FTS from replicated rows, keep serving after one voter
dies and refuse readiness after two — and that a single-user, single-node
happy-path script runs without returning an error. That is a real
replication proof. It is not a correctness proof: eleven authorization and
accounting predicates were deleted from the production store and the
mandatory gate printed "all M1b/M1c/M1d failure contracts passed" in 52
seconds.

---

## 4. Docs, measurement and release hygiene

### 4.1 Blocker H — the milestone's own first acceptance criterion is unmet

`CLUSTERING-PLAN.md:447` acceptance: *"all store contracts pass on both
backends"*; §6.5:445: *"Run the full parity suite against SQLite and three
voters."* The parity suite is `crates/plurx-core/tests/store_contract.rs`,
whose only harness is `for_each_sqlite_backend` (`:175`) over
`SqliteStore::open` + `open_in_memory` (`:160-164`), and whose header still
reads *"Replicated backends join the same factory list in later
milestones"* (`:4`). No hiqlite factory was added. The only hiqlite evidence
is the hand-written `plurx-cluster-check` binary — a different, non-shared
exercise, whose blind spots §3 measures. The section nonetheless reads as
accepted under an unqualified "Backend slice delivered".

This is the single highest-leverage fix in the PR: adding a hiqlite factory
to `store_contract.rs` would have caught most of §2 and all of §3.1.

### 4.2 Blocker I — the "cost rerun" did not rerun the number that mattered, and the obligation to rerun it was deleted

| | pr88/main | pr90 |
|---|---|---|
| SQLite p95 | 0.041458 ms | 0.051834 ms |
| hiqlite p95 | 0.076834 ms | 0.083333 ms |
| RSS | +7,077,888 | +6,832,128 |
| SQLite growth | 2,759,224 | 2,742,648 |
| **hiqlite growth** | **8,309,777** | **8,309,777** |

`git log -S"8,309,777" pr90 -- docs/PHASE3-SPIKE.md` shows the value was
introduced by `3f3391a` (M0) and only re-touched by `04d95cb`. Main's own
text at that spot said *"M1d must coalesce writes **and remeasure
post-compaction growth** before the replicated store becomes production."*
This PR **deleted that sentence** (`PHASE3-SPIKE.md:156-157`) rather than
discharging it.

The benchmark could not discharge it anyway:
`spikes/hiqlite-m0/tests/hiqlite_m0.rs:269` is `#[ignore]`, invoked only by
`make hiqlite-baseline`, which appears in **no** CI workflow (`ci.yml:145`
runs `make hiqlite-spike`, a different target) and **no** `points.toml`
check; and it fires 10 000 *raw* `put_progress` calls straight at hiqlite,
bypassing `ProgressCoalescer` entirely — so it structurally cannot measure
post-coalescing steady-state growth. No committed artifact exists for either
run, and the provenance paragraph (`:126-134`) is unchanged, so a reader
cannot tell the M1c-recorded run from the claimed M1d rerun; both read as
"the recorded M0 cost run … taken 2026-08-07".

### 4.3 Should-fix — docs this PR falsifies and does not update

- `docs/ARCHITECTURE.md:94-96` still says the coalescer *"lands in
  CLUSTERING-PLAN M1d"* and that each five-second heartbeat *"still performs
  a watch-state read and durable write"*. M1d is this PR. (The read half is
  still true — `http/watch.rs:33, 38-44` still does a per-beat `get_item`
  and `watch_state` read; only writes were reduced.) Not in the diff.
- `docs/ARCHITECTURE.md:88` and `store/mod.rs:12` state the acked-⇒-durable
  invariant that §1.3 breaks. Neither is updated.
- `CLUSTERING-PLAN.md:455` / `PHASE3-SPIKE.md:220` — *"paths … stay
  node-local"* is wrong. `hiqlite_durable.rs:62` declares
  `relative_dir TEXT NOT NULL` inside the replicated
  `transcode_cache_locations`, and `:78` declares `source_path TEXT NOT
  NULL` inside `offline_packages`; both are in the convergence digest
  (`:193, :200`). Only the *bytes* are node-local. Say so.
- `PHASE3-SPIKE.md:219` — *"the replicated-SQL source guard covers the new
  schema"* is false for the three statements in §2.6.
- `docs/ROADMAP.md:121-134` — Phase 4 is still five undifferentiated bullets
  with no state markers while every other phase carries `✅ … complete
  <date>`; M0 and M1a are on main. `ROADMAP.md:161` is the repo's own rule
  against this.

### 4.4 Should-fix — no changelog entry, no status page update; #90 repeats #88

`git diff --stat pr88 pr90 -- Cargo.toml CHANGELOG.md docs/STATUS.html
docs/ROADMAP.md README.md` is **empty**. Workspace version is `0.2.7` at
main, pr88 and pr90.

`docs/RELEASING.md:70-78` makes the version bump a release-cut action, so no
per-PR bump is owed — but it makes `CHANGELOG.md`'s `[Unreleased]` the
accumulator, and `grep -i "cluster\|hiqlite\|replicat"` over it returns
**zero hits** across a 1,087-line section that is actively maintained for
every other workstream. The entire Phase-4 line is invisible.

`docs/STATUS.html` is untouched since main: its Phase 4 block (`:206-214`)
lists every item as `○ todo` with no M0/M1a/M1b/M1c/M1d entries, and its own
freshness stamp (`:276`) reads *"reviewed against main @ 854d575b · page
refreshed 2026-08-06"* — eight commits behind `origin/main` and predating
the 2026-08-07 delivery dates this PR writes into `CLUSTERING-PLAN.md`.

### 4.5 Nits

`points.toml:372` — adding `progress` to `watch-state.sync`'s paths adds
routing, not coverage: `rust-gate` is in `always_checks` so the three unit
tests run regardless, and `user-journey`'s only progress call
(`http/mod.rs:4091`) is a single POST that takes the leading-beat branch and
asserts `position_ms == 1000` — it cannot fail if coalescing regresses ·
`CLUSTERING-PLAN.md:345` says "114-method parity inventory" while `:46, :59,
:376, :452` say 116 (reconciled 31 lines later, but a new reader hits 114
first) · nothing asserts the `Arc<dyn Store>` claim at compile time — a
one-line `const _:` coercion would make the doc self-enforcing ·
"M1b is **green** under review" (`:3`) and "The watch-rate row is **green**"
(`PHASE3-SPIKE.md:167`) apply CI vocabulary to an unmerged PR; the pr88
wording ("under review") was verifiable and this isn't ·
`**Revised:** 2026-08-07` was not moved, which is correct — both commits are
dated 2026-08-07.

### 4.6 Refuted in §4

- **"all 116 `Store` methods"** — true, counted: SettingsStore 4, UserStore
  13, ApiKeyStore 6, LibraryStore 7, MediaStore 38, WatchStore 11,
  TraktStore 7, WatchedOutboxStore 4, TranscodeCacheStore 10,
  OfflinePackageStore 16 = 116. 114 explicit + 2 provided defaults.
- **"closes the write-rate gate"** — the arithmetic holds. Web
  `index.html:4952` 5 s · Android `PlayerScreen.kt:695-697` 10 s · Apple
  `PlayerController.swift:1510` 10 s of *content* time. Steady state is
  1 commit / 10 s / (user,item), so N streams = 0.1·N commits/s, exactly the
  `≤1 commit / 10 s / active stream` budget at `CLUSTERING-PLAN.md:35`, and
  830.98 B × 0.1/s × 6 h × 4 streams = 6.85 MiB/day.
- **"SQLite remains the daemon's selected `Store`"** — enforced by
  construction, not asserted: `cluster.rs:47-76` has no branch, and
  `ClusterConfig` has no backend selector.
- **"lease creation and renewal keep the durable token and ephemeral guard
  in one transaction"** — true, `:1520-1565` is one `client.txn`.
- **`cluster.auth`'s broadened contract text** is otherwise supported by the
  digest coverage and the `cache_hit(recipe, "other-node") → None`
  assertion; no *other* point's contract is falsified by this PR.
- **The `Revised:` date** did not need moving.
- One earlier framing is worth correcting: "`exercise()` never asserts a
  negative" is too strong — it asserts eight. The accurate and worse
  statement is in §3.1.

---

## 5. What is solid

The SQL work is the best of this series. The schema port is table-for-table,
index-for-index, constraint-for-constraint identical to the fifteen SQLite
migrations, with defaults deliberately dropped because every value is bound;
zero forbidden identifiers and zero placeholder-ordering violations across
the whole file; `local_durable_digest` covers all seven tables and every
column deterministically, so the M1c `items_fts` hole does not repeat. The
three hardest operations were collapsed correctly into single guarded
statements — admission into one `INSERT … SELECT … WHERE … RETURNING`,
claiming into one `UPDATE … WHERE id = (SELECT … LIMIT 1) AND state='queued'
RETURNING`, and lease minting into a five-statement `txn()` that carries its
read-branch through a scratch guard table instead of a round-trip — and the
concurrency probes show all three actually hold under contention. Node
scoping is applied in the SQL and re-enforced at the HTTP boundary. The
coalescer's core state machine is also well built: the per-key mutex is held
across the store await, lock ordering is consistent, the sweep's
`Arc::strong_count` reasoning is genuinely correct, and the error path
restores the pending value and recovers cleanly from transient failures.

The gap is not the SQL. It is that essentially none of it is defended by a
test, and that the one piece which ships today was written as if it owned
the watch row exclusively.

---

## 6. Suggested order of work

1. Add a hiqlite factory to `tests/store_contract.rs` (§4.1). This is the
   root cause of §2 and §3.1 and the milestone's own acceptance criterion.
2. Make the coalescer flush a compare-and-set, or invalidate the entry from
   every other write path (§1.1, §1.2). Drain on shutdown (§1.3).
3. `COALESCE` the three outbox sums (§2.1); add the CAS to
   `update_trakt_tokens` and a claim column to `watched_outbox` (§2.2);
   decide whether the offline global budget is per-node or cluster-wide and
   make the SQL and the setting agree (§2.3).
4. Add a second user and a wrong-node call to `exercise()`, and one
   post-condition per refused transition (§3.1). Fix the six vacuous
   assertions (§3.2).
5. Rerun the post-compaction growth measurement through the coalescer, or
   restore the sentence that says it still must happen (§4.2).
6. Docs: `ARCHITECTURE.md` §94-96 and §88, the "paths stay node-local"
   wording, `CHANGELOG.md [Unreleased]`, `STATUS.html` Phase 4 and its
   freshness stamp, `ROADMAP.md` Phase 4 markers.
