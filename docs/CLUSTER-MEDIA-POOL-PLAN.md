# Cluster media pool — make every node improve playback

**Status:** ready to build after the M3 transport dependencies in §2.2 ·
**Executes:** M4–M5 from [CLUSTERING-PLAN.md](CLUSTERING-PLAN.md) and M4 from
[PERF-PLAN.md](PERF-PLAN.md) · **Written:** 2026-08-21 against `main`
`a543dcaa`

Companion to [CLUSTERING-PLAN.md](CLUSTERING-PLAN.md), which owns consensus,
membership, and the ten-second failover promise; [PERF-PLAN.md](PERF-PLAN.md)
§7, which owns the transcode-pool and HLS takeover mechanics; and
[PLAYBACK.md](PLAYBACK.md), which owns the client-visible direct · remux ·
transcode decisions. Work this document milestone by milestone. If a change
appears to require putting media, artwork, subtitle, playlist, or segment
bytes through Raft, stop: this plan replicates facts and fences, then routes,
proxies, shares, or regenerates bytes.

## 1. Objective — spend the cluster on latency and capacity

Three voters should behave like one media server with three sets of hardware.
A request may enter through any node, but the cluster should execute it where
the required bytes and proven pipeline make it cheapest. Idle nodes should
prepare likely-next media without competing with a viewer. A failed owner
should cost a bounded interruption rather than a terminal player error.

The cluster improves two different shapes and the measurements keep them
separate:

| Shape | What improves | What does not |
|---|---|---|
| One stream | A completed cache hit or placement on a faster compatible GPU lowers start/seek cost | Direct play does not become faster merely because more voters exist |
| Concurrent streams | Free encoder slots, CPU pools, and local caches on every node become usable through one ingress | All nodes still share the NAS and client network ceilings |
| Node loss | Another owner resumes the delivery inside the failover budget | A client cannot recover until it knows another address or its ingress retries |
| Background work | Whole-title jobs spread across idle workers; singleton jobs run once | Splitting one title into cross-node segment ranges remains out of scope |

### 1.1 Success budgets are observable at the client boundary

| Measure | Budget | Why this is the gate |
|---|---:|---|
| Placement decision | p95 ≤200 ms with three healthy LAN voters | A critical-path fan-out that costs more than the live start it optimizes has made playback worse |
| Remote start overhead | p95 ≤250 ms above the same request sent directly to the selected worker | Proxying one LAN hop is acceptable; another cold-start phase is not |
| Completed-cache start | TTFF ≤1.5 s | Preserves the shipped pre-transcode objective in PERF-PLAN §6 |
| Completed-cache seek | decoded frame ≤0.5 s after the seek request | A VOD cache hit must continue to seek like direct play |
| Capacity routing | zero capacity refusals while a compatible healthy node has a free proven slot | This is the primary aggregate-throughput win |
| Background yield | live playback receives the encoder lane ≤5 s after asking | Matches the existing foreground admission bound |
| Session takeover | decoded media resumes ≤10 s after owner death | REQ-HA-4 and CLUSTERING-PLAN §3.5 own this promise |
| Session coordination writes | at 80 active sessions: ≤1 quorum transaction per owner node per 2 s, p95 commit ≤100 ms, and ≤6 MiB/min session-attributable replicated WAL growth | Heartbeats must not turn playback progress into a consensus write storm |
| Stale publication | zero writes accepted from an expired fence | Availability may degrade; two owners publishing one generation may not |
| Scheduler duplication | one active owner per singleton resource | Three voters must not triple-scan the NAS or triple-hit metadata providers |

The corpus records p50 · p95 · failure rate · selected node · placement reason
for each delivery method. A median alone can hide the one cold or contended
start the cluster exists to avoid.

### 1.2 The measurement manifest is fixed before implementation

Run the same source snapshots and output contracts before enabling a milestone
and after it lands. Preserve the request id, node snapshots, placement trace,
client timestamps, relevant metrics, and daemon logs as one artifact bundle.

| Scenario | Required variants | Boundary recorded |
|---|---|---|
| Start | direct play · copy remux · cold transcode · completed-cache hit | request received → first decoded audio/video frame |
| Seek | direct · remux · transcode · cache hit at 10% and 80% | seek issued → first decoded frame at the new position |
| Capacity | 1 · 2 · 4 · 6 concurrent starts on three two-slot workers | placement decision, refusal, encoder start, and first frame |
| Contention | speculative encode active, then live direct/remux/transcode start | live request → background permit released → first frame |
| Owner loss | owner killed and partitioned during playlist and mid-segment delivery | last decoded frame → first decoded frame from the successor |
| Degradation | slow peer · stale snapshot · absent source · missing cache bytes · quorum loss | bounded decision or explicit failure, never an unbounded wait |

Use one LAN client clock for end-to-end budgets and monotonic daemon clocks for
phase attribution. Record source size, codec/profile, resolution, HDR state,
selected tracks, cache warmth, and node hardware so a later run is comparable.

## 2. Starting point — replicated state is not a media pool

### 2.1 What `main` already provides

The following seams are implemented and should be extended rather than
replaced:

| Seam | Current contract | Consequence |
|---|---|---|
| `Arc<dyn Store>` | Durable application state is backend-neutral and Hiqlite writes are quorum-acked | Coordination tables join the same storage boundary |
| Membership | Every voter has a stable node id plus persisted Raft/API addresses | Internal media RPCs address nodes without exposing those addresses publicly |
| Cache recipe/location split | A recipe may have separate node-local locations | Placement can prefer a holder without copying bytes first |
| `TranscodeManager` | Boot-validated encoder caps, tone-map pipeline, admission counts, speed history, cache lookup, and production already live together | A media offer can report the same facts the selected worker will actually use |
| Pre-transcode producer | Ranking, content-addressed production, crash resume, and live-work preemption are shipped | Distribution changes ownership and dispatch, not encoding behavior |
| Session telemetry | Published/fetched frontiers, media origin, sequence indexes, delivery speed, and hold state already exist in process | Takeover publishes a bounded projection of real session state rather than inventing another clock |
| Cluster harness | Real separate voter processes already exercise membership, loss, and replicated store contracts | Every new distributed invariant belongs in that harness |

