# Clustering plan review — the right first slice, on top of eight contracts that do not hold

**Status:** review complete · **Reviews:**
[CLUSTERING-PLAN.md](CLUSTERING-PLAN.md) and the rest of commit `5c9023ff`
on `codex/finish-cinema-handoff` · **Verified against:** `origin/main` @
`f9279945` (the branch's merge-base; local `main` is at the same commit) ·
**Written:** 2026-08-06 · **Outcome:** review only; no code was changed

Companion to [ARCHITECTURE.md](ARCHITECTURE.md) §2 (the system shape this
plan executes), [PHASE3-SPIKE.md](PHASE3-SPIKE.md) (what was actually
measured), [PERF-PLAN.md](PERF-PLAN.md) §7.3 (the session-takeover contract
that already went through a review cycle), and
[REQUIREMENTS.md](REQUIREMENTS.md) §7 (REQ-HA-1–6). This document says what
must change before an implementing agent treats the plan as a handoff
contract. It does not replace the plan and it does not authorize M0.

**How it was reviewed.** The branch tip was extracted read-only with
`git archive` and every claim checked against that tree — never the working
tree, never a stale `main`. Four scoped passes (claim verification against
the code, distributed-systems design, the commit's concurrency change,
cross-document truthfulness) produced findings; each headline finding was
then handed to a second pass whose default posture was *refuted*. Three
findings did not survive and are recorded in §11 rather than deleted,
because they are the ones a future reader will re-derive. Findings cite
`file:line` at `5c9023ff`.

**Severity legend:** **BLOCKING** = the plan specifies something the code,
the schema, or the process gate will not do, and building it as written
produces a wrong contract or a failed acceptance check · **SHOULD-FIX** =
real cost, wrong estimate, or a contract that permits unsafe behavior ·
**NIT** = polish. §§7–8 cover the code and status-page changes that ship in
the same commit.

## 1. Verdict — keep the sequencing, rewrite §3 and §4, decompose M1

The strategy is right and should not be relitigated. Splitting node identity
from logical identity before a second voter exists is the correct first
slice, and the reasoning for it is exactly right. Refusing a permanent
`cluster_enabled` fork, because two storage paths make single-node the path
that gets less exercise, is the decision that makes the whole approach worth
doing. The replication-class discipline — recipes and ownership facts through
raft, bytes never — is applied consistently, including the standing
instruction to stop and flag if a step appears to need replicating media.
Refusing two-node HA *and reporting two voters as a visible failure* is
stronger than the doc warning most projects ship. And the closing standard —
"no milestone is complete because three processes stayed up" — is the right
bar; most of what follows is just making individual acceptance checks live
up to it.

The plan is not buildable as written. Eight contracts describe something the
tree does not do, and one of them (B1) will destroy data on an existing
install if M0 ships exactly as specified. The interface in §3.3 cannot
enforce the invariants §3.3 and §3.4 state in prose: the fence is advisory
and `put_session` is a blind last-writer-wins put, so both of the plan's
safety properties are comments rather than mechanisms. §3.4's
`ReplicatedSession` is missing four of the six fields
[PERF-PLAN.md](PERF-PLAN.md) §7.3 already specified after its own review, and
names a type that does not exist. §4 is a procedure, not a migration: it has
no quiesce, no atomic target directory, and no defined action for any of its
own failure modes. M1 is not a shippable slice — it is 114 trait methods over
~7,700 lines of rusqlite, eleven of which are interactive transactions that a
batch API cannot express, with no replica comparison until M3.

Nothing here argues against Phase 4 or against hiqlite. The corrections are
mostly *additions to the same shape*: fence tokens as parameters of the
writes they guard, a CAS on session ownership, the takeover fields that
already exist in another document, a migration with a rollback, and an M1
split into four slices that can each be reviewed in a sitting.

## 2. Blocking findings

### B1 — M0's identity split strands the cache and offline packages, then deletes their bytes (§2, §5.1, §7)

> §7: "introduce local node identity … removes the current
> `instance.id == node_id` assumption that would corrupt cache ownership as
> soon as a second node joins."

The assumption is real — `AppState::node_id` is `instance.id`
(`crates/plurxd/src/main.rs:313`, passed at `:332`; `state.rs:141`) — and
splitting it is right. What the plan never says is what happens to the rows
already keyed by the old value. Today that value is the key of every cache
location and every offline package:

- `transcode_cache_locations` is `PRIMARY KEY (recipe_hash, node_id,
  storage_class)` (`crates/plurx-core/src/store/sqlite/mod.rs:349-359`), and
  `cache_hit` filters `node_id = ?2`
  (`crates/plurx-core/src/store/sqlite/cache.rs:44-51`). A new node id means
  every lookup misses.
- Eviction, the stale sweep, the byte total, and `forget_cache_entry` all
  filter on the same column (`cache.rs:150-155`, `:177-190`, `:231-241`,
  `:255-262`). Rows under the old id are unreachable by LRU, by budget, and
  by cleanup — `cache_bytes` returns 0, so the budget believes the disk is
  empty.
- `offline_packages.node_id` gates both delivery and pickup: a ready package
  404s as `package_unavailable` (`crates/plurxd/src/http/offline.rs:640-645`)
  and a queued one is never claimed again
  (`store/sqlite/offline.rs:382-384`).

Then the bytes go. `sweep_orphan_dirs` builds its keep-list from
`all_cache_rows(node_id)` (`crates/plurxd/src/cachekeep.rs:380`, SQL at
`cache.rs:214-222`), and under a new id that returns `Ok(vec![])` — a
*successful* answer meaning "this node owns nothing". Every directory under
the cache root then falls through to `remove_dir_all`
(`cachekeep.rs:421-435`), the emptied prefixes go (`:437-441`), and the
staging tree goes with them (`:449-476`). The fail-closed guard immediately
above is worth quoting, because it defends against exactly this outcome and
does not catch it:

```rust
// crates/plurxd/src/cachekeep.rs:380-389
let rows = match store.all_cache_rows(node_id).await {
    Ok(rows) => rows,
    Err(error) => {
        // Ownership uncertainty must fail closed. Treating a store error
        // as an empty keep-list would recursively delete valid media.
        tracing::warn!(%error, "cache: could not enumerate filesystem owners; skipping orphan sweep");
        return 0;
    }
};
```

An identity change is not an `Err`. It is an `Ok` that means the same thing.

**Scope, honestly.** Both sweep call sites are behind operator-set intervals
that default to off (`crates/plurxd/src/state.rs:1466-1481`;
`schedule.rs:56-59`), so an install with `transcode_cleanup_mins` and
`cache_produce_mins` both unset only leaks the cache instead of losing it.
Nothing outside the cache root is at risk: `entry_dir` refuses `..` and
absolute paths (`cachekeep.rs:342`) and the walk is exactly two levels deep.
And the transcode cache is regenerable. What is *not* regenerable without
the user noticing is ready offline packages: they are pinned against
ordinary eviction by `NOT EXISTS` clauses (`cache.rs:161-166`, `:263-268`)
that the orphan sweep does not consult, and they are the one thing a viewer
took a deliberate action to create.

**Fix.** M0 owes a data step, and its acceptance check must have a populated
fixture. Either seed `node.id` from the existing `instance.id` on a data
directory that already has one — which needs a stated conversion rule,
because `instance.id` is a UUID string (`store/sqlite/mod.rs:786-796`) and
§3.1 wants a non-zero `u64` — or add a migration that rewrites
`transcode_cache_locations.node_id` and `offline_packages.node_id` in the
same transaction that mints the new id. Then replace §5.1's acceptance
("create two data directories and observe distinct node ids", which passes on
empty directories) with: *a populated single-node fixture keeps every cache
entry, every offline package, and every byte across the identity split, with
`transcode_cleanup_mins` set.*

### B2 — The fence is advisory; no publishing write takes a token (§3.3, §5.5)

> §3.3: "`JobLease` must be fenced: an expired owner cannot publish a scan or
> producer result after another node has acquired a newer lease."
> §5.5: "A job checks its fence immediately before publishing a batch or
> advancing a durable cursor."

Check-then-write is not fencing; it is the TOCTOU this sentence exists to
prevent. The publishing writes cross `Arc<dyn Store>` —
`upsert_item`, `claim_cache_entry`, `complete_cache_entry`
(`crates/plurx-core/src/store/mod.rs:712-783`) — and not one of them takes a
token, so the window between the check and the commit is bounded only by how
long a process can be stopped (GC, an NFS stall, `SIGSTOP`, a starved
runtime). `JobLease` is declared with no fields, no `renew`, no `release`,
and no fence accessor. The TTL's meaning is also unstated: whose clock, and
what happens when two nodes disagree — there is no time-sync requirement
anywhere in the plan, [REQUIREMENTS.md](REQUIREMENTS.md), or
[OPERATIONS.md](OPERATIONS.md).

**Fix.** Make the fence a parameter of the write, validated inside the same
replicated transaction: a replicated `job_leases(job PK, owner_node, fence
INTEGER, expires_at)`, a `fence: u64` argument on every publishing `Store`
method, and an implementation whose first statement is `UPDATE job_leases SET
expires_at = ? WHERE job = ? AND fence = ?` with the whole transaction
aborting on zero rows affected. Then clock skew and TTL choice affect
liveness only, never safety — which is the only version of this that survives
contact with a real pause. Add NTP to §3.2's prerequisites either way.

### B3 — `put_session` is last-writer-wins, so the generation rule has no mechanism (§3.3, §3.4)

> §3.3: `async fn put_session(&self, id: &str, recipe: &ReplicatedSession,
> ttl: Duration) -> Result<(), StoreError>;`
> §3.4: "Every takeover increments `generation`. A dead owner's late writer
> may not publish after a higher generation exists."

Two survivors both notice node A is gone, both read generation 5, both write
6. The second put silently wins and both start ffmpeg against the same
session. Nothing in the signature can reject the loser, and the invariant in
§3.4 is prose with nothing behind it.

**Fix.** Make it a compare-and-set — `put_session(id, expected_generation,
…) -> Result<Accepted, Conflict { current: ReplicatedSession }>` — and build
it on the *same* primitive as B2 (`acquire(resource, ttl) -> Lease { fence }`
where the resource is `job:scan` or `session:<playback_id>`), rather than
shipping two unrelated fencing mechanisms in M4 and M5.

### B4 — `ReplicatedSession` drops the fields the takeover contract already specified, and the playlist mechanics are missing (§3.4, §5.6)

> §5.6: "remux/transcode read `ReplicatedSession`, acquire a higher
> generation, restart at `last_published_boundary_ms`, and emit
> `EXT-X-DISCONTINUITY` once."

[PERF-PLAN.md](PERF-PLAN.md) §7.3 defines this handoff already, after its own
review cycle: `SessionOwner { …, lease_expires_at, produced_playable_through,
fetched_through }` plus the resume boundary ("one segment before
`fetched_through`"), the takeover playlist's first `MEDIA-SEQUENCE`, and a
`DISCONTINUITY-SEQUENCE` that advances by one. §3.4 keeps a boundary and a
generation. Since PERF-PLAN's M4 explicitly "waits for Phase 4's plumbing"
(`docs/PERF-PLAN.md:8-9`), this freezes the plumbing without the fields its
only consumer needs. Concretely, four things break in the walk-through:

1. **Segment numbering collides.** Both HLS argument builders hardcode
   `-start_number 0` (`crates/plurx-core/src/transcode/mod.rs:972`, `:1401`)
   and each session writes into its own directory. The survivor's ffmpeg
   writes `seg00000.ts` for what the client calls segment 400. No field
   carries the takeover `MEDIA-SEQUENCE` and nothing renumbers.
2. **`EXT-X-DISCONTINUITY` alone is not enough.** The served playlist becomes
   a sliding window once retention prunes a prefix (`served_live_playlist`,
   `crates/plurxd/src/transcode.rs:514-560`) — it rewrites `MEDIA-SEQUENCE`
   and drops `PLAYLIST-TYPE:EVENT` but never emits
   `EXT-X-DISCONTINUITY-SEQUENCE`. When the failover discontinuity slides out
   of the window, the client's discontinuity numbering shifts under it. That
   is the case RFC 8216 §6.2.1 requires the tag for.
3. **The copy path breaks harder than the transcode path**, and M5 covers it
   explicitly. Copy sessions are fMP4 with a single
   `#EXT-X-MAP:URI="init.mp4"` (`crates/plurxd/src/copyseg.rs:178`, `:684`);
   a new ffmpeg on node B writes a *different* `init.mp4` under the same URI,
   and every client that cached the old one decodes garbage. Copy sessions
   also snap to the preceding keyframe and report `media_origin_ms` once at
   session start (`crates/plurxd/src/http/hls.rs:277`), so a takeover landing
   on a different keyframe silently invalidates the client's source-time
   mapping — and therefore the watch position it posts.
4. **The client that buffered past the boundary** has no answer here. The
   write cadence for `last_published_boundary_ms` is unstated, so publication
   lags the writer and the survivor regenerates time ranges hls.js has
   already appended. PERF-PLAN's answer — publish exactly one overlap segment
   — is absent.

**Fix.** Adopt PERF-PLAN §7.3's struct verbatim, add the write cadence, and
extend M5's acceptance: the takeover playlist re-declares `EXT-X-MAP` for
fMP4, advances `EXT-X-DISCONTINUITY-SEQUENCE`, never lists a URI whose bytes
no owner durably published, and re-reports `media_origin_ms`. Cross-link the
two documents; today the plan never mentions PERF-PLAN §7.

### B5 — `TranscodeRecipe` does not exist, and the type that does cannot be replicated (§3.4)

> §3.4: `pub recipe: TranscodeRecipe,` … "do not create a second spelling of
> encoder, track, subtitle, HDR, or audio choices."

The type is `Recipe<'a>` (`crates/plurx-core/src/transcode/recipe.rs:81`,
re-exported at `transcode/mod.rs:21`). It *borrows* everything
(`digest: &'a PipelineDigest, file: &'a MediaFile, opts: &'a
TranscodeOptions`) and derives only `Debug, Clone` — no `Serialize`, no
`Deserialize`. It cannot be a field of a replicated value. Producing an
owned, serializable mirror is exactly the second spelling the same paragraph
forbids.

**Fix.** Say which one you are buying. Either make the existing structs owned
and serializable and replicate them (one spelling, a real refactor to
schedule in M0), or replicate the *inputs* — `file_id`, `TranscodeOptions`,
`audio_copied` — and have the survivor rebuild the recipe locally from its
own `PipelineDigest`, which is the only correct move anyway (see S6). Name
the type correctly either way; a handoff document that invents a type name is
how the second spelling gets written by accident.

### B6 — M1 is not a shippable slice (§5.2)

> §5.2: "Implement every composed subtrait from `SettingsStore` through
> `OfflinePackageStore`… Build one backend-parity suite…"

The composition is ten subtraits and **114 async methods**
(`crates/plurx-core/src/store/mod.rs:903-916`; counted per trait: Settings 4,
User 13, ApiKey 6, Library 7, Media 36, Watch 11, Trakt 7, WatchedOutbox 4,
TranscodeCache 10, OfflinePackage 16) over ~7,700 lines in
`store/sqlite/*.rs`. Four things make "reuse the existing SQL" less true than
it sounds:

- Every row mapper takes `&rusqlite::Row<'_>`
  (`store/sqlite/cache.rs:21`), and ten call sites use
  `conn.unchecked_transaction()`. The SQL strings port; the mappers and the
  transaction handling do not.
- Eleven methods are interactive read-branch-write transactions
  (`store/sqlite/offline.rs:379` `claim_next_offline_package`,
  `cache.rs:74`, `watch.rs:259`) that a batch `txn` API cannot express. Each
  is a redesign, not a port.
- The schema is full of statements whose value depends on where and when they
  run: 26 `DEFAULT (unixepoch())` plus 20+ inline `unixepoch()` calls in
  UPDATEs, `conn.last_insert_rowid()` (`media.rs:249`, `outbox.rs:27`), and
  an FTS5 contentless-external virtual table maintained by triggers
  (`store/sqlite/mod.rs:124-138`). Whether any of that is replica-safe
  depends on whether hiqlite replicates statements or row images —
  [PHASE3-SPIKE.md](PHASE3-SPIKE.md) tested none of it (`:17-18`: a STRICT
  migration, one insert through raft, one `query_as_one` read-back).
- The ~53 existing store tests construct `SqliteStore` directly, so "build
  one backend-parity suite" is itself an extraction project.

And the plan never compares two replicas. Membership arrives in M3, after all
the SQL is ported.

**Fix.** Split it, and move a three-voter harness into the *first* hiqlite
slice:

- **M1a — extract the parity harness** against `Arc<dyn Store>` with SQLite
  as the only backend. Independently valuable, mergeable in days, and it is
  the gate M2 should reuse.
- **M1b — `Settings` + `User` + `ApiKey` on three voters**, with a
  replica-equality acceptance (dump every table on all three, assert
  byte-identical). Small surface, exercises writes from any node, and finds
  the determinism bugs before the other 90 methods exist. Decide the
  CAS-transaction pattern here once, instead of discovering it eleven times.
- **M1c — `MediaStore` + FTS5**, with a stated fallback: keep `items_fts`
  node-local and rebuilt from replicated `items`, since a
  contentless-external index is derived structure, not consensus state.
- **M1d — the remainder.**

Rule `unixepoch()` and `last_insert_rowid()` out of the replicated dialect in
M1a (bind timestamps as parameters; use `RETURNING id`), and add an M0 spike
item that proves FTS5, triggers, `RETURNING`, and `txn` under hiqlite 0.14
before any of this is scheduled.

### B7 — §4 is a procedure, not a migration (§4, §5.3)

Every failure mode is undefined:

- **Write traffic during import.** Nothing says the scheduler, scanner, Trakt
  outbox, and HTTP listener are stopped. Step 1's "stop before binding HTTP"
  applies only to the newer-database case — and that guard already exists
  (`store/sqlite/mod.rs:746-752`, refusing `user_version > MIGRATIONS.len()`,
  reached at `main.rs:206`, ~130 lines before the bind at `main.rs:363`).
- **A crash between steps 4 and 6.** The target is not described as created
  fresh, so the next boot re-imports into a half-populated state machine and
  dies on primary-key conflicts.
- **Step 5 failing.** There is no automatic action. §5.3's rollback is an
  operator-invoked command, so the operator gets a half-migrated data
  directory and a daemon that will not start.
- **The check is weak.** Row counts, `foreign_key_check`, and ~10 typed reads
  out of 114 methods pass over silently mangled values — seconds read as
  milliseconds, a NULL/empty-string flip. `items_fts` counts are meaningless;
  it is contentless-external and must be rebuilt, which the existing chain
  already knows how to do (`store/sqlite/mod.rs:229-247`).
- **Schema drift is never resolved.** Step 4 preserves `PRAGMA user_version`
  and step 6 records a "target schema version", implying they can differ,
  but the plan never says whether the replicated backend replays the same
  `MIGRATIONS` array.

**Fix.** Import into `data/hiqlite.incoming`, fsync, rename after the marker,
and delete any `*.incoming` on boot. State that migration runs before any
background task and before HTTP bind; that any failure deletes the target,
leaves `plurx.db` as the active store, and exits non-zero naming the rollback
command. Replace row counts with an ordered content hash per table for the
ten highest-value tables, and reuse M1a's parity harness against the migrated
store as the real acceptance. Declare that the replicated backend replays the
same `MIGRATIONS` array, applied once by the leader.

### B8 — `[cluster]` breaks the rollback promise, and replicated DDL outruns older followers (§3.2, §5.3, §5.7)

`Config` and every section of it are `#[serde(default, deny_unknown_fields)]`
(`crates/plurx-core/src/config.rs:19`, `:26`, `:43`), pinned by a test that
requires a typo'd key to fail the load (`config.rs:136-141`). The moment an
operator writes `[cluster]` into `plurx.toml`, **every earlier binary refuses
to start** — the rollback in §5.3 fails on the config file before it reaches
the database.

The same shape appears one layer down and M6 is too late for it. In a mixed
version cluster a new leader applies migration v15 and raft replicates that
DDL to a v14 follower, which applies it and then runs code that does not know
the new columns. The one protection plurx has — refusing to open a database
newer than the binary — runs at `open`, not on incoming log entries, so it is
bypassed by exactly the mechanism M2 introduces. REQ-OPS-3 requires rolling
upgrades.

**Fix.** Make the cluster section forward-compatible (relax
`deny_unknown_fields` for it, or land an empty tolerated `[cluster]` section
in the release *before* the one that reads it — that is an M0 item, not an
M3 one). Move the schema/protocol compatibility rule from M6 into M1/M3: a
follower must refuse to apply a replicated migration above its own
`MIGRATIONS.len()` and mark itself degraded rather than corrupt itself, and
the cluster must refuse to elect a leader that would migrate while any voter
is older.

## 3. Should-fix

### S1 — REQ-HA coverage is asserted, not traced

The header claims to execute REQ-HA-1–6; no `REQ-` identifier appears
anywhere else in the document. Three gaps:

- **REQ-HA-2** ("All nodes serve API traffic **and streams concurrently**",
  `REQUIREMENTS.md:93`) becomes "any node accepts API traffic" (§1). The word
  *concurrent* appears nowhere in the plan, and no acceptance check has more
  than one node streaming at a time. M5's check kills the single serving
  node.
- **REQ-HA-6** ("Clients receive the node list and fail over client-side;
  … VIP (keepalived) or k8s Service/Ingress — both documented",
  `REQUIREMENTS.md:97`). M5 is web-only and defers mobile with no milestone
  that finishes it; `keepalived`, `k8s`, and `Ingress` appear zero times.
  §5.7's Helm example is a fair partial for the k8s pattern; nothing covers
  the VIP documentation, which [ROADMAP.md](ROADMAP.md) Phase 4 also still
  requires. Phase 4 could pass this plan's §1 with both native clients unable
  to fail over — and they will read a node death as a decode failure and drop
  to their existing rescue ladders (`apple.established-hdr-recovery`,
  `android.compatibility-fallback` in [PLAYBACK.md](PLAYBACK.md)).
- **REQ-HA-5's "one settings surface"** appears nowhere. Related and
  unscoped: `state.direct_plays` is in-memory
  (`crates/plurxd/src/http/watch.rs:58-60`), so the activity page shows only
  the local node's viewers.

Add a traceability table (requirement → milestone → acceptance check) and
either schedule the mobile slice as M7 or amend REQ-HA-6.

### S2 — §1's single-node invariant has no budget, and REQ-HA-1 promises the opposite

> §1: "behave exactly as it did before clustering."
> REQ-HA-1: "Single node runs with clustering dormant (**zero overhead**,
> zero config)."

§3.2 replaces dormancy with a permanent one-voter raft, which is the right
call — but a one-voter raft still appends and fsyncs every write before
applying it, plausibly doubling fsyncs per write on the SD-card and NAS-class
hardware REQ-HA-1 recommends as a third voter. The header calls
[PHASE3-SPIKE.md](PHASE3-SPIKE.md) the record of "the measured hiqlite …
decisions"; that document measured the segment-restart decision thoroughly
and measured *nothing* about hiqlite's cost — its only figure is "~65 MB RAM
for an HA node (per upstream)" (`PHASE3-SPIKE.md:46`).

State a numeric budget in §1 (p95 `put_progress_at`, RSS delta, data-dir
growth), take the baseline in **M0** while backing out is still cheap, make
it a `make cluster-check` regression gate, and amend REQ-HA-1 to "one-voter
raft within the measured budget".

### S3 — The replicated write rate is never costed, and one existing claim about it is false

[ARCHITECTURE.md](ARCHITECTURE.md) §2.2 says watch-state ticks "batch to ~1
write / 10 s / stream server-side". They do not: `progress` does a
`watch_state` read and an unconditional `put_progress_at` on every 5 s beat,
per open player (`crates/plurxd/src/http/watch.rs:27-49`). Under raft that is
a quorum-committed write every 5 s per stream. Separately, §3.4's
`last_published_boundary_ms` is written per segment boundary and
`SEGMENT_SECONDS` is 2 (`transcode/mod.rs:48`) — a raft write every two
seconds per transcode session, never stated or costed. Either implement the
coalescing ARCHITECTURE already claims (and pin it with a test) or delete the
claim; state §3.4's cadence and put both in S2's budget. Say whether
`put_session` is a KV/TTL put or a SQL commit.

### S4 — §3.2 has no security surface at all

[PHASE3-SPIKE.md](PHASE3-SPIKE.md):59-60 already found that hiqlite requires
`enc_keys`, `secret_raft`, and `secret_api` and called it "one-time setup,
surfaced in plurx config and the join-token flow". None of the three appear
in §3.2, and §3.1 applies the right handling — atomic write, mode `0600`,
never in replicated settings — to `node.id` and then not to the actual
secrets. As written, raft and the internal API bind `0.0.0.0` with no stated
authentication and no transport posture. Add the three keys with
[SECURITY.md](SECURITY.md)'s rules, state explicitly whether inter-node
traffic is authenticated-but-cleartext (bind to a trusted network) or TLS,
say that the join token carries them, and add rotation to M6's drills.

### S5 — A partitioned node keeps serving, and M5's acceptance cannot see it

The plan fences writes (imperfectly — B2) and never fences *serving*.
Playlists and segments authenticate by capability with no header, so any
client on the losing side of a partition keeps watching from a node that has
lost quorum, with a live ffmpeg on shared storage and silently failing
progress posts. State the self-fencing rule — on quorum loss or lease expiry
a node stops serving playlists and segments (503 plus the node list), kills
its session children, and fails `/readyz` — and make it a partition drill.
Related: §5.5's "restore the old owner and prove it cannot publish with its
stale fence" tests a *restarted* process, which has lost its in-memory fence
and will legitimately re-acquire, so it can pass without ever exercising the
real case. Rewrite it as `SIGSTOP` past the TTL, take the lease elsewhere,
`SIGCONT`, and assert the store rejects the resumed process's writes.

### S6 — `recipe_hash` is a node-local address, not a content address

The hash folds in `ffmpeg_build`, `encoder`, and `pipeline`
(`crates/plurx-core/src/transcode/recipe.rs:53-64`), deliberately: "a
QSV-produced entry is not a thing a software node may serve" (`:43-48`). So
`ReplicatedSession.recipe_hash` is not a cluster-wide key, and §2's "replicate
only the facts needed for placement" holds only for a homogeneous cluster
running byte-identical ffmpeg builds. This does *not* break takeover — the
output contract fields in the digest are literal constants
(`recipe.rs:68-72`), every encoder arm is 8-bit H.264 (`transcode/encoder.rs`,
pinned by `every_encoder_is_h264_because_the_badge_hard_codes_sdr`), the
advertised `CODECS` string is fixed, and PHASE3-SPIKE explicitly does not
require byte-determinism (`:25-27`). But it does mean cross-node cache
sharing will never happen in a mixed fleet, and it should be written down as
a node-local address (or the field dropped) rather than presented as content
addressing. Note the same mechanism already costs hit-rate on one node:
`serve_cached` overwrites `digest.encoder` with the encoder chosen for *this*
request (`transcode.rs:2342-2343`), so an entry produced on the GPU rung
misses when the title is later admitted on the software rung.

### S7 — Node-local bytes behind a singleton lease become permanent 404s

§5.5 puts **artwork repair** behind a distributed lease. `artwork_dir` is
node-local (`main.rs:127`) and `items.poster_path` is a bare filename served
from it (`crates/plurxd/src/http/images.rs:26-40`), so a singleton lease
guarantees one node materializes the file and the other two never do: the
replicated row says `poster.jpg` and two of three nodes 404 it, forever.
"Regenerable" is also only half true — local `poster.jpg` adoption re-reads
shared storage, but TMDB-fetched art is regenerable only while the provider
is reachable, against REQ-META-4's promise that a scanned library keeps
working offline indefinitely. Offline packages have the same shape:
`offline_packages.node_id` means a package the UI calls "ready" exists on
exactly one node, and no milestone addresses cross-node fetch.

Split M4's list into *singleton* jobs (scan, metadata refresh, genre backfill
— writes to replicated state) and *per-node materialization* jobs (artwork
repair — writes node-local bytes from replicated facts), exempt the latter
from leases, and state the fallback for a node-local byte referenced by a
replicated row: proxy to a holder, or 404 and enqueue local repair. That
decision is the same one M5 needs for segments; make it once.

