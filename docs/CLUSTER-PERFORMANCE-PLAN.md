# Cluster performance — turn replicated correctness into useful capacity

**Status:** ready to build · **Extends:** [CLUSTERING-PLAN.md](CLUSTERING-PLAN.md)
after functional multi-voter membership · **Written:** 2026-08-21 against
`main` @ `aee2cbe0`

Companion to [ARCHITECTURE.md](ARCHITECTURE.md) (why plurx embeds Raft),
[OPERATIONS.md](OPERATIONS.md) (how to operate membership), and
[PERF2-PLAN.md](PERF2-PLAN.md) (playback-path performance) — this plan makes a
working multi-voter cluster responsive under ordinary API, catalogue, and
playback load. Read §3 before changing a consistency boundary, then execute
§6 one pull request at a time. Re-verify every source path and literal against
the branch being built; if a step appears to require stale authorization,
unacknowledged durable writes, or an unhealthy follower serving traffic, stop
and amend the contract rather than improvising.

## 1. Objective — spend consensus only where correctness needs it

Raft is the authority for state whose acknowledged loss would be a defect. It
is not the request router, the transcode scheduler, or a reason to send every
read and activity timestamp through the leader. The completed work must:

1. preserve the existing promise that a successful Store write is
   quorum-acknowledged;
2. preserve immediate, fail-closed authorization and membership decisions;
3. make three voters the normal minimum-HA topology and document what a fourth
   voter costs;
4. let healthy followers serve reads whose callers explicitly tolerate bounded
   staleness;
5. remove avoidable Raft entries produced by activity bookkeeping;
6. expose enough bounded telemetry to distinguish network, disk, leader, read,
   and application latency before any timer is tuned; and
7. use additional machines for HTTP, streaming, and transcode capacity without
   requiring every machine to increase the voting quorum.

The first optimization is less work, not looser durability.

## 2. Evidence — the current shape leaves capacity on the table

### 2.1 Four voters pay for a larger quorum without surviving another loss

The quorum is `floor(voters / 2) + 1`:

| Voters | Quorum | Failures tolerated | A write needs |
|---:|---:|---:|---|
| 3 | 2 | 1 | leader + fastest one of two followers |
| 4 | 3 | 1 | leader + fastest two of three followers |
| 5 | 3 | 2 | leader + fastest two of four followers |

Three and four voters both tolerate one unavailable node. Four voters add one
replica's network, apply, storage, heartbeat, and snapshot work while moving
the commit point from the fastest follower acknowledgement to the second
fastest. Five voters are justified only when surviving two voter losses is an
explicit requirement.

This plan does not automatically remove an operator's fourth voter. §6.1 first
records a comparable four-versus-three-voter workload, then the operator
removes a follower through the existing safe membership API. The current
leader is never a removal target.

### 2.2 Authentication performs consensus bookkeeping on the request path

`AuthUser::from_request_parts` calls `Store::user_for_token` for every
authenticated request. The Hiqlite implementation performs a consistent user
lookup and then submits this statement through Raft:

```sql
UPDATE tokens SET last_seen_at = $1
WHERE token_hash = $2 AND last_seen_at < $3
```

The predicate prevents a SQLite row change inside the state machine, but it
does not prevent the command from becoming a Raft log entry. A segment, image,
or JSON request inside the 60-second activity window therefore still pays for
a no-op consensus write.

Scoped API keys have the same shape with no SQL-side window at all:

```sql
UPDATE api_keys SET last_used_at = $1 WHERE id = $2
```

`last_seen_at` and `last_used_at` are operator activity hints, not authorization
facts. They must remain durable enough to be useful, but they do not earn one
physical commit per request.

### 2.3 Most replicated reads still rendezvous at the leader

At the plan baseline, the `hiqlite*.rs` Store implementation contains about 85
consistent-query call sites and 13 local-query call sites. Hiqlite's
`query_consistent_map` runs on the leader, takes network round trips, and pauses
Raft at a quorum-applied point. Adding HTTP nodes does not distribute that read
work when ordinary catalogue calls retain this primitive.

The opposite blanket change would also be wrong. A partitioned follower can be
arbitrarily stale, so authorization, admission, leases, membership, and
write-followed-by-read behavior cannot silently become local reads. §3.2 makes
the classification explicit and §6.4 adds a lag gate before callers may opt
in.