Re-verify the exact interfaces against
[`store/mod.rs`](../crates/plurx-core/src/store/mod.rs),
[`membership.rs`](../crates/plurx-core/src/cluster/membership.rs),
[`state.rs`](../crates/plurxd/src/state.rs), and
[`transcode.rs`](../crates/plurxd/src/transcode.rs) at the start of each
milestone. File positions will move; the ownership boundaries may not.

### 2.2 Merge dependencies before media placement begins

Three active M3 changes own prerequisites and must land or be explicitly
superseded before §8.4:

| Dependency | Surface this plan consumes | Rule |
|---|---|---|
| PR #405 / issue #325 | Node-specific discovery and replicated server name | Do not build a second node-list advertisement |
| PR #420 / issue #417 | Authenticated internal activity transport and retained internal API addresses | Generalize its peer client/signature envelope; do not invent another cluster credential |
| PR #490 | Artwork holder fetch/repair and graceful leave | Reuse its holder and drain semantics; do not race a second leave protocol |

Pure coordination storage in §8.2 may start independently if it does not touch
those branches' files. Media snapshots, offers, proxying, and leave-time
session drain wait for the dependencies.

### 2.3 What is still process-local

- Every daemon starts its own scheduler. Replicated timestamps reduce repeat
  frequency but are not mutual exclusion; simultaneous ticks can still
  dispatch the same work.
- A live HLS session, its child process, and its segment directory belong to
  the process that created them.
- Cache read guards protect one process. A replicated location row does not
  prove another node can read the named bytes.
- Hardware limits and recent speed are measured locally. The replicated
  setting `transcode.max_hw_sessions` is policy, not proof of free capacity.
- A node receiving an unknown session capability cannot yet find or proxy to
  the owner.

These are the exact boundaries this plan moves. It does not rework the
playback decision engine, encoder arguments, cache recipe hash, or client
quality policy unless a milestone names that change.

## 3. System shape — facts through Raft, bytes over the LAN

```text
                              replicated Hiqlite
                    ┌────────────────────────────────┐
                    │ leases · work queue · owners   │
                    │ cache locations · frontiers    │
                    └──────────────┬─────────────────┘
                                   │
 client ──▶ ingress node ── quote/score ──▶ selected worker
                │                                  │
                │ owner lookup                     ├─ direct source bytes
                │                                  ├─ local cached VOD
                └──── streamed proxy ◀─────────────└─ live HLS child
                                   │
                         authenticated peer HTTP
                   snapshots · offers · media bodies
```

There are three replication classes:

| State | Transport | Reason |
|---|---|---|
| Ownership, monotone fences, queue state, cache indexes, session recipe/frontiers | Raft-backed SQL | Losing or forking it can publish conflicting work |
| Encoder/load snapshot, offer details, proxy bodies | Authenticated peer HTTP | It is volatile or large; repeating it through consensus wastes the write budget |
| Media, artwork, subtitle, playlist, segment bytes | Local/shared filesystem or streamed peer response | Bytes are regenerable or operator-owned and do not belong in the Raft log |

At the intended 3–7 node scale, each node polling every peer every ten seconds
is bounded and simpler than a separate gossip system. Revisit the transport at
more than seven voters or when snapshot traffic exceeds 1% of cluster-API
bandwidth.

## 4. Coordination contract — one lease primitive, every stale writer fenced

### 4.1 Types separate storage from node identity

Add `cluster/coordination.rs` and compose its backend methods into `Store`.
Names below are the contract; re-verify surrounding error types before build.

```rust
pub struct Lease {
    pub resource: String,
    pub owner_node_id: String,
    pub fence: u64,
    pub expires_at_unix_ms: i64,
}

pub enum LeaseClaim {
    Acquired(Lease),
    Held {
        owner_node_id: String,
        fence: u64,
        expires_at_unix_ms: i64,
    },
}

#[async_trait]
pub trait CoordinationStore: Send + Sync + 'static {
    async fn acquire_lease(
        &self,
        resource: &str,
        owner_node_id: &str,
        now_unix_ms: i64,
        expires_at_unix_ms: i64,
    ) -> Result<LeaseClaim, StoreError>;

    async fn renew_lease(
        &self,
        lease: &Lease,
        now_unix_ms: i64,
        expires_at_unix_ms: i64,
    ) -> Result<Option<Lease>, StoreError>;

    async fn release_lease(
        &self,
        lease: &Lease,
        now_unix_ms: i64,
    ) -> Result<bool, StoreError>;
}
```

`StoreCoordinator` holds `Arc<dyn Store>` plus the local `node_id`, supplies
the clock, clamps TTLs, and exposes `acquire(resource, ttl)`. Store methods
stay explicit about the owner so backend contract tests can create competing
nodes without constructing a daemon. It is the concrete implementation of the
`ClusterCoordinator` seam named in CLUSTERING-PLAN; the repository must not
grow two independent lease abstractions.

Media sessions deliberately do not call generic `acquire_lease` and then edit
a second row. `claim_session`, `renew_session`, `publish_session_frontier`,
`supersede_session`, and `end_session` are the only session coordination API.
Each locks/validates `job_leases["session:<incarnation_id>"]` and the matching
`media_sessions` row in one transaction, and requires
`job_leases.fence == media_sessions.owner_epoch`. The claim transaction alone
advances both. A mismatch is corruption and fails closed; neither row is an
independent source of authority.

### 4.2 Reusable `job_leases` never delete the monotone fence

Add the table to SQLite and the replicated schema:

```sql
CREATE TABLE job_leases (
    resource       TEXT PRIMARY KEY,
    owner_node_id  TEXT NOT NULL,
    fence           INTEGER NOT NULL CHECK (fence > 0),
    expires_at_ms   INTEGER NOT NULL,
    updated_at_ms   INTEGER NOT NULL
) STRICT;
```

