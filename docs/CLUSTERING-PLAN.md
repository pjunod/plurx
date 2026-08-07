# Clustering transition — from one plurxd node to Phase 4

**Status:** revised after [CLUSTERING-PLAN-REVIEW.md](CLUSTERING-PLAN-REVIEW.md)
· **Executes:** Phase 4 from [ROADMAP.md](ROADMAP.md) and REQ-HA-1–6 from
[REQUIREMENTS.md](REQUIREMENTS.md) · **Written:** 2026-08-06 · **Revised:**
2026-08-06

Companion to [PHASE3-SPIKE.md](PHASE3-SPIKE.md), which chose hiqlite and
proved restart-at-boundary media behavior; [PERF-PLAN.md](PERF-PLAN.md) §7,
which owns the transcode-takeover protocol; and
[ARCHITECTURE.md](ARCHITECTURE.md) §2, which owns the system shape. Execute
this document milestone by milestone. If a step appears to require replicating
media, artwork, subtitle, or segment bytes through raft, stop and flag it:
replicate ownership facts and recipes, then proxy, share, or regenerate bytes.

## 1. Objective — earn the power-pull demo without hiding one-node cost

Phase 4 is complete when three `plurxd` processes form one logical server,
serve API traffic and streams concurrently, preserve every acknowledged
durable write through one node loss, run singleton work once, and resume a
viewer within 10 seconds after the serving node dies. Web, Apple, and Android
must all consume the same node-list and retry contract before REQ-HA-6 is done.

One node remains the default deployment and uses the same one-voter replicated
store as three nodes. Zero configuration means no required cluster stanza or
manual secret generation; it does not mean zero physical overhead. The
single-node gate is:

| Measure | Budget against the SQLite baseline | Why this is the gate |
|---|---:|---|
| p95 `put_progress_at` latency | ≤25 ms and ≤2× baseline | A five-second player beat must not become visible UI latency. |
| Idle RSS after warm-up | ≤100 MiB additional | The recommended third voter includes Pi/NAS-class hardware. |
| 10,000 progress writes | ≤2× logical payload + 64 MiB fixed growth | A raft log that grows without compaction will fill small data volumes. |
| Watch-state commit rate | ≤1 commit / 10 s / active stream | [ARCHITECTURE.md](ARCHITECTURE.md) §2.2 already promises coalescing; clustering must make it true. |

M0 records the SQLite baseline and hiqlite result on the same machine. A budget
change requires measured evidence in [PHASE3-SPIKE.md](PHASE3-SPIKE.md), not a
quietly wider threshold.

## 2. Starting point — the seams exist, eight assumptions do not

| Existing seam | Current truth | Phase 4 consequence |
|---|---|---|
| `Arc<dyn Store>` | All durable application state crosses the composed traits in [`store/mod.rs`](../crates/plurx-core/src/store/mod.rs). | Keep handlers stable, but port 114 methods in reviewable groups rather than one backend rewrite. |
| `instance.id` | Stable logical-server UUID in settings; also passed as `AppState::node_id`. | Split the concepts without changing the first node's existing cache/offline ownership key. |
| Cache/offline `node_id` | Existing rows are keyed by the current `instance.id`. | An existing install seeds local `node.id` from that exact string; changing it would strand rows and let orphan cleanup delete bytes. |
| `Recipe<'a>` | Borrowed, node-specific cache-key input; its hash includes ffmpeg build, encoder, and pipeline. | The hash is a node-local cache address, not a cluster recipe. Replicate owned request inputs and rebuild against the survivor's pipeline. |
| Session takeover | [PERF-PLAN.md](PERF-PLAN.md) §7.3 defines owner epoch, lease, playable/fetched frontiers, overlap, and playlist sequence. | Adopt that contract; do not invent a smaller `ReplicatedSession`. |
| Scheduler | Every daemon starts scans, metadata, cleanup, and production. | Fence replicated publishing work; keep byte materialization per node. |
| Artwork/cache bytes | Rows may replicate while bytes are local. | Missing bytes proxy to a holder and enqueue local repair; a replicated filename is not proof of a local file. |
| Discovery | mDNS/GDM derive hostname and service name from shared identity/name. | Advertise each node under a node-specific hostname with one logical id and the full node list. |