### S8 — Three nodes collide on the discovery surface

> §5.4: "Discovery advertises one logical server id plus reachable node
> addresses."

One sentence, and the current implementation contradicts it in a way that
needs code. `mdns_service_info` derives the `.local` hostname from the
instance id and uses the human server name as the DNS-SD instance name
(`crates/plurxd/src/main.rs:686-716`), so three nodes sharing `instance.id`
and `server.name` register the same hostname and the same service instance;
conflict resolution picks one and native clients resolve to an arbitrary node
with no failover. The GDM responder has the same problem, and `server.name`
lives in the per-node config file (`config.rs:28`) where three nodes can
simply disagree — against REQ-HA-5's "one server name". Make M3 concrete:
hostname from `node_id`, logical id and node list in TXT, one advertisement
per node under one service instance, `server.name` moved into replicated
settings with the config file as a bootstrap seed only.

### S9 — Process gates the plan will trip

- `validation/points.toml:249` enumerates the daemon's files literally
  (`crates/plurxd/src/{cachekeep,logbuf,main,schedule,state,storeprobe}.rs`)
  and `crates/**` is audited (`points.toml:7-21`), so a new
  `crates/plurxd/src/cluster.rs` fails `make validation-lint` — inside
  `make check` — until the catalog is updated in the same commit.