Acquiring an absent row writes fence `1`. Acquiring an expired or explicitly
released row increments the existing fence. Renew succeeds only for the exact
resource · owner · fence while the old lease is still live. Release sets
`expires_at_ms = now`; it does not delete the row, because deleting would let a
later owner reuse fence `1` and make an old token current again.

The only bounded-lifecycle exception is a random server-minted session
incarnation that the session APIs permanently mark ended and will never claim
again. After its retry/child-shutdown window, its detail and lease rows may be
deleted together; a stale publish then finds no exact row and rejects, while a
future playback uses a different incarnation/resource. Generic lease acquire
is never used for these resources. Scheduled job names and every other reusable
logical resource retain their rows forever.

Wall clocks may delay or hasten takeover, but they cannot authorize a stale
write: every fenced publication validates the exact fence in the same SQL
transaction. Nodes should run time synchronization; the system remains safe
under skew and reports availability loss rather than pretending timestamps
are a consensus clock.

### 4.3 Checking before a write is not fencing the write

Every operation that publishes durable singleton work gains a `&Lease` and
validates it in the same transaction as each mutation, not merely at the start
or end of a pass. A scan that writes 500 media rows performs fenced bounded
batches, so a stale scanner cannot interleave item writes with its successor.
The first conversion set is:

```text
mark_library_scanned_fenced
scan reconcile batch/cursor publication
metadata and genre item publication
pre-transcode queue enqueue/claim/complete/fail
cache claim heartbeat/completion/forget for distributed work
```

The SQL predicate includes resource · owner · fence and a live expiry against
the same caller-supplied `now_unix_ms`. Zero affected rows returns
`StoreError::FenceRejected`; it never reports success after silently dropping
the mutation.

Process-local cleanup, telemetry pruning, local-cache reader protection, and
artwork materialization remain node-local and do not take a cluster singleton
lease. P6 explicitly replaces that assumption for shared-cache readers with
cluster-visible pins. Provider fetch selection and shared index publication do.

### 4.4 Lease durations match the failure they detect

| Resource | TTL | Renewal | Takeover behavior |
|---|---:|---:|---|
| Scan / refresh / genre backfill | 90 s | 30 s | Successor restarts the bounded pass; scan cursor resume waits for a later explicit milestone |
| Candidate generation | 90 s | 30 s | Successor recomputes idempotent dedupe keys |
| Pre-transcode queue job | 30 s | 10 s | With node-local staging, the old node may resume only while it retains the lease; a successor starts from zero. Shared staging resume waits for verified shared storage in P6 |
| Media session | 6 s | 2 s | Successor claims the next owner epoch and resumes one segment before the fetched frontier |

The owner stops its child and refuses publication as soon as renewal fails.
A five-second live-admission wait is not extended by a ninety-second scheduler
lease: foreground preemption terminates the background child independently.

## 5. Distributed pre-cache — parallelize whole titles, not one timeline

### 5.1 Candidate generation and execution are separate jobs

One lease owner evaluates Continue Watching · Next Up · Recently Added and
enqueues work. Every compatible node may claim execution. This prevents three
nodes from choosing the same candidates while still spending three idle GPUs.

```sql
CREATE TABLE pretranscode_jobs (
    id                TEXT PRIMARY KEY,
    dedupe_key        TEXT NOT NULL,
    file_id           INTEGER NOT NULL,
    source_size       INTEGER NOT NULL,
    source_mtime      INTEGER NOT NULL,
    target_height     INTEGER NOT NULL,
    policy_generation TEXT NOT NULL,
    requirements_json TEXT NOT NULL,
    reason            TEXT NOT NULL CHECK (
                          reason IN ('in_progress', 'next_up', 'recent')),
    priority          INTEGER NOT NULL,
    state             TEXT NOT NULL CHECK (
                          state IN ('queued', 'running', 'ready', 'failed',
                                    'cancelled')),
    owner_node_id     TEXT,
    fence             INTEGER NOT NULL DEFAULT 0,
    lease_expires_ms  INTEGER,
    attempts          INTEGER NOT NULL DEFAULT 0,
    not_before_ms     INTEGER NOT NULL,
    last_error_code   TEXT,
    recipe_hash       TEXT,
    storage_id        TEXT,
    created_at_ms     INTEGER NOT NULL,
    updated_at_ms     INTEGER NOT NULL
) STRICT;

CREATE INDEX pretranscode_jobs_due
    ON pretranscode_jobs(state, not_before_ms, priority, created_at_ms);

CREATE UNIQUE INDEX pretranscode_jobs_active
    ON pretranscode_jobs(dedupe_key)
    WHERE state IN ('queued', 'running');
```

`dedupe_key` covers file id · size · mtime · target height · policy generation.
The worker still builds the canonical node-local `Recipe` immediately before
claiming cache output; the queue is scheduling identity, not byte identity.
Track selection and validated effective rate control continue through the
same `TranscodeManager` functions as live and offline production.

`requirements_json` is a versioned, bounded claim filter containing decoder,
acceptable encoder-family set, output contract, tone-map, output-grade, and
scratch-space requirements. It never requires only the candidate generator's
local QSV/NVENC/VideoToolbox family when another family can produce the same
contract. The claiming worker chooses its local family and includes that choice
in its recipe hash. A node that cannot satisfy the contract never claims the
row. Library deletion cancels active
rows and removes terminal rows in the media tombstone transaction. A ready row
names its recipe/storage result; if no verified complete location remains,
candidate generation may create a new active row for the same dedupe key.
Eviction therefore rewarms, while the partial unique index still prevents two
active copies.

### 5.2 Queue claims allocate their own fence atomically

