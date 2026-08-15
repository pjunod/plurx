# Clustering transition — from one plurxd node to Phase 4

**Status:** executing — M0 through M1d and M2's source backup, table import,
bounded parity verification, and post-coalescer compacted-growth gate are
complete; import orchestration and daemon activation remain pending
· **Executes:** Phase 4 from [ROADMAP.md](ROADMAP.md) and REQ-HA-1–6 from
[REQUIREMENTS.md](REQUIREMENTS.md) · **Written:** 2026-08-06 · **Revised:**
2026-08-14

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
| p95 `put_progress_at` latency | ≤25 ms and ≤max(2× baseline, baseline + 0.5 ms) | A five-second player beat must not become visible UI latency; the additive branch is derived from the measured fixed Raft tax, while the ratio branch takes over on slower storage. |
| Idle RSS after warm-up | ≤100 MiB additional | The recommended third voter includes Pi/NAS-class hardware. |
| 10,000 incoming progress beats | ≤512 bytes/beat (5,120,000 bytes total) after production-threshold snapshot, purge, and WAL-settle cycles | Pre-compaction log size is not disk occupancy; the gate compares equally compacted states so retained WAL allocation is not mistaken for growth. |
| Physical progress commits under that load | ≤64 commits/active stream (5,120 for the 80-stream fixture) | The 125 beats represent 620 seconds at the shipped five-second cadence: 63 commit windows per stream, plus one window of scheduling headroom. A disk-size result can stay small after compaction even when every heartbeat reached consensus, so the same gate rejects lost coalescing directly. |
| Watch-state commit rate | ≤1 commit / 10 s / active stream | [ARCHITECTURE.md](ARCHITECTURE.md) §2.2 already promises coalescing; clustering must make it true. |

M0 records the raw SQLite baseline and hiqlite result on the same machine. M2's
bounded `make cluster-growth` command records the coalesced result under the
production 10,000-log snapshot policy. A budget change requires measured
evidence in this plan and [PHASE3-SPIKE.md](PHASE3-SPIKE.md), not a quietly
wider threshold.

## 2. Starting point — the seams exist, eight assumptions do not

| Existing seam | Current truth | Phase 4 consequence |
|---|---|---|
| `Arc<dyn Store>` | All durable application state crosses the 120-method composed traits in [`store/mod.rs`](../crates/plurx-core/src/store/mod.rs); a compile-time coercion keeps that object-safety claim executable. | Keep handlers stable, but port the methods in reviewable groups rather than one backend rewrite. |
| `instance.id` | Stable logical-server identifier in settings; new installs mint a UUID, but the schema historically accepted arbitrary strings. It is also passed as `AppState::node_id`. | Split the concepts without changing the first node's existing cache/offline ownership key. |
| Cache/offline `node_id` | Existing rows are keyed by the current `instance.id`. | An existing install seeds local `node.id` from that exact string; changing it would strand rows and let orphan cleanup delete bytes. |
| `Recipe<'a>` | Borrowed, node-specific cache-key input; its hash includes ffmpeg build, encoder, and pipeline. | The hash is a node-local cache address, not a cluster recipe. Replicate owned request inputs and rebuild against the survivor's pipeline. |
| Session takeover | [PERF-PLAN.md](PERF-PLAN.md) §7.3 defines owner epoch, lease, playable/fetched frontiers, overlap, and playlist sequence. | Adopt that contract; do not invent a smaller `ReplicatedSession`. |
| Scheduler | Every daemon starts scans, metadata, cleanup, and production. | Fence replicated publishing work; keep byte materialization per node. |
| Artwork/cache bytes | Rows may replicate while bytes are local. | Missing bytes proxy to a holder and enqueue local repair; a replicated filename is not proof of a local file. |
| Discovery | mDNS/GDM derive hostname and service name from shared identity/name. | Advertise each node under a node-specific hostname with one logical id and the full node list. |

M0 now provides tolerated cluster configuration and local node identity; M1a
provides the full backend-neutral parity inventory; M1b provides the first
hiqlite auth/settings backend; M1c adds libraries, media, watch state,
node-local FTS, and bounded root-aware reconciliation; and M1d completes the
120-method store plus the progress write-rate gate. M2 now provides the
content-addressed source backup and the inert fresh-target row importer with
per-table parity evidence. The daemon still opens the complete SQLite store.
Import orchestration, membership/join, full-store activation,
lease-fenced publication, replicated session ownership, serving self-fencing,
and client failover do not exist yet.

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