What does not exist yet: a hiqlite dependency, cluster configuration, local
node identity, membership/join flow, transactional fencing, replicated session
ownership, quorum self-fencing, or client failover.

## 3. Contracts — safety lives in signatures and transactions

### 3.1 Logical identity, application node identity, and raft id are separate

```rust
pub struct ClusterIdentity {
    pub cluster_id: String, // replicated settings key: instance.id
    pub node_id: String,    // local UUID file: <data_dir>/node.id
    pub raft_id: u64,       // hiqlite membership id; non-zero by raft convention
}

pub struct StoreHandle {
    pub store: Arc<dyn Store>,
    pub identity: ClusterIdentity,
    pub cluster: Arc<dyn ClusterCoordinator>,
}

pub async fn open_store(config: &Config) -> Result<StoreHandle, StoreError>;
```

Re-verify these signatures against [`config.rs`](../crates/plurx-core/src/config.rs),
[`store/mod.rs`](../crates/plurx-core/src/store/mod.rs), and
[`main.rs`](../crates/plurxd/src/main.rs) when M0 begins.

`node.id` is a UUID string because existing ownership columns already use that
shape. On an existing data directory, create it atomically from the current
`instance.id`; the concepts become separate while every cache location and
offline package keeps its key. On a fresh or joining node, generate a new UUID.
Write with mode `0600` because copied local identity can impersonate a member.
Never place it in replicated settings. `raft_id` is allocated by membership
and is not used as a filesystem-ownership key.

A copied data directory must not create a duplicate member. Startup refuses a
`node.id` already held by a live member and prints the explicit regenerate
command. It never silently changes ids on a directory containing cache or
offline rows.

### 3.2 Configuration is rollback-compatible and secrets stay local

M0 adds a tolerated, default-empty `ClusterConfig` before any release requires
its keys. Older M0 binaries therefore accept later `[cluster]` files during a
rollback. `Config` continues rejecting typos in known top-level and server
settings; only the cluster section is designed for versioned extension.
`ClusterConfig` is `#[serde(default)]` without `deny_unknown_fields`;
`Config`, `ServerConfig`, and `StorageConfig` keep their strict unknown-field
checks. M0 pins both sides: `[cluster]\nsome_future_key = 1` loads, while the
existing `[server]\nnmae = "typo"` rejection remains unchanged.

```toml
[cluster]
raft_bind = "0.0.0.0:32401"      # raft replication, adjacent to public 32400
api_bind = "0.0.0.0:32402"       # authenticated node-to-node requests
advertise_host = ""              # empty derives this node's reachable address
join_token_file = ""             # absent bootstraps/reopens one voter
trusted_network = ""             # required if transport is not TLS
```

`enc_keys`, `secret_raft`, and `secret_api` live in mode-`0600` files beside
`node.id`, generated atomically. They are never logged, exposed in settings, or
stored in replicated SQL. A single-use join token contains cluster id,
bootstrap addresses, expiry, the new raft id, and an encrypted envelope for
those secrets; the one-time token secret wraps the envelope and is invalidated
after membership commits.

M0 must verify hiqlite 0.14's transport support. If it cannot provide TLS, the
first clustering release is explicit: authenticated-but-cleartext inter-node
traffic is allowed only on `trusted_network`, startup rejects a public bind,
and [SECURITY.md](SECURITY.md) documents the exposure. NTP is a deployment
prerequisite for useful lease expiry and logs, although fence safety never
depends on synchronized clocks.

### 3.3 One migration dialect, with protocol guards before membership

The replicated backend replays the same `MIGRATIONS` array as SQLite, once by
the leader. Replicated SQL binds timestamps as parameters; it does not use
`unixepoch()` inside consensus statements. Generated ids use `RETURNING`, not
connection-local `last_insert_rowid()`. FTS5 is derived node-local state rebuilt
from replicated `items` unless M0 proves hiqlite replicates its virtual table
and triggers deterministically.

Every member advertises protocol version and `MIGRATIONS.len()`. A voter
refuses an incoming migration above its binary's schema ceiling, marks itself
degraded, and cannot become leader. A leader refuses to migrate while any
voter is below the required protocol/schema version. These rules land with the
first three-voter store slice, not in the final operations milestone.

### 3.4 One lease primitive fences jobs and sessions