`claim_pretranscode_job(node, capabilities, now)` moves one due row to running,
increments its fence, and returns its immutable source snapshot. Renew,
complete, yield, and fail compare id · owner · fence in their update. Staging
and final directories include the job fence/generation. The worker completes
the filesystem rename first, then one fenced transaction publishes that exact
generation's cache-location row and completes the job. An expired worker may
leave unreferenced bytes, but cannot replace an authoritative URI or publish a
location. Reconciliation later removes unreferenced generations. A successor
with node-local staging starts from zero rather than trusting inaccessible
partial files. That one transaction is also the crash boundary: it writes the
complete cache location plus `state='ready'`, `recipe_hash`, and `storage_id`.
A successor checks for that canonical result before encoding. No state may say
ready without a complete location or running after its own completed location
was accepted.

Completion also writes an immutable generation manifest containing format
version, generation id, object count, ordered names/sizes/digests, and one
manifest digest. Publication atomically points the cache location at that
manifest. Full object verification happens once at publication and later in a
background scrub, not on every placement request.

Retries use bounded exponential backoff and retain the last stable code. Five
failed attempts make the row terminal until a new source snapshot or policy
generation produces a new dedupe key. A transient capacity miss yields without
incrementing the failure count.

### 5.3 Existing live priority remains absolute

The distributed worker enters the same background admission lane as today's
producer. A live waiter causes checkpoint · terminate · permit release. It does
not suspend a GPU process, because a suspended background process still owns
the hardware session the viewer needs.

Offline preparation stays between live and speculative work:

```text
live playback  ▶  offline package  ▶  speculative pre-transcode
```

No worker may claim speculative work while it reports a live waiter, an active
live encoder, or offline work waiting for that encoder family.

## 6. Media placement — ask every node the question it can prove

### 6.1 Snapshots are private, bounded, and stale by construction

Generalize the authenticated M3e peer transport into one internal peer client.
Add:

```text
GET /internal/v1/media/snapshot
POST /internal/v1/media/offers
POST /internal/v1/media/sessions
ANY /internal/v1/media/sessions/{session_id}/...
```

Every request uses the existing cluster-scoped signature envelope. These
routes never accept a household bearer, never appear in public discovery, and
never log a capability URL or media path.

```rust
pub struct MediaNodeSnapshot {
    pub node_id: String,
    pub observed_at_unix_ms: i64,
    pub build: String,
    pub protocol_version: i64,
    pub encoders: Vec<EncoderOfferCapability>,
    pub tone_map: Vec<ToneMapOfferCapability>,
    pub hardware_slots_used: u32,
    pub hardware_slots_max: u32,
    pub software_threads_used: u32,
    pub software_threads_max: u32,
    pub scratch_bytes_free: u64,
    pub scratch_pressure: PressureBand,
    pub source_io_pressure: PressureBand,
    pub egress_pressure: PressureBand,
    pub live_waiting: bool,
    pub background_active: bool,
}
```

`PressureBand` is a fixed protocol enum (`idle`, `moderate`, `high`,
`unavailable`), not a peer-supplied arbitrary label. Thresholds are local and
reported in diagnostics so heterogeneous nodes can describe themselves safely.

Snapshots contain no usernames · titles · file paths · session ids · tokens.
Each node polls healthy peers every ten seconds with a two-second deadline and
expires a snapshot after fifteen seconds. A request never waits for the poll;
it reads the latest in-memory directory.

### 6.2 Offers normalize on the worker that would execute them

A heterogeneous ingress cannot safely guess another node's recipe hash or
tone-map graph. It sends an owned request containing file id · source snapshot
· target/track choices · output contract. The candidate loads replicated media
facts, uses proactively refreshed root-scoped readability facts to shortlist,
resolves its proven pipeline, computes its local recipe, and responds:

```rust
pub struct MediaOffer {
    pub node_id: String,
    pub request_fingerprint: String,
    pub eligible: bool,
    pub refusal_code: Option<String>,
    pub cache_hit: bool,
    pub source_likely_readable: bool,
    pub scratch_bytes_required: u64,
    pub scratch_bytes_free: u64,
    pub source_io_pressure: PressureBand,
    pub egress_pressure: PressureBand,
    pub method: DeliveryMethod,
    pub encoder: String,
    pub pipeline: String,
    pub free_hardware_slots: u32,
    pub free_software_threads: u32,
    pub recent_speed: Option<f64>,
}
```

Offer fan-out is parallel and bounded to 200 ms. A missing or late offer is an
ineligible node for this request, not a cluster-wide error. A proven local
cache hit may return immediately while other offers are cancelled; otherwise
the ingress uses fresh snapshots to shortlist compatible nodes before the
bounded fan-out. `cache_hit` is true only after a bounded check of the immutable
generation root and its atomically published manifest/digest, backed by a
short-lived generation-level integrity verdict. A replicated recipe or
location row alone is not proof. Serving validates the specifically requested
object against that manifest; any mismatch invalidates the verdict/location
and falls back. A validated complete cache hit may remain eligible when the
source mount is asleep because no encoder has to open the original.

Offer fan-out never opens every copy of a sleeping NAS path and only estimates
scratch headroom; offers reserve nothing. For live production, the selected
candidate atomically reserves encoder/software capacity plus scratch, then
performs the authoritative open/stat against the offered source snapshot.
Failure releases that single reservation and advances to the next candidate.
Scratch exhaustion is a hard refusal. Coarse bounded storage-I/O and egress
pressure bands participate in ordering, not high-cardinality measurements. A
stale source open, reservation loss, or disk-full result is a retryable
placement change under the same overall deadline.

### 6.3 Placement is lexicographic, not a pile of magic weights

Choose the maximum tuple in this order:

1. eligible and protocol-compatible;
2. complete cache hit;
3. can start now without waiting for background work;
4. scratch, source-I/O, and egress pressure band, least loaded first;
5. proven recent speed margin, descending;
6. spare capacity ratio, descending;
7. request-scoped rendezvous hash, spreading otherwise equivalent requests;
8. local node, avoiding a proxy when all material facts tie.

Hard constraints never become weights a lower-priority advantage can
outscore. A node without the required decoder or tone-map graph cannot win
because it is idle.