New and joining nodes use UUID strings for `node.id`. On an existing data
directory, create it atomically from the exact current `instance.id`, including
an older non-UUID value; the concepts become separate while every cache
location and offline package keeps its key. Equivalent UUID spellings normalize
to the exact `instance.id` ownership key. On a fresh or joining node, generate a
new UUID.
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

The shipped build keeps `secret_raft` and `secret_api` in mode-`0600` files
beside `node.id`, generated atomically. Hiqlite's `enc_keys` field is compiled
out because plurx uses `default-features = false`; it returns only if a later
M3 backup/dashboard design deliberately enables and threat-models that feature.
These secrets are never logged, exposed in settings, or stored in replicated
SQL. A single-use join token contains cluster id,
bootstrap addresses, expiry, the new raft id, and an encrypted envelope for
those secrets; the one-time token secret wraps the envelope and is invalidated
after membership commits.

Trakt is the exception to the otherwise hash-only credential rows: its access
and refresh tokens are live bearer secrets. Replicating plaintext would copy
them into every voter database, raft WAL, snapshot, and backup, where row
deletion cannot erase historical log entries.

**Delivered 2026-08-14.** Both columns are envelope-encrypted before they reach
the `Store`: `TraktAuth` carries `SealedSecret`, not `String`, and outside
`plurx-core` sealing with the key is the only way to make one. Because the
upgrade path must still be able to read a pre-encryption row, the durable write
also refuses a value that is not an envelope, so the guarantee does not rest on
the type alone. The SQLite→Hiqlite importer does not go through that write, so
it audits its source's credential columns up front and refuses a pre-encryption
backup rather than replicating one. Only ciphertext, key id, and non-secret
metadata enter raft, and a copied voter disk is not sufficient to use a
household's Trakt account.

```toml
[cluster]
credential_key_file = ""         # empty means <data_dir>/credentials.key
```

The wrapping key is a 32-byte mode-`0600` file beside `node.id`, minted on
first boot and never written to a durable row. It is node-local configuration
in the §3.2 sense — not replicated, and distributed to other voters out of band
exactly like `secret_raft` and `secret_api`. Being an addition to `[cluster]`,
it also inherits this section's rollback rule: an M0 binary reading a config
that sets it still loads.

The envelope is `plxenc:v1:<key id>:<hex nonce‖ciphertext‖tag>` under
XChaCha20-Poly1305, with the row's `user_id` as authenticated additional data
so a re-pointed row fails to open rather than yielding another user's account.
The refresh-token compare-and-set in `update_trakt_tokens` and
`delete_trakt_auth_if_current` operates on the stored envelope, so a rotation
race is still decided by exact equality on bytes every voter already agrees on
— no voter has to hold the key to arbitrate one.

Unwrapping happens only at an outbound call. There is no cleartext fallback
anywhere: `open_trakt` returns an error for an unwrapped value rather than the
value, and startup refuses outright when sealed rows exist and the key file
does not, instead of minting a key that cannot open them.

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

**Delivered 2026-08-07.** PRs 82–83 establish the identity, semantic spike,
cost record, replicated-SQL guardrails, and the then-114-method parity
inventory this plan requires.

Add the default-empty tolerated cluster section, `ClusterIdentity`, and
crash-safe `node.id` seeding. Keep `SqliteStore` in production. Isolate
hiqlite 0.14 in a non-shipping spike crate and prove FTS5/triggers,
`RETURNING`, transaction CAS, statement-vs-row replication semantics,
transport authentication, and log compaction. Record the §1 baselines. Update
`validation/points.toml` with any new daemon module in the same commit.

**Acceptance:** `make check` plus `make validate-staged`; a populated v14
fixture with cleanup enabled retains every cache row, offline package, and
byte across identity initialization; two fresh data directories get distinct
node ids; restarts preserve ids; the feature spike records every result in
[PHASE3-SPIKE.md](PHASE3-SPIKE.md). The config fixture accepts an unknown key
inside `[cluster]` while the existing unknown-key rejection for `[server]`
continues to pass.

### 6.2 M1a — add backend parity before porting a backend

**Delivered 2026-08-07.** The `store_contract` inventory and
`store::replicated` policies are the executable port boundary.

Add a parallel store contract suite behind an `Arc<dyn Store>` harness while
SQLite is the only implementation; retain the deeper SQLite-specific tests.
Classify explicit transaction boundaries and define the replicated CAS pattern
once. Rule connection-, clock-, and RNG-dependent values out of replicated
SQL.