```rust
pub struct Lease {
    pub resource: String,
    pub owner_node_id: String,
    pub fence: u64,
    pub expires_at_unix_ms: i64,
}

#[async_trait]
pub trait ClusterCoordinator: Send + Sync + 'static {
    async fn members(&self) -> Result<Vec<ClusterMember>, StoreError>;
    async fn acquire(
        &self,
        resource: &str,
        ttl: Duration,
    ) -> Result<Option<Lease>, StoreError>;
    async fn renew(&self, lease: &Lease, ttl: Duration) -> Result<Lease, StoreError>;
    async fn release(&self, lease: &Lease) -> Result<(), StoreError>;
    async fn claim_session(
        &self,
        session_id: &str,
        expected_owner_epoch: u64,
        next: &SessionOwner,
        ttl: Duration,
    ) -> Result<Lease, SessionConflict>;
    async fn get_session(&self, session_id: &str)
        -> Result<Option<SessionOwner>, StoreError>;
}
```

`job_leases(resource PRIMARY KEY, owner_node, fence, expires_at)` is replicated.
Every acquisition increments `fence`. Publishing methods such as scan-batch
upsert, cursor advance, cache completion, and queue completion take `&Lease`.
The same replicated transaction first validates `resource`, `owner_node`, and
`fence`; zero matching rows aborts the entire write. A pre-write check is not a
fence. Clock skew may delay takeover, but it cannot let an old owner commit.

`claim_session` is compare-and-set. Two survivors reading epoch 5 cannot both
write epoch 6: one commits the higher lease fence and the other receives
`SessionConflict { current }`. Session publication carries that same fence,
so an old ffmpeg process cannot publish a playlist, segment location, or cache
location after takeover.

For a `session:<playback_id>` resource, the lease `fence` **is**
`SessionOwner.owner_epoch`. There is one monotone counter per session,
allocated by `claim_session`; `expected_owner_epoch` is compared with it in
the same replicated transaction.

In M4 the publishing surface is explicit: `upsert_item`, `claim_cache_entry`,
`complete_cache_entry`, `forget_cache_entry`, `claim_next_offline_package`,
`complete_offline_package`, and the scan-cursor writers gain a leading
`lease: &Lease` parameter. A publishing method without that parameter is a
review failure, because a check performed outside the write transaction is
not a fence.

### 3.5 Session takeover adopts PERF-PLAN §7.3, including playlist state

The canonical contract is [PERF-PLAN.md](PERF-PLAN.md) §7.3. Clustering owns
its storage and CAS, not a second spelling:

```rust
pub struct SessionOwner {
    pub session_id: String,
    pub owner_node_id: String,
    pub owner_epoch: u64,
    pub lease_expires_at_unix_ms: i64,
    pub recipe: SessionRecipeInputs,
    pub produced_playable_through_ms: i64,
    pub fetched_through_ms: i64,
    pub media_origin_ms: i64,
    pub media_sequence: u64,
    pub discontinuity_sequence: u64,
}

pub struct SessionRecipeInputs {
    pub file_id: i64,
    pub target_height: i64,
    pub video_bitrate_kbps: u32,
    pub audio_channels: u32,
    pub audio_bitrate_kbps: u32,
    pub audio_index: Option<i64>,
    pub subtitle_burn: Option<SubtitleBurnInput>,
    pub tone_map_required: bool,
    pub audio_copied: bool,
}
```

This is an owned wire type rebuilt from `TranscodeOptions`; it is not the
borrowed `Recipe<'a>`. The survivor loads `file_id`, rebuilds its own
`PipelineDigest`, chooses a locally valid encoder/pipeline, and computes its
own node-local `recipe_hash`. Mixed ffmpeg/encoder fleets therefore retain
takeover correctness but do not share cache entries unless their local digests
match.

These field names are also the canonical spelling in
[PERF-PLAN.md](PERF-PLAN.md) §7.3. `tone_map_required = false` means the
session started with `ToneMap::None`; `true` asks the survivor to select a
locally valid `Zscale` or `Libplacebo` pipeline. The initial
`TranscodeOptions::start_seconds` is deliberately not replicated: takeover
resumes from the published and fetched frontiers, not from the session's
original seek point.

