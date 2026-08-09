# PR #82 review — the right milestone, with a one-line bug that defeats the contract it exists to keep

**Status:** review complete · **Reviews:** [PR #82][pr82] "Add clustering M0
identity and hiqlite proof", head `41aca7eb` on `codex/clustering-m0` (draft) ·
**Verified against:** `origin/main` @ `bb3f6a1c`, which is also the merge-base ·
**Written:** 2026-08-06 · **Outcome:** **changes requested** · **Follows:**
[CLUSTERING-PLAN-REVIEW.md](CLUSTERING-PLAN-REVIEW.md) (the eight contracts)
and the review of [PR #79][pr79] (the plan revision that authorized M0)

[pr82]: https://github.com/pjunod/plurx/pull/82
[pr79]: https://github.com/pjunod/plurx/pull/79

Companion to [CLUSTERING-PLAN.md](CLUSTERING-PLAN.md) §3.1–§3.2 and §6.1 —
this is *whether M0 as built keeps the contracts the plan promised*. Findings
cite `file:line` at `41aca7eb`.

**How it was reviewed.** The PR head and `origin/main` were checked out as
pinned detached worktrees from a fresh clone; nothing was read from a working
tree. Four scoped passes ran independently — identity and config contracts,
the hiqlite proofs, dependency and build risk, and the cost gate plus process
gates. Every CONFIRMED finding was reproduced by running code.

**Build state at head:** `cargo test -p plurx-core -p plurxd` is green —
328 + 323 passed, 1 pre-existing ignored storage test. Full workspace:
678 passed. `cargo clippy --workspace --all-targets -- -D warnings` clean in
both the default and `hiqlite-spike` configurations.
`make validation-lint` passes: `catalog ok: 17 points · 20 checks · 325
audited files`.

**Severity legend:** **BLOCKER** = defeats a contract M0 exists to keep, or
turns a working install into a broken one · **SHOULD-FIX** = real cost, a
false claim, or a gate that does not gate · **NIT** = worth doing, not worth
blocking.

---

## 1. Verdict

The milestone is the right one and most of it is built well. The atomic
identity publish is correct line by line — `create_new` with mode 0600 *at
creation*, `write_all` → `sync_all` → `link(2)` (never `rename`, so no
clobber) → directory-handle fsync, with the read-NotFound → link race handled
so the loser adopts the winner's id. Fail-closed everywhere; identity is
established before the store handle exists, therefore before any sweep can
run. B8 is satisfied and verified by matrix: `[cluster] some_future_key = 1`
loads, `[server] nmae` still fails. The dependency work — the riskiest-looking
part on paper — came out clean: exactly one `libsqlite3-sys`, zero hiqlite
units in a default build, and the SQLite 3.50.2 → 3.53.2 bump verified inert
against this repo's own planner-sensitive queries.

What blocks it is narrower and sharper than any of that:

**`validate_uuid` parses the UUID and throws it away** (`cluster.rs:184`), so
a non-canonical `node.id` — uppercase, braced, unhyphenated, `urn:uuid:` — is
adopted **verbatim** as the SQL ownership key. `TEXT =` is byte-sensitive.
The result is B1, the exact hazard M0 exists to prevent, arriving one cache
write later and with no log line. Reproduced end to end, including the byte
deletion.

Two more are worth blocking on. The `mode != 0o600` **equality** check turns a
working data directory into an unbootable one under a stricter umask
(reproduced) and on any filesystem that reports its mount-option mode — CIFS
with `file_mode=0644` is a normal way to put a media server's data dir on a
NAS. And the **statement-replication proof does not exist**: the assertion is
`assert_eq!(entropies, vec![4_242, 4_242, 4_242])` over a bound literal, which
is identical under statement-based or row-based replication. That is the one
semantic M0 was chartered to prove, and probing it directly surfaced a finding
worth more than the six claims combined — see §3.

Everything else is fixable in the same sitting.

---

## 2. Identity and config

### A · BLOCKER — a non-canonical `node.id` is adopted verbatim, stranding then deleting pre-M0 bytes

`crates/plurx-core/src/cluster.rs:184`:

```rust
fn validate_uuid(label: &str, value: &str) -> Result<(), StoreError> {
    let id = uuid::Uuid::parse_str(value)
        .map_err(|error| StoreError::Identity(format!("{label} is not a UUID: {error}")))?;
    if id.is_nil() { ... }
    Ok(())                       // <-- `id` is parsed and thrown away
}
```

`initialize_identity` (`:84-99`) then uses the **raw file bytes** as
`node_id`. `Uuid::parse_str` accepts five spellings of the same UUID and every
one becomes the ownership key for `transcode_cache_locations` and
`offline_packages`.

**CONFIRMED:**

```
PROBE uppercase: node_id="B09814BB-5A72-4CFA-8DB7-0CE3F198AE33"
                 cluster_id="b09814bb-..."  equal=false
PROBE accepted: "9a83b0fa0c85419a941ce8d3106c4468"
PROBE accepted: "urn:uuid:f58ea941-6439-415e-95db-888b6644b7ff"
PROBE accepted: "{90d17397-3696-45e9-b7f9-82ee6f0a44dd}"
```

**Failure scenario.** §3.1 ships a "regenerate `node.id`" operator command, so
hand-written identity files are an *expected* input. An operator pastes the id
out of `sqlite3 plurx.db "select value from settings where key='instance.id'"`
in uppercase, or copies it from a tool that braces UUIDs. Startup succeeds —
`validate_uuid` says it's a UUID. Then:

```
PROBE gen1 (fresh node.id):        swept=(0,0,0) bytes_exist=true
                                   cache_hit_old=true cache_hit_new=false
PROBE gen2 (new node has one row): swept=(0,0,1) OLD_BYTES_EXIST=false
```

Generation 1 looks healthy only because `sweep_orphan_dirs`
(`cachekeep.rs:512-517`) fails closed on an empty inventory — the guard added
in #79. The moment the server transcodes **one** thing under the new key the
inventory is non-empty and the next sweep deletes every pre-M0 cache directory,
ready offline packages included. Silent, and delayed just long enough that the
operator will not connect it to the identity edit.

**Fix.** Return `Ok(id)` from `validate_uuid` and store `id.to_string()`
(canonical lowercase hyphenated) — or reject anything not already canonical.
One line either way.

### B · BLOCKER — the mode check is an equality, so a stricter umask or a mode-reporting filesystem bricks startup

`crates/plurx-core/src/cluster.rs:143`:

```rust
if mode != 0o600 {
    return Err(StoreError::Identity(format!(
        "{} was created with mode {mode:o}, expected 600", temporary.display())));
}
```

`OpenOptions::mode()` is the *requested* mode; the kernel and the filesystem
decide the result.

**CONFIRMED:**

```
PROBE umask 0277 -> Err("cluster identity error: /tmp/.../.node.id.<uuid>.tmp
                        was created with mode 400, expected 600")
PROBE umask leftovers: []
```

Nothing is ever written, so this is not transient — every subsequent start
takes the same branch and fails identically. No override, no self-heal, and
the error names a temp file the operator has never seen.

**SUSPECTED, same code path:** the equality also fires on any filesystem that
does not honor Unix modes and reports its mount-option mode instead — vfat or
exFAT on an external drive, NTFS-3g with `fmask=`, CIFS/SMB mounted with
`file_mode=0644`, several FUSE and Docker Desktop bind mounts. Those same
filesystems fail `link(2)` with `EPERM`/`EOPNOTSUPP` and hit
`identity_io("publishing", …)` for the same result. On `main`
(`plurxd/src/main.rs:232`) those deployments start fine today. I could not
reproduce this half — no `mkfs.vfat` in the sandbox — but it is the identical
branch the umask repro exercises.

**Fix.** Accept `mode & 0o077 == 0` (nothing beyond owner) and `warn!` on a
wider mode rather than aborting; fall back to `rename`-into-place after a
`create_new` probe when `hard_link` reports `EPERM`/`ENOSYS`.

### C · SHOULD-FIX — validating `instance.id` as a UUID is a new, unrecoverable startup failure with no M0 benefit

`cluster.rs:75` — `validate_uuid("instance.id", cluster_id)?;`

```
PROBE non-uuid instance.id -> cluster identity error:
  instance.id is not a UUID: invalid character: found `i` at 1
```

...and no `node.id` is written, so it recurs on every restart. Nothing in the
schema constrains `instance.id`'s shape — it is a plain `settings` row, and the
repo's own migration fixture stores `'fixture-instance'`
(`store/sqlite/mod.rs:986`, asserted to survive at `:1064`). B1 does not need
this check: the seed only has to be byte-identical to whatever the rows are
keyed by.

**Failure scenario.** Any install whose `instance.id` came from a dev build, a
restored settings row, or a fixture-derived database upgrades to M0 and gets
`opening store in ./data: cluster identity error: instance.id is not a UUID`
on every boot. Recovery needs `sqlite3` surgery on the production DB — and the
obvious surgery, minting a fresh UUID, is precisely the B1 data loss the
milestone exists to prevent. Downgrade to a warning, or seed unconditionally
and validate only `node.id`.

### D · SHOULD-FIX — the M0 acceptance test does not exercise any M0 code

`crates/plurxd/src/cachekeep.rs:1275`,
`node_identity_initialization_preserves_populated_v14_ownership_and_bytes`.

Three independent ways of showing it:

**(a) It passes against base semantics.** Replacing `open_store`'s body with
the pre-PR behavior — `node_id = instance.id`, `initialize_identity` never
called, no `node.id` ever written — and running the test verbatim:

```
test cachekeep::tests::node_identity_initialization_preserves_populated_v14_ownership_and_bytes ... ok
test result: ok. 1 passed; 0 failed
```

It pins the invariant, not the implementation. None of the 296 new lines of
atomic-publish machinery is covered: it never asserts `node.id` exists, nor
its content, mode, trailing newline, or restart behavior.

**(b) Under the real regression it dies before the fixture exists.** Mutating
`create_node_id_noclobber`'s seed to `Uuid::new_v4()`:

```
panicked at crates/plurxd/src/cachekeep.rs:1285:13:
assertion `left == right` failed: the first node must preserve the pre-M0 ownership key
```

Line 1285 is the *first* assertion, inside the first `open_store` — no
library, no cache row, no offline package, no sweep. The 130 lines of
"populated v14 fixture with cleanup enabled" contribute nothing to detecting
the regression they are named for.

**(c) The byte assertions are provably vacuous.**
`assert_eq!((swept.stale, swept.evicted, swept.orphans), (0,0,0))` and
`assert!(entry_dir.join("index.m3u8").exists(), ...)` both hold *under* the
regression — the gen1 probe in finding **A** returned exactly
`swept=(0,0,0) bytes_exist=true` with a deliberately wrong node id. The doc
comment's claim that "the cache directory becomes an orphan" is false at
generation 1 and only becomes true at generation 2, which the test never
reaches.

To pin §6.1's acceptance the test must (i) assert `node.id` exists with mode
0600 and content equal to `instance.id`, and (ii) seed at least one cache row
for the *current* node before sweeping, so the inventory is non-empty and the
orphan pass is actually armed. Related: it is not a v14 *fixture* — it builds
a current-schema DB through the public API with no migration involved.

### E · SHOULD-FIX — shipping an active `[cluster]` stanza breaks the first rollback across the M0 boundary

`plurx.example.toml:15`:

```toml
[cluster]
raft_bind = "0.0.0.0:32401"
```

Base `Config` (`/tmp/base82/crates/plurx-core/src/config.rs:19-23`) is
`#[serde(default, deny_unknown_fields)]` over `{server, storage}`. The error
shape is confirmed:

```
PROBE unknown top-level section => ERR unknown field `secrets`,
      expected one of `server`, `storage`, `cluster`
```

**Failure scenario.** Operator upgrades to M0, adopts the shipped example
config, hits an unrelated M0 problem, rolls the image back one tag — and the
pre-M0 binary refuses to start with `unknown field 'cluster'`. The rollback
that was supposed to be the escape hatch becomes the outage. B8's *forward*
half is satisfied; this is the backward half, and the section is dormant in M0
anyway. Comment the stanza out: it documents the keys and keeps both
directions working.

### F · SHOULD-FIX — `node.id != instance.id` is accepted silently, with no reconciliation and no log line

`cluster.rs:84-99`. §3.1 says: *"It never silently changes ids on a directory
containing cache or offline rows."* There is no such check, and `cluster.rs`
contains no `tracing` call at all — unlike `instance.id` minting, which logs
`"generated new instance id"` (`store/sqlite/mod.rs:795`).

**Failure scenario.** A later release does what §3.1 describes and mints a
distinct `node.id` for a joining node. The operator rolls that node back to
M0. M0 adopts the joiner's `node.id`, every row keyed by `instance.id` is
stranded, generation 1 looks healthy, generation 2 deletes the bytes — the
same reproduced sequence as finding **A**. Same shape for a `plurx.db`
restored into a data dir that already has a `node.id`. Minimum viable guard:
when `node.id != instance.id`, count rows keyed by `instance.id` and `warn!`
(or refuse) if any exist.

---

## 3. The hiqlite proofs

Credit first: the harness is real. Three genuine raft voters, separate data
dirs, TLS loopback transport, and per-replica *local* reads
(`Client::query_as_one` is local-only). This is not a fake cluster, and it
would not pass against a stub. The problem is the assertions.

| # | Claim | What the test does | Verdict |
|---|---|---|---|
| 1 | FTS5 + triggers | `:82-94` loops all 3 clients, local-reads `items_fts`. Toy schema: one `AFTER INSERT` trigger, one FTS column, no `_ad`/`_au` triggers, no `'delete'`/`'rebuild'` form. Never compares the value — `.expect(...)` only asserts a match existed | **WEAKER** |
| 2 | `RETURNING` | `:70-78` `execute_returning_one(… RETURNING id)` issued **through follower node 2**, `assert_eq!(item_id, 1)`. Both production `last_insert_rowid()` sites are this exact shape | **PROVES IT** |
| 3 | Transaction CAS | `:97-127` — two `txn()` calls **sequential, one task**. The "loser" is a stale-version no-op run *after* the winner committed. Zero contention | **WEAKER** |
| 4 | Statement replication | `:129-134` — a comment plus `assert_eq!(entropies, vec![4_242, 4_242, 4_242])`, a **bound literal** | **PROVES NOTHING** |
| 5 | TLS / API auth | `:136-165` — a real negative case exists (`intruder_rows == 0`), which is right. But `Client::remote(addrs, true, true, …)` sets `tls_no_verify`, and the assertion passes on a *timeout* as well as a rejection | **WEAKER** (auth mostly; TLS not) |
| 6 | Log compaction | `:169-183` — 96 writes, then `snapshot.is_some() && purged.is_some()`. Real as far as it goes, but **no node is killed, restarted, or made to catch up** | **WEAKER** |

### G · BLOCKER — statement replication is asserted by comment, and the comment is wrong

`crates/plurx-core/tests/hiqlite_m0.rs:129-134` claims hiqlite's write
connection "actively replaces unixepoch(), random(), date/time, and related
functions with rejecting guards."

Running the proof the PR omitted: hiqlite 0.14 guards exactly ten names
(`state_machine.rs:401-510`) — `date, datetime, julianday, now, random,
randomblob, strftime, time, timediff, unixepoch` — installed **only on the
write connection**.

```
PROBE unixepoch:          Ok(Err(Sqlite("SQL logic error ... \"unwinding panic\"")))
PROBE random:             Ok(Err(Sqlite("SQL logic error ... \"unwinding panic\"")))
PROBE CURRENT_TIMESTAMP:  Ok(Ok(1))          <-- ACCEPTED
PROBE surviving rows:     Ok(Ok([3]))        <-- only the CURRENT_TIMESTAMP row survives
```

`CURRENT_TIMESTAMP` / `CURRENT_DATE` / `CURRENT_TIME` are SQL **keywords**,
not functions — `create_scalar_function` cannot override them. They pass
straight through and are evaluated independently on each replica. And the
divergence is not a tight race: a node that is down and replays the log later
evaluates the clock **at replay time**, so snapshot catch-up produces
guaranteed disagreement.

Also confirmed that every replica genuinely re-executes:

```
PROBE3 unixepoch on 3 voters:
forbidden usage of `unixepoch()` ...   (x3 — one panic per voter)
```

### H · BLOCKER — 24 schema columns use `DEFAULT (unixepoch())`; the DDL is accepted and every subsequent insert fails

```
PROBE5 DDL with DEFAULT (unixepoch()):  Ok(true)   <-- migration appears to succeed
PROBE5 INSERT relying on the default:   Ok(Err(Sqlite("... unwinding panic")))
```

This is the worst failure shape in the audit: a migration that reports success
and then breaks every write to that table, with no mention of `unixepoch` at
the call site. M0 should have found this and did not.

Related: the rejection mechanism is a `panic!` inside a `create_scalar_function`
callback (`state_machine.rs:506`), unwound through the `libsqlite3-sys` C
stack. It is a **post-commit, apply-time** failure — the raft entry is already
replicated before the guard fires. M1 needs to know whether a guard panic can
poison the writer thread; the PR's own test already leaves one panicking
writer thread behind on every run (see **J**).

### The number that sizes the M1 port

`crates/plurx-core/src/store/sqlite/`, 10 files:

| Construct | Sites | hiqlite behavior | M1 consequence |
|---|---:|---|---|
| `unixepoch()` | **70 occurrences / 68 lines** | rejected (apply-time panic) | mandatory rewrite of every site |
| — schema `DEFAULT (unixepoch())` | **24** (all `mod.rs`) | DDL accepted, inserts then fail | migration "succeeds", table becomes write-dead |
| — inline in DML/queries | **44** | rejected | caller binds a leader-computed timestamp |
| `last_insert_rowid()` | 2 | n/a | covered by the proven `RETURNING` path |
| `random()` | 0 | rejected | none today |
| `CURRENT_TIMESTAMP`/`_DATE`/`_TIME` | 0 | **accepted, diverges silently** | **unguarded — ban with a CI grep before M1** |

By file: `mod.rs` 26, `offline.rs` 15, `watch.rs` 10, `media.rs` 8,
`cache.rs` 4, `library.rs` 2, `users.rs` 2, `apikeys.rs` 1.

Read that as ~68 mandatory rewrites, loudly enforced — no silent corruption
today — plus one silent-divergence hole that is currently unused. That is a
bounded, plannable number, and it is exactly what M0 was supposed to produce.
Every figure came from probes, not from `hiqlite_m0.rs`.

### I · BLOCKER — CI never compiles, let alone runs, any of this

- `hiqlite_m0.rs:7` — `#![cfg(feature = "hiqlite-spike")]`
- `validation/points.toml:62-67` — `rust-gate` → `make rust-check` →
  `fmt-check lint test` → `cargo clippy --workspace --all-targets` +
  `cargo test --workspace`. Neither passes `--features hiqlite-spike`.
  `grep -rn hiqlite .github/workflows/ scripts/` → **no matches**. No
  `--all-features` anywhere.
- Verified: `cargo test -p plurx-core --test hiqlite_m0 -- --list` →
  `0 tests, 0 benchmarks`.

Because the whole file is `#![cfg]`-gated, **clippy does not lint it either**.
It is invisible to every gate and will bit-rot silently — on the next hiqlite
or `libsqlite3-sys` bump, which is precisely the coupling this PR just
discovered. Worse, `validation/points.toml:231` registers
`crates/plurx-core/tests/hiqlite_m0.rs` under `checks = ["rust-gate"]`, so the
catalog **claims** coverage that does not exist. `make check` does not run
these tests. They are documentation, not proof.

Compounding it: the only performance evidence — the thing the design review
demanded after `PHASE3-SPIKE.md` produced no numbers — is `#[ignore]`d
(`:201`) and runs only via `make hiqlite-baseline` on a developer's machine.
And `Makefile:66-68` filters on the literal test name, and `cargo test` exits
0 when a filter matches nothing:

```
$ cargo test --locked -p plurx-core --lib no_such_test_name_xyz
running 0 tests
test result: ok. 0 passed; ... 328 filtered out
EXIT=0
```

Rename the benchmark during the Phase 4 slices and the one manual cost gate
becomes a no-op that looks like a pass. Add `--exact` and assert the test ran.

### J · SHOULD-FIX — CAS is uncontended, and the harness cannot prove restart at all

The plan needs "two survivors reading epoch 5 cannot both write epoch 6".
`:97-127` runs both txns from one task, in order. The contended version:

```
PROBE cas writer 0: Ok(1)
PROBE cas writer 1: Ok(0)
PROBE contended CAS winners = 1
PROBE final gen = Ok(6)
```

Good news — raft serialization does give exactly one winner. But the loser
gets **`Ok(0)`, not an error**, so M1's fence-token API must check
rows-affected: a silently-ignored CAS is indistinguishable from success at the
`Result` level. That is exactly the design insight M0 existed to surface, and
the PR's test cannot surface it. ~20 lines to fix.

Separately, every run leaves a panicking background thread:

```
thread '<unnamed>' panicked at hiqlite-0.14.0/.../writer.rs:225:40:
oneshot tx to never be dropped: Ok(1)
```

from the deliberate `drop(clients)` at `:185-191`, whose comment admits
`Client::shutdown` "would … hang the test process" under TLS. A panic on a
non-test thread does not fail the test, so it is invisible in the pass/fail
signal.

And the missing restart-from-compacted-log proof **is not achievable in this
harness**:

```
thread 'probe_restart_from_compacted_log' panicked at .../client/mgmt.rs:376:18:
The global Hiqlite shutdown handler to always listen: SendError { .. }
```

hiqlite 0.14 has a *process-global* shutdown handler, so three voters in one
process cannot be individually stopped and restarted. **M0 therefore cannot
prove crash recovery, snapshot catch-up, or leader failover at all** with this
design — that needs a multi-process harness. Say so plainly in the PR rather
than leaving it implied by a passing test named
`three_voters_prove_the_sql_and_transport_contracts`.

### K · SHOULD-FIX — the largest M1 risk is not on the list of six

hiqlite's `txn()` takes a fixed `[(sql, params)]` list and returns
`Vec<Result<usize>>` — no reads, no branching, no `RETURNING`. Production has
**11 `unchecked_transaction()` sites** that read-then-branch-then-write inside
the transaction (e.g. `store/sqlite/offline.rs:92-112`: `SELECT … .optional()?`
→ `if let Some(existing)` → conditional commit/return). None can be ported
mechanically. The M0 test works around this with a `WHERE changes() = 1` trick
(`:104`) but never names the limitation. This is the single biggest unmeasured
item blocking the 114-method port, and one paragraph in the PR would be worth
more than several of the six claims.

---

## 4. The cost gate

**Verdict: both — the reasoning is principled, the number is convenient.**

The one-line diff. Base `docs/CLUSTERING-PLAN.md:31`:

```
| p95 `put_progress_at` latency | ≤25 ms and ≤2× baseline | ... |
```

PR:

```
| p95 `put_progress_at` latency | ≤25 ms and ≤2× baseline + 1 ms | ...; the additive
bound permits the measured fixed Raft tax without letting it scale away on slower storage. |
```

Rows 2–4 (RSS, growth, write-rate) are byte-identical.

| | Bound | Measured 0.276 ms | Result |
|---|---:|---:|---|
| OLD | min(25, 2 × 0.041) = **0.082 ms** | 0.276 | **FAIL by 3.37×** |
| NEW | min(25, 2 × 0.041 + 1) = **1.082 ms** | 0.276 | **PASS at 25.5% of budget** |

- Loosening factor **13.2×**. The constant is **92.4%** of the new budget, so
  the "≤2× baseline" term contributes 7.6% and is decorative on this hardware.
- Measured raft tax: 0.276 − 0.041 = **0.235 ms**. The new gate permits
  **4.43×** that.
- **A 3× slowdown from here (0.828 ms) passes.** The 25 ms ceiling is 90× away
  and will never bind on sub-millisecond storage.

The shape change is defensible — a pure ratio gate on a sub-millisecond
baseline measures fixed overhead, not proportional overhead, and on a
5 ms-baseline Pi the ratio term correctly takes over. Credit also for
following the plan's own escape hatch (`CLUSTERING-PLAN.md:36-38`: "A budget
change requires measured evidence in PHASE3-SPIKE.md, not a quietly wider
threshold"). This is a documented widening, not a quiet one.

But it no longer constrains the class of regression M1a–M1d will actually
introduce. **Principled and tight**, gating the delta since the argument is
that the tax is fixed:

```
| p95 put_progress_at | ≤25 ms and ≤ max(2× baseline, baseline + 0.5 ms) |
```

On this machine that is 0.541 ms — 0.276 passes at 51% of budget, a 2×
regression fails, and on a slow-storage node the ratio term takes over
automatically. The 0.5 ms is derived from the measured 0.235 ms tax rather
than being a round number chosen after seeing the result.

### L · BLOCKER — the growth measurement was taken under a non-production raft config, and the doc doesn't say so

`docs/PHASE3-SPIKE.md:86-89` records machine, OS, Rust version and release
mode, but not the raft tuning. `node_config()` — shared by the semantic test
and the cost gate (`hiqlite_m0.rs:417-419`) — sets:

```rust
wal_size: 8 * 1024,
raft_config: NodeConfig::default_raft_config(32),
```

hiqlite's defaults are `wal_size: 2 * 1024 * 1024` and
`default_raft_config(10_000)` (`hiqlite-0.14.0/src/config.rs:158-163`).
`default_raft_config(n)` sets `snapshot_policy: LogsSinceLast(n)`.

So the 10,847,744-byte figure was taken with the raft log snapshotted and
purged **312× more aggressively** than production, over a WAL segment **256×
smaller**. Under the shipped default, no snapshot occurs at all inside a
10,000-write run. The test's own comment at `:165` shows the author knew — *"A
small snapshot threshold makes compaction observable in a bounded M0 test"* —
but that caveat is stated for the Compaction row and omitted for the growth
row, which reuses the same config.

The "Pass" in the growth row is therefore not evidence about the shipping
configuration, and M1d's acceptance ("§1 … growth budgets pass") will be
measured against a number nobody has taken. Re-run with
`default_raft_config(10_000)` and `wal_size: 2 MiB`, or state the tuning at
`PHASE3-SPIKE.md:86` and mark the row a lower bound.

### M · SHOULD-FIX — extrapolate the growth number before accepting it

1,084.8 bytes/write. Confirmed beat rates: web 5 s
(`crates/plurxd/src/web/index.html:4952`), Android 10 s
(`PlayerScreen.kt:696`), Apple 10 s (`PlayerController.swift:1483`);
`http/watch.rs:48` calls `put_progress_at` on **every** beat, no coalescer.

Four concurrent streams × 6 h/day:

| Beat | Writes/day | Growth/day | Growth/year |
|---|---:|---:|---:|
| Web, 5 s (today) | 17,280 | 17.9 MiB | **6.37 GiB** |
| Native, 10 s (M1d target) | 8,640 | 8.9 MiB | **3.19 GiB** |
| **What the gate permits** | 17,280 | 112.2 MiB | **40.0 GiB** |

The row's stated rationale is "A raft log that grows without compaction will
fill small data volumes", and the recommended third voter is "Pi/NAS-class
hardware". 40 GiB/year of permitted growth from watch progress alone fills a
32 GB card in ten months; even the measured 6.37 GiB/year is not obviously
acceptable there. Note the underlying data is **constant** — the benchmark
upserts one `watch_state` row 10,000 times — so all 10.35 MiB is log/WAL churn,
which is exactly what compaction should reclaim. The honest measurement is
post-compaction steady-state directory size, which the test never takes.

The row's `2× logical payload` term is dead weight (960,000 bytes = 1.4% of
the 68,068,864-byte budget; plain SQLite already grew 2,759,224). Not this
PR's doing, but this PR's numbers are the first evidence it is toothless.

### N · SHOULD-FIX — the recorded run is not reproducible

`PHASE3-SPIKE.md:86-87` says "Rust 1.95", but `rust-toolchain.toml` pins
`1.97.1` (unchanged from base), so a reader running `make hiqlite-baseline`
gets 1.97.1. Either the number is wrong or the run bypassed the pinned
toolchain. No date is given for the M0 cost run itself, and no storage medium.
Release mode **is** correctly claimed — `Makefile:67` does use `--release`.

Also stale, four lines above the new section: `PHASE3-SPIKE.md:59-61` still
says config requires `enc_keys`. `Cargo.toml:30` pins hiqlite with
`default-features = false, features = ["macros", "sqlite"]`, and in 0.14
`NodeConfig::enc_keys` is `#[cfg(any(feature = "s3", feature = "dashboard"))]`
— **the field does not exist in this build**. It also conflicts with
`CLUSTERING-PLAN.md:112-115`, which puts these secrets in 0600 files, never in
config. (The design review's "no perf numbers" charge *is* fixed; the secrets
charge was resolved by the plan, not by this PR, and this line now contradicts
both.)

### O · NIT — two smaller measurement notes

`PHASE3-SPIKE.md:98-99` says the old gate was "0.083 ms"; 0.041 × 2 = 0.082.
Presumably the unrounded p95 was ~0.0415 — in which case the table's `0.041`
is rounded and the two cannot both be right. In a paragraph justifying a 13×
loosening, publish the unrounded figure. Line 100's "two orders of magnitude
inside the 25 ms ceiling" is 90.6×, i.e. 1.96 orders — rounded in the
flattering direction.

Neither side fsyncs per write (SQLite is `WAL` + `synchronous=NORMAL`,
`store/sqlite/mod.rs:685-688`; hiqlite's `ImmediateAsync` acks before
`flush_async()`), so the comparison is fair — but a reader of "an acknowledged
upsert" against REQ-HA-1's durability language would assume otherwise. One
clause fixes it.

---

## 5. Dependencies — clean, and worth saying so

This looked like the riskiest part on paper and came out the best.

**The core claim is true.** hiqlite 0.14.0 requires `rusqlite ^0.40`;
rusqlite 0.37.0 requires `libsqlite3-sys ^0.35.0` and 0.40.1 requires
`^0.38.1`. `libsqlite3-sys` declares `links = "sqlite3"`, so two majors is a
hard Cargo resolution error. Keeping 0.37 was not an option.

| | base `bb3f6a1c` | PR `41aca7eb` |
|---|---|---|
| rusqlite | 0.37.0 | **0.40.1** |
| libsqlite3-sys | 0.35.0 | **0.38.1** (exactly one) |
| bundled SQLite | 3.50.2 | **3.53.2** |
| hiqlite / openraft | — | 0.14.0 (`=` pin) / 0.9.25 |
| openssl-sys / native-tls | absent | **still absent** — rustls end to end |

- **Exactly one `libsqlite3-sys`**: yes. **hiqlite absent from default
  builds**: yes — `cargo tree -p plurxd -e normal,build | grep -icE
  'hiqlite|openraft|aws-lc'` → `0`.
- **New crates**: lockfile 238 → 433 (+195, 176 new names). **Default build
  compiles zero of them** — the compiled-unit sets differ by exactly the four
  version bumps, 192 → 191 units (net −1). Under the feature: +141.
- **Zero rusqlite call-site changes is correct, not silent breakage.** Every
  changed surface was checked against the ~7,855-line store: the `u64`/`usize`
  `ToSql`/`FromSql` move is behind a new off-by-default `fallible_uint`
  feature (repo only casts `execute`'s return); `From<ValueRef>` → `TryFrom`
  is a strict improvement the repo never uses; `Error::Utf8Error` gained a
  field the repo never matches; transaction defaults unchanged; all five
  pragmas the repo passes remain valid identifiers.
- **The SQLite bump is empirically inert here.** Compile-time define sets are
  byte-identical, and both amalgamations produce identical
  `EXPLAIN QUERY PLAN` for the `idx_items_missing_artwork` partial-index query
  (`media.rs:652-669`) and the FTS5 search join (`media.rs:410-418`), plus
  identical `unixepoch()`, `COLLATE NOCASE`, FTS5 `unicode61` tokenizer
  output, `RETURNING`, JSON, `LIKE`/`GLOB`, and STRICT behavior.
- **Build cost is a wash** on the default set: 82 s / 574 MB vs 82 s / 576 MB.
  Under the feature: 215 s (+162%), 1053 MB (+83%).

Two things to decide deliberately rather than inherit:

**P · SHOULD-FIX — hiqlite drags two independent `aws-lc-sys` C/assembly
builds into the feature graph.** `aws-lc-sys 0.39.1` (via
`s3-simple ← cryptr ← hiqlite`) and `0.43.0` (via `aws-lc-rs`), both compiled.
`cryptr` is a *non-optional* hiqlite dependency carrying `features = ["s3"]`,
which pulls `s3-simple` → `quinn` (QUIC) + `md5` + `csv` into a media server's
graph. I tested whether the PR caused it: switching the workspace `rustls` dep
to `ring` and re-resolving produced an identical 333-crate graph, so this is
inherent to hiqlite 0.14. Still the largest supply-chain and build-cost item,
and `make hiqlite-baseline` means two release-mode `aws-lc-sys` builds on
every cold CI cache.

**Q · SHOULD-FIX — +176 crate names enter RustSec scope though none ship.**
`cargo audit` reads `Cargo.lock`, not the compiled unit graph, and
`rust-audit.yml`'s `paths:` already includes `**/Cargo.lock`. New scope
includes `openraft 0.9.25` (pre-1.0), `cryptr`, `s3-simple`,
`fastwebsockets`, plus revived `thiserror 1.0.69` and `rand 0.8.7`. Made worse
by two pins — the PR's `hiqlite = "=0.14.0"` and hiqlite's internal
`time = "=0.3.51"`. An advisory on `time 0.3.51` fails `main` for a dependency
no shipped artifact contains, and `cargo update -p time` cannot fix it. Decide
now whether audit should scope to the default feature set.

**R · NIT — under the feature, rustls has both crypto providers enabled.**
Default: `ring,std,tls12`. With `hiqlite-spike`:
`aws-lc-rs,aws_lc_rs,log,logging,prefer-post-quantum,ring,std,tls12`.
`rustls-0.23.42/src/crypto/mod.rs:265-286` returns `None` from
`from_crate_features()` when both are on, so
`get_default_or_install_from_crate_features()` panics. Harmless today —
reqwest names its provider and `hiqlite_m0.rs:27,264` installs aws-lc-rs
first — but any future direct `rustls::ClientConfig::builder()` under the
feature panics at runtime rather than failing to compile. Also: the cost
baseline is measured on **aws-lc-rs** while hiqlite's own `start.rs:30`
installs **ring**, so the numbers are not the numbers a hiqlite-default
deployment produces.

---

## 6. M0 acceptance checklist

Source: `docs/CLUSTERING-PLAN.md:333-347`.

| Item | Delivered? | Evidence |
|---|---|---|
| `make check` | **Yes** | 678 passed workspace-wide; `validation-lint` → `catalog ok: 17 points · 20 checks · 325 audited files`; `history-check` 0 errors on head (1 on base) |
| Populated v14 fixture retains every cache row, package, and byte | **No** | `cachekeep.rs:1275` — passes against base semantics; see finding **D** |
| Two fresh data dirs get distinct node ids | **Yes** | `cluster.rs:202` |
| Restarts preserve ids | **Yes** | `cluster.rs:219`, `:245`, `:258`, `:282` |
| Spike records **every** result in `PHASE3-SPIKE.md` | **Partial** | 7 contracts + 4 budget rows recorded; the Replication-unit row is asserted, not observed — finding **G** |
| Config accepts unknown `[cluster]` key while `[server]` still rejects | **Yes** | `config.rs` matrix verified both directions |
| Prove FTS5/triggers, `RETURNING`, txn CAS, transport auth, compaction | **Partial** | 1 of 5 proven, 4 weaker — §3 table |
| Prove **statement-vs-row replication semantics** | **No** | Bound literal; finding **G** |
| Record the §1 baselines | **Partial** | Taken under non-production raft config; finding **L** |
| Update `points.toml` with any new module in the same commit | **Yes** | Both new files added; the `-1` is a rewritten brace line, nothing dropped — verified by running the base catalog against the PR tree (fails with exactly the two missing-point errors) |

The `regressions.toml` claim checks out fully: removed hash is `24733d6f`
("docs: review the clustering plan and the cache-reader fix"),
`git merge-base --is-ancestor 24733d6f bb3f6a1c` → **not an ancestor** (it
survives only on the PR-79 branch and was squashed into `bb3f6a1`).
`audit_history()` on base → exactly one error naming it; on the PR → 0. The
entry was `ignore = true` with empty `points`/`checks`, so removing it
disables no guard.

---

## 7. Checked and clean

**Identity.** Fail-closed everywhere — every error path yields
`StoreError::Identity`, `open_store`'s `?` propagates to `main.rs:232` and
aborts `run()`; no log-and-continue, no default identity, no `unwrap_or_else`
fallback in the module · ordering is correct: identity is established inside
`open_store` before `StoreHandle`, therefore before `AppState`, before
`reset_interrupted_offline_packages`, and before any sweep is spawned · the
atomic-publish claim holds line by line, with 0600 **at creation** (no
world-readable window) and `link(2)` (never `rename`) · the read-NotFound →
link race is handled correctly (loser gets `EEXIST`, re-reads, adopts the
winner's id; their 8-thread test passes, and the cross-process case is the
same atom) · no temp-file leak on any error path, confirmed by directory
listings in every probe · empty `node.id`, a directory at that path, a BOM
prefix, and an embedded NUL all fail closed and are distinguishable; trailing
`\n`, `\r\n`, and surrounding spaces are trimmed; nil UUID rejected ·
`AppState::node_id` now comes from `identity.node_id` (`main.rs:307`, passed
at `:327`) while client-facing identity is untouched — `http/system.rs:39,164`
and `http/plex.rs:92,98` still call `store.instance_id()`, and mDNS/GDM still
use `instance_id` (`main.rs:373,400`) · `raft_id` exists only as
`SINGLE_VOTER_RAFT_ID: u64 = 1` and keys nothing · `StoreError::Identity` is
used at six sites with no exhaustive `match` outside the store module ·
`plurx.example.toml`'s stanza matches `ClusterConfig` field-for-field and
round-trips to the struct defaults, and matches `CLUSTERING-PLAN.md:105-111`
verbatim.

**Feature gating.** `hiqlite-spike = ["dep:hiqlite", "dep:rustls"]`, both
`optional = true` — the gate is on the dependency, not just the test · no
default feature enables it and it is not transitively reachable
(`grep -rn hiqlite --include=Cargo.toml .`) · no non-test production code sits
behind it; `cluster.rs` is ungated but never references the crate ·
`rust-toolchain.toml` unchanged at 1.97.1, which builds and lints both
configurations.

**Measurement method.** 10,000 samples; `percentile_95` indexes
`(10000*95).div_ceil(100)-1 = 9499` — correct and adequate · one voter is the
*right* configuration, not a floor to apologize for: `CLUSTERING-PLAN.md:24-27`
and REQ-HA-1 both specify single-node deployments run "the same one-voter
replicated store", and this is the single-node gate · both sides go through
comparable code shapes, and SQLite carries an extra `spawn_blocking` hop and a
`SELECT unixepoch()` that hiqlite does not, which *inflates* the baseline and
makes the 2× term more generous · all unit conversions check out (8,650,752 B
= 8.25 MiB; 10,847,744 = 10.345 MiB; gate 68,068,864 = 2 × 48 × 10,000 +
64 MiB) · the watch-rate row is honestly red and marked "Known M1d blocker",
matching `ARCHITECTURE.md:95-97` · no doc changed by this PR claims M0 is
complete.

---

## 8. What must change before merge

1. **A** — return the parsed `Uuid` from `validate_uuid` and store its
   canonical form. One line; without it, M0 reintroduces B1.
2. **B** — make the mode check `mode & 0o077 == 0` with a warning, and fall
   back to `rename` when `hard_link` reports `EPERM`/`ENOSYS`.
3. **G** + **H** — ~15 lines: execute `unixepoch()` and assert rejection on all
   three voters; execute `CURRENT_TIMESTAMP` and assert it is *accepted*, so
   the hole is documented; insert into a table with `DEFAULT (unixepoch())`
   and assert failure. Then record the 68-site / 24-column number in
   `PHASE3-SPIKE.md` — it is the most valuable thing M0 can hand M1.
4. **I** — either add `--features hiqlite-spike` to a CI job, or remove
   `hiqlite_m0.rs` from `validation/points.toml:231` so the catalog stops
   claiming coverage it doesn't have. Add `--exact` to the baseline target.
5. **L** — re-run the growth baseline under the production raft config, or
   state the tuning and mark the row a lower bound.
6. **The gate number** — `max(2× baseline, baseline + 0.5 ms)`, or another
   bound derived from the measured 0.235 ms tax rather than chosen after it.

**C**, **D**, **E**, **F**, **J**, **K**, **M**, **N**, **P**, **Q** are worth
doing but should not hold the merge. Add two sentences to the PR while you're
in there: that crash recovery, snapshot catch-up, and failover are **out of
scope for M0** because hiqlite 0.14's process-global shutdown handler forbids
in-process node restart, and that `txn()` cannot express the 11 existing
read-then-write transactions. Those two facts are worth more to M1 than four
of the six claims.

The identity machinery, the config split, and the dependency work are all
sound. The blockers are one discarded return value, one over-strict
comparison, and a set of assertions that stop just short of the thing they
were written to prove.