**Acceptance:** the additive suite exercises every method against in-memory
and file SQLite with no behavior change; `make check` remains the gate. The
inventory reached 118 methods when M1c added reconciliation, root-reset, and
derived-search recovery contracts; M1d's progress and Trakt compare-and-set
operations bring the current boundary to 120.

The executable M1a contract is `store_contract`: each scenario receives only
`Arc<dyn Store>`, and its name inventory fails unless every `async fn` declared
in `src/store/mod.rs` remains represented. `store::replicated` owns the shared
replicated-SQL policy, single-row CAS result contract, and source-pinned
classification of every explicit production SQLite transaction boundary.

### 6.3 M1b — settings, users, and API keys on three voters

Implement the smallest security-bearing store surface first and introduce
`make cluster-check`. Bring up three voters now, not after the full port.
Land schema/protocol election guards and quorum-aware `/readyz` here.

**Acceptance:** writes through every node produce byte-identical table dumps
on all replicas; kill leader and follower in separate runs; acknowledged
writes survive; an older-schema process fails the remote preflight. M3 owns
making that preflight an enforced gate on real membership and elections.

**Backend slice delivered 2026-08-07; daemon activation remains open.**
`HiqliteAuthStore` implements the 23 settings, user/token, and API-key methods
with bound timestamps, `RETURNING` ids, consistent reads, and fail-closed scope
decoding. `make cluster-check` starts three separate voter processes, drives
the lifecycle through every node, compares SHA-256 digests of ordered local
table dumps, kills a follower and leader in fresh runs, and proves both
acknowledged-write survival and no-quorum readiness failure. A remote
schema/protocol preflight rejects an incompatible client before it starts a
voter. The M1b harness does not dynamically add that process to membership;
M3 must place this preflight in the join/start coordinator and prove a rejected
node never appears in `voter_ids()`.

This type intentionally cannot satisfy the complete `Store` trait yet. Its
dependency is behind the `hiqlite-store` feature, enabled only by the cluster
harness, so the ordinary daemon dependency closure remains unchanged until
the full M1d/M2 activation. `plurxd` therefore still drives `/readyz` through
SQLite. The hiqlite
`SettingsStore::ping` is quorum-aware and is the route's eventual backend, but
claiming the production route before the remaining store composition lands
would create a hybrid source of truth. Full daemon activation stays with the
M1d/M2 switch.

### 6.4 M1c — media plus node-local FTS

Port libraries/media/watch state and rebuild `items_fts` locally from
replicated `items`. Bind all timestamps and use `RETURNING` for generated ids.
Add media-root fingerprints; a leased scanner refuses reconciliation when its
mounted root does not match cluster truth, and a configured prune bound stops a
stale-but-present mount from deleting a library cluster-wide.

**Acceptance:** all three nodes return identical browse/search results; delete
and rebuild one node's FTS index; run a stale-root fixture and prove no prune
commits.

**Backend slice delivered 2026-08-07; daemon activation remains open.**
`HiqliteAuthStore` now also implements all library, media, and watch methods.
Authoritative reads are quorum-consistent; search reads each voter's derived
contentless FTS index. A missing local index can no longer make an authoritative
item delete fail. `reconcile_library` records root identity and puts
the root comparison, prune budget, vanished-file delete, and empty-hierarchy
prune in one transaction. The ordinary scanner now publishes through that
boundary with `storage.scan_prune_percent` (10% by default); a root mismatch or
over-budget scan records an error and commits no delete. An empty first scan of
an upgraded library cannot establish root trust. Editing paths clears the old
identity, and the admin root-reset action covers a verified mount replacement.

`make cluster-check` writes catalogue and watch state through every voter,
compares ordered local table digests, and compares voter-local authoritative
catalogue digests and search results. It then deletes voter 2's FTS
rows directly, proves authoritative truth remains unchanged while the full
derived-state digest and local search change, invokes the product rebuild
contract, and requires all three views to converge before the existing
follower-loss and leader-loss cases continue. The remaining lease token is
deliberately M4's
signature change: M1c owns root identity and the atomic publication boundary;
M4 makes that same boundary reject an expired scanner.

### 6.5 M1d — remaining store contracts and write-rate gates

Port Trakt/outbox, transcode cache, offline packages, and the remaining watch
methods. Implement the 10-second progress coalescer and storage-class-scoped
cache ownership. Run the full parity suite against SQLite and three voters.

