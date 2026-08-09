# WO-01 — Server correctness: scan prune floor + clustering store hardening

**Repo:** `~/code/plurx` · **Baseline:** `origin/main` @ `e8a910f` · **Priority: P0 (task 1), P1 (rest)**
All file:line references are at that sha. Work on a branch, PR to main, keep commits per-task.

## Context

The merged clustering work (M0–M1c) is in good shape — every pre-merge blocker was verified fixed. What remains is one live correctness bug that ships to every SQLite install today (task 1) and a set of hardening items on the hiqlite side that become production hazards the day M2 activates the replicated store.

## Tasks

1. **P0 — Prune limit needs a floor (small libraries wedge forever).**
   `crates/plurx-core/src/scan/mod.rs:316-322`: `prune_limit = known.saturating_mul(percent) / 100` — integer division, no floor. A 9-file library at the default `storage.scan_prune_percent = 10` gets `limit 0`; delete one file and every scan yields `RefusedPrune { requested: 1, limit: 0 }` plus a red error, forever. The root-identity establish path refuses too (`scan/mod.rs:455`: `known_count == 0 || (!seen.is_empty() && gone.len() <= prune_limit)`). Home-video libraries are exactly this small.
   Fix: when `percent > 0 && known > 0`, use `(known * percent / 100).max(1)`. Keep `percent == 0` meaning "pruning disabled" (that semantic is documented in OPERATIONS.md — don't let the floor resurrect pruning there).
   Acceptance: new scan test — 5-file library, default percent, delete 1 file → `Applied { deleted_files: 1, .. }`; and a `percent = 0` test still refusing.

2. **P1 — Extend the M1b timeout contract to every M1c/M1d hiqlite call.**
   M1b wrapped auth calls in `timeout_store` (3 s, `store/hiqlite.rs:25,692-699`) because hiqlite 0.14 has no internal timeout and a wedged leader hangs callers forever. M1c reintroduced ~30 raw `self.client.query_consistent_map / execute_returning_map / txn` sites in `hiqlite_media.rs` (e.g. `:602, :753, :821, :1624, :1675, :1737`) and `hiqlite_catalog.rs` (`:366-379, :453-475`) with no timeout. Latent until M2 activation, then it's production hangs.
   Fix: route every client access through `timeout_store` (make it the only accessor), and add a guard in the same spirit as the transaction-site inventory: a test that greps the two files for raw `self.client.` outside the helper and fails on any hit.

3. **P1 — `validate_sql` on read paths.**
   Writes validate `$N` first-appearance ordering; most M1c reads don't (`hiqlite_media.rs:601-614` and the `scalar_with`/`pairs`/`rows` helpers). hiqlite binds positionally by first appearance, so a future edit that reorders placeholders in an unvalidated read mis-binds silently — wrong rows, no error. Fix: validate inside the shared query helpers. Acceptance: a test feeding deliberately mis-ordered SQL through each helper expects `Err`.

4. **P2 — In-txn prune-budget refusal is mislabeled.**
   `hiqlite_media.rs:1632-1645`: if file count grows between the pre-count and the txn's re-check, the refusal reports `RefusedRoot` with `expected` == observed fingerprint — a self-contradictory operator message for what is actually a prune-budget refusal. Re-derive the reason (or emit `RefusedPrune`) on that path.

5. **P2 — Replace inlined `IN (…)` id lists with `json_each($1)`.**
   `hiqlite_media.rs:1522-1526, 1551-1559` (also `watch_rollups:1975`, `item_media_facts:1349`): ~100 k gone ids inline ~800 KB of SQL per statement through the raft log, approaching SQLITE_MAX_SQL_LENGTH. `watch_map` (`:1783-1790`) already shows the `json_each` pattern — use it.

6. **P2 — Two hygiene items.**
   - `store/hiqlite.rs:3-6` module doc still says the type "intentionally implements only Settings/User/ApiKey" — false since M1c (it also implements Library/Media/Watch). Update, consider renaming `HiqliteAuthStore`.
   - `crates/plurx-core/tests/store_contract.rs:905` is still `let _ = store.prune_empty_items(...)` — the #88 review flagged this weakened assertion and it was never restored. Assert the return value (≥1 with a real fixture) on both backends. (If you're also doing WO-02, note #90's branch reworks this file — coordinate.)

## Don't

- Don't re-audit the M1c hiqlite SQL port (schema parity, FTS triggers, forbidden-identifier ban list, parameter-order rule) — it was adversarially verified clean twice.
- Don't touch the `contentless_delete=1` FTS design — it's the accepted fix for the desynced-voter delete failure.

## Acceptance (whole WO)

`make check` green; `make cluster-check` green; the two new guard tests (raw-client grep, misordered-SQL) red when their targets are reverted — prove that by reverting once before merging.
