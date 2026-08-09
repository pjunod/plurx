# PR #84 review — clustering M1b, replicated auth state

**Verdict:** changes requested · **Verified against:** PR head
`8003072` (`codex/clustering-m1b-auth-store`), merge-base and `origin/main`
at review time `d4977ee` / `27ac86c` · **Reviewed:** 2026-08-08

Companion to [PR82](PR82-CLUSTERING-M0-REVIEW.md) (M0 identity + hiqlite
proof) and [PR83](PR83-CLUSTERING-M1A-REVIEW.md) (backend-neutral parity) —
this is the first slice that puts hiqlite in the shipping resolution.

Everything below was verified by execution in a pinned cloud checkout, not by
reading the diff. Where a claim is unproven it says so.

## What landed

+3,840 lines over 14 non-vendor files, plus **23,382 lines of vendored crate
source** under `vendor/`. The substance:

| Piece | Size | What it is |
|---|---:|---|
| `crates/plurx-core/src/store/hiqlite.rs` | 926 | `HiqliteAuthStore`, 23 methods |
| `crates/plurx-cluster-check/src/main.rs` | 776 | separate-process 3-voter harness |
| `vendor/rust_decimal` | 21,154 | crates.io 1.42.1, rkyv edge removed |
| `vendor/s3-simple` | 2,228 | crates.io 0.8.0, quick-xml 0.39→0.41 |
| `.github/workflows/ci.yml` | +21 | new `cluster_auth` job |

Baseline at this sha in the sandbox: `cargo build --workspace --all-targets`
green in 4m59s · `make cluster-check` green in ~70s · both new unit tests pass.

## Credit where it is due — three things this PR gets right

1. **`pr_gate` now actually gates `hiqlite_spike`.** That job existed since M0
   but was not in `pr_gate.needs` — it could fail without blocking a merge.
   This PR adds both it and `cluster_auth` to the needs list *and* the result
   loop (`ci.yml:424-425`, `:450`). That is a real pre-existing hole closed in
   passing, and `tests/operations/test_contracts.py:141-145` pins it.
2. **My #83 blocker was fixed before M1a merged.** `FORBIDDEN_IDENTIFIERS`
   (`replicated.rs:231-249`) now carries **17** names covering all ten hiqlite
   guards (`hiqlite-0.14.0/.../state_machine.rs:401-508`) plus the three
   unguardable `CURRENT_*` keywords, and `replicated.rs:376-400` asserts the
   superset invariant. `git diff f757663 HEAD -- .../replicated.rs` is empty —
   fixed on the M1a branch, not here. Nothing open.
3. **The schema's missing `DEFAULT (unixepoch())` clauses are correct, not an
   oversight.** Seven columns differ from the SQLite DDL in exactly that way
   (`AUTH_SCHEMA` vs `sqlite/mod.rs:39-...`); that is the price of §G's guard
   and it was paid deliberately. `execute_returning_map_one` instead of
   `last_insert_rowid()` is likewise the right call. And `ping`'s consistent
   read is load-bearing, not decoration — proven below.

---

## Blockers

### A. hiqlite is now a hard dependency of a daemon that never calls it

`crates/plurx-core/Cargo.toml:16` adds `hiqlite` as a **non-optional**
dependency, and `store/mod.rs:17` is a bare `mod hiqlite;` with no cfg gate.
`grep -rn "hiqlite\|HiqliteAuthStore" crates/plurxd/src` returns **nothing** —
plurxd still goes through `cluster::open_store` → `SqliteStore`. So the
shipping daemon pays for a backend it cannot reach.

Measured, clean release builds of `cargo build --release -p plurxd` (exactly
what `Dockerfile:15` ships), base `d4977ee` vs PR head, sequential, own target
dirs:

| | base | PR | Δ |
|---|---:|---:|---:|
| crates compiled | 191 | **331** | **+140 (+73%)** |
| CPU-seconds | 643.8 | 844.5 | +200.7 (+31%) |
| build-script CPU-s | 108.5 | **269.7** | +161 |
| stripped binary | 21,605,112 B | 22,977,112 B | +1,372,000 B |
| `Cargo.lock` names | 240 | **417** | +177 |
| `target/` after build | 442 MB | 879 MB | +437 MB |