The selected node re-normalizes and atomically obtains a short local admission
reservation before starting. If its offer is stale, source/scratch changed, or
capacity was reserved first, it returns a stable retryable response. The
ingress then tries the next eligible candidate, without repeating one, until
one reserves or all candidates/one overall placement deadline are exhausted.
Cancellation and expiry release unused reservations. There is no unbounded
retry loop behind a player's spinner, and concurrent starts are spread before
they collide on the same worker.

### 6.4 Idempotency becomes cluster-wide before forwarding

When a request id is present, the ingress first claims
`(user_id, request_id)`, preserving today's attempt-level conflict semantics,
then resolves the current
`(user_id, playback_id)` pointer. It mints a random, never-reused incarnation
id before remote start. The session lease resource is
`session:<incarnation_id>`; playback ids are not fences.

```sql
CREATE TABLE media_session_requests (
    user_id             INTEGER NOT NULL,
    request_id          TEXT NOT NULL CHECK (
                            length(request_id) BETWEEN 1 AND 128),
    request_fingerprint TEXT NOT NULL,
    state               TEXT NOT NULL CHECK (
                            state IN ('starting', 'resolved', 'failed')),
    claim_expires_at_ms INTEGER NOT NULL,
    incarnation_id      TEXT,
    response_json       TEXT,
    updated_at_ms       INTEGER NOT NULL,
    PRIMARY KEY (user_id, request_id)
) STRICT;

CREATE INDEX media_session_requests_expiry
    ON media_session_requests(state, claim_expires_at_ms);

CREATE TABLE media_playback_pointers (
    user_id                INTEGER NOT NULL,
    playback_id            TEXT NOT NULL CHECK (
                               length(playback_id) BETWEEN 1 AND 128),
    current_incarnation_id TEXT NOT NULL UNIQUE,
    updated_at_ms           INTEGER NOT NULL,
    PRIMARY KEY (user_id, playback_id)
) STRICT;
```

An identical request retry waits only to the bounded claim deadline and then
returns the persisted, versioned normalized response or safely recovers the
same incarnation. A fingerprint conflict retains the current conflict error.
A missing request id remains a fresh, non-idempotent attempt, matching today's
POST and deprecated GET bridge; it skips `media_session_requests` but still
uses user-scoped playback supersession. Requiring request ids waits for a
versioned public API and migrated clients.
A request is normalized and length-checked before any consensus write. Limit
each user to 32 in-flight claims and 64 current/starting incarnations, with an
explicit stable overload error. Prune abandoned `starting` rows after their
claim deadline plus recovery window, failed rows after one hour, resolved rows
after the 24-hour retry window, and obsolete playback pointers with their ended
incarnations. Rate/spam tests assert bounded rows and WAL under endlessly
unique authenticated ids.
A new request for the same user/playback atomically changes the pointer and
ends the predecessor. Two users may use the same playback id without sharing
state. The selected worker receives no user bearer; the signed internal
request carries only the already-authorized user id and resolves the current
display name from replicated user state.

## 7. Owner proxy and takeover — the capability URL stays stable

### 7.1 `media_sessions` owns routing and the failover frontier

The replicated session row follows the canonical CLUSTERING-PLAN §3.5 fields
and adds lifecycle/fingerprint columns needed for routing:

```sql
CREATE TABLE media_sessions (
    incarnation_id                TEXT PRIMARY KEY,
    session_id                    TEXT NOT NULL UNIQUE,
    user_id                       INTEGER NOT NULL,
    playback_id                   TEXT NOT NULL,
    request_fingerprint           TEXT NOT NULL,
    owner_node_id                 TEXT NOT NULL,
    owner_epoch                   INTEGER NOT NULL CHECK (owner_epoch > 0),
    lease_expires_at_ms           INTEGER NOT NULL,
    state                         TEXT NOT NULL CHECK (
                                      state IN ('starting', 'active', 'ended')),
    recipe_json                   TEXT NOT NULL,
    produced_playable_through_ms  INTEGER NOT NULL,
    fetched_through_ms            INTEGER NOT NULL,
    media_origin_ms               INTEGER NOT NULL,
    media_sequence                INTEGER NOT NULL,
    discontinuity_sequence        INTEGER NOT NULL,
    updated_at_ms                 INTEGER NOT NULL
) STRICT;

CREATE INDEX media_sessions_owner
    ON media_sessions(owner_node_id, state, lease_expires_at_ms);
```

The server never reuses an incarnation id. After the request-retry window and
maximum child-shutdown window, ended detail/request rows and their session
lease rows may be pruned while the user/playback pointer is replaced or
removed. A stale publisher names the retired incarnation, finds no exact live
lease/session row, and is rejected; it cannot become current if the playback
id is later reused. Historical activity remains in its existing history
tables. CI gates bounded row/WAL growth plus playback reuse after pruning.

`recipe_json` is a versioned owned normalized-session wire object, not the
borrowed node-local cache `Recipe`. It exhaustively maps every current
byte-, timing-, and playlist-affecting field from `SessionRequest` and
normalized `StartInfo`: file/source snapshot · delivery method · Auto intent ·
height/bitrate and effective rate-control · output grade/HDR10 · Dolby Vision
preservation · audio action/index/rate/channels/offset · subtitle burn and
native-HLS rendition identity · master-playlist selection · tone-map
requirement. The survivor selects a locally valid encoder and computes its own
pipeline digest without changing that contract. A compile-time conversion and
serialized fixture fail when a new relevant request/start field has no
explicit wire decision.

### 7.2 Proxy first; redirects wait for clients that carry node lists

An external playlist, segment, subtitle, status, or delete request checks the
local session map, then a bounded in-memory `session_id → owner/epoch` routing
cache populated at creation or the first owner lookup. Ordinary playlist and
segment requests use that cache without reading Raft. TTL expiry,
`wrong_owner`, lease-expired, and takeover notifications trigger one consistent
owner-row repair before retry. The ingress streams the signed peer response,
preserving status · content type · content length · range semantics · ETag ·
cache control. Do not buffer a segment in memory and do not expose an internal
address to the client.