- `database-upgrades` runs in the `ci`/`full`/`nightly` profiles, not
  `commit` (`points.toml:159`), so `make validate-staged` will not run the
  migration contract locally for M2's work.
- `tests/operations/test_contracts.py:20-24` asserts the compose port lines
  verbatim, so adding 32401/32402 to `deploy/docker-compose.yml` is a
  governed change that fails `make operations-check` until that test moves.
- §7's standing gate lists `make cluster-check` for every milestone while
  §5.2 introduces it. Say "M0 uses `make check` + `make validate-staged`;
  `cluster-check` joins from M1a".

### S10 — Smaller corrections

- **"Cinema node" in the title.** Cinema is the client app name; the server,
  the repo, and the process are `plurx`/`plurxd` ([PUBLISHING.md](PUBLISHING.md)).
- **§5.6's "within the measured budget"** names no budget. §1 says "within
  seconds"; every other acceptance in the document is a command or an
  observable fact.
- **§3.2's ports 32401/32402** arrive with no rationale and no note on their
  relationship to 32400 (`config.rs:14`) — one clause each, in a document
  that explains everything else.
- **§3.1's `0600` and "non-zero `u64`"** are asserted without their reasons;
  the non-zero part is a raft convention worth naming.
- **In-flight scans.** M4's "kill the owner mid-scan and observe one
  successor resume" has nothing to resume from: scan state is
  `Mutex<HashMap<i64, ScanStatus>>` in memory (`state.rs:359-364`). Either
  say resume means restart-from-zero or add a replicated cursor.
