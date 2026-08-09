# PR #79 review — the ownership mechanism holds, the bookkeeping around it does not

**Status:** review complete · **Reviews:** [PR #79][pr79] "Finish Cinema
handoff and harden cache cleanup", head `299473ad` on
`codex/finish-cinema-handoff` · **Verified against:** `origin/main` @
`854d575b`, which is also the merge-base · **Written:** 2026-08-06 ·
**Outcome:** **changes requested** · **Follows:**
[CLUSTERING-PLAN-REVIEW.md](CLUSTERING-PLAN-REVIEW.md), whose eight blocking
contracts this PR answers

[pr79]: https://github.com/pjunod/plurx/pull/79

Companion to [CLUSTERING-PLAN-REVIEW.md](CLUSTERING-PLAN-REVIEW.md) (what the
plan got wrong) — this is *whether the revision fixed it, and whether the code
that shipped alongside it is safe*. Findings cite `file:line` at `299473ad`.

**How it was reviewed.** The PR head and `origin/main` were checked out as
pinned detached worktrees from a fresh clone; nothing was read from a working
tree. Three scoped passes ran independently — cache-ownership correctness
(with executable probes), the eight-contract adjudication against the revised
plan, and cross-document truthfulness. Every CONFIRMED finding below was
reproduced by running code, not by reading it.

**Build state at head:** `cargo test -p plurx-core -p plurxd` is green —
322 + 320 passed, 1 pre-existing ignored storage test.
`make validation-lint` passes: `catalog ok: 17 points · 20 checks · 323
audited files`.

**Severity legend:** **BLOCKING** = a regression against `main`, or a claimed
protection that does not hold · **SHOULD-FIX** = real cost or a contract that
permits unsafe behavior · **NIT** = worth doing, not worth blocking.

---

## 1. Verdict

The reader/eviction ownership mechanism is **structurally sound**. It was
stress-tested — 8 worker threads, 60 rounds, modelling `serve_cached` exactly
— and produced 12,815 guarded hits, 3,855 genuine eviction blocks, 1,330
row-gone races, and **zero** cases of a held guard over deleted bytes. The
TOCTOU window is closed, deregistration is RAII and survives task
cancellation, and the registry key is internally consistent at every site.
That is the hard part and it is right.

Seven of the eight blocking plan contracts are now resolved, several with more
rigor than the review asked for. The eighth (**B4**, segment numbering) is
half-done.

What blocks the merge is the bookkeeping *around* the new guard. Making the
two sweeps disjoint removed an accidental safety property, and the PR did not
replace it:

- **Two concurrent sweeps now each evict the full required amount.** In a
  10-entry probe the cache went from 4 survivors on `main` to **0 survivors —
  wiped — 5 rounds out of 5**, with both sweeps logging that they had stopped
  exactly at the ceiling.
- The **empty-`Ok(vec![])` → delete-the-whole-cache-root** hazard is *not*
  closed. The guard is byte-for-byte the base guard; it still catches only
  `Err`. This PR is the one that claims to harden orphan cleanup, so it is the
  right place to close it.
- **Offline package downloads read the same directories with no reader
  guard** — the one hole in "process-local reader ownership across all
  cleanup paths".

---

## 2. Code findings

### A · BLOCKING — concurrent sweeps over-evict; two overlapping passes can empty the cache

`crates/plurxd/src/cachekeep.rs:289` (and the identical `:256` in the stale
loop):

```rust
let _eviction = match readers.begin_eviction(&entry.recipe_hash) {
    Ok(guard) => guard,
    Err(CacheBusy::Readers) => { out.protected += 1; continue; }
    Err(CacheBusy::Evicting) => continue,     // <-- `used` is NOT adjusted
};
let bytes = entry.bytes.max(0);
if forget(store, root, node_id, &entry).await { … used -= bytes; }
```

`CacheBusy::Readers` correctly leaves `used` alone — those bytes stay.
`CacheBusy::Evicting` is the opposite case: another sweep is deleting those
bytes *right now*. This sweep skips the entry, keeps its stale `used`, and
walks further down the cold list evicting additional entries to hit its own
target.

**On `main` this was self-limiting.** Both sweeps raced on the *same* entries;
the loser's `remove_dir_all` returned `NotFound`, `forget_cache_entry` was a
no-op `Ok`, `forget` returned `true`, and the loser decremented `used` for
bytes someone else had freed. Wrong bookkeeping, right outcome. The new guard
makes the sweeps **disjoint**, which is exactly what removes the accidental
safety.

**CONFIRMED — reproduced:**

| Scenario | `main` | PR #79 |
|---|---|---|
| 6 entries × 0.2 GB, 1 GB ceiling (1 must go) | 5 survivors (4 in 1/5 rounds) | **4 survivors, 5/5 rounds** |
| 10 entries × 0.2 GB, 1 GB ceiling (5 must go) | 4 survivors | **0 survivors — cache wiped** |

Both sweeps reported `evicted: 5, bytes_freed: 1073741820, bytes_after:
1073741820` while the store held 0 bytes. Each believed it had stopped exactly
at the ceiling. Amplification is N× for N overlapping sweeps.

**Failure scenario.** `transcode_cleanup_mins = 60`, `cache_produce_mins =
360`, `cache_max_gb = 50`, cache at 100 GB on a NAS. Tick T spawns
`produce_pass` (`state.rs:1294`), whose first action is a sweep; the orphan leg
walks the whole two-level tree over SMB. If that walk outlives the gap to the
next due `CleanupTranscode` (`state.rs:1192`, inline in `run_due_jobs`), the
two sweeps overlap and each evicts 50 GB — the cache empties instead of
halving.

**Reachability, honestly.** The settings API clamps intervals to ≥ 15 min
(`http/system.rs:1045`), and within a single tick `CleanupTranscode` is
ordered before `ProduceCache` (`schedule.rs:95,101`), so the overlap needs
`produce_pass`'s sweep to run ≥ 15 minutes. Plausible on a large cache over a
slow mount, not routine. The *accounting* is unconditionally wrong regardless,
and the fix is one line.

**Fix.** In the `Evicting` arm, `used -= entry.bytes.max(0); continue;` —
another owner is freeing them — or bail out of the budget pass entirely, since
a peer sweep owns the work.

### B · SHOULD-FIX — the empty-`Ok(vec![])` whole-root delete is still open

`crates/plurxd/src/cachekeep.rs:445-453`:

```rust
let rows = match store.all_cache_rows(node_id).await {
    Ok(rows) => rows,
    Err(error) => {
        // Ownership uncertainty must fail closed. Treating a store error
        // as an empty keep-list would recursively delete valid media.
        tracing::warn!(%error, "cache: could not enumerate filesystem owners; skipping orphan sweep");
        return (0, 0);
    }
};
```

Byte-for-byte the base guard: only `Err` is caught. `Ok(vec![])` still yields
an empty keep-list, and every directory under the root — including `tmp/`
staging — is deleted as an orphan. The PR's only addition here is the
per-recipe reader guard, so the sole survivor is whatever someone happens to
be watching at that instant.

**CONFIRMED — reproduced:**

```
PROBE1 out=Swept { orphans: 3, … }   a=false b=false staging=false
PROBE2 out=Swept { protected: 1, orphans: 1, … }   a=true b=false
```

**Failure scenario.** `node_id` is `instance.id`, a UUID minted at migration
and stored in `settings` (`store/sqlite/mod.rs:880`). Restore `plurx.db` from
a backup taken before that key existed — or recover from corruption by
recreating the DB — while `data_dir/cache/transcode` survives on disk, which
it deliberately does across reboots (`main.rs:247-253`). The new process has a
new `instance_id`; `all_cache_rows(new_node)` returns `Ok(vec![])`; the first
enabled `CleanupTranscode` tick recursively deletes every cached transcode
plus in-flight staging. Only recipes with a live playback session survive.

This is cache loss (hours of GPU), not media loss — the root is
`data_dir/cache/transcode`, a sibling of `data_dir/transcode`. Blast radius is
still limited by both sweep intervals defaulting to off
(`state.rs:1474`); enabling either arms it.

**Fix.** `if rows.is_empty() { warn; return (0, 0) }` unless the root is also
empty. *"This node owns nothing, and yet the disk is full of directories"* is
not a fact the sweep should act on. No test covers this case —
`an_active_reader_survives_row_loss_and_the_orphan_pass` deletes one row while
an unrelated reader is held; it never exercises the all-rows-gone shape.

### C · SHOULD-FIX — offline package downloads stream from cache directories with no reader claim

`crates/plurxd/src/http/offline.rs:638-675` (`package_dir`) resolves the same
`relative_dir` and streams from it while holding nothing. `serve_cached`
(`transcode.rs:2349`) is the **only** `begin_read` call site in the tree.

Today this path is protected by a different mechanism — `cache_by_age`,
`cache_bytes`, and `stale_cache_claims` all exclude recipes with a
`queued|preparing|ready` package (`store/sqlite/cache.rs:162-167,200-205,273-278`),
and the orphan sweep keeps the row's directory via `all_cache_rows`.

**Failure scenario — the exact cascade this PR was written to defend against.**
A scan reconciles the source file away → `files` delete → `ON DELETE CASCADE`
through `transcode_cache_recipes` → `transcode_cache_locations`
(`store/sqlite/mod.rs:343,350`). The offline package's cache row vanishes, so
the directory is now an orphan with **no reader claim**, and the next orphan
pass deletes it mid-download. A live *playback* session survives this — that's
the fix working. A live *download* does not.

Impact is bounded: `package_dir` is re-resolved per request and returns
`410 package_evicted` afterwards, so the worst case is a torn segment read or
a failed download, not silent corruption.

### D · NIT — the registry key is `recipe_hash` alone while `forget_cache_entry` is now three-part

`cachekeep.rs:44,78,95` vs `store/mod.rs:778-783`. Half the pair was scoped.

This is **not** the classic register-vs-check mismatch — both `begin_read` and
all four `begin_eviction` sites use the bare `recipe_hash`, so the key is
internally consistent, and the error is in the safe direction (a reader on
`H/shared` blocks eviction of `H/local`: over-protection, a bounded leak that
clears when the session ends). Node scoping is genuinely redundant, since the
registry is per-`TranscodeManager` and there is one per process. Worth a
comment saying the asymmetry is deliberate, so the next person doesn't "fix"
it into under-protection.

Latent, same area: `cache_by_age` returns rows of any `storage_class`, and
`forget()` (`cachekeep.rs:360`) resolves `entry_dir(local_root, …)`
regardless — evicting a future `'shared'` row would `remove_dir_all` a
*local* path. Unreachable today (`claim_cache_entry` only inserts `'local'`,
`store/sqlite/cache.rs:91`), fires the day shared storage lands.

### E · NIT — `Err(CacheBusy::Evicting)` is invisible in `Swept`

`cachekeep.rs:256,289,501,544`. Skipped-because-a-peer-owns-it increments
nothing, so the sweep takes the `"still over budget after a full eviction
pass"` branch (`:313`) — whose comment says that means "something is wrong
with the sizes" — when the real cause is a concurrent sweep. Also, an entry
protected in both step 1 and step 3 double-counts into `out.protected`
(log-only; the Prometheus gauge reads `active_entries()` independently).

### F · NIT — every new test is sequential, so none pins the TOCTOU fix

`every_reader_must_leave_before_eviction_can_claim_a_recipe`,
`a_lookup_during_eviction_is_an_ordinary_cache_miss`,
`an_active_reader_survives_row_loss_and_the_orphan_pass`,
`active_cache_playback_is_not_evicted_under_its_reader`, and
`forgetting_one_storage_class_keeps_the_other_copy` are all real and
non-vacuous. But none races a reader against a sweep, none covers the
empty-`Ok` orphan case, and none covers the concurrent-evictor case that
finding **A** lives in. The PR's claimed "eviction races" test is a two-line
sequential state-machine check. The metric assertion (`http/mod.rs:2713`)
pins name, label, and format but only against the value `0`, so it cannot
catch `active_entries()` returning a constant.

### G · NOTE — ownership is process-local, by design

Two plurxd processes on one data directory have independent registries;
process B's sweep will delete a directory process A is serving. `main` had no
protection here either, so this is not a regression — but the PR's framing
("ownership") invites the assumption that it is covered. One sentence in
[OPERATIONS.md](OPERATIONS.md) would close the gap in expectations.

---

## 3. The eight contracts

| | Verdict | Justification |
|---|---|---|
| **B1** node_id split strands cache bytes | **RESOLVED** | §3.1:84 "create it atomically from the current `instance.id`" — seed choice stated, UUID-shape rationale given, M0 acceptance is a *populated* v14 fixture "with cleanup enabled" |
| **B2** advisory fence | **RESOLVED** | §3.4:171-176 makes it a parameter: replicated `job_leases`, "Publishing methods … take `&Lease`", "zero matching rows aborts the entire write. A pre-write check is not a fence." Owner M4, acceptance is the `SIGSTOP`/`SIGCONT` drill |
| **B3** `put_session` LWW | **RESOLVED** | §3.4:159-165 `claim_session(session_id, expected_owner_epoch, next, ttl) -> Result<Lease, SessionConflict>`, built on the same primitive as B2 |
| **B4** `ReplicatedSession` field loss | **PARTIAL** | All six PERF-PLAN fields restored plus three more; overlap, DISCONTINUITY-SEQUENCE, and generation-specific `EXT-X-MAP` covered. **`-start_number 0` is still never mentioned** — see below |
| **B5** `TranscodeRecipe` doesn't exist | **RESOLVED** | §2:47 names `Recipe<'a>` and calls its hash "a node-local cache address, not a cluster recipe"; §3.5:216-221 picks the owned wire type |
| **B6** M1 = 114 methods, no comparison until M3 | **RESOLVED** | §6.2–§6.5 are M1a–M1d; three voters arrive in M1b with byte-identical replica dumps as acceptance; `unixepoch()`/`last_insert_rowid()` ruled out, FTS5 node-local |
| **B7** no quiesce/atomic target/rollback | **RESOLVED** | §4 now has quiesce, a `data/hiqlite.incoming` staged target, ordered content hashes + `foreign_key_check` + M1a's parity suite, fsynced marker, atomic rename, and a named rollback command |
| **B8** `deny_unknown_fields` unrollbackable | **RESOLVED** | §3.2:96-99 tolerated default-empty `ClusterConfig` before any release requires its keys; M2:357 names the rollback floor |

**Every new claim the revision makes about the tree was checked. None is
false** — including the 114-method count, `PipelineDigest::feed`, the
`instance_id`/`server.name` mDNS derivation, `MIGRATIONS.len() == 14`, and
that `database-upgrades` is absent from the `commit` profile.

### B4 — the one remaining hole

The field exists; nothing connects it to bytes on disk.
`crates/plurx-core/src/transcode/mod.rs:972` and `:1401` both pass
`-start_number 0`, and `served_live_playlist`
(`crates/plurxd/src/transcode.rs:514-570`) computes
`#EXT-X-MEDIA-SEQUENCE:{first_retained}` from the *filename* index. A survivor
starting a fresh ffmpeg gets `seg00000.ts` again and MEDIA-SEQUENCE resets
with it. Add to §3.5, after "advance `MEDIA-SEQUENCE`":

> The survivor's writer starts at the replicated sequence, not at zero: both
> HLS argument builders (`transcode/mod.rs:972`, `:1401`) pass
> `-start_number {media_sequence}` instead of the current hardcoded `0`, and
> the copy segmenter names its first output with the same index.
> `served_live_playlist` continues to derive `MEDIA-SEQUENCE` from the
> retained segment index, which is then correct by construction; it must
> additionally emit `EXT-X-DISCONTINUITY-SEQUENCE` from
> `SessionOwner.discontinuity_sequence` whenever it drops a prefix, because a
> failover discontinuity that slides out of the window otherwise shifts the
> client's discontinuity numbering (RFC 8216 §6.2.1).

And to M5's acceptance (§6.9:395):

> Assert that after takeover no two segment URIs in the session's lifetime
> name different bytes, and that a client reloading the playlist after the
> failover discontinuity has been pruned still sees the same
> `EXT-X-DISCONTINUITY-SEQUENCE` for the remaining segments.

### Four smaller plan edits

**The fence and the epoch are two counters with one job.** §3.4:174 says
session publication "carries that same fence"; §3.5:194 stores `owner_epoch`;
PERF-PLAN §7.3 event 6 says publications carry `owner_epoch`. Nothing says
they are the same number. Add to §3.4:

> For a `session:<playback_id>` resource, the lease's `fence` **is**
> `SessionOwner.owner_epoch`; there is one monotone counter per session,
> allocated by `claim_session`, and `expected_owner_epoch` is compared against
> it inside the same replicated transaction.

**Name the publishing methods.** §3.4:172's "such as" is not checkable. Add:

> Concretely, in M4 these `Store` methods gain a leading `lease: &Lease`
> parameter: `upsert_item`, `claim_cache_entry`, `complete_cache_entry`,
> `forget_cache_entry`, `claim_next_offline_package`,
> `complete_offline_package`, and the scan-cursor writers
> (`store/mod.rs:712-783`). A publishing method without the parameter is a
> review failure, not an oversight.

**The quiesce claim covers `run()` only.** §4 step 2 is true of `run()` (store
opens at `main.rs:239`, first spawn at `:338`) but `plurxd reset-password`
(`main.rs:206`) and `plurxd refresh-metadata` (`main.rs:130`) also call
`SqliteStore::open`, which runs `migrate()`, and both are documented as safe
beside a running server on WAL (`main.rs:184`). Add:

> Only `plurxd run` may perform the hiqlite import. `reset-password` and
> `refresh-metadata` must refuse to run against an unmigrated data directory
> after M2, rather than starting an import from a second process.

**Make the B8 tolerance testable.** Add to §3.2 and M0's acceptance:

> `ClusterConfig` is `#[serde(default)]` **without** `deny_unknown_fields`;
> `Config`, `ServerConfig`, and `StorageConfig` keep theirs. M0's acceptance
> includes a test that `[cluster]\nsome_future_key = 1` loads while
> `[server]\nnmae = "typo"` still fails — the existing pin at
> `config.rs:124-145` must keep passing unchanged.

Note `plurx.example.toml` is a governed path (`validation/points.toml:251`),
so the example `[cluster]` stanza lands in the same commit as the catalog
update.

### Problems the revision introduced

1. **Two spellings of `SessionOwner` now exist.** §3.5 declares PERF-PLAN §7.3
   canonical "not a second spelling", then renames every field:
   `owner_node`→`owner_node_id`, `lease_expires_at`→`lease_expires_at_unix_ms`,
   `produced_playable_through`→`produced_playable_through_ms`,
   `fetched_through`→`fetched_through_ms`, and replaces `recipe` with
   `SessionRecipeInputs`. PERF-PLAN §7.3 was not updated in this PR. Either
   edit §7.3 to match, or drop the struct from §3.5 and link.
2. **`tone_map_required: bool` collapses a three-valued enum.** `ToneMap` is
   `{Zscale, Libplacebo, None}` (`transcode/mod.rs:406-415`) and `None` is a
   user-visible escape hatch. The bool is probably the right reduction, but
   §3.5 must say "`false` means the session was started with `ToneMap::None`"
   so an implementer cannot get it backwards. `TranscodeOptions::start_seconds`
   is also dropped without comment.
3. **`ARCHITECTURE.md:94-95` is still false and the plan now leans on it.**
   §1's fourth budget row cites it as "already promises coalescing; clustering
   must make it true" — correct framing, and M1d:349 schedules the coalescer —
   but `http/watch.rs:40-48` still does an unconditional `watch_state` read
   plus `put_progress_at` per 5s beat, and ARCHITECTURE.md was not touched.
   Add a one-line correction there so the two documents don't disagree while
   M1d is open.
4. **REQ-HA-5's "one settings surface" is half-traced.** §5 promises admin
   shows leader, lag, voters, degraded state, but nothing addresses
   `state.direct_plays` being in-memory (`http/watch.rs:58-60`), so the
   activity page still shows one node's viewers. One acceptance clause in M3
   closes it.

### The §7 open questions

All seven answered, six consistently. **Seed `node.id` from `instance.id`**
(§3.1:84, with `raft_id` split out and explicitly not a filesystem key) ·
**decompose M1** into M1a–M1d (§6.2–§6.5, three-voter harness in M1b) ·
**tolerated empty `[cluster]` in M0** (§3.2:96, M2:357 names the rollback
floor) · **mobile failover as M7** (§6.11, physical-device acceptance;
REQ-HA-6 correctly left unchanged) · **artwork on three nodes** = per-node
materialization + proxy-to-holder (§3.6:248-252, M4:380-381, same rule applied
to segments in M5:391) · **budget is a gate** (§1:27, four numeric rows,
baseline in M0, enforced in M1d). The inconsistent one is **adopt PERF-PLAN
§7.3 verbatim** — answered "yes in principle", done differently in practice;
see problem 1 above.

**Process gates** are now all accounted for: M0:303 covers the literal file
list at `points.toml:249`, §8:456 names the `database-upgrades` command as an
M2 gate, M5:393 covers new ports, and §3.6:241-246 covers the mDNS/GDM
collision. One correction to my own prior review:
`tests/operations/test_contracts.py:20-24` are `assertIn` presence checks, so
adding ports 32401/32402 would **not** fail them. What is governed is
`deploy/**` (`points.toml:16,449`).

---

## 4. Documentation findings

### H · SHOULD-FIX — `ROADMAP.md` still says Perf M2 is unaccepted; `STATUS.html` says it's done and deleted the operator task

`docs/ROADMAP.md:112-121` (unchanged by this PR):

> `- 🚧 **M2 in progress** — tone-map on the GPU … Remaining: the acceptance
> run, which needs a box with a GPU`

There is **no M3 entry at all**. Meanwhile, in the same PR:
`STATUS.html:92` still says the page "mirrors `docs/ROADMAP.md`";
`STATUS.html:177` says `✓ done … M2 GPU tone-map: accepted on nynuc at 4.89×
the CPU chain`; `:178` says `✓ done … M3 pre-transcode cache`; and `:267`
**deletes** the operator item "Perf M2 acceptance run on a box with a GPU",
replacing it with `✓ measured`.

`docs/PERF-PLAN.md:3-8` sides with STATUS ("M1, M2 and M3 complete
(2026-07-29) … 4.89× the CPU chain … 9.05 Mb/s peak against a permitted
13.6"), so ROADMAP is the stale document. But a status page deleting a
hardware release gate while its own declared source still lists it as owed is
exactly the failure this convention exists to prevent.

**Correction:** flip the ROADMAP bullet to `✅ M2 complete 2026-07-29` with the
nynuc figures and add an `✅ M3 complete` bullet. `PERF-PLAN.md:1259` ("Still
owed in M2: the acceptance run below") is stale in the same direction and
contradicts its own file header.

### I · SHOULD-FIX — `STATUS.html` deleted the HLS master-ladder operator item, which is still open and still device-only

Removed at `STATUS.html:264-270`:

> `The two master ladder rungs are compiled in and OFF —
> PLURX_HLS_CLOSED_CAPTIONS_NONE, PLURX_HLS_FORCED_AUTOSELECT. One per deploy,
> observed on the device`

Still true in the tree: `crates/plurxd/src/http/hls.rs:979-980` reads both env
vars, both default off; `docs/APPLE-NATIVE-SUBTITLES-PLAN.md:619-620`
(untouched here) says "**neither rung has been enabled on a node or observed
on a device**"; `OPERATIONS.md:62-63,78-79` and `CHEATSHEET.md:145-146`
document them as live experiments. The PR retitled the operator section to
"hardware evidence that source tests cannot supply" — which is precisely what
these rungs are. The DV `-12927` resolution (2026-08-03) closed the *DV* item,
not the two authoring-rules experiments. **Restore one operator line.**

### J · SHOULD-FIX — `OPERATIONS.md` conflates the sweep counter with the Prometheus gauge

`OPERATIONS.md:376-383`:

> "`protected > 0` means the sweep found bytes an active playback still
> owns … The Prometheus gauge `plurx_cache_protected_entries{reason=
> "active_playback"}` **reports the same condition continuously**."

They measure different things. `Swept.protected`
(`cachekeep.rs:250,286,494,538`) increments only when a sweep *attempted* an
eviction and was refused — a per-sweep skip count. The gauge
(`cachekeep.rs:118-125` → `transcode.rs:2101-2103` → `http/system.rs:1619`)
counts every recipe held by a live cached HLS session, whether or not
housekeeping ever wanted it.

**Failure scenario.** Cache well under `cache_max_gb`, one viewer watching a
cache-hit title. No sweep skips anything, so the log never prints `protected`.
The gauge nevertheless reads `1`. An operator following the paragraph's own
advice ("low free space while it is non-zero means stop new cache production")
treats an ordinary cache hit as an incident. `PERF-PLAN.md:1443-1444` has the
correct wording — "reports the current number of held recipes".

### K · SHOULD-FIX — "the next sweep reclaims it", but on a default install there is no next sweep

`OPERATIONS.md:370-374` promises reclamation. The only two production sweep
call sites are `state.rs:1192` (`DueJob::CleanupTranscode`) and `state.rs:1294`
(`produce_pass`). Both are gated by `due()` (`schedule.rs:58-59`), which
returns `false` for `interval_mins <= 0`, and both intervals come from
`job_interval()` (`state.rs:1472-1474`), whose default is `0` — off. Neither
`transcode_cleanup_mins` nor `cache_produce_mins` is documented anywhere in
OPERATIONS.md.

**Add:** "Sweeps only run when `transcode_cleanup_mins` or `cache_produce_mins`
is set; both default to `0` (off), and with both off nothing reclaims the
cache at all."

### L · SHOULD-FIX — `APPLE-NATIVE-SUBTITLES-HANDOFF.md:46` contradicts this PR's own edits

> `- [ ] Android still needs the native subtitle client work`

Refuted by `APPLE-NATIVE-SUBTITLES-PLAN.md:659` and `STATUS.html:196`, both
**added by this PR**, plus `ANDROID-CLIENT-PARITY.md:7`,
`CLIENTS-REMEDIATION-PLAN.md:3`, and the CHANGELOG. The PR touched this file
(it rewrote §8's heading), so §1's checklist was in scope. Flip it to
`- [x] Android native text subtitles shipped (CLIENTS-REMEDIATION-PLAN §5.4);
physical acceptance remains.`

### M · NIT — smaller documentation items

- **CHANGELOG doesn't name the new metric.** `CHANGELOG.md:437` says "Metrics
  expose the entries temporarily protected from housekeeping"; the series is
  `plurx_cache_protected_entries{reason="active_playback"}`
  (`http/system.rs:1643-1646`). A new Prometheus series is operator-visible
  surface and this CHANGELOG names such surface elsewhere (`:106-107` names
  both `PLURX_HLS_*` vars inline).
- **`REQUIREMENTS.md:92`** says "the **measured** latency, memory, write-rate,
  and disk-growth budgets", but `CLUSTERING-PLAN.md:36-38` says M0 records
  them and M0 has not run. Reword to "…budgets in CLUSTERING-PLAN §1, measured
  in M0." The rest of the REQ-HA-1 rewrite is correct and actually fixes a
  contradiction with `ARCHITECTURE.md:311-312`.
- **`STATUS.html:275`** footer review base is one commit stale — `f9279945` is
  `#78`; the head merges `854d575b` (`#80`).
- **`STATUS.html:191`** Native-clients meter reads 72% against 3-of-5 done
  items; the Performance meter's 83% matches its 5-of-6 exactly. Two
  conventions on one page.
- **`STATUS.html:255`** Media badges is correctly `✓ done` but is the only done
  item left inside "Backlog & standing deferrals". Move it.

---

## 5. Checked and clean

Stated so the absence of a finding is meaningful.

**Code.** Registry key consistency at every construction site — production
directory basenames *are* the recipe hash (`transcode.rs:2698`,
`cachekeep.rs:191`), so no mismatch · RAII deregistration
(`cachekeep.rs:120,145`), leak-free on every early return, on `?`, on panic
(poison recovered at `:71`), and on **task cancellation** — verified by abort
mid-await returning `active_entries()` to 0, and by 50 concurrent tasks
draining the registry fully · client disconnect bounded by the 60s idle reaper
· **TOCTOU closed**: `begin_read` precedes both the row lookup and the
filesystem check; `begin_eviction` is held across `remove_dir_all` *and*
`forget_cache_entry`; the `std::sync::Mutex` is never held across an await
(only the guard object spans awaits); the `states→sessions` /
`sessions→states` ordering is an inversion on paper but not a cycle · **all
three cleanup paths consult the registry** — LRU `:283`, stale `:250`, orphan
published `:494`, orphan staging `:537` · the `files` `ON DELETE CASCADE` →
orphan trigger **is** closed for playback, pinned by a non-vacuous test ·
`storage_class` scoping complete at the store, all three call sites pass a
class, SQL keys `(recipe_hash, node_id, storage_class)` matching the PK,
`CACHE_COLS` reorder and `cache_from_row` indices consistent · eviction
ownership is mutually exclusive by construction and **in-memory on
`TranscodeManager`**, so it self-clears on process death — no persisted claim,
no permanent leak · one shared registry confirmed (`main.rs:353`) · metric is a
gauge, `usize` so non-negative, sourced from live map state so it cannot drift
monotonically, HELP/TYPE emitted · path-escape guard and `remove_dir` of
emptied prefixes unchanged · suites run: `cachekeep::` 21/21,
`store::sqlite::cache` 8/8, `http::tests` 71/71.

**Docs.** `v0.2.7` tile matches `Cargo.toml`; the "release/deploy evidence
pending" hedge is right (no `## [0.2.7]` heading yet, matching RELEASING.md's
convention) · **the prior review's live inconsistency is fixed, not worsened**
— the ship/deploy/TestFlight operator item is restored at `STATUS.html:268`
and names both the `787eaa6` fleet ledger and the un-uploaded Apple client;
`clients/apple/README.md:15` was corrected 30 → 32 to match
`project.yml:18` · all five CHANGELOG clauses verified against the code ·
the Apple M4 rewrite does **not** overclaim — the DV-resolved-2026-08-03
records were already on `main`, and the M1/M2/M3-landed-2026-08-02 framing is
preserved verbatim · parity rows spot-checked against real Swift/Kotlin
(offline downloads, PGS `pgs-v1` default-off, media badges, `native`-flag
rendition eligibility) · OFFLINE-VIEWING-PLAN still says MPEG-TS not fMP4, and
both permit and lease survive untouched · the new PERF-PLAN paragraph sits in
§6.4 (what M3 built) and does not touch §7.3's takeover contract ·
`make validation-lint` passes, the PR adds **no new file** under
`crates/plurxd/src/`, and the `regressions.toml` change is one commit hash
appended to an existing ignore group (verified `24733d6f` is docs-only) · both
regression tests named in OFFLINE-VIEWING-PLAN exist · the
`clients/apple/README.md` edit does not trip the `mobile-version` check ·
README and phase numbering consistent.

**No phantom capabilities.** Every `✓ shipped` claim checked in STATUS.html
resolves to code that exists in the tree.

---

## 6. What must change before merge

1. **A** — `used -= entry.bytes.max(0)` in the `Evicting` arm (or bail out of
   the pass). One line; without it, two overlapping sweeps empty the cache.
2. **B** — treat `Ok(vec![])` as ownership uncertainty, not as "delete
   everything", and add the test.
3. **B4** — the `-start_number` paragraph and the M5 acceptance clause above.
4. **H**, **I**, **J**, **K**, **L** — five documentation corrections, each
   one or two lines, listed with exact replacement text in §4.

**C** (offline reader guard), **D**–**G**, the four smaller plan edits, and
**M** are worth doing but should not hold the merge.

The ownership mechanism itself is the hard part of this PR and it is correct
under adversarial stress. Everything blocking is bookkeeping around it.