**Acceptance:** all store contracts pass on both backends; §1 write-rate,
latency, RSS, and growth budgets pass; offline/cache rows retain node-local
ownership on all replicas.

**Implementation complete; daemon activation remains open.**
`HiqliteAuthStore` now implements all 120 `Store` methods. Trakt credentials,
the watched outbox, cache recipes and locations, offline packages, and offline
leases use deterministic replicated writes. Trakt refresh and unlink are
compare-and-set operations, and watched delivery rows carry short claims so
several voters cannot settle the same attempt over one another. Cache and
offline rows replicate ownership, state, recipe, and path metadata; the named
node's media, cache, and package bytes remain node-local. Offline admission
applies complete request idempotency, row limits, per-user bytes, and a
per-node byte budget in one conditional statement; one consistent fallback
snapshot classifies a refusal. Lease creation and renewal keep the durable
token and ephemeral guard in one transaction.

`make cluster-check` first runs the same `Arc<dyn Store>` contract scenarios
against memory SQLite, file SQLite, and a three-voter Hiqlite cluster. Its
separate-process loss harness then varies user and node identity; verifies
refused-state postconditions, cache heartbeat cutoffs, every Trakt/outbox
field, and terminal offline-state guards; and compares independently hashed
replica dumps both before and after follower or leader loss. The server-side
progress coalescer commits the leading beat, replaces intermediate beats with
one trailing value, and flushes that newest value at the ten-second boundary.
A new 95% watched transition commits synchronously. Manual writes cannot be
overwritten by a delayed flush, dated offline imports remain ordered facts,
and graceful shutdown makes a bounded drain attempt.

The existing raw-write cost record measures 0.083333 ms p95 progress latency,
6,832,128 bytes (6.52 MiB) additional idle RSS, and 8,309,777 bytes of data
growth for 10,000 direct store writes. The committed `make cluster-growth`
gate now drives 10,000 incoming beats through the production
`ProgressCoalescer`: 80 user/item streams × 125 beats at a deterministic
five-second cadence represent 620 seconds of playback per stream. The real
ten-second window produces 63 commits per stream, or 5,040 total; the gate
allows one extra window per stream, for a 5,120-commit budget. Injecting only
the monotonic clock keeps the ratio honest without turning the CI gate into a
ten-minute campaign; every store read, compare-and-set, and replicated write
remains real.

With hiqlite's production 10,000-log snapshot policy, two successive
snapshot/purge cycles normalize retained WAL rollover before each directory
comparison. The 2026-08-14 record is 3,832,601 bytes before, 8,273,961 bytes
after, and 4,441,360 bytes of compacted growth — 444.136 bytes/incoming beat
against the measured 512-byte budget. The same run bypasses the coalescer: its
10,000 commits produce 8,454,240 compacted bytes (845.424 bytes/beat), so the
live control must violate both the 5,120-commit and 5,120,000-byte limits.
Paused-time tests still prove the ten-second window and terminal watched
exception independently. M2 also owns importing an existing SQLite store and
selecting the complete backend at startup.

### 6.6 M2 — migrate an existing install

Implement §4 and make one-voter hiqlite the default only after the import gate
passes. Do not write required cluster keys into config until the M0-compatible
binary is the documented rollback floor.

**Acceptance:** migrate populated v14 and failure-injected fixtures; compare
content hashes and full parity behavior; kill each import step and prove the
next boot either resumes from the completed atomic target or deletes incoming
state and starts SQLite unchanged.

**Source preparation and row import delivered 2026-08-09; activation remains
open.** `prepare_sqlite_import` refuses a future schema before mutation,
removes an abandoned incoming directory, and publishes a fsynced,
content-addressed SQLite online backup with committed WAL pages included. The
backup uses canonical delete journaling, so later verification never creates
an untracked WAL sidecar beside immutable source material.

`HiqliteAuthStore::import_sqlite_backup` accepts only a fresh bootstrapped
target whose `instance.id` matches the source. It imports all 17 shared durable
tables in foreign-key order and 64-row Raft transactions, preserving explicit
ids, timestamps, nullable text, and integer values. Items compute their
parent-first id order in one recursive source pass, then load each bounded
chunk through indexed point reads; numeric id order cannot violate their
self-reference, and large catalogues do not re-run and re-sort the full tree
for every 64 rows. A v14 source contributes empty scan-reconciliation tables
and zero outbox claim deadlines; v15–v17 preserve their newer durable facts.
`playback_events`, `items_fts`, and `offline_lease_guards` never cross the
boundary: telemetry is node-local, FTS is rebuilt, and the lease guard is an
internal transaction scratch table.