- **Filesystem split-brain.** The scanner refuses to reconcile after a
  partial walk, which covers a *missing* root — not a mount that is present
  but stale, where one node's leased scan prunes the library cluster-wide.
  A media-root fingerprint in replicated state plus a bound on how much one
  scan may prune would close it.
- **Missing from M6, each needing one named deliverable:** cluster backup and
  restore (§5.3 promises "a cluster restore operation" that is specified
  nowhere), and replication observability — name the metrics (leader
  identity, commit index per voter, applied-vs-committed lag, lease
  acquisitions and rejections, fence rejections, takeover count and
  duration).

## 4. What the plan gets right

Keep all of this through the revision:

1. **Identity split first (M0), before a second voter.** Small, reviewable,
   and it defuses a real corruption path. The best-chosen first slice in the
   document — it just needs B1's data step.
2. **No permanent `cluster_enabled` fork.** "Two storage paths would make
   single-node the path that receives less exercise" is the reasoning that
   justifies the whole approach.
3. **The replication-class discipline** (§6.1, §6.2, and the standing
   instruction in the preamble to stop and flag if a step seems to need
   replicating media bytes).
4. **Refusing two-node HA and reporting two voters as degraded** (§5.4) —
   and making "attempting to present two nodes as healthy HA fails visibly"
   an acceptance check rather than a warning.
