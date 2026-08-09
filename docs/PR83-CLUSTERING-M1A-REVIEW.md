# PR #83 review — the seam is honest, the guardrail is not

**Reviewed:** 2026-08-07
**PR:** [#83 — Clustering M1a: add backend-neutral store parity](https://github.com/pjunod/plurx/pull/83)
**Branch:** `codex/clustering-m1a-store-parity` @ `034aafbc1d39c40491f267ee96f64dd7c564acc4`
**Base / merge-base:** `origin/main` @ `3f3391a66dec225b7ba9d6010ec16463f121566c`
(the PR is exactly one commit off a current main; main is not ahead)
**Outcome:** review only; no code was changed
**Verdict:** **changes requested** — one blocker, five should-fixes, two nits

**Diff:** 6 files, +1684/−1. `crates/plurx-core/src/store/replicated.rs` (new,
312) · `crates/plurx-core/tests/store_contract.rs` (new, 1358) ·
`store/mod.rs` (+2) · `docs/CLUSTERING-PLAN.md` (+6) · `docs/PHASE3-SPIKE.md`
(+4/−2) · `validation/points.toml` (+3). No production behavior or schema
change — correct scope for a seam PR.

**How it was reviewed.** The PR was cloned in a sandbox from public HTTPS,
`refs/pull/83/head` fetched, and a detached worktree pinned at `034aafbc`; the
worktree on the Mac was never read. Every claim below that says "proven" was
proven by executing code — mutating implementations in a throwaway copy and
re-running the suite, or probing the new policy type directly. hiqlite's
behavior is cited from the 0.14.0 crate source downloaded from crates.io, not
from this PR's description of it. Full workspace suite is green at this sha
(331 plurxd + 340 core lib + 11 contract, `cargo fmt --check` clean, `cargo
clippy --workspace --all-targets -- -D warnings` clean).

**Severity legend:** BLOCKER = must change before merge · SHOULD-FIX = a real
defect or a false claim, mergeable if tracked · NIT = polish. Findings cite
`file:line` in the reviewed tree.

---

## 1. Verdict

M1a set out to do three things. It does the first two well and the third one
wrong in the one way that matters.

The **seam** is real: the contract suite genuinely runs every scenario through
`Arc<dyn Store>` with no downcasting, against two backends that genuinely
differ, and the 114-method inventory is an honest count that really does trip
when an `async fn` is added to `store/mod.rs`. The **CAS contract** is exactly
right, and it is the correct answer to the M0 finding that a losing
compare-and-swap returns `Ok(0)` rather than an error.

The **replicated-SQL policy** is the problem. `FORBIDDEN_IDENTIFIERS`
(`replicated.rs:158-166`) lists seven names. hiqlite 0.14 panics on ten. The
overlap is **three**. Seven of the ten functions that will kill a voter —
`date`, `datetime`, `julianday`, `now`, `strftime`, `time`, `timediff` — pass
`ReplicatedSql::new()` today, and hiqlite's rejection is a `panic!` unwound
through the C stack at **apply time, after the entry is already committed and
replicated**. A guardrail whose failure mode is "the write already happened on
every voter" needs to be complete before anyone writes SQL against it, and
this one is the artifact M1b is supposed to build on top of.

Everything else is fixable in place and mostly a matter of narrowing four
sentences that currently claim more than the code does.

**Carried over from PR #82, now clean:** both blockers were fixed before
merge. `cluster.rs:240-247` `validate_uuid` now returns `Ok(id)` (the parsed
value, not the raw bytes), closing the B1 reinstatement; and `cluster.rs:195`
is now `mode & 0o077 == 0` rather than the `!= 0o600` equality. Confirmed at
`3f3391a`.

---

## 2. BLOCKER — the replicated-SQL policy admits 7 of the 10 functions hiqlite panics on

`crates/plurx-core/src/store/replicated.rs:158-166` bans:

```
current_date, current_time, current_timestamp, last_insert_rowid,
random, randomblob, unixepoch
```

hiqlite 0.14 registers panicking scalar-function stubs on the **write
connection** for exactly ten names — `state_machine.rs:403,414,425,436,447,458,469,480,491,502`
in `hiqlite-0.14.0`:

```
date, datetime, julianday, now, random, randomblob,
strftime, time, timediff, unixepoch
```

Intersection: `random`, `randomblob`, `unixepoch`. **Missing: `date`,
`datetime`, `julianday`, `now`, `strftime`, `time`, `timediff`.**

Proven by execution against this PR's own module:

```
ACCEPTED BY POLICY BUT PANICS ON HIQLITE:
  ["date", "datetime", "julianday", "now", "strftime", "time", "timediff"]

SLIPS THROUGH: INSERT INTO watch(user_id, updated_at)
                 VALUES ($1, strftime('%s','now'))
SLIPS THROUGH: UPDATE tokens SET last_used_at =
                 CAST(julianday('now') AS INTEGER) WHERE id = $1
SLIPS THROUGH: UPDATE sessions SET seen = datetime('now') WHERE id = $1
caught (unixepoch): UPDATE offline_packages
                 SET expires_at = unixepoch('now','+7 days') WHERE id = $1
```

Note the shape of that last pair: `unixepoch('now', ...)` is caught and
`datetime('now')` is not, so the policy teaches a developer that clock reads
are governed while leaving the six other spellings of the same clock read
wide open. `strftime('%s','now')` is the single most idiomatic way to write a
unix timestamp in SQLite, and it is one of the ones that slips.

**Why the module's own test cannot catch this.** `replicated.rs:247-257`
iterates `FORBIDDEN_IDENTIFIERS` itself and asserts each entry is rejected. It
proves the list matches itself. It is structurally incapable of detecting a
name that isn't in the list — the same self-referential shape as PR #82's
`assert_eq!(entropies, vec![4_242, 4_242, 4_242])`.

**Fix.** Pin the ten hiqlite names as their own named constant with a source
citation, and make the test assert the ban list is a **superset** of it, so a
hiqlite bump that adds an eleventh name fails loudly rather than silently
widening the hole:

```rust
/// hiqlite 0.14.0 `store/state_machine/sqlite/state_machine.rs:401-510`
/// registers a panicking stub for each of these on the write connection.
const HIQLITE_GUARDED: &[&str] = &["date", "datetime", "julianday", "now",
    "random", "randomblob", "strftime", "time", "timediff", "unixepoch"];
```

**Not a live bug.** None of the seven missing names appears in production SQL
today (`date`/`datetime` occur only in four prose comments in `domain.rs`,
`scan/nfo.rs`, `scan/home.rs`). This is a hole in the guardrail, not a defect
in shipping behavior — which is precisely why it should be closed now, while
it costs one line.

**Credit where due, and one extension.** The three names this PR adds *beyond*
hiqlite's list — `CURRENT_TIMESTAMP`, `CURRENT_DATE`, `CURRENT_TIME` — are the
genuinely valuable part. They are SQLite **keywords**, not functions, so
hiqlite cannot register a stub for them: they are accepted, executed, and
divergent per replica with no error anywhere. Catching them is real work that
hiqlite does not do for you. The same reasoning extends to `changes()`,
`total_changes()`, and `sqlite_version()` — connection-scoped, unguarded by
hiqlite, silently divergent. All three are currently unused in `crates/`, so
adding them costs nothing.

---

## 3. SHOULD-FIX — the transaction guard watches 4 of the 10 SQLite modules

`docs/PHASE3-SPIKE.md:102-104` (added by this PR):

> M1a keeps that classification executable in `store::replicated`; a
> source-site guard fails when a transaction is added or removed without
> updating the inventory.

`replicated.rs:292-297` `include_str!`s only `watch.rs`, `media.rs`,
`cache.rs`, `offline.rs`. The other six modules under
`crates/plurx-core/src/store/sqlite/` — `apikeys.rs`, `library.rs`, `mod.rs`,
`outbox.rs`, `trakt.rs`, `users.rs` — are invisible. All six have zero sites
today, so the count of 10 is correct *now*, but the guard does not do what the
doc says.

Proven: appending a real `conn.unchecked_transaction()` call to
`sqlite/users.rs` in a scratch copy leaves `every_sqlite_transaction_site_is_classified`
**passing** (5 passed, 0 failed).

This is not hypothetical. `sqlite/mod.rs` is the largest file in the directory
and **already contains an unclassified transaction**: `mod.rs:781` runs
`conn.execute_batch(&format!("BEGIN;\n{sql}\nCOMMIT;"))` inside `migrate`
(`mod.rs:762`). It is doubly invisible — wrong file *and* wrong API — and the
idiom is established, with three more instances in `#[cfg(test)]` fixtures at
`mod.rs:998`, `:1035`, `:1214`.

Three further weaknesses in the same test:

- **The count is compensating.** Delete one transaction from a watched file
  and add another elsewhere in a watched file and the count stays 10; the
  assertion passes with a now-wrong inventory.
- **The name check is a substring, not a definition check.**
  `replicated.rs:306` asserts `sqlite_sources.contains("async fn {method}")`.
  It never checks the named function is the one holding a transaction — an
  `async fn set_watched_tree_helper` in a test module satisfies the assertion
  for `set_watched_tree`.
- **Path-call form evades it entirely.**
  `Connection::unchecked_transaction(&conn)` does not contain the literal
  `unchecked_transaction()`, and neither does a one-line `fn tx(...)` helper
  called from N sites.

**Fix.** Either scope the doc sentence to "the four SQLite modules that hold
transactions today", or — better, since the point is to catch the file nobody
thought about — enumerate the directory at test time and count across all ten,
and count raw `BEGIN` batches alongside `unchecked_transaction`.

---

## 4. SHOULD-FIX — the inventory frames the porting problem too narrowly

`unchecked_transaction()` is not the population that is hard to port.
hiqlite's `txn()` cannot read, branch, or use `RETURNING` — and the store has
a larger set of sequences that need exactly those things while holding no
transaction at all, so they are invisible to this inventory:

- **`watch.rs:101` `put_progress_at`** — the strongest omission. Reads the
  probe duration (`:117-124`), clamps in Rust, reads `SELECT unixepoch()`
  (`:141`), writes an upsert with **`RETURNING`** (`:156`), and on
  `QueryReturnedNoRows` falls back to a second `SELECT` (`:165-173`). Read →
  branch → write → RETURNING → conditional re-read, and it is not atomic
  today either.
- **`mod.rs:719` `backfill_hdr_format`** — reads a settings flag, early-returns,
  reads every unprobed row, then one conditional `UPDATE` per row driven by
  Rust-side JSON parsing (`:747-751`).
- **`mod.rs:762` `migrate`** — reads `PRAGMA user_version`, branches, then per
  migration wraps a raw `BEGIN;…COMMIT;` and reads `pragma_foreign_key_check`.
- **`users.rs:145` `user_for_token`** — textbook read-then-conditional-write
  (`:160-167`), no transaction, unwatched file.
- **`media.rs:222` `insert_item`** and **`outbox.rs:27`** — `last_insert_rowid()`,
  connection-scoped, on this PR's own ban list, uncounted here.
- **`RETURNING` outside any transaction** — `media.rs:795`, `apikeys.rs:43`,
  `users.rs:32`, `library.rs:52/75/97`.

The doc and the constant both say "interactive transactions", which is
accurate to what is counted; the risk is that a reader takes "exhaustive M1a
inventory" (`replicated.rs:110`) as the M1b work list. It is a proper subset
of it. One sentence naming the excluded population would fix this.

---

## 5. SHOULD-FIX — two shapes overstate the work, one is misordered

I read all ten transaction bodies. No shape *understates* difficulty — there
is no `BatchWrite` that secretly reads, which is the failure that would
matter most. Three labels are off in the other direction:

| Method | Labelled | Actually |
|---|---|---|
| `claim_cache_entry` (`cache.rs:61`) | `CompareAndSwap` | Two blind `INSERT … ON CONFLICT DO NOTHING` (`:76-95`). No read, no Rust branch; the outcome is `taken > 0`. **Ports to `txn()` verbatim.** |
| `forget_cache_entry` (`cache.rs:234`) | `WriteThenConditionalCleanup` | Both statements unconditional; the "condition" is `NOT EXISTS (SELECT 1 …)` *inside* the second DELETE (`:257-263`). A plain 2-statement batch. **Ports verbatim.** |
| `renew_offline_package_for_user` (`offline.rs:223`) | `ReadBranchWrite` | **UPDATE-first**: statement 1 is an UPDATE (`:232-237`), the branch is on `changed == 0` (`:238`) — rows-affected, not a read — then a second UPDATE, then a read-back SELECT. |

The third one matters for planning: `txn()` *does* return per-statement
rows-affected, so this is a CAS plus a conditional follow-up, materially
easier than the four genuine `ReadBranchWrite` sites (`create_offline_package`,
`claim_next_offline_package`, `put_offline_lease`, `offline_package_for_lease`).
Correcting these three turns "10 transactions to redesign" into "2 port
verbatim, 1 is a CAS, 7 need designs" — which is the number the plan actually
wants.

Everything else checks out: the 1:1 site-to-method mapping holds (no method
holds two transactions, no transaction hides in a differently-named helper),
and the other seven shapes are accurate.

---

## 6. SHOULD-FIX — the contract suite's headline claim is not enforced

`store_contract.rs:210` fails with the message *"every trait method needs a
parity scenario"*. Nothing checks that. The inventory test (`:183-212`) scans
`include_str!("../src/store/mod.rs")` for lines with the literal prefix
`    async fn `, and diffs that set against ten hardcoded `const &[&str]`
lists (`:18-146`). It is a name-list diff with **zero linkage** to whether any
scenario calls the method. Delete every behavioral test in the file and the
inventory test still passes.

Proven by mutation — these implementations were replaced with constant
returns and **all 11 tests still passed**:

- **`create_api_key` persisting `"[]"` instead of the caller's scopes.** Scopes
  are written at `:313-320` and never read back; `:323` checks only
  `.len() == 1`. This is the surface `docs/CLUSTERING-PLAN.md:365` designates
  as *"the smallest security-bearing store surface"* for M1b, and the contract
  that is supposed to gate it does not notice the scopes vanishing.
- **`instance_id` returning a fresh random UUID on every call.**
  `settings_contract:232-233` only parses one result as a UUID; stability is
  never asserted, though `store/mod.rs:203` documents it as *"the **stable**
  unique id of this logical server"* — and M0 just built node identity on top
  of it.
- **`search_items` ignoring its `query` argument entirely.** `:779-784` is
  `.any(|item| item.item.id == movie)`; nothing checks non-matches are
  excluded.
- **`reset_interrupted_offline_packages`** — `:1275-1281` asserts the result
  `== 0`, i.e. asserts the method does nothing. `Ok(0)` satisfies it, and it
  is the only assertion on that method.
- **`get_trakt_auth`** — asserted only at `:1042` and `:1075`, both
  `is_none()`. `Ok(None)` passes.
- Plus no-op stubs for `touch_api_key`, `mark_library_scanned`,
  `set_nfo_seeded`, `set_file_audio_offset`, `update_trakt_tokens`,
  `touch_cache_claim`, `touch_cache_entry`, `set_offline_package_recipe`,
  `update_offline_progress`, `fail_offline_package` — all pass.

Structurally weak assertions elsewhere: ~20 methods called with no assertion
on the return value at all; nine `.any()` filters
(`items_needing_metadata:597`, `items_needing_artwork:607`,
`items_missing_artwork:613`, `items_missing_genres:619`,
`files_missing_probe:746`, `list_top_items_in_genre:761`,
`continue_watching:948`, `trakt_sync_candidates:1065`, `search_items:779`)
that an implementation returning *everything* satisfies;
`stale_cache_claims(node, i64::MAX)` at `:1172-1179`, where the `i64::MAX`
cutoff makes everything stale so the parameter is untested.

**Fix (highest leverage first).** Assert on values, not existence, for the
four security- and identity-bearing ones: `list_api_keys` scopes,
`instance_id` stability across two calls, `renew_offline_package_for_user`'s
`expires_at`, and `search_items` **excluding** a non-matching item. Then
either derive the const lists from the scenarios or make `:210`'s message
honest about being a name-list diff.

**What the suite genuinely does catch** — this is not a vacuous suite, and it
should not be read as one. `mark_offline_package_ready` stubbed to a no-op
**fails** at `:1312`. `watch_contract:963-991` is the strongest test in the
repo's new surface: `set_watched_tree` → rollup `(2,2)` → clear →
`apply_remote_watch(ep1)` → `next_up` yields `episodes[1]`, a real
cross-method invariant. `transcode_cache_contract:1164-1167` pins the CAS
(second claim returns false). `offline_package_contract:1241-1254` and
`:1312-1325` pin `Created`/`Existing` and `Created`/`Renewed` idempotency.
`media_contract:794` asserts `delete_files` does *not* over-delete. And the
114 count is honest: `mod.rs` has exactly 114 `^    async fn ` lines
distributed 4/13/7/36/11/7/6/4/10/16 across the ten subtraits, the const lists
match element-for-element, and every one of the 114 names is invoked at least
once.

---

## 7. SHOULD-FIX — the inventory scan is blind to non-async and out-of-file methods

`docs/CLUSTERING-PLAN.md:362-364` (added by this PR):

> its inventory test fails unless every method declared on the 10 store traits
> remains represented

The scan is `strip_prefix("    async fn ")` over one file. Proven: adding
`fn sync_gap_method(&self) -> bool { true }` to `SettingsStore` leaves the
inventory test **passing**, while adding an `async fn` correctly **fails** it.
A future sync capability method (`fn backend_name()`, `fn supports_x()` — a
plausible addition precisely when a replicated backend joins the factory) is
invisible, as is any subtrait declared outside `store/mod.rs`.

Both preconditions happen to hold today (`grep -c '^    fn ' mod.rs` = 0, all
ten traits live in `mod.rs`), so the sentence is not currently false. Narrow
it to "every `async fn` declared in `src/store/mod.rs`" — or make the scan
match `    fn ` too, which is a one-character fix and makes the doc true as
written.

Related, and worth correcting in the same edit: `docs/CLUSTERING-PLAN.md:359`
calls this "the extracted suite". Nothing was extracted. The pre-existing
`#[tokio::test]`s inside `crates/plurx-core/src/store/sqlite/*.rs` are
untouched by this diff; this is an additive, shallower parallel layer over
`dyn Store`. Some behaviors the new suite misses are in fact already covered
there (`apikeys.rs:122` round-trips scopes; `sqlite/mod.rs:974` pins
`instance_id` stability across reopen) — but only against the concrete
`SqliteStore`, which is exactly what M1a exists to stop relying on.

---

## 8. NIT — a stale catalog entry the PR edited around

`validation/points.toml:233` still registers
`crates/plurx-core/tests/hiqlite_m0.rs`. That file does not exist at either
sha — commit `733152f` (PR #82's review fix) moved it to
`spikes/hiqlite-m0/tests/hiqlite_m0.rs` and added `:235` without removing the
old line:

```
$ cargo test -p plurx-core --test hiqlite_m0 -- --list
error: no test target named `hiqlite_m0` in `plurx-core` package
help: available test targets:
    store_contract
```

`scripts/validate lint` does not catch it: `validate_catalog`
(`validation/runner.py:360-367`) only compiles the glob
(`runner.py:322-326`); nothing asserts a registered path resolves to a real
file. This PR edited that exact block. Same class as PR #82's finding I — a
catalog rule that every non-glob `paths` entry must resolve to a tracked file
would have caught both.

## 9. NIT — the `docs/**` points entry is unnecessary and widens CI

`validation/points.toml:234` adds `docs/{CLUSTERING-PLAN,PHASE3-SPIKE}.md` to
`core.media`. `docs/**` is not in `settings.audit_paths` (`points.toml:6-22`),
so nothing required it. Its only effect is impact selection: a mixed diff
touching one of these docs plus an unrelated non-`core.media` file now drags
in `core.media` and its whole consumer graph (apple, android_jvm, web_layout).
It cannot make a docs-only PR run more — `is_docs_only()`
(`validation/ci_scope.py:91-94`) short-circuits first.

---

## 10. What is built right

Worth stating plainly, because two of these were the open risks going in.

- **CI wiring is honest — PR #82's zero-tests bug does not repeat.**
  `store_contract.rs` has no `#![cfg(...)]` at all, `plurx-core` declares only
  a `fixtures` feature and neither new file references it, `rust-gate` is in
  `settings.always_checks` (`points.toml:5`), and the gate is
  `cargo test --workspace` (`Makefile:35,48`) rather than a literal name
  filter — so the "rename silently no-ops" failure mode is gone too.
  Observed: `--test store_contract -- --list` → 11 tests; `--lib -- --list |
  grep replicated` → 5 tests; `cargo test -p plurx-core` → 340 passed
  (335 pre-existing + 5). `scripts/validate plan --changed-from 3f3391a`
  selects `core.media` → `rust-gate`, and `pr_gate` would block the merge on
  a failure. This was the number-one carry-over risk and it is clean.
- **The CAS contract is exactly right.** `classify_cas` (`replicated.rs:61-67`)
  maps `0 → Stale`, `1 → Applied`, `≥2 → CasCardinalityError`. That is the
  correct answer to the settled M0 finding that a losing contended CAS returns
  `Ok(0)` and not an error — the single most likely way a fence API gets
  written wrong — and treating `≥2` as a broken single-row fence rather than
  success is the right paranoia.
- **Banning the three `CURRENT_*` keywords is real work hiqlite does not do.**
  They are keywords, unguardable by a scalar-function stub, accepted and
  silently divergent per replica. Correctly identified and correctly added.
- **The tokenizer is careful.** Single-quoted strings, double-quoted and
  backtick and `[bracket]` identifiers, `--` line comments and `/* */` block
  comments are all skipped (`:168-240`), with doubled-quote escapes handled.
  Its own test for this (`:260-268`) is not self-referential and passes, and
  my probes agree: `current_timestamp_column` and `time_ms` are correctly left
  alone. The one avoidable false positive is a bare column or alias literally
  named after a banned function (`SELECT unixepoch AS t` is rejected) — no
  such column exists in the schema, so this is theoretical.
- **The seam claims hold.** No `downcast`/`as_any` anywhere in the test file;
  `SqliteStore` appears only at `:16`, `:157`, `:161` (import and
  construction) and every scenario body takes `Arc<dyn Store>`. Both backends
  really run per test, and they genuinely differ — `open()` builds a
  2-connection read-only read pool (`sqlite/mod.rs:654-674`) while
  `open_in_memory()` leaves `reads = None` (`:678-680`), so the file-backed
  pass exercises a different read path rather than being redundant.
- **Scope discipline.** No production behavior change, no schema change, no
  touched call sites. For a PR whose job is to install a seam before the
  backend arrives, that is the right shape.

---

## 11. What was not verified here

- **hiqlite's runtime behavior.** The ten guarded names and the panic text are
  read from the 0.14.0 crate source; that the panic is post-commit and
  apply-time is carried from the M0 probe work, not re-run here.
- **Anything on real hardware.** No node, device, or fleet verification was
  in scope — this diff ships no runtime behavior to verify.
- **`make check` / `make validate-staged` as the PR body reports them.** The
  underlying `cargo test --workspace`, `cargo fmt --check`, `cargo clippy
  --all-targets -D warnings`, and `scripts/validate plan` were each run
  directly and are green; the wrapper targets themselves were not invoked
  end-to-end in the sandbox.

## 12. Fix list, in merge order

1. **`replicated.rs:158-166`** — add `date`, `datetime`, `julianday`, `now`,
   `strftime`, `time`, `timediff`; pin hiqlite's ten as a cited constant and
   assert the ban list is a superset. Consider `changes`, `total_changes`,
   `sqlite_version` in the same edit. *(blocker)*
2. **`replicated.rs:292-297`** — scan all ten `sqlite/` modules, not four, and
   count raw `BEGIN` batches alongside `unchecked_transaction()`; classify
   `mod.rs:781`. Or narrow `docs/PHASE3-SPIKE.md:103` to match what the guard
   does.
3. **`store_contract.rs`** — assert values, not existence, for `list_api_keys`
   scopes, `instance_id` stability, `renew_offline_package_for_user` expiry,
   and `search_items` exclusion; make `:210`'s message honest.
4. **`replicated.rs:115-156`** — relabel `claim_cache_entry` and
   `forget_cache_entry` as verbatim-portable, and
   `renew_offline_package_for_user` as branch-on-rows-affected.
5. **`docs/CLUSTERING-PLAN.md:359,363`** — "the extracted suite" → additive
   parallel suite; "every method declared on the 10 store traits" → "every
   `async fn` declared in `src/store/mod.rs`" (or widen the scan).
6. **`replicated.rs:110`** — name the population the inventory excludes
   (untransacted read-branch-write, `RETURNING`, `last_insert_rowid`).
7. **`validation/points.toml:233`** — delete the phantom `hiqlite_m0.rs`
   entry; consider a lint that non-glob paths must resolve to a tracked file.
8. **`validation/points.toml:234`** — drop the unnecessary `docs/**` entry.