### 2.4 Durable state and heavy scratch I/O share one configured root

`storage.data_dir` contains `hiqlite/`, the content-addressed cache, offline
packages, subtitle cache, and live transcode work. Operators can place child
directories on separate mounts, but plurx has no first-class durable-versus-
scratch path contract. A cache sweep, offline preparation, or ffmpeg write can
therefore contend with the log and state-machine database on one device.

The Hiqlite node is also built with fixed production choices: a 16 MiB WAL, a
10,000-entry snapshot policy, the default local read pool of four connections,
500 ms heartbeats, and 1.5–3 second election timeouts. These are coherent fast-
LAN defaults. They are not evidence that exposing all of them as knobs will
improve a real workload.

### 2.5 Existing metrics describe playback better than the Store

`GET /metrics` exposes playback TTFF, stalls, sessions, cache results, offline
work, and bounded application counters. Replication status is visible in the
admin system projection, but Prometheus cannot currently answer:

- which Store class is slow: local read · consistent read · write;
- how many requests were forwarded to the leader;
- how long a quorum write or consistent read took;
- whether commit-to-apply lag grew on this node;
- whether leader changes correlate with CPU, disk, or network pressure; or
- how much work auth-touch suppression removed.

Timer tuning without those answers would trade one unexplained symptom for
another.

## 3. Contract — performance does not weaken the HA promise

### 3.1 Writes remain durable at the Store boundary

Every successful Store mutation remains quorum-acknowledged. No milestone may:

- acknowledge a durable write before Hiqlite reports success;
- switch the durable WAL to interval sync or memory-only storage;
- move user, settings, watch, library, membership, or offline ownership rows
  into a node-local cache; or
- hide a failed write behind an optimistic HTTP response, except for the
  already-documented intermediate progress coalescer.

Coalescing is allowed only when the API contract already treats the field as
activity or replaceable progress. The coalescer must publish its durability
boundary in code and documentation.

### 3.2 Every read belongs to one named consistency class

| Class | Primitive | Allowed examples | Refused examples |
|---|---|---|---|
| `Authority` | leader/quorum-consistent | token and key validation · settings that gate work · membership · leases · offline ownership · migration/version guards | catalogue cards solely because the existing code used a consistent helper |
| `ReadYourWrite` | return the write result, revision-fenced local read, or consistent fallback | a settings save response · watch/unwatch response · membership mutation result | an unfenced follower query immediately after mutation |
| `BoundedReplica` | local query only with a fresh quorum-confirmed leader watermark and applied lag within the declared budget | catalogue browse · item/file metadata · genre/filter facts · dashboard aggregates | auth · locks · admission · ownership · schema state |
| `NodeLocal` | process or node-local storage | playback telemetry · transcode scratch · regenerable indexes and caches | any acknowledged user or library fact |

The code should name the class at the call site. A generic helper named only
`query` or `query_map` is insufficient once two correctness contracts exist.

`BoundedReplica` requires two bounds. First, the node must hold a leader-issued
watermark containing `(term, leader_id, committed_index, issued_at)` that was
renewed by a successful quorum `ReadIndex` or equivalent heartbeat proof. The
term and leader identity must match, and the proof may be at most one second
old initially. An isolated former leader cannot renew it. Second, the local
applied index must be known and no farther behind that committed index than the
configured entry budget. The time lease bounds commits made after the sampled
index; the entry gate bounds apply backlog at the sample. A node that cannot
prove every fact falls back to an `Authority` read or fails readiness according
to the caller's contract. Local `metrics_db()` state and
`last_log_index - last_applied` alone are explicitly insufficient: they can
look current on a partitioned follower while a new majority keeps committing.

### 3.3 Authorization remains immediately revocable

Token and API-key lookup remains an `Authority` read until an invalidation
protocol independently proves an equivalent revocation guarantee. The first
auth optimization only suppresses no-op activity writes:

- include the durable activity timestamp in the consistent lookup;
- submit a touch only when that timestamp is outside the 60-second window;
- reserve that credential in a process-local keyed singleflight before the
  mutation and release the reservation if the write fails;