5. **`/readyz` must fail when the voter cannot commit** (§5.2). Today it is
   `SELECT 1` on a local connection (`http/mod.rs:280-288`;
   `store/sqlite/mod.rs:842-848`) — exactly the "readiness from a local
   cache" the plan says must not happen.
6. **§4 step 1 asks for something that already exists and works** —
   `migrate()` refuses a database newer than the binary. The gap is only
   that the guard must be re-established for the replicated path and extended
   to incoming raft DDL (B8).
7. **Web first, mobile second** (§5.6): "The server contract, not four
   independently invented retry loops, is the product." Right sequencing —
   it just needs the mobile milestone written down (S1).
8. **The closing standard.** "No milestone is complete because three
   processes stayed up. It is complete when the failure it owns is induced
   and the promised state survives it."

## 5. The cache-reader change in the same commit

The mechanism is sound and merges: claim ordering is right, the eviction
guard covers both halves of `forget`, every early return between `begin_read`
(`transcode.rs:2345`) and the `Session` construction (`:2385`) releases it, no
lock is held across an await, and the guard's lifetime is genuinely tied to
the session (`stop_session` at `transcode.rs:4275`, the 60 s idle reaper at
`:4601`, `reap_superseded` at `:3334`; no Arc cycle). The new test has teeth:
reverting any one of `cachekeep.rs:84-86`, `cachekeep.rs:233-236`, or
`transcode.rs:2385` makes it fail.