The importer runs source and target foreign-key checks, rebuilds FTS, compares
the exact ordered JSON rows for every imported table, and returns the row count
and SHA-256 for each table only after parity. Source validation, row loading,
and digest scans share one blocking worker, so synchronous SQLite work never
occupies an async runtime worker. Row data crosses that boundary in bounded
chunks; one parent-first item-id ordering remains O(items) so the importer does
not rebuild and re-sort the full item tree for every 64 rows. Target parity
advances through 64-row primary-key pages and feeds an incremental digest
instead of returning a whole table through the leader. Each page is a
consistent read against the quiescent incoming target; activation does not
start a concurrent writer before parity succeeds.

The three-voter contract imports populated v14 and current fixtures, including
single- and multi-column primary-key tables large enough to cross the import
and parity page boundary. It covers a child id that sorts before its parent,
nonzero current outbox claims, current scan state, checksum refusal,
merge-style retry refusal, identity mismatch, the v14 schema floor, cyclic
parent refusal, injected target corruption before parity, source playback
telemetry exclusion, and runtime multi-column keyset progress. Unit tests pin
the keyset SQL shape and prove async-executor responsiveness while SQLite is
blocked.

This code is deliberately inert. The next M2 slice still owns steps 2, 5, 8,
and 9 from §4: startup quiescence, one-voter incoming-cluster lifecycle, the
fsynced completion marker, atomic activation, failure injection at each step,
and fallback to unchanged SQLite. The post-coalescer blocker is closed by the
2026-08-14 `make cluster-growth` record: 4,441,360 compacted bytes for 10,000
incoming beats at five-second logical cadence (444.136 bytes/beat), 5,040
physical commits for 80 streams, and a raw 10,000-write control that violates
both the commit and byte budgets. Bounded target parity and blocking-worker
source reads are now in place; the next boundary is the activation coordinator,
not another importer expansion.

**Trakt credential encryption is no longer a blocker (2026-08-14).** Both
bearer columns are envelope-encrypted under a node-local key file before they
reach the `Store` (§3.2), an existing cleartext install is migrated forward on
upgrade, and a boot that finds sealed rows it cannot open refuses to start
rather than falling back to cleartext — whether the key file is missing, is a
replaced key that opens none of those rows, or is readable by more than its
owner. The importer does need a change, and has it. Copying `trakt_auth` as
text is only safe when the source install has already been sealed, and a
backup taken from a pre-encryption install has not: import is the one
production path that bypasses the durable write, so it audits the backup's
credential columns and refuses one carrying cleartext before submitting any
row to raft.

With the growth record and credential encryption both closed, no §6.6
activation blocker remains outstanding.

### 6.7 M3 — membership, secrets, discovery, and one settings surface

Add single-use join, add/remove, health, server-name replication, node-specific
mDNS/GDM, and the admin membership surface. Two voters are a visible degraded
reconfiguration state, never supported HA.

Node removal must also resolve offline work owned by that node. A `preparing`
package cannot be silently re-homed because its replicated `source_path` does
not prove the survivor has the same mount. Before removal commits, M3 must
either verify an equivalent source and explicitly requeue on a chosen node, or
mark the package failed with a stable `node_removed` code so its reservation is
released and the client can retry. Leaving it preparing until seven-day expiry
is an activation blocker.

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

## 8. Handoff checkpoint — M2 can verify rows, not activate them

M0 through M1d and M2's source-backup plus row-import slices are on `main`;
the importer can now prove the 17 shared durable tables without unbounded
leader responses or blocking the async executor. It still does not select the
target in `plurxd`. Keep SQLite as the daemon's selected store until
incoming-cluster orchestration, credential encryption, node removal, and the
remaining activation gates are ready. Post-coalescer compacted growth is now
recorded and gated; the next implementation boundary is the fsynced completion
marker plus failure-injected one-voter activation. That coordinator must delete
partial incoming state and keep SQLite active on every failure.

```bash
make check                    # M0 and every milestone: repository baseline
make validate-staged          # changed behavior contracts
make cluster-check            # M1b-M2 durable state, import, FTS, and loss gate
make cluster-growth           # 10,000-beat compacted growth + raw control
cargo test -p plurx-core store::sqlite::tests:: -- --nocapture
                              # explicit local M2 database-upgrade gate
```

No milestone is complete because three processes stayed up. It is complete
when the failure it owns is induced and the promised state survives it.