- retain the SQL predicate as the race-safe final guard; and
- bound a concurrent window-edge burst to at most one submitted mutation per
  serving process. Across `N` HTTP nodes, at most `N` entries may race; the
  SQL predicate permits only one timestamp change.

A future successful-auth cache requires explicit revoke notifications, a
bounded fallback TTL, loss/reconnect tests, and fail-closed behavior. It is not
part of the first implementation slice.

### 3.4 Readiness means this node is safe for the traffic it receives

`/healthz` remains process liveness. `/readyz` remains the load-balancer
surface, but its replicated-store answer must eventually include local apply
health once `BoundedReplica` reads ship. A node with a live process and leader
connectivity but excessive local apply lag must not serve stale catalogue
traffic merely because a remote consistent ping succeeded.

Streaming requests should be sticky to preserve process-local sessions and
cache warmth. Stickiness is an optimization, not an availability dependency:
when a node fails readiness, the load balancer may send the next request to a
survivor and plurx's deterministic segment contract remains responsible for
recovery.

### 3.5 Configuration is symmetric and bounded

Every Raft setting must be identical on all voters. New config fields require:

- one safe default matching today's behavior;
- a documented range and unit;
- startup refusal for invalid combinations;
- an operations entry describing when the setting takes effect; and
- a cluster-upgrade procedure that never runs incompatible timer sets
  indefinitely.

The first tunable is `read_pool_size`, after local reads create evidence that
the pool is a limit. Election and snapshot settings stay internal until §6.6
shows a repeatable need.

## 4. Target shape — three voting replicas, distributed application work

```text
                         clients
                            │
                readiness + sticky streams
                            ▼
                 ┌─────────────────────┐
                 │ reverse proxy / VIP │
                 └──────┬──────┬───────┘
                        │      │
             ┌──────────┘      └──────────┐
             ▼                            ▼
      ┌─────────────┐              ┌─────────────┐
      │ voter A     │◀──── Raft ───▶│ voter B     │
      │ HTTP + work │               │ HTTP + work │
      └──────┬──────┘               └──────┬──────┘
             │          Raft               │
             └──────────┬──────────────────┘
                        ▼
                 ┌─────────────┐
                 │ voter C     │
                 │ HTTP + work │
                 └─────────────┘

     optional fourth machine after §6.7:
       non-voting learner/read worker · proxy · metrics · backup
```

Every voter must be capable of leadership: comparable durable storage, stable
wired networking, and CPU/I/O headroom protected from ffmpeg saturation. A
leadership preference is not a substitute for making all voters safe leaders.

## 5. Measurement contract — establish causality before tuning

### 5.1 Bounded Prometheus surface

Add these families with fixed label vocabularies:

| Metric | Type | Labels | Source and meaning |
|---|---|---|---|
| `plurx_store_operation_seconds` | histogram | `class`, `outcome` | `TimedClient`: local read · authority read · write latency |
| `plurx_store_operations_total` | counter | `class`, `outcome` | `TimedClient`: volume and error rate by consistency cost |
| `plurx_auth_activity_writes_total` | counter | `kind`, `result` | auth coalescer: token/key touch submitted · suppressed · failed |
| `plurx_raft_commit_index` | gauge | none | most recent quorum-confirmed leader watermark; meaningful only while its validity gauge is `1` |
| `plurx_raft_applied_index` | gauge | none | OpenRaft local metrics watch: latest entry applied on this node |
| `plurx_raft_apply_lag_entries` | gauge | none | saturating watermark commit minus local applied; meaningful only while both sources are valid |
| `plurx_raft_leader_changes_total` | counter | none | OpenRaft metrics watch: process-observed leader identity changes |
| `plurx_raft_snapshot_seconds` | histogram | `outcome` | explicit snapshot build/install start-to-finish hooks, not monitor polling |
| `plurx_raft_metric_sample_errors_total` | counter | `source` | failed samples from the fixed `watermark` · `local` · `snapshot` vocabulary |
| `plurx_raft_metric_sample_age_seconds` | gauge | `source` | age of the last successful sample from each fixed source |
| `plurx_raft_metric_sample_valid` | gauge | `source` | `1` only while the source is present and inside its freshness contract |