What is over-claimed is the scope.

### C1 — BLOCKING · The orphan pass still deletes directories under live readers

`sweep_orphan_dirs` takes no registry (`cachekeep.rs:370`), and its keep-list
is `all_cache_rows` alone (`:380`). Any path that removes a location row
out-of-band turns a served directory into an "orphan". Two reachable
triggers:

1. **The `files` cascade.** `transcode_cache_recipes.file_id REFERENCES
   files(id) ON DELETE CASCADE` (`store/sqlite/mod.rs:343`), locations
   cascade off recipes (`:350`), foreign keys are on (`:686`), and scan
   reconciliation deletes vanished files (`scan/mod.rs:391`) — pinned by an
   existing test, `deleting_a_file_deletes_what_was_cached_from_it`
   (`store/sqlite/cache.rs:612`). Move the source file while someone is 20
   minutes into the cached copy, let a scan finish, let one cleanup tick run:
   the directory goes. The in-flight segment survives on its open handle
   (`transcode.rs:4361-4367`), the next one 404s, and the playlist 404s with
   it — segment and playlist serving touch the filesystem only, so nothing
   fails earlier and masks it.
2. **The offline producer's error path** calls `forget_cache_entry`
   (`transcode.rs:2731`) with no eviction guard, reachable when
   `claim_cache_entry` reported the entry already complete but a leftover
   staging directory sent it down the resume branch (`:2717`).