Newly compiled in the shipping build: `openraft`, `hiqlite`, `hiqlite-wal`,
`cryptr`, `quinn`, `axum-server`, `fastwebsockets`, a **second major of
reqwest** (0.13.4 beside 0.12.28), and **two versions of `aws-lc-sys`**
(0.43.0 via rustls←axum-server←hiqlite, 0.39.1 via the vendored
s3-simple←cryptr←hiqlite) — 707 `.o` files and two 18 MB static archives,
~200 CPU-seconds of C compilation, with `cmake` now a build dependency.

Rust-level DCE does its job: probing for `openraft`, `hiqlite`, `Raft`,
`aws_lc`, `quinn`, `cryptr` string/panic-path literals finds **zero**
occurrences in either binary, and 35 of the 36 long string literals in
`hiqlite.rs` are absent (the 36th is duplicated verbatim in
`sqlite/mod.rs:900`). The static aws-lc archives are built and not linked
(`ldd` identical). **The +1.37 MB is feature unification, not hiqlite code**:
hiqlite drags `reqwest 0.13` + `axum-server`, which turns on `hyper`'s `http2`
feature for the shared `hyper 1.10.1` and pulls the whole `h2` stack into
plurxd's own server. `h2::` literal count goes 0 → 27.

**This is avoidable today with four lines**, and I verified the fix compiles
both crates: `optional = true` on the dependency, a
`hiqlite-store = ["dep:hiqlite"]` feature, `#[cfg(feature = "hiqlite-store")]`
on the `mod` and the `pub use`, and `features = ["hiqlite-store"]` on
`plurx-cluster-check`'s path dep. Result: `cargo tree -p plurxd -e
normal,build` returns to **191** units — identical to base — and
`cargo check --release -p plurx-cluster-check` still finishes green. The
Docker/shipping path pays nothing; only `ci.yml:333`'s
`cargo build --release --workspace` still pays, in one job.

`plurx-core` already has a `[features]` section (`fixtures = []`), so no new
machinery is needed. Ask: gate it, and say in CLUSTERING-PLAN §6.3 that the
daemon's dependency closure is unchanged until M1d flips the switch.

### B. The compatibility contract is not proven, and the docs' claim is false

`PHASE3-SPIKE.md` (M1b table, "Compatibility" row) says: *"A remote consistent
preflight reads schema and protocol bounds before raft startup"*, and
CLUSTERING-PLAN §6.3 says *"an incompatible process neither migrates nor
participates in an election."*

The code does not do that. In `main.rs:396-404`, `hiqlite::start_node` runs
**before** any store call; `Request::Open` → `HiqliteAuthStore::open` →
`verify_compatibility` happens afterwards, over an already-joined voter.
Nothing in the tree calls `preflight_voter` before `start_node` — the only
caller is `run_incompatible_preflight`, which spawns a short-lived
`Client::remote` process with no node id, never calls `start_node`, and exits
42.

Proven, with a temporary `election` probe mode (three real voters; node 3 runs
`open()`'s check with `schema_version - 1`, then reverted):

```
node3 store open  -> Error { "cluster schema 1 is incompatible with voter schema 0" }
metrics(from 1)   -> Metrics { leader: Some(1), voters: [1, 2, 3] }
kill 1            -> NEW LEADER = 3
```

**The voter whose store was refused for schema incompatibility stayed in
`membership_config.voter_ids()` and was elected leader.**

The gate's own guard against this is vacuous. `main.rs:121-124`:

```rust
let current_leader = cluster.leader().await?;
if current_leader == target_id { bail!("refused voter {target_id} became leader"); }
```

`target_id` is the **follower killed at `main.rs:96`**, twenty-five lines
earlier; `leader()` skips `None` slots and a dead process cannot campaign, so
this assertion can never fire. It also conflates the killed follower with the
preflight process, which was never a voter at all. Corroborating: mutating
`verify_compatibility_rows` to `Ok(())` fails at the **exit-code** check
(`main.rs:719`), never here.

Ask: either (1) make the preflight actually gate `start_node` in the harness's
`node()` — refuse to start the raft runtime when the remote preflight fails —
and then re-assert that the refused node never appears in `voter_ids()`; or
(2) delete the vacuous assertion and rewrite both doc rows to claim only what
is true: *a client-side preflight function returns `Err` on schema drift; raft
membership is not yet gated on it*. Option 1 is the contract §6.3 promises.

### C. Vendoring silently exempts two crates from the RustSec gate forever

The advisory work itself is real and I verified it. `rkyv` is **entirely
absent** from `Cargo.lock` (`cargo tree -i rkyv` → no such package); `quick-xml`
resolves to **0.41.0 only**; `cargo audit` on the PR head is clean (417 deps, 1
allowed `bincode` unmaintained warning) while the same advisory DB on
`spikes/hiqlite-m0/Cargo.lock` reports all three vulnerabilities. Both vendored
trees are byte-faithful to crates.io — I checksummed the downloads against the
`checksum =` lines the repo's own spike lockfile still records
(`be2a24f5…` rust_decimal 1.42.1, `0fa335ba…` s3-simple 0.8.0) and diffed
recursively. The documented patches are exactly what changed, modulo
whitespace.

The problem is what is **not** documented. `rustsec-0.33.0/src/database.rs:148-153`:

```rust
if package.source.as_ref().is_none_or(|source| !source.is_default_registry()) {
    continue;
}
```

Path-sourced lockfile entries are skipped before any advisory lookup. Control
experiment: stripping only the `source`/`checksum` lines from the `quick-xml
0.39.4` entry in a copy of the spike lockfile made **both** quick-xml
advisories vanish while rkyv's remained.

So `s3-simple 0.8.0` and `rust_decimal 1.42.1` are now permanently outside
`rust-audit.yml` for *future* advisories. Nothing is suppressed today (RustSec
has no advisories for either crate), but the exemption is silent and
open-ended. **"No RustSec ignore was added"** (`PHASE3-SPIKE.md:85`, `:101`) is
literally true and materially misleading: this is the same class of risk an
explicit `[[advisories.ignore]]` carries, minus the review trail.

Ask: state the exemption plainly in both `PLURX-PATCH.md` files and in
PHASE3-SPIKE, and add a mechanical re-check — e.g. a CI step that runs
`cargo audit` against a synthesized lockfile with the vendored entries restored
to their registry source, so a future advisory against either crate still
fires. Also record the removal trigger as an actual task, not prose.

---

## Should-fix

### D. Every read and write except `ping` can hang forever

`timeout_consistent` (3 s, `hiqlite.rs:634-641`) wraps exactly two call sites:
`preflight_voter` (`:136`) and `ping` (`:258`). Unwrapped: `get_setting`
(`:275`), `count_users` (`:308`), `list_users` (`:358`), `count_admins`
(`:380`), `user_optional` (`:234` — backs `get_user`, `get_user_by_username`,
`user_for_token`), `key_optional` (`:247`), `list_api_keys` (`:494`), every
`execute()` (`:221`), both `execute_returning_map_one` (`:328`, `:485`), and
`client.batch` (`:97`).

hiqlite 0.14 supplies no timeout of its own — `grep -c timeout` over
`client/query.rs` and `client/execute.rs` is **0** for both. The awaits are
bare oneshots (`client/query.rs:332`, `client/execute.rs:71`), and on the
leader path `execute` resolves only when openraft commits. The one bound in
the crate, `cleanup_buffer_timeout` (`client/stream.rs:211`), is armed *after*
a successful WebSocket connect — during an outage the stream manager sits in
`stream.rs:184-193` (`sleep(1000ms); continue;`) forever and in-flight
requests stay parked in `in_flight_buf`. Worse, `tx_client_db` is
`flume::bounded(1)` (`client/create.rs:158`), so after one queued request the
`send_async` blocks too. Note also that `query_consistent` never short-circuits
for a local leader (`query.rs:33`, `:52`) — it round-trips its own WebSocket
and then needs `ensure_linearizable()`.

There is no `TimeoutLayer` on plurxd's router. On activation, quorum loss
means `/readyz` correctly reports unhealthy at 3 s while **every login and
every authenticated request hangs with no response and no error**, holding its
connection. This is the largest activation risk in the file, and it is cheap
to fix now: route every store call through `timeout_consistent` (or a
per-operation budget), not just the health probe.

### E. Nine of seventeen mutations leave `make cluster-check` green

I mutated `hiqlite.rs` one change at a time, reverting between each, and ran
the full gate after every mutation (~31 runs). Detected = the gate failed.

| Mutation | Gate | Message when caught |
|---|---|---|
| `put_setting` no-op | detected | `instance.id missing — migration invariant broken` |
| `get_setting` → `Ok(None)` | detected | same |
| `delete_tokens_for_user` deletes one row | detected | `bulk token revocation did not include every token` |
| `ping` drops the consistent read | detected | `one voter reported ready without quorum: Ok` |
| `ping` → `Ok(())` | detected | same |
| `verify_compatibility_rows` → `Ok(())` | detected | `incompatible voter exited Some(0)` |
| `touch_api_key` no-op | detected | `API key touch/disable contract failed` |
| `set_api_key_disabled` no-op → `true` | detected | same |
| **`create_api_key` persists `"[]"` not caller scopes** | **PASS** | — |
| **`local_dump_digest` returns a constant** | **PASS** | — |
| **`instance_id()` returns a constant** | **PASS** | — |
| **`set_password` binds a constant hash** | **PASS** | — |
| **`now()` returns a constant** | **PASS** | — |
| **`create_user` always binds `is_admin = true`** | **PASS** | — |
| **`create_token` drops `device`, binds NULL** | **PASS** | — |
| **`user_for_token` never updates `last_seen_at`** | **PASS** | — |
| **`execute()` drops `validate_sql`** | **PASS** | — |

The four that matter:

- **Replica equality is self-certifying.** `wait_for_equal_dumps`
  (`main.rs:374`) compares digests each node computes with the *same*
  `local_dump_digest`; a constant return makes all three trivially equal.
  Nothing anchors the digest to expected content, so the contract cannot
  distinguish "replicas agree" from "the digest function is broken." Fix: have
  the controller assert one known-good digest, or dump one plaintext row set
  from one node and compare it to an expected literal.
- **API-key scopes are never asserted** — the same mutation that defeated the
  M1a suite defeats this one. `main.rs:604-616` only checks
  `!row.allows("scan:trigger")` *while the key is disabled*, which is
  vacuously true for `[]`. Assert `allows("scan:trigger")` on an **enabled**
  key.
- **Two assertions are tautologies.** `set_password`: the harness writes
  `"hash-v2"` and asserts the value is `"hash-v2"`. `instance_id`: the harness
  compares against the same `INSTANCE_ID` constant the store would return.
  Both pass against a hard-coded bind that never touches the table.
- **Determinism is not tested.** `PHASE3-SPIKE.md` claims "every timestamp is
  computed once by the caller and bound"; a constant clock is undetectable,
  because agreement follows from *binding a value once*, not from the value
  being right. The gate proves the binding, which is the important half —
  say that, rather than claiming replay determinism.

Also worth stating in the doc: `make cluster-check` is the **only** runtime
exercise of `hiqlite.rs` anywhere. Its two unit tests are pure functions
(`validate_sql` on literals, `verify_compatibility_rows` on hand-built rows) —
`cargo test -p plurx-core --lib store::hiqlite -- --list` = 2, and neither
constructs a `Client`. **Zero of the 23 methods are covered by `cargo test`.**
`store_contract.rs:174-181` iterates `sqlite_fixtures()` only and structurally
cannot take `HiqliteAuthStore`. And `points.toml:168` scopes `cluster-auth` to
`profiles = ["ci","full","nightly"]` — it is not in the `commit` profile, so
`make check` does not run it.

### F. `bootstrap` overwrites `instance.id`, the exact key B1 was about

`hiqlite.rs:95-118` protects `cluster_meta` with `ON CONFLICT(singleton) DO
NOTHING` (`:109`) and then, eight lines later, writes the logical identity
through `put_setting` (`:116`), which is an upsert (`:287-289`,
`DO UPDATE SET value = excluded.value`). Two writes in one function, opposite
conflict policies. A second `bootstrap` with a different id silently replaces
the cluster's identity, last-writer-wins.

`SqliteStore` cannot do this: `sqlite/mod.rs:800-814` selects first and inserts
only `if existing.is_none()`. And the key matters — `cluster.rs:56-57` feeds
`instance_id()` into `initialize_identity`, whose own comment (`:82-85`) says
*"all cache locations and offline packages written before M0 use that exact
string as their owner"*; that `node_id` filters `all_cache_rows`
(`store/mod.rs:785`), which is the keep-list for `sweep_orphan_dirs`
(`cachekeep.rs:497`). The drift guard at `cluster.rs:58-66` exists only on
`SqliteStore` (`sqlite/mod.rs:685-698`), in the SQLite-only `open_store` path.

**Latent, not live** — the only caller is `main.rs:439` with the constant
`"m1b-cluster-check"`, so the harness can never observe it, and `exercise`
asserts the id against that same constant. It becomes live the moment a real
bootstrap coordinator generates the id. Make it `DO NOTHING` + read-back-and-
compare, and fail closed on mismatch.

### G. `validate_parameter_order` has three parser defects and skips half the SQL

I ported `hiqlite.rs:571-628` verbatim and enumerated 35 statements, then
checked SQLite's real assignment with `Statement::parameter_name`. The premise
is sound — SQLite genuinely assigns `$N` by first appearance
(`SELECT $2 AS a, $1 AS b` → `["$2","$1"]`) and hiqlite genuinely binds
positionally (`writer.rs:203-209`), with its own `check_stmt_params_count` net
being `#[cfg(debug_assertions)]` and only an `error!`. The ordering logic
itself is right: `$1 $3` rejects, `$1 $1` accepts (needed for
`VALUES ($1,$2,$3,$4,$4)` at `:428`), `$2 $1` rejects, `$0` rejects.