Do not label by SQL, route, token, user, node UUID, item, session, or dynamic
error text. Those values either leak data or create unbounded series.

### 5.2 Comparable scenarios

Each benchmark run records the build SHA, voter count, leader, request target,
node hardware, storage device, network path, dataset size, concurrency, and raw
result. At minimum:

| Scenario | Load | Readout |
|---|---|---|
| auth burst | one token · 120 authenticated requests in 60 s | physical activity commits · p50/p95/p99 request latency |
| browse | home, library, item, and search mix against each node | authority/local counts · p50/p95/p99 · follower lag |
| progress | existing 80-stream deterministic coalescer fixture | incoming beats · physical commits · compacted bytes |
| topology | identical write mix on four voters, then three | p50/p95/p99 write latency · CPU · network · disk |
| follower loss | kill one non-leader under load | errors · latency peak · recovery time |
| leader loss | kill the reported leader under load | failed requests · election time · time until readiness |
| snapshot | cross two 10,000-entry snapshot cycles | write tail latency · snapshot duration · retained bytes |

P0 runs each scenario three times on the named cluster-performance runner and
commits the median baseline plus raw artifacts under a versioned schema. It
also records concrete go/no-go limits before later behavior PRs begin. The
initial limits are: zero additional request errors; no more than a 10% p99
regression in unaffected Store classes; less than 2% CPU and wall-time overhead
from metrics-only P2; at least a 95% reduction in sequential auth activity
entries for P1; and at least a 70% reduction in authority catalogue reads for
the P3 slice without a greater than 10% browse p99 regression. P0 replaces
these provisional percentages only with reviewed numbers justified by runner
variance.

Deterministic CI contracts enforce correctness and physical-entry bounds.
Hardware results enforce the recorded comparative budgets; noisy benchmark
percentiles do not become unit-test assertions. Until that baseline exists, a
PR must not claim an absolute latency improvement from a synthetic
microbenchmark alone.

## 6. Milestones — one correctness boundary per pull request

### 6.1 P0 — document and measure the three-versus-four-voter choice

**Change:** After membership PRs #490 and #491 land, rebase before touching
their shared cluster harness and `OPERATIONS.md` surfaces. Add the odd-voter
guidance, reverse-proxy readiness contract, sticky stream recommendation, and
durable-versus-scratch storage layout to the authoritative operations text.
Extend the cluster benchmark harness to run the same write mix with three and
four voters and emit the §5.2 record.

Do not remove a live voter from automation. The operator selects the follower
after confirming it owns no in-flight transfer and the existing removal API
accepts the change.

**Acceptance:** CI's replicated-storage contract passes; the benchmark artifact
contains both voter counts and reports quorum, commit percentiles, lag, and
resource use with the same dataset and load. The named runner, raw three-run
results, median baseline, variance, and reviewed comparative budgets are
committed before P1-P3 performance claims are accepted.

### 6.2 P1 — suppress no-op auth activity writes

**Change:** In `HiqliteAuthStore::user_for_token`, return the durable
`last_seen_at` beside the user and skip `execute` inside the 60-second window.
Apply the same due check to API-key `last_used_at` before the extractor calls
`touch_api_key`. Add a process-local per-credential singleflight reservation
before either mutation so concurrent requests on one process submit only one
entry; clear the reservation on write failure. Keep both authority lookups and
the SQL race predicates.

Pin the predicate as `last_activity < now - 60`: never-used is due, 59 seconds
and exactly 60 seconds are suppressed, 61 seconds is due, and a clock rollback
suppresses the best-effort touch. Disabled, deleted, or otherwise unauthorized
credentials never reserve or touch. Add real replicated-store budget tests that
compare the Raft log delta for sequential and simultaneous requests. One
process may submit at most one touch per credential per window; `N` serving
processes may submit at most `N`, independent of request count.

**Acceptance:** the `cluster.auth` CI point and HTTP auth matrix pass; revocation
remains immediate; 120 sequential requests inside one window produce no more
than one physical activity entry after the initial due read. A synchronized
120-request burst on each of three HTTP nodes produces no more than three
entries and exactly one durable timestamp change.

### 6.3 P2 — instrument Store and Raft cost

