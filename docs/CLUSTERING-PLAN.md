# Clustering transition — from one Cinema node to Phase 4

**Status:** ready to build · **Executes:** Phase 4 from
[ROADMAP.md](ROADMAP.md) and REQ-HA-1–6 from
[REQUIREMENTS.md](REQUIREMENTS.md) · **Written:** 2026-08-06

Companion to [PHASE3-SPIKE.md](PHASE3-SPIKE.md), which records the measured
hiqlite and segment-restart decisions, and
[ARCHITECTURE.md](ARCHITECTURE.md) §2, which owns the system shape. Read those
first, then execute this document milestone by milestone. Re-run each
acceptance check before moving on. If a step appears to require replicating
media files, artwork, or cached segments, stop and flag it: those are
operator-owned or regenerable by design, not consensus state.

## 1. Objective — earn the power-pull demo without regressing one node

Phase 4 is complete when three identical `plurxd` processes form one logical
server, any node accepts API traffic, an acknowledged durable write survives
one node loss, singleton jobs run once, and a viewer resumes within seconds
after the serving node dies.

The first invariant is less glamorous and more important: an existing
single-node install must boot with no new configuration, preserve its current
`plurx.db`, and behave exactly as it did before clustering. One node is the
smallest supported cluster, not a separate edition.

## 2. Starting point — the seams exist, the runtime does not

The current tree already provides these load-bearing pieces:

| Existing seam | Current truth | Phase 4 consequence |
|---|---|---|
| `Arc<dyn Store>` | All durable application state crosses the composed traits in [`store/mod.rs`](../crates/plurx-core/src/store/mod.rs) | A replicated backend can replace `SqliteStore` without changing handlers. |
| `instance.id` | Stable logical-server identity in replicated settings | Keep it cluster-wide. Never reuse it as a node id. |
| `AppState::node_id` | Currently receives `instance.id` in [`main.rs`](../crates/plurxd/src/main.rs) | Split identity before adding a second voter; cache-location rows already assume node-local identity. |
| Transcode recipes | Content-addressed and independent of resume position | Replicate the recipe and latest safe boundary, not segment bytes. |
| Cache locations | Recipe and node location are separate tables | Leave bytes local; replicate only the facts needed for placement. |
| Scheduler | Every daemon can start scans, metadata work, cleanup, and production | Put singleton jobs behind a distributed lease before enabling more than one node. |
| Client recovery | Playback routes already reopen sessions at film positions | Add a node list and bounded retry; do not invent a second player architecture. |

What does **not** exist yet: a hiqlite dependency in `Cargo.toml`, cluster
configuration, a local node identity, membership or join tokens, distributed
job ownership, replicated session recipes, or client node failover.

## 3. Contracts — decide these before copying SQL

### 3.1 Logical identity and node identity are different facts

Add a local identity beside the data directory and return both identities from
store startup:

```rust
pub struct ClusterIdentity {
    pub cluster_id: String, // replicated settings key: instance.id
    pub node_id: u64,       // local file: <data_dir>/node.id
}

pub struct StoreHandle {
    pub store: Arc<dyn Store>,
    pub identity: ClusterIdentity,
    pub cluster: Arc<dyn ClusterCoordinator>,
}

pub async fn open_store(config: &Config) -> Result<StoreHandle, StoreError>;
```

Re-verify the actual signatures against
[`config.rs`](../crates/plurx-core/src/config.rs),
[`store/mod.rs`](../crates/plurx-core/src/store/mod.rs), and
[`main.rs`](../crates/plurxd/src/main.rs) at build time.

`node.id` is generated once as a non-zero `u64`, written atomically with mode
`0600`, and never placed in replicated settings. A copied data directory must
not silently create a duplicate voter: startup refuses a node id already held
by a live member and tells the operator how to regenerate the local file.

### 3.2 Default configuration still means one node

Extend `Config` with a cluster section whose defaults require no operator
input:

```toml
[cluster]
raft_bind = "0.0.0.0:32401"
api_bind = "0.0.0.0:32402"
advertise_host = ""              # derived from the reachable server address
join_token_file = ""             # absent = bootstrap/reopen this node
```