The owner renews its six-second lease and publishes both frontiers every two
seconds. Watch progress is coalesced separately to at most one durable commit
per 10 seconds per stream. On takeover, restart one segment before
`fetched_through_ms`, publish exactly that overlap, retain the trusted prefix,
advance `MEDIA-SEQUENCE`, emit one `EXT-X-DISCONTINUITY`, and advance
`EXT-X-DISCONTINUITY-SEQUENCE`. For fMP4, re-declare a generation-specific
`EXT-X-MAP`; never overwrite an init URI a client may have cached. Re-report
`media_origin_ms` after the new keyframe snap. A playlist never names bytes no
owner durably published.

The survivor's writer starts at the replicated sequence, not at zero: both
HLS argument builders pass `-start_number {media_sequence}`, and the copy
segmenter names its first output with that same index. `served_live_playlist`
continues deriving `MEDIA-SEQUENCE` from the retained filename index, which is
then correct by construction. Whenever a prefix is dropped it also emits
`EXT-X-DISCONTINUITY-SEQUENCE` from
`SessionOwner.discontinuity_sequence`; otherwise pruning the failover marker
silently renumbers every remaining discontinuity.

### 3.6 Quorum, discovery, and local bytes have explicit failure behavior

On quorum loss or lease-renewal failure, a node fails `/readyz`, stops serving
playlists and segments with `503` plus healthy node addresses, and terminates
session children. It may serve immutable direct bytes only if the request does
not mutate progress and the client already has the node list; the default is
self-fence, because stale serving hides failed writes.

Each node advertises a hostname derived from `node.id`, not `instance.id`.
mDNS and GDM include the logical server id, node id, protocol version, and
healthy node list. `server.name` moves into replicated settings; the config
value is only the bootstrap seed. One service instance represents the logical
server while node-specific records remain addressable without hostname
collision.

Replicated state never implies local bytes exist. Segment and artwork requests
proxy to a known holder when local bytes are absent. Artwork repair is per-node
materialization from replicated facts, not a singleton. If no holder exists,
the request queues local repair and returns the existing bounded error. This
keeps provider-fetched art offline-safe while avoiding raft byte storage.

## 4. Data migration — quiesced, staged, verified, and reversible

Re-verify `MIGRATIONS.len()` in
[`store/sqlite/mod.rs`](../crates/plurx-core/src/store/sqlite/mod.rs) at build
time. Migration runs before background tasks and before HTTP bind:

1. Refuse a source database newer than the binary.
2. Quiesce by construction: no scheduler, scanner, Trakt outbox, listener, or
   offline producer exists yet.
3. Remove any abandoned `data/hiqlite.incoming` from a previous failed import.
4. Copy `plurx.db` to `data/migration/`, fsync file and parent directory, and
   record its SHA-256.
5. Create a fresh one-voter target at `data/hiqlite.incoming`; never import
   into the active target directory.
6. Apply the shared migration chain, import durable tables in foreign-key
   order, bind timestamps/ids explicitly, and rebuild derived FTS state.
7. Compare ordered content hashes for users, settings, libraries, items,
   files, watch state, Trakt/outbox, cache locations, and offline packages;
   run `foreign_key_check` and M1a's full parity suite.
8. Write and fsync a completion marker containing source checksum, schema
   version, imported table hashes, and cluster id.
9. Atomically rename `hiqlite.incoming` to the active target and fsync its
   parent. Keep the original backup.

Any failure removes the incoming target, leaves `plurx.db` active, exits
non-zero, and names the rollback command. File-copy rollback is supported only
before a second member joins. After membership, restore is a quorum-aware
cluster operation specified and drilled in M6.

Only `plurxd run` may perform this import. After M2, `reset-password` and
`refresh-metadata` refuse an unmigrated data directory instead of starting an
import from a second process beside the server.

## 5. Requirement traceability — every HA promise has an owner

| Requirement | Milestone | Acceptance evidence |
|---|---|---|
| REQ-HA-1 · one or 3+, no two-node claim | M0 · M1b · M3 | One-voter budget table passes; two voters report degraded; three retain quorum after one loss. |
| REQ-HA-2 · active-active API and streams | M3 · M5 | Three simultaneous streams enter through three nodes; direct, remux, and transcode continue during one-node loss. |
| REQ-HA-3 · replicated durable/session state, embedded only | M1b–M1d · M5 | Replica table hashes match; acknowledged writes and CAS session state survive a voter loss. |
| REQ-HA-4 · ≤10 s stream failover, zero acknowledged-write loss | M5 | Power-pull corpus proves direct/remux/transcode resume budget and monotone progress. |
| REQ-HA-5 · one identity/name/settings/activity surface and join token | M3 | One cluster id/name is advertised by three node-specific records; admin shows leader, lag, voters, degraded state, and viewers from every node. |
| REQ-HA-6 · client node lists plus VIP/k8s patterns | M5 · M6 · M7 | Web, Apple, and Android use the same retry corpus; keepalived and Service/Ingress runbooks are exercised. |