**Change:** Instrument `TimedClient` once so every replicated Store module uses
the same bounded local-read · authority-read · write histograms and counters.
Extend the OpenRaft/Hiqlite metrics boundary with the explicit §5.1 sources:
local applied/leader observations from the metrics watch, quorum-confirmed
commit watermarks from a bounded background sampler, and snapshot duration
from build/install start-and-finish hooks. `ReplicationMonitor` projects those
samples but does not invent unavailable values. Expose them through the
existing `/metrics` body.

Metrics collection must be non-blocking and must not execute a Store operation
to describe a Store operation. A failed sample preserves the last known value,
sets its validity gauge to `0` after the freshness limit, publishes sample age,
and increments the bounded source error counter rather than delaying the
request path.

**Acceptance:** exact exposition tests prove HELP/TYPE lines, fixed labels,
monotonic counters, age/validity transitions, and absence of SQL or identifiers.
Cluster CI proves a write, consistent read, and local read each increment only
their class. The named performance runner rejects instrumentation above the P0
overhead budget.

### 6.4 P3 — add lag-gated bounded-replica reads

**Change:** Introduce named authority and bounded-replica helpers. Start with a
small catalogue slice: library list · item/file lookup · recently added · genre
and technical aggregates. Keep search local as it is today. Do not move watch
state, authentication, settings, leases, membership, jobs, cache ownership, or
offline state.

Before a bounded-replica query, require the fresh quorum-confirmed term/leader/
commit watermark and local applied-index proof from §3.2. If either proof is
absent, mismatched, expired, or outside the entry budget, use an authority read.
Extend readiness so a persistently unproved or lagged node leaves load-balancer
rotation before it serves stale browse traffic.

Write-followed-by-read handlers return the write result or use authority until
a revision fence exists. They do not rely on timing.

**Acceptance:** three-voter tests pause one follower's apply path, prove that it
falls back or becomes unready, resume it, and prove local reads return only
after the lag gate recovers. A separate test partitions one follower from the
leader while the remaining majority continues committing; the isolated node
must stop local reads before the one-second watermark lease expires even when
its local log/apply gap is zero. Existing store parity remains byte-identical
after follower and leader loss. Metrics meet the P0 authority-read reduction
and browse p99 budgets instead of merely reporting them.

### 6.5 P4 — coalesce remaining replaceable write traffic

**Change:** Inventory every recurring `execute` from auth, membership, cache,
leases, and playback. Classify it under §3.1. Coalesce only replaceable activity
facts, beginning with cache-use touches and node heartbeat rows. Prefer one
newest value per identity and a bounded flush interval; terminal state changes
remain synchronous.

Membership reachability must not become optimistic. If heartbeat batching
changes its interval, update the 30-second reachable window and prove the node
cannot remain reachable after its allowed missed beats.

**Acceptance:** deterministic paused-time tests pin each commit window and
terminal exception. A load control bypassing the coalescer must violate the
physical-commit budget, matching the existing progress-growth gate pattern.

### 6.6 P5 — separate storage pressure and tune only proven limits

**Change:** Add distinct configuration for durable Hiqlite state, persistent
cache/offline bytes, and disposable transcode scratch while preserving
`storage.data_dir` as the compatibility default. Migration must never infer a
new durable database from a scratch path. Document mount, permissions,
capacity, restart, and rollback behavior.

After P3 metrics show local read-pool contention, expose a bounded
`cluster.read_pool_size` with default `4` and benchmark `4 · 8 · 16`. Retain the
smallest value whose p95 improves without increasing write p99 or memory beyond
the recorded budget.

Keep the 16 MiB WAL, 10,000-entry snapshot policy, immediate-async sync, and
current election timers unchanged unless the §5 evidence isolates one as the
limit. Never change `max_in_snapshot_log_to_keep`; Hiqlite ties it to disaster
recovery.

**Acceptance:** upgrade tests prove old configuration selects identical paths;
restart tests preserve cache/offline content and discard only declared scratch;
disk-pressure tests on the cache/scratch device do not corrupt or relocate the
durable target. The chosen read-pool value has a retained benchmark artifact.

### 6.7 P6 — make the fourth machine useful without adding a vote