The defects:

1. **Bracketed identifiers are not skipped.** `replicated.rs:259` handles
   `b'['`; this function has no such arm. False rejects
   (`SELECT [$2] FROM t WHERE id = $1`), and worse, **false accepts**: an
   apostrophe inside brackets opens a string scan that never closes, so
   `SELECT [a'b], $2, $1 FROM t` — genuinely misordered — is waved through.
   Two validators in the same module disagreeing on bracket handling is itself
   the smell.
2. **`$01` is read as ordinal 1**, but SQLite prepares `SELECT $1, $01, $2`
   with **three** parameters (`["$1","$01","$2"]`). The `parse::<u32>()` at
   `:613` erases leading zeros; `params!(a, b)` would leave `$2` NULL.
3. **Only `$N` is scanned at all** — `?N`, bare `?`, `:name`, `@name` are
   invisible. `SELECT ?2, ?1` accepts, and positional binding swaps them.

None of these is reachable from today's SQL, which is why this is should-fix
and not a blocker. But note it is a **self-check, not a guard**: `execute`
takes `sql: &'static str` and every one of the 11 call sites passes a literal
written three lines above its `params!()`. It also **skips**
`preflight_voter`'s query (`:137`) and all five `local_dump_digest` queries
(`:160-200`). If this is meant to guard, move it into a `#[test]` that
enumerates every SQL constant in the module — cheaper at runtime and strictly
more coverage.