## 6. Milestones — each leaves a reviewable boundary

### 6.1 M0 — identity compatibility, dependency proof, and baselines

Add the default-empty tolerated cluster section, `ClusterIdentity`, and
crash-safe `node.id` seeding. Keep `SqliteStore` in production. Feature-gate
hiqlite 0.14 and prove FTS5/triggers, `RETURNING`, transaction CAS,
statement-vs-row replication semantics, transport authentication, and log
compaction. Record the §1 baselines. Update `validation/points.toml` with any
new daemon module in the same commit.

**Acceptance:** `make check` plus `make validate-staged`; a populated v14
fixture with cleanup enabled retains every cache row, offline package, and
byte across identity initialization; two fresh data directories get distinct
node ids; restarts preserve ids; the feature spike records every result in
[PHASE3-SPIKE.md](PHASE3-SPIKE.md). The config fixture accepts an unknown key
inside `[cluster]` while the existing unknown-key rejection for `[server]`
continues to pass.

### 6.2 M1a — extract backend parity before porting a backend

Move store contract tests behind an `Arc<dyn Store>` harness while SQLite is
the only implementation. Classify interactive read-branch-write methods and
define the replicated CAS pattern once. Rule `unixepoch()` and
`last_insert_rowid()` out of replicated SQL.

**Acceptance:** the extracted suite proves all 114 methods against in-memory
and file SQLite with no behavior change; `make check` remains the gate.

### 6.3 M1b — settings, users, and API keys on three voters

Implement the smallest security-bearing store surface first and introduce
`make cluster-check`. Bring up three voters now, not after the full port.
Land schema/protocol election guards and quorum-aware `/readyz` here.

**Acceptance:** writes through every node produce byte-identical table dumps
on all replicas; kill leader and follower in separate runs; acknowledged
writes survive; an older-schema voter refuses migration and cannot lead.

### 6.4 M1c — media plus node-local FTS

Port libraries/media/watch state and rebuild `items_fts` locally from
replicated `items`. Bind all timestamps and use `RETURNING` for generated ids.
Add media-root fingerprints; a leased scanner refuses reconciliation when its
mounted root does not match cluster truth, and a configured prune bound stops a
stale-but-present mount from deleting a library cluster-wide.

**Acceptance:** all three nodes return identical browse/search results; delete
and rebuild one node's FTS index; run a stale-root fixture and prove no prune
commits.

### 6.5 M1d — remaining store contracts and write-rate gates

Port Trakt/outbox, transcode cache, offline packages, and the remaining watch
methods. Implement the 10-second progress coalescer and storage-class-scoped
cache ownership. Run the full parity suite against SQLite and three voters.

**Acceptance:** all store contracts pass on both backends; §1 write-rate,
latency, RSS, and growth budgets pass; offline/cache rows retain node-local
ownership on all replicas.

### 6.6 M2 — migrate an existing install

Implement §4 and make one-voter hiqlite the default only after the import gate
passes. Do not write required cluster keys into config until the M0-compatible
binary is the documented rollback floor.

**Acceptance:** migrate populated v14 and failure-injected fixtures; compare
content hashes and full parity behavior; kill each import step and prove the
next boot either resumes from the completed atomic target or deletes incoming
state and starts SQLite unchanged.

### 6.7 M3 — membership, secrets, discovery, and one settings surface

Add single-use join, add/remove, health, server-name replication, node-specific
mDNS/GDM, and the admin membership surface. Two voters are a visible degraded
reconfiguration state, never supported HA.

**Acceptance:** grow one node to three without changing `instance.id`; reject
expired/reused tokens and public cleartext binds; advertise three distinct
node records under one logical identity/name; remove one follower while
preserving quorum. The activity page aggregates direct-play and session rows
from all healthy nodes instead of exposing only the process that answered the
request.