The CHANGELOG and STATUS.html are careful to scope the claim to the *budget*
sweep, so neither is false — but [OFFLINE-VIEWING-PLAN.md](OFFLINE-VIEWING-PLAN.md)
§7.3 now reads "The playback-cache reader/eviction race was resolved", which
is broader than what shipped. Pass `readers` into `sweep_orphan_dirs` (skip
and count any directory whose recipe is claimed), correct §7.3's wording, and
add the regression: reader active, row deleted out from under it, sweep, the
directory survives. That test fails today, which is the point.

### C2 — SHOULD-FIX · The registry ignores `storage_class`, and clustering is what will add one

The reader registry keys on `recipe_hash` alone (`cachekeep.rs:65`, `:82`)
while `forget_cache_entry` deletes **all** locations for `(recipe_hash,
node_id)` regardless of class (`store/sqlite/cache.rs:231-249`), and step 1's
stale-claim `forget` takes no guard (`cachekeep.rs:208`). This is unreachable
today only because the sole INSERT hardcodes `'local'` (`cache.rs:90`). The
`'shared'` value exists in the CHECK constraint and the primary key
(`store/sqlite/mod.rs:353`, `:359`) for the shared-storage case this very
plan and PERF-PLAN's `cache.shared_dir` will introduce — at which point a
stale `'shared'` claim swept in step 1 deletes the complete `'local'` row of
an entry a reader is serving. Key the registry on `(recipe_hash,
storage_class)` or scope `forget_cache_entry` to a class before that lands.

### C3 — SHOULD-FIX · `protected` is invisible, and the budget is now soft

The change deliberately converts a hard ceiling into a soft one — correct
trade — but `Swept` is discarded at both call sites (`state.rs:1191-1199`,
`:1294-1301`), `protected` is missing from the `"cache: swept"` info line
(`cachekeep.rs:270-279`), and there is no gauge beside the existing
`plurx_cache_pinned_bytes` (`http/system.rs:1640`). The only signal is a warn
that requires `used > ceiling`, and there is no free-space check anywhere in
the daemon. Log `protected` unconditionally, add
`plurx_cache_protected_entries`, and note the exception in
[OPERATIONS.md](OPERATIONS.md) and [PERF-PLAN.md](PERF-PLAN.md) §6.4 — that
section exists precisely for "where the code says something the plan did
not".

### C4 — SHOULD-FIX · Three regressions should land with the fix

Each is cheap and each covers a line that can be reverted with the suite
still green: deleting `cachekeep.rs:69` (the `Evicting => return None` arm)
leaves the test passing — nothing asserts a lookup during eviction becomes an
ordinary miss; making `CacheReadGuard::drop` always remove
(`cachekeep.rs:99-103`) also leaves it passing — only one reader is ever
modelled; and C1's reader-across-the-orphan-pass case is untested.

### C5 — NIT

- Two sweeps can overlap (`CleanupTranscode` inline on the tick while
  `produce_pass` sweeps from a task spawned earlier; `self.producing`
  serializes only `produce_pass` against itself), so one sweep can count the
  other's in-progress eviction as `protected` and log "active playback keeps
  cache over budget" with no viewer present. Harmless, misleading.
- `debug_assert!(false, …)` in `CacheReadGuard::drop` (`cachekeep.rs:105`)
  runs while the mutex is held and is unreachable by construction — but it
  compiles out in release (`Cargo.toml:77-79` sets no `debug-assertions`),
  where the same branch falls through with `remove = false` and leaves a
  `Readers(n)` entry pinned forever with no log. Prefer
  `tracing::error!` plus a counter, computed outside the lock; make
  `CacheEvictionGuard::drop` conditional rather than removing first and
  asserting after; and use `unwrap_or_else(|e| e.into_inner())` instead of
  `.expect()`, since this data has no unwind-broken invariant and a panic
  under that lock would kill `schedule_loop` for the process lifetime
  (`state.rs:1084-1089`, spawned with the handle dropped, nothing restarts
  it).
- The `#[cfg(test)] async fn sweep` shim (`cachekeep.rs:286-289`) is honest
  about production, but it means all ten pre-existing `cachekeep` tests now
  run against an empty registry — the interaction with steps 1 and 3, which
  is where C1 and C2 live, is structurally unexercised.
- `ActiveCacheReaders::default()` is publicly constructible, so any module
  can call `sweep_with_readers` with a fresh registry and get the old
  behavior. Taking `&TranscodeManager` would make the registry
  non-substitutable.

## 6. STATUS.html — three corrections before this is pinned