These are typed proxies, not a general header tunnel. Requests allow only the
route's Range and conditional headers; responses allow only the documented
content, range, validator, length, disposition, and cache headers. Strip Host,
Connection and every hop-by-hop header, household Authorization, cookies, and
forwarded identity. Synthesize peer authentication, binding its signature to
method, normalized path, query, body digest, timestamp, and nonce. Tests send
forbidden headers and secrets and assert that neither peer requests nor logs
contain them.

P5 also removes raw capability/session ids from daemon logs, traces, metrics,
and peer diagnostics. Those surfaces use a non-secret correlation id or keyed
digest; tests assert that neither a full capability URL nor its raw session id
appears on success, error, proxy, delete, or drain paths.

DELETE first calls `end_session` on the authoritative state machine. That CAS
marks the incarnation ended and invalidates renewal/takeover before returning;
stopping the observed worker is best-effort cleanup. Repeated deletes are
idempotent, and a delete racing takeover resolves to ended without a replacement
child remaining alive.

Proxying is the compatibility baseline for Safari, Apple TV, Android, Plex
clients, and a reverse proxy with no session awareness. A later 307 path is
allowed only for clients that already carry the node list and have passed the
same interruption corpus.

### 7.3 Takeover publishes one new generation after a fenced claim

Each owner node submits at most one bounded consensus transaction every two
seconds containing renewal plus both frontiers for all of its changed sessions.
Playlist and segment requests never write consensus state. The batch validates
each incarnation/owner/epoch independently, so one stale member cannot reject
the others. After expiry, voters run bounded takeover offers from the persisted
normalized contract. A candidate must prove exact pipeline/source/scratch
eligibility and hold a short local admission reservation before it may call
`claim_session`; losers release or expire their reservations. Only that eligible
candidate can CAS the active incarnation to the next owner epoch. It restarts
one complete segment before `fetched_through_ms`, starts numbering at the
replicated `media_sequence`, emits one discontinuity, and increments
`discontinuity_sequence`.

The old prefix is sequencing metadata, not proof that its bytes survived. The
replacement playlist advertises only overlap and new-generation objects present
on the survivor (or on a separately verified shared holder). fMP4 creates a
generation-specific init URI and `EXT-X-MAP`; it never overwrites an init object
a client may have cached. With the old node and disk absent, every URI in every
newly served playlist must still be retrievable.

Every durable frontier/cache-location pointer publication compares the owner
epoch in the same transaction. Playlist and segment objects use immutable
generation URIs; serving them checks the locally tracked epoch and successful
renewal deadline without adding a consensus write. If the old owner wakes after
`SIGSTOP`, its next write/serve is rejected and it kills its child. No URI may
name different bytes at two points in one session lifetime.

Renew uncertainty immediately stops new playlist/segment publication and the
mutable child, no later than the owner's locally tracked lease deadline. A
longer readiness or process-shutdown grace applies only to read-only work that
cannot conflict with a successor.

Direct play takeover is a range retry against the same source snapshot. Remux
and transcode takeover use the same owner/epoch machinery but build their own
replacement child. The web client consumes the proven server response first;
Apple and Android follow only after the web corpus is green.

### 7.4 Shared cache has storage identity and distributed reader pins

P6 migrates producer-keyed locations to storage-keyed locations. A local root
gets `storage_id = node:<node_id>:cache`; a verified shared root gets a stable
id derived from cluster id plus the operator's shared id. Existing locations
migrate one-for-one to their producer's local storage id.

```sql
CREATE TABLE cache_storage_members (
    storage_id          TEXT NOT NULL,
    node_id             TEXT NOT NULL,
    storage_class       TEXT NOT NULL CHECK (
                            storage_class IN ('local', 'shared')),
    verified_at_ms      INTEGER NOT NULL,
    verification_state TEXT NOT NULL,
    PRIMARY KEY (storage_id, node_id)
) STRICT;

CREATE TABLE cache_consumer_pins (
    storage_id       TEXT NOT NULL,
    recipe_hash      TEXT NOT NULL,
    generation_id    TEXT NOT NULL,
    consumer_kind    TEXT NOT NULL CHECK (
                         consumer_kind IN ('media_session', 'offline_package',
                                           'offline_download')),
    consumer_id      TEXT NOT NULL,
    consumer_epoch   INTEGER NOT NULL,
    expires_at_ms    INTEGER NOT NULL,
    PRIMARY KEY (
        storage_id, recipe_hash, generation_id, consumer_kind, consumer_id)
) STRICT;
```

The cache-location key and APIs become
`(recipe_hash, storage_id, generation_id)` with member lookup separated from
producer identity. Pin acquisition and location validation are one transaction:
it inserts/renews the typed consumer pin only if that exact generation remains
complete and current. Failure forces routing fallback. Media sessions renew
one pin in the node's existing owner-liveness batch; they do not write per
segment. Offline package creation and active downloads use the same typed pin
contract, so shared GC cannot invalidate any durable consumer. Exactly one holder of
`shared-cache-gc:<storage_id>` may delete shared generations, and only after
excluding live pins for the exact generation in the same fenced transaction
that retires its location. Pin-acquire and retire therefore serialize on the
same generation identity. Filesystem deletion follows pointer retirement and
uses immutable generation paths. Shared deletion stays disabled during rolling
activation until every voter and offline path supports pins; local LRU behavior
remains process-local.

The node-local TOML surface is
`cluster.shared_cache_dir`/`cluster.shared_cache_id`, deliberately under the
forward-compatible `[cluster]` section rather than strict `[storage]`. Older
binaries ignore these keys on rollback. The directory is never written to the
replicated settings store; peers see only storage id and verification state.

## 8. Milestones — one invariant per mergeable PR

### 8.1 P0 — land this contract and its measurement manifest

Add this document to the README reading path and record the benchmark
scenarios/metrics that later milestones must populate. Resolve adversarial
review findings in the same PR; a design finding deferred without a named
milestone is unresolved.