### H. The gate is flaky at roughly 3%

One of ~31 runs failed spuriously inside the metrics/leader polling path
(`main.rs:341`); the identical tree then passed 2/2. The fixed deadlines —
`wait_for_three_voters` 20 s, `leader()` 15 s, `wait_for_equal_dumps` 20 s —
are tight for a loaded shared runner, and `free_port()` (`main.rs:770`) binds
and drops, which is a TOCTOU race on a busy host. `cluster_auth` is now in
`pr_gate.needs`, so a spurious failure blocks a merge. Raise the deadlines
(they only cost time on failure) and retry port allocation on bind conflict.

### I. Doc rows that overclaim

Beyond §B's compatibility row: the "Replica equality" and "Deterministic
replay" rows should be narrowed per §E · PHASE3-SPIKE:84 cites only
RUSTSEC-2026-0195 for the quick-xml bump, but 0.39.4 is also hit by
**RUSTSEC-2026-0194** (same fix, ≥0.41.0 — the doc just undercounts, and
`PGS-OVERLAY-M0-FEASIBILITY.md:148` already names both) · both PLURX-PATCH.md
files say hiqlite is why `rust_decimal` resolves, but **`byte-unit 5.2.5` (via
`openraft`) also depends on it**, so removing hiqlite alone would not remove
the vendor.