The rewrite is mostly a *correction*: the perf rows were stale, and M2/M3
being done is supported by [PERF-PLAN.md](PERF-PLAN.md):3-5 (4.89× on nynuc)
and :8-9. Three things need fixing before the page is trustworthy.

**The hero tiles and dateline were not updated with the sections below them,
so the page now contradicts itself.** `STATUS.html:102` still calls M2 "🚧
GPU tone-map — the milestone that fixes 4K" against `:181`'s ✓ done and
`:267`'s "✓ measured"; `:101` still says "Apple build 6 built, not yet
uploaded" while `clients/apple/project.yml:17-18` is at 0.2.7 / build 32;
`:99` still pins v0.2.0 "released 2026-07-30"; and `:95` is dated 2026-08-05
under a footer that says the page was refreshed 2026-08-06 (`:274`).

**The ship/deploy/TestFlight operator item was deleted without evidence that
it happened.** The specific target is obsolete — build 6 no longer exists —
but the substance is not: `clients/apple/README.md:23` still says the app
"has not been uploaded to TestFlight",
[APPLE-NATIVE-SUBTITLES-PLAN.md](APPLE-NATIVE-SUBTITLES-PLAN.md):659-661
still carries the unchecked box, and the only deployment ledger in the tree
([APPLE-NATIVE-SUBTITLES-HANDOFF.md](APPLE-NATIVE-SUBTITLES-HANDOFF.md):339-341,
[APPLE-CLIENT-PARITY.md](APPLE-CLIENT-PARITY.md):18-20) still has every node
on server build `787eaa6`. Nothing anywhere records a later deploy or any
upload. The new checklist has no publish-or-deploy item at all. Restore one,
updated to build 32 — this is the item most likely to be the actual reason a
fix is not on a device.

**The row's evidence documents were not updated with it.** The Apple row is
defensible as written (see §9), but [APPLE-CLIENT-PARITY.md](APPLE-CLIENT-PARITY.md)
is stamped 2026-08-02 and still lists offline as "Missing" (`:68`) and the
fleet as behind (`:19-21`), and
[APPLE-NATIVE-SUBTITLES-PLAN.md](APPLE-NATIVE-SUBTITLES-PLAN.md):577-578
still reads "M4 … open. Nothing in M4 has been attempted" after the DV
acceptance item was deleted as done. House rule is that docs change with the
claim; the status page moved and its sources did not.

Two things that are **fine** and worth recording so they are not re-audited:
the CHANGELOG entry is correctly placed under `## [Unreleased] → ### Fixed`
(`CHANGELOG.md:9`, `:425-435`), and no version bump is owed —
[RELEASING.md](RELEASING.md):66-95 makes the workspace bump a release action,
and `validation/mobile_versions.py:19-36` only triggers on workspace-version
or `clients/{apple,android}` release-path changes, neither of which this
commit touches.

## 7. Decisions for Paul

1. **`node.id` on an existing install: seed from `instance.id`, or mint fresh
   and migrate the rows?** (B1.) Seeding is one line and zero risk but needs
   a UUID→u64 rule; minting is cleaner and needs a real migration plus a
   populated-fixture acceptance.
2. **Does M1 get decomposed?** (B6.) Four slices with a three-voter harness
   in the second, or one long milestone with no replica comparison until M3.
3. **`ReplicatedSession`: adopt PERF-PLAN §7.3's `SessionOwner` verbatim?**
   (B4, B5.) If yes, PERF-PLAN §7.3 becomes the contract and CLUSTERING-PLAN
   §3.4 links to it instead of restating it.
4. **Does M0 land an empty tolerated `[cluster]` section now**, so the first
   release that reads it is not the first release that can never be rolled
   back? (B8.)
5. **Mobile failover: an M7 in this plan, or an amendment to REQ-HA-6?** (S1.)
6. **Artwork on three nodes: per-node materialization, a shared directory, or
   proxy-to-holder?** (S7.) The same answer should cover segments in M5.
7. **Is the single-node overhead budget a gate or a note?** (S2.) A gate
   means measuring in M0, before the SQL port.

## 8. What this review could not verify

- **hiqlite 0.14's actual semantics.** Whether it replicates statements or
  row images, whether `txn` can express a read-branch-write, and how FTS5
  triggers behave under it decide B6's shape. The sandbox has no network and
  [PHASE3-SPIKE.md](PHASE3-SPIKE.md) tested none of it. That spike is M0
  work.
- **Anything requiring a build or a run.** No `cargo`, no ffmpeg fixtures, no
  three-process harness; every code finding here is traced from source, and
  the concurrency reasoning in §5 is argued, not executed.
- **Fleet and device state.** Whether the nodes were deployed after
  2026-08-02 and whether any build reached TestFlight are questions only the
  repo's own records answer, and those records say no (§6).

## 9. Findings that did not survive verification

Recorded because they are plausible enough that a future reader will
re-derive them:

- **"The Apple row's PGS claim is false."** It is not. The row is badged
  `✓ source + DV device`, not plain done; the client renderer exists
  (`clients/apple/Sources/PGSOverlay.swift`) and
  [PGS_OVERLAY_PLAN.md](PGS_OVERLAY_PLAN.md):1090-1093 calls the client
  implementation complete behind the server's default-off gate. The parity
  doc's "PGS … retains burn-in" line describes gate-off behavior in a
  document that predates the feature. The row would read better with the
  "staged, default-off" qualifier the other docs carry, but it is not an
  overstatement.
- **"Step 1's stale-claim sweep is safe only by accident."** It is stated
  (`store/sqlite/cache.rs:41-43`) and pinned by two tests (`cache.rs:320`,
  `:460-496`), and nothing resets `complete` to 0. The real hazard is the
  `storage_class` one (C2).
- **"The `debug_assert!` in `Drop` is a crash waiting to happen."** It is
  unreachable by construction and compiled out in release. What survives is
  the release-mode silent fallthrough (C5).