**Acceptance:** documentation links resolve · every milestone has a runnable
or observable gate · `git diff --check` passes · PR CI is green.

### 8.2 P1 — implement the backend-neutral lease primitive

Add `CoordinationStore`, SQLite and Hiqlite schemas/implementations, the
monotone release rule, `FenceRejected`, and backend-neutral contract tests.
Do not wire scheduler behavior yet. This slice may proceed before §2.2 if it
avoids their files.

**Acceptance:** PR CI proves both backends agree on acquire · held · renew ·
release · expiry takeover · monotone fence · stale renewal; the real-process
cluster harness proves two voters racing after expiry produce one winner.

### 8.3 P2 — fence singleton scheduler work

Acquire named leases at the common scan/refresh/probe/provider/genre/candidate
execution boundaries used by scheduled, startup, manual, and integration
triggers. Add transaction-level fences to every durable publishing method.
Keep telemetry prune, local cache cleanup, and artwork byte materialization
local. Generalize shared/provider artwork scheduling only after PR #490's
rules are present.

**Acceptance:** pause an owner past TTL, let a successor acquire, resume the
old process, and prove its next publication is rejected; three simultaneous
scheduler ticks produce one scan/provider pass.

### 8.4 P3 — turn pre-transcode candidates into a distributed queue

Add `pretranscode_jobs`, singleton candidate enqueue, worker claim/renew/yield/
complete, and node-local cache publication. Reuse the existing producer,
admission lane, checkpoint parts, and stop control. Do not add shared cache or
live placement in this PR.

**Acceptance:** three idle workers claim distinct whole-title jobs; a killed
worker's node-local job restarts elsewhere; incompatible workers cannot claim
it; a stale worker cannot publish; location plus ready state is atomic; source
deletion cancels it; eviction re-enqueues it; the generation manifest detects
a corrupt requested object without an offer-time full walk; one live start preempts
speculative work inside five seconds; the resulting local cache hit starts
with no ffmpeg child.

### 8.5 P4 — publish media snapshots and bounded offers

After PR #420, refactor its peer client into a shared authenticated transport,
add snapshot polling and offer fan-out, and expose placement diagnostics without
changing which node starts a session. This isolates observation from action.

**Acceptance:** forged/stale signatures fail · snapshots expire · one slow
peer cannot exceed the 200 ms offer budget · heterogeneous fixtures reject an
incapable idle node and prefer a byte-verified cache holder · scratch-full,
stale-source, and saturated-egress nodes do not win.

### 8.6 P5 — place and proxy new HLS sessions

Add cluster-wide session idempotency, remote start, owner lookup, and streamed
playlist/segment/status/delete proxying. Implement the bounded two-second
per-owner batch that renews session liveness; it need not publish takeover
frontiers yet. Keep failover disabled: an owner that actually expires still
produces the existing terminal error until §8.8. At drain start, a node stops
offering immediately, cancels unused reservations, and adds owned sessions and
active reservations to PR #490's removal barrier. P7 may transfer sessions;
P5 safely refuses removal until they end.

**Acceptance:** a request entering a GPU-less node starts on the capable node;
six simultaneous starts consume free slots across three two-slot workers before
any capacity refusal; segment bodies stream without full buffering; a repeated
request returns the persisted response for one user/session while conflicting
fingerprints and cross-user playback ids retain their semantics; legacy POST
and GET starts without request ids still work; client identity spam remains
bounded; forbidden proxy headers and raw capability ids never cross or enter
logs; authoritative delete wins against a concurrent
start and reaches the worker; healthy sessions remain live beyond the lease;
a draining node receives no starts and cannot be removed while it owns one.

### 8.7 P6 — add verified shared-cache roots as an optional fast path

First ship local-holder routing. Then add the storage-id migration,
`cluster.shared_cache_dir`/`cluster.shared_cache_id`, an authenticated two-way
canary proof that nodes see the same writable filesystem, distributed pins,
and fenced shared GC. Extend P5's owner-liveness batch with typed pin renewals.
A path string alone is not proof of a shared mount. The
canary is an admission proof, not a permanent guarantee: every serve still
validates named bytes. Runtime `ENOENT`, I/O, or identity failure disables
shared classification, marks the location suspect, and falls back to a verified
holder or production until a later canary passes.

**Acceptance:** a node that did not produce an entry serves it from the shared
root without proxying; mismatched or missing mounts fail the canary and fall
back to local-holder routing; a remote reader's session pin prevents deletion;
offline package/download pins also prevent deletion; pin acquisition racing GC
has one winner; loss after startup disables shared classification; rolling
rollback ignores node-local keys safely.

### 8.8 P7 — implement web session takeover

Extend the existing owner-liveness batch with frontiers/sequence state,
self-fence on renewal loss, run eligible-survivor placement, claim on that
survivor, and stitch the replacement generation. Add web node-list retry only
after proxy takeover passes.

**Acceptance:** direct play resumes by a byte-correct range retry against the
same source snapshot. For copy-remux and transcode, including owner loss during
a segment response, decoded media resumes within ten seconds with exactly one
HLS discontinuity, monotone progress, a generation-specific fMP4 map, no reused
URI with different bytes, and every newly advertised URI retrievable after the
old disk disappears. An incapable survivor cannot claim; delete racing takeover
leaves no replacement child; 80 concurrent sessions stay within the named Raft
budget.

### 8.9 P8 — finish operations and native-client consumption

Add load-balancer/keepalived/Kubernetes routing examples, cluster media status,
documentation for the shipped rolling-drain barrier, and backup implications.
Then implement the same node-list
retry semantics in Apple and Android without translating node loss into their
codec-compatibility fallback ladders.

**Acceptance:** PR CI plus the named physical-device corpus passes; one-voter
mode retains the same routes and no peer polling; an operator can explain every
placement, proxy, queue, fence rejection, and takeover from Settings/metrics.

## 9. Failure behavior — degraded must remain correct