The first implementation may hide this section behind an experimental build
feature while migration is being proven, but the final default is a one-voter
hiqlite cluster. Do not ship a permanent `cluster_enabled` fork: two storage
paths would make single-node the path that receives less exercise.

### 3.3 Coordination is smaller than durable storage

Keep replicated ephemeral state out of the SQL traits. Add one narrow runtime
boundary:

```rust
#[async_trait]
pub trait ClusterCoordinator: Send + Sync + 'static {
    async fn members(&self) -> Result<Vec<ClusterMember>, StoreError>;
    async fn acquire_job_lease(
        &self,
        job: &str,
        ttl: Duration,
    ) -> Result<Option<JobLease>, StoreError>;
    async fn put_session(
        &self,
        id: &str,
        recipe: &ReplicatedSession,
        ttl: Duration,
    ) -> Result<(), StoreError>;
    async fn get_session(
        &self,
        id: &str,
    ) -> Result<Option<ReplicatedSession>, StoreError>;
}
```

The single-node implementation uses the same hiqlite KV/lock primitives as
three nodes. `JobLease` must be fenced: an expired owner cannot publish a scan
or producer result after another node has acquired a newer lease.

### 3.4 Replicated sessions carry recipes, not processes

The first wire/storage shape is deliberately boring:

```rust
pub struct ReplicatedSession {
    pub version: u16,
    pub cluster_id: String,
    pub playback_id: String,
    pub file_id: i64,
    pub recipe_hash: String,
    pub recipe: TranscodeRecipe,
    pub media_origin_ms: i64,
    pub last_published_boundary_ms: i64,
    pub owner_node_id: u64,
    pub generation: u64,
}
```

Every takeover increments `generation`. A dead owner's late writer may not
publish after a higher generation exists. The recipe is re-verified against
[`transcode/mod.rs`](../crates/plurx-core/src/transcode/mod.rs) before build;
do not create a second spelling of encoder, track, subtitle, HDR, or audio
choices.

## 4. Data migration — preserve the only copy before forming a quorum

The current schema is SQLite migration v14; re-verify `MIGRATIONS.len()` in
[`store/sqlite/mod.rs`](../crates/plurx-core/src/store/sqlite/mod.rs) at build
time. Existing installs must move into the replicated state machine without a
destructive in-place gamble.

The migration sequence is:

1. Stop before binding HTTP if `plurx.db` is newer than the binary.
2. Create a byte-for-byte backup under `data/migration/` and fsync it.
3. Start a one-voter hiqlite store with no external members.
4. Import all durable tables in one logical migration, preserving primary
   keys, foreign keys, `PRAGMA user_version`, and `instance.id`.
5. Compare row counts per table, run `foreign_key_check`, and exercise one
   typed read from every `Store` subtrait.
6. Write an atomic migration-complete marker containing the source checksum
   and target schema version.
7. Keep the original backup. Never delete the user's rollback point as part of
   normal startup.

If hiqlite 0.14 can adopt the existing SQLite state-machine file directly,
prove that with a throwaway copy first; do not infer it from both systems using
SQLite. The acceptance criteria below stay the same either way.

## 5. Milestones — each leaves a shippable boundary

### 5.1 M0 — separate identities and pin the dependency

Add `node.id`, `ClusterIdentity`, cluster config parsing, and a feature-gated
hiqlite 0.14 compile/spike test. Leave the production store on `SqliteStore` in
this milestone; its job is to remove the identity collision and confirm the
selected dependency still exposes the APIs measured in Phase 3.

**Acceptance:** `make check`; restart twice and observe the same cluster and
node ids; create two data directories and observe distinct node ids; a copied
`node.id` is detected rather than silently accepted once membership exists.

### 5.2 M1 — implement the complete `Store` contract on one voter

Implement every composed subtrait from `SettingsStore` through
`OfflinePackageStore`. Reuse the existing SQL, migrations, and row mappers;
do not fork business rules between backends. Build one backend-parity suite
that runs the same contract tests against in-memory/file SQLite and a one-voter
hiqlite node.

**Acceptance:** every store contract test passes against both backends;
acknowledged writes survive process restart; `/readyz` fails when the voter
cannot commit, rather than claiming readiness from a local cache.

### 5.3 M2 — migrate an existing single-node install safely