---

## Nits

- `vendor/rust_decimal`'s rkyv removal is incomplete in the inert half:
  `Cargo.toml:135-136` still declares `[[example]] name = "rkyv-remote"`,
  `README.md:119,226-235` still documents the deleted features,
  `make/tests/misc.toml:31-33` still runs `--features=rkyv --features=rkyv-safe`
  (now unrunnable), and `tests/decimal_tests.rs:239-265` is gated on a feature
  that no longer exists. `src/error.rs` also has a whitespace-only edit, so
  "removes only the rkyv integration" is imprecise.
- The vendored crates are not workspace members and the root manifest has no
  `workspace.exclude`, so `cargo metadata` **errors** inside either directory.
  Their 5,268 lines of tests, 453 of benches and 53 of examples are unrunnable
  in place. Either exclude them and strip the dead weight, or say in
  PLURX-PATCH.md that only `src/` and the manifest are load-bearing.
- `list_api_keys` is `ORDER BY created_at, id` (`hiqlite.rs:496`) vs
  `ORDER BY created_at` (`sqlite/apikeys.rs:56`). hiqlite's is the more correct
  one — SQLite's tie-break is unspecified — but they are not the same
  function, so fix the SQLite side rather than leaving a silent divergence.
- The cluster-check cascade assertion (`main.rs:579-582`) proves cascade via
  `user_for_token`, which is `FROM users u JOIN tokens t` — the JOIN returns
  zero rows whether or not the orphan token survives. It happens to check a
  true fact (hiqlite 0.14 sets `foreign_keys = ON` at
  `state_machine.rs:381`, on both the writer and every read-pool connection,
  matching `sqlite/mod.rs:704`) for the wrong reason. Count `tokens` rows
  instead.