### 6.8 M4 — transactional fences and materialization ownership

Put scans, metadata refresh, genre backfill, scheduled cache production, and
offline queue ownership behind fenced leases. A scan resumes by restarting
from zero until a replicated cursor is deliberately added. Keep cleanup and
artwork materialization per node; proxy missing local bytes to a holder and
queue repair.

**Acceptance:** `SIGSTOP` an owner past TTL, acquire elsewhere, `SIGCONT`, and
prove its resumed write is rejected inside the transaction. Kill a scan owner
and observe one successor restart. Partition a serving node and prove it fails
readiness, kills children, and stops capability-URL serving.

### 6.9 M5 — web failover for direct, remux, and transcode

Implement the exact §3.5/[PERF-PLAN.md](PERF-PLAN.md) §7.3 contract. Proxy
non-owner requests first; add web node-list retry after the server corpus is
green. Update `validation/points.toml` and the operations contract whenever
new ports or daemon files enter governed surfaces.

**Acceptance:** kill the serving node during direct, copy-remux, and transcode,
including mid-segment response. Resume within 10 seconds with one
discontinuity, correct `DISCONTINUITY-SEQUENCE`, a generation-specific fMP4
map, monotone watch progress, and no URI whose bytes were never published.
Across the session lifetime, no two identical segment URIs may name different
bytes. After the failover discontinuity slides out of the live window, a
reloading client must still observe the same discontinuity numbering for the
remaining segments. Run three concurrent streams entering through three
different nodes.

### 6.10 M6 — operations, backup/restore, upgrades, and drills

Add three-node Compose and Helm examples, keepalived and k8s Service/Ingress
runbooks, rolling-upgrade rules, secret rotation, quorum-aware backup/restore,
and deterministic drills for leader/follower loss, netsplit, disk-full, stale
owner, mixed versions, and one-node recovery.

Expose leader id, voter health, commit index, applied-vs-committed lag, lease
acquisitions/rejections, fence rejections, takeover count/duration, and
compaction growth. Every number includes a sentence in
[OPERATIONS.md](OPERATIONS.md) explaining healthy and actionable shapes.

**Acceptance:** `make cluster-check` exercises the failure harness; a fresh
operator reaches three healthy nodes in under 10 minutes; backup, destroy, and
restore preserves the content hashes from M2; rolling upgrade never elects an
incompatible leader.

### 6.11 M7 — Apple and Android consume the proven failover contract

Only after the web corpus is green, implement the same node-list, retry budget,
and server error semantics in both native clients. Do not translate node loss
into their existing compatibility-fallback ladders.

**Acceptance:** the shared fixture corpus plus physical iPhone/iPad, Apple TV,
Android, and Google TV runs survive one serving-node loss in every delivery
mode without lowering quality or losing selected tracks.

## 7. Non-goals — guardrails for the first clustering release

1. **Do not replicate media or derived bytes through raft.** Consensus owns
   facts; shared storage, proxying, and regeneration own bytes.
2. **Do not support two-node HA.** Two voters survive no failure and create a
   confidence trap.
3. **Do not add PostgreSQL, etcd, Redis, or a broker.** The embedded deployment
   shape is a product constraint.
4. **Do not require byte-identical transcodes across mixed encoders.** The
   output protocol must match; node-local recipe hashes may differ.
5. **Do not combine clustering with adaptive quality or TV ports.** Those
   changes obscure the induced failure each milestone must prove.
6. **Do not call a VIP the failover implementation.** A VIP locates a process;
   replicated state, fencing, and takeover let it continue the film.

## 8. Handoff checkpoint — M0 is documentation-compatible, not data-empty

Start M0 on `main` with identity seeding, the tolerated empty config section,
the feature-gated hiqlite semantic spike, and the single-node baselines. Do not
change the production store. The populated identity fixture is mandatory:
empty data directories cannot expose the cache/offline ownership failure this
slice exists to prevent.

```bash
make check                    # M0 and every milestone: repository baseline
make validate-staged          # changed behavior contracts
make cluster-check            # joins in M1b with the first three-voter slice
cargo test -p plurx-core store::sqlite::tests:: -- --nocapture
                              # explicit local M2 database-upgrade gate
```

No milestone is complete because three processes stayed up. It is complete
when the failure it owns is induced and the promised state survives it.