Implement §4, then make one-voter hiqlite the default for a copied production
database. Keep a rollback command that restores the pre-migration backup only
while no cluster member has joined; after replication begins, rollback is a
cluster restore operation, not a file copy.

**Acceptance:** migrate a populated v14 fixture; compare every table count and
selected typed values; boot the web and native API fixtures; restart; verify
users, tokens, libraries, watch state, settings, offline rows, and cache
locations are unchanged.

### 5.4 M3 — membership, join tokens, and one logical server

Add bootstrap-token creation, single-use join, add/remove, health, and member
listing. Tokens contain the cluster id, bootstrap addresses, expiry, and a
random secret; logs redact the token and any URL carrying it. Two voters are
allowed only as a transient reconfiguration state and are reported as
degraded, never as supported HA.

Discovery advertises one logical server id plus reachable node addresses. The
admin surface names leader, voters, health, protocol version, and degraded
quorum without exposing raft secrets.

**Acceptance:** one node grows to three without changing `instance.id`; a
fourth join with an expired/reused token fails; removing a follower preserves
quorum; attempting to present two nodes as healthy HA fails visibly.

### 5.5 M4 — singleton jobs acquire fenced leases

Put full scans, metadata refresh, artwork repair, genre backfill, scheduled
cache production, and offline production behind named distributed leases.
Node-local cleanup remains local. A job checks its fence immediately before
publishing a batch or advancing a durable cursor.

**Acceptance:** start three nodes with one due scan and observe one provider
walk; kill the owner mid-scan and observe one successor resume; restore the old
owner and prove it cannot publish with its stale fence.

### 5.6 M5 — web failover for direct, remux, and transcode

Return the healthy node list with the logical server identity. Implement
bounded web-client retry first: direct range requests retry immediately;
remux/transcode read `ReplicatedSession`, acquire a higher generation, restart
at `last_published_boundary_ms`, and emit `EXT-X-DISCONTINUITY` once.

Do not put mobile clients on the new path until the web corpus proves the
server contract. The server contract, not four independently invented retry
loops, is the product.

**Acceptance:** during direct, copy-remux, and transcode playback, kill the
serving node. The web player keeps its buffered media, reaches a surviving
node, and resumes within the measured budget. Watch progress remains monotone
and only one generation continues publishing.

### 5.7 M6 — operations, upgrades, and failure drills

Add three-node Compose and Helm examples, protocol-version compatibility,
rolling-upgrade rules, quorum-aware readiness, metrics, and repeatable drills
for leader loss, follower loss, netsplit, disk-full, stale owner, and one-node
recovery.

**Acceptance:** `make cluster-check` runs the deterministic local harness;
the README takes a fresh operator from three empty data directories to a
healthy cluster in under 10 minutes; the power-pull demo meets §1.

## 6. Non-goals — guardrails for the first clustering release

1. **Do not replicate media files.** Shared-storage availability is the
   operator's responsibility; plurx mounts media read-only.
2. **Do not replicate artwork, subtitle, overlay, or segment bytes through
   raft.** Replicate recipes and ownership facts; regenerate or use shared
   storage for bytes.
3. **Do not support two-node HA.** A two-voter quorum survives no failure and
   creates a confidence trap.
4. **Do not add PostgreSQL, etcd, Redis, or a broker.** The embedded deployment
   shape is a founding constraint, not an optimization target.
5. **Do not combine clustering with adaptive quality, TV ports, or unrelated
   client polish.** Those changes obscure the failure boundary Phase 4 must
   prove.
6. **Do not call a VIP the failover implementation.** A VIP finds a process;
   replicated state and fenced takeover are what let that process continue the
   film safely.

## 7. Handoff checkpoint — what to do next

Start with M0 on `main`: introduce local node identity and the feature-gated
hiqlite compile/spike without changing the production database. That slice is
small enough to review on its own, exposes any dependency drift before SQL is
copied, and removes the current `instance.id == node_id` assumption that would
corrupt cache ownership as soon as a second node joins.

The standing gate for every milestone is:

```bash
make check                    # repository baseline
make validate-staged          # changed behavior contracts
make cluster-check            # added in M1; one-voter then three-voter harness
```

No milestone is complete because three processes stayed up. It is complete
when the failure it owns is induced and the promised state survives it.