**Change:** Add an explicit non-voting learner/read-worker role only after P3's
lag gate exists. The role receives replication, serves readiness-gated local
reads and ordinary HTTP/media work, never becomes leader, and never contributes
to quorum. Joining as a voter remains the default because silently changing an
existing token's role would alter availability.

Promotion and removal are explicit admin operations. A learner may not claim
HA redundancy, run leader-singleton jobs, or serve authority reads locally.
Loss of every voter still makes authority operations unavailable even if the
learner process is healthy.

**Acceptance:** a three-voter-plus-learner cluster preserves quorum size three,
distributes bounded reads to the learner, refuses learner leadership, catches
up after restart, and promotes only through the explicit operation. The UI and
operations text distinguish compute/read capacity from voting redundancy.

### 6.8 P7 — close the loop with load-balancer and failure drills

P2 and P7 fulfill and extend the metrics, mixed-version, proxy, and failure
proofs already owned by `CLUSTERING-PLAN.md` M5/M6; they do not create a second
operations contract. Update both plans in the behavior PR, and treat the older
milestone as satisfied only when its original acceptance plus the performance
budget here are both met.

**Change:** Ship Caddy/nginx/Traefik-neutral guidance: probe `/readyz`, keep HLS
and segment requests sticky, cap connection/drain times, and fail over without
retrying unsafe mutations automatically. Extend the M5/M6 cluster validation
harness with an executable local proxy fixture rather than blessing one
vendor's syntax as the application contract.

Record three-voter, three-voter-plus-learner, follower-loss, leader-loss, and
lagged-worker runs. Lock absolute SLOs only from those artifacts.

**Acceptance:** under the §5.2 workload, a follower loss preserves writes, a
leader loss recovers inside the accepted election budget, a lagged learner
leaves rotation, and an HLS session survives one backend loss with the existing
documented discontinuity behavior.

## 7. Pull-request sequence — small diffs, explicit dependencies

### 7.1 Sequence

| PR | Scope | Depends on | May proceed in parallel with |
|---|---|---|---|
| 0 | this plan | functional membership | none |
| 1 | auth activity-write suppression | PR 0 reviewed, not necessarily merged | PR 2 design only |
| 2 | Store/Raft metrics | PR 0 | PR 1 after shared-file conflict check |
| 3 | lag-gated catalogue reads | PR 2 | PR 4 inventory only |
| 4 | replaceable-write coalescers | PR 2 | PR 5 path design |
| 5 | storage roots + measured read pool | PR 2; P3 measurements for pool | PR 4 |
| 6 | non-voting learner/read worker | PR 3 | PR 5 |
| 7 | proxy fixture + final failure/SLO record | PRs 3, 5, and 6 | none |

### 7.2 Existing work and shared-file ownership

Membership PRs #490 and #491 own overlapping changes in
`crates/plurx-cluster-check/src/lib.rs`, `crates/plurx-core/tests/store_contract.rs`,
`docs/CLUSTERING-PLAN.md`, and `docs/OPERATIONS.md`. At this plan revision #491
has landed and #490 remains open. P0 waits for #490 and rebases before extending
its harness or operations contract. P1 may develop in parallel with #490
because it has no semantic dependency, but the two share `store_contract.rs`:
whichever becomes merge-ready first may land, then the other rebases, resolves
the shared contract deliberately, and reruns its required CI. Later milestones
extend the merged M5/M6 contracts rather than copying them into a second
harness or document.

Before opening each PR, compare its path list with every open cluster PR. One
branch owns a shared contract at a time; another either waits, moves an
independent test to a non-overlapping surface, or declares the dependency in
its PR body. A green head from before the dependency merged is not sufficient.

### 7.3 Mixed-version and downgrade matrix

Every behavior PR records supported old/new combinations and exercises them in
the `CLUSTERING-PLAN.md` M6 mixed-version fixture:

| Milestone | Feature gate and old-node behavior | Upgrade order | Downgrade gate |
|---|---|---|---|
| P1-P2 | no schema/wire change; old nodes remain correct but do not coalesce or export new metrics | non-leader nodes, then current leader | unrestricted after disabling dashboards that require the new series |
| P3 | bounded reads stay off unless the serving node and quorum-confirmed watermark source advertise the same protocol feature; old nodes use `Authority` | upgrade all voters, verify feature advertisements, then enable per-node traffic | force the authority-read kill switch cluster-wide before installing an old binary |
| P5 | new paths are node-local config; an omitted field preserves the old root exactly | move one non-leader only after its reverse path is proven | move bytes back and restore old config before downgrade |
| P6 | learner membership mutation is refused until every voter advertises the learner protocol; old binaries must reject rather than reinterpret that membership | upgrade all voters first, then add a learner, then enable learner traffic | remove every learner and verify voter-only membership before downgrade; if the chosen compatibility marker is irreversible, old-binary downgrade is unsupported and rollback is a forward fix |
| P7 | proxy behavior keys only on stable readiness/HTTP contracts | upgrade backends before enabling new routing policy | restore the prior routing policy before backend downgrade |

Each PR pins `protocol_min`/`protocol_max` expectations, old-binary startup or
refusal, feature negotiation, upgrade order, and rollback outcome. An older
node may never promote a learner or opt another node into bounded reads by
accident.

Each behavior PR contains its tests and updates the authoritative operations or
architecture text in the same commit. A branch is cut from current `main` in a
fresh clone, uses the `codex/` prefix, and contains no unrelated cleanup.
GitHub CI is the acceptance environment; do not spend a second full local run
before asking CI the same question. If CI exposes a focused failure, reproduce
only when a local run adds diagnostic value, fix it, push once, and require the
new PR head to pass.

## 8. Guardrails — deliberate non-goals

- **Do not promise linear write scaling.** Raft serializes writes at one leader;
  additional nodes improve availability and application capacity, not the
  number of independent write authorities.
- **Do not make four voters the recommended HA topology.** It tolerates the same
  one failure as three and raises the quorum.
- **Do not route all HTTP traffic to the leader.** That saves a hop by turning
  the leader into the application bottleneck and defeats bounded-replica work.
- **Do not cache authorization merely because catalogue caching worked.** A
  stale poster title is tolerable; a revoked token is not.
- **Do not put the Raft log on shared media storage.** NFS/SMB availability and
  latency must not become consensus durability.
- **Do not expose every OpenRaft knob.** A setting without a measured workload,
  safe range, and cross-node rollout contract is an operational footgun.
- **Do not make sticky sessions mandatory.** They improve cache locality; the HA
  contract must still recover on another node.
- **Do not use benchmarks as tests of correctness.** Deterministic contracts
  reject regressions; benchmark artifacts explain cost and set SLOs.

## 9. Rollout and rollback — optimize one boundary at a time

For each behavior PR:

1. capture the before artifact on the same topology and workload;
2. prove the §7.3 mixed-version row, then deploy to one non-leader only when
   its feature gate permits the combination;
3. confirm readiness, apply lag, and auth/write counters before adding traffic;
4. expand traffic while watching p95/p99, leader changes, lag, and errors;
5. exercise follower loss, then leader loss, before declaring the slice done;
6. roll back the application change without deleting Hiqlite state if any
   correctness or tail-latency gate fails; and
7. preserve both before and after artifacts with the build SHA.

P3 local-read rollout has an additional kill switch: force all replicated reads
back to `Authority` without changing schema or membership. P6 learner rollout
can remove the learner without changing the three-voter quorum. P5 path rollout
requires a documented reverse move before any operator relocates durable data.

## 10. Revisit as the cluster grows

Re-evaluate the design when one of these facts becomes true:

- the operator explicitly requires two simultaneous voter failures — compare
  five voters with three, not four with three;
- authority reads dominate after catalogue reads move local — design an auth
  invalidation protocol rather than lengthening a blind TTL;
- one leader's write CPU saturates after replaceable writes are coalesced —
  revisit data shape and transaction batching before changing durability;
- learner read capacity exceeds voter read capacity — add capacity-aware proxy
  weights while retaining lag-gated readiness;
- snapshot duration dominates write p99 — measure incremental or off-path
  snapshot options without changing disaster-recovery retention; or
- clusters span sites — keep the voting quorum in one low-latency failure
  domain and treat remote copies as learners/backups unless cross-site quorum
  latency is an accepted product requirement.

The plan ends when three voters provide the durability promise, every extra
machine adds measured application capacity without silently raising quorum,
and the metrics can explain the next bottleneck without another architecture
guess.