- `AUTH_SCHEMA:67` adds `CREATE INDEX api_keys_hash ON api_keys(key_hash)` on
  a column already declared `UNIQUE` — a redundant second index. Present in
  the SQLite DDL too, so this is inherited, not new.
- `leader()` trusts whichever live node answers first; a stale follower can
  name an old leader, in which case the "leader loss" case actually kills a
  follower and still passes. Confirm from the target's own metrics before
  killing it.

## Two things I checked that turned out fine — recorded so they are not re-raised

- **Foreign-key cascade parity.** I expected hiqlite to leave FK enforcement
  off and orphan `tokens` rows on `delete_user`. It does not:
  `hiqlite-0.14.0/.../state_machine.rs:381` runs
  `conn.pragma_update(None, "foreign_keys", "ON")` inside `apply_pragmas`,
  called from `connect()` (`:307`) for the writer and every read-pool
  connection (`:326-345`). Both backends declare the same
  `ON DELETE CASCADE` and both rely on it rather than deleting tokens
  explicitly (`sqlite/users.rs:81-86`, `hiqlite.rs:370-375`). Same semantics.
- **"Corrupt scope data must fail closed"** (`hiqlite.rs:803-804`,
  `unwrap_or_default()`). The comment is accurate. `sqlite/apikeys.rs:22` does
  the identical thing, and `domain.rs:436` is
  `!self.disabled && self.scopes.iter().any(|s| s == scope)` — `any()` on an
  empty vector is `false`, with no "unscoped means unrestricted" branch
  anywhere. The single production caller is `ScopedKey::require`
  (`plurxd/src/http/extract.rs:74`), returning `Forbidden` on `false`, and
  `http/keys.rs:73` already rejects creating a key with no scopes.

## Merge condition

Fix **A** (four lines, verified), **B** (gate the voter start or narrow the
claim), and **C** (document the exemption + a mechanical re-check). D and E
are cheap now and get much more expensive once plurxd holds this store — I
would take them in the same revision, but they do not have to block the merge
of a backend nothing calls yet.