| Failure | Required behavior | Forbidden behavior |
|---|---|---|
| Peer snapshot/offer timeout | Exclude that offer; use another eligible node or the existing local refusal | Wait indefinitely or guess the peer is capable |
| Selected capacity changes | Try each remaining offer at most once inside one overall deadline | Retry a node twice or retry forever |
| Source path missing on candidate | Reject live production there; a verified complete cache may still serve | Treat a replicated path as proof of a mount |
| Quorum lost | Fail readiness; stop mutable serving immediately on renewal uncertainty and no later than the local lease deadline; grace only read-only shutdown | Continue serving mutable session state as if writes succeeded |
| Lease owner paused | Successor takes higher fence; old owner is rejected on resume | Accept both because their wall clocks disagree |
| Cache row exists but bytes do not | Mark the location suspect, try a verified holder or live production | Return a playlist that will 404 on its next segment |
| Shared-root canary fails | Disable shared classification on that node and retain local/proxy behavior | Join paths with the same spelling and assume the storage is shared |
| Mixed protocol versions | Exclude incompatible peers and refuse unsafe rolling activation | Send a newer recipe/session shape and hope unknown fields are ignored |
| Owner dies mid-response | Retry/proxy to the new owner and preserve sequence semantics | Reuse the old segment URI for replacement bytes |

## 10. Observability — every optimization explains itself

Add bounded labels only; node ids and reason enums are bounded by cluster size
and code vocabulary, while session/file/title values never become labels.

```text
plurx_cluster_media_placement_total{reason,method,remote}
plurx_cluster_media_offer_seconds
plurx_cluster_media_offer_failures_total{reason}
plurx_cluster_proxy_active
plurx_cluster_proxy_bytes_total{kind}
plurx_cluster_proxy_failures_total{reason}
plurx_cluster_job_leases{kind}
plurx_cluster_fence_rejections_total{kind}
plurx_pretranscode_jobs{state}
plurx_pretranscode_takeovers_total
plurx_media_session_takeovers_total{method,outcome}
plurx_media_session_takeover_seconds{method}
plurx_media_session_batch_size
plurx_media_session_batch_commit_seconds
plurx_media_session_wal_bytes_total
```

The activity/UI projection names ingress node · owner node · placement reason ·
cache scope · proxy yes/no. A healthy shape is explainable: “entered through
node A, cache hit on node C” is useful; “cluster optimized” is not.

## 11. Upgrade and rollout — compatibility before cleverness

1. **Schema-only lease support first.** The first schema step adds coordination
   tables without changing peer HTTP. Re-verify the then-current replicated
   schema number and add an explicit previous-version migration; do not assume
   `v7` remains current after the M3 dependencies merge.
2. **Protocol bumps follow wire changes.** Media snapshots/offers/session RPCs
   bump the cluster protocol. A node advertises compatibility before it may
   receive work.
3. **Feature activation is cluster-wide.** `cluster.media_pool_enabled` defaults
   off through observation-only §8.5, then may be enabled only when every voter
   supports the protocol. One-node mode behaves exactly as today.
4. **Placement and takeover have separate switches.** Placement may ship while
   takeover remains dark; a failed owner then follows the existing terminal
   behavior rather than a half-implemented splice.
5. **Rollback never reads newer session/queue rows with an older binary.** The
   maintenance window and marker rules in OPERATIONS apply. Mixed-version
   routing is refused instead of silently downgrading a session contract.

Configuration introduced by this plan:

| Key | Scope | Default | Meaning |
|---|---|---|---|
| `cluster.media_pool_enabled` | replicated policy | `0` | Enable remote offer selection and start forwarding after every voter is compatible |
| `cluster.session_takeover_enabled` | replicated policy | `0` | Enable owner lease expiry and replacement generation after the failover corpus passes |
| `cluster.shared_cache_dir` | node-local TOML | empty | Optional completed-cache root mounted on multiple nodes |
| `cluster.shared_cache_id` | node-local TOML | empty | Operator-chosen identity confirmed by the two-way canary; same path text is insufficient |

Shared paths remain node-local configuration; secrets and filesystem layout
never enter replicated settings.

## 12. Non-goals — the first release stays whole-title and LAN-shaped

1. **Do not split one encode across nodes.** Whole-title queue parallelism uses
   every GPU without PTS reset, audio join, and cross-worker muxing seams.
2. **Do not replicate cache bytes through Raft.** Consensus is for ownership
   and publication facts, not multi-gigabyte regenerable outputs.
3. **Do not make shared cache mandatory.** Local NVMe plus owner routing is a
   complete supported mode and often the lower-latency one.
4. **Do not require byte-identical mixed encoders.** Takeover changes generation
   at an explicit discontinuity; node-local recipe hashes stay honest.
5. **Do not place direct play by CPU load.** Direct bytes are storage/network
   work; a GPU score is irrelevant.
6. **Do not expose peer addresses or cluster credentials through public status.**
   Public clients receive the deliberately designed node-list contract only.
7. **Do not turn placement into adaptive quality.** This plan chooses a worker
   for the already-decided output contract; Performance II owns rung changes.

## 13. Delivery discipline — review and CI are part of the implementation

Each milestone uses a fresh branch from updated `origin/main`, one or more
cohesive commits, and one PR. Behavior and its documentation land in the same
commit. Before opening or updating a PR:

1. run an independent adversarial agent review against the diff and the
   contracts in this document;
2. fix every correctness, safety, security, compatibility, and missing-test
   finding or record the exact later milestone that owns it;
3. run only cheap non-duplicative local hygiene (`cargo fmt --all -- --check`
   when Rust changed · `git diff --check` always);
4. let PR CI run the test matrix once, inspect the first causal failure, fix
   it, and avoid rerunning unchanged green work locally;
5. watch the required checks through the aggregate gate, then merge only when
   green and mergeable.

No later milestone starts from an unmerged predecessor branch. This keeps the
sequence reviewable and prevents a passing stack from hiding which invariant a
commit actually introduced.
