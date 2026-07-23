# Architecture — how plurx is built, and why

Companion to [FEATURES.md](FEATURES.md) (everything the server does) and
[OPERATIONS.md](OPERATIONS.md) (how to run it) — this is *how it's built and
why it's built that way*. One Rust binary, `plurxd`: run one for a normal
server, run three and they form an active-active HA cluster with no external
infrastructure. The founding decisions are recorded here with their rationale
and their scars, because a decision without its reason gets "cleaned up" by the
next person who reads the code. Ecosystem facts and versions were verified
2026-07.

Diagrams are ASCII on purpose: they render identically in a terminal, on
GitHub, and in the in-app docs viewer, and they diff cleanly in a PR instead of
rotting in a separate asset pipeline.

## 1. System overview

Every node is identical — there are no roles to configure. In a cluster the raft
leader is an internal detail (it serializes writes); every node serves reads,
streams, and transcodes.

```
        ┌──────────────────────── clients ─────────────────────────┐
        │  web / Tizen / webOS      tvOS · Android TV · Roku        │
        │  (native /api/v1)         (native /api/v1)                │
        │  Kodi-family Plex clients ──▶ Plex-compat façade + GDM    │
        └───────────────┬───────────────────────────┬──────────────┘
                        │ native JSON               │ Plex XML/JSON
                        ▼                           ▼
        ┌──────────────────────── plurxd node (×1 or ×3+) ─────────────────┐
        │                                                                   │
        │   axum HTTP  ──▶  auth (Argon2id, bearer tokens)                  │
        │      │                                                            │
        │      ├──▶ decision engine ─▶ stream server ─┬─ range (direct)    │
        │      │      (pure fn)                        ├─ remux  (fMP4)     │
        │      │                                       └─ transcode ─▶ HLS  │
        │      │                                            │               │
        │      │                                     ffmpeg (child proc)    │
        │      ├──▶ scanner + metadata agents                               │
        │      └──▶ Store trait ──▶ SQLite (1 node)  |  hiqlite raft (3+)   │
        └───────────────┬───────────────────────────────┬──────────────────┘
                        │ read-only                      │ HTTPS
                        ▼                                ▼
              shared media storage            TMDB · AniList (metadata)
              (NFS / SMB / cephfs)            cached locally, then offline
```

The load-bearing boundary is the **`Store` trait**: every read and write goes
through it, so single-node SQLite and the replicated cluster store are the same
call sites. That is what makes HA a backend swap (Phase 4) instead of a rewrite
— it existed from the first commit specifically so this promise could be kept.

## 2. Cluster & state (the differentiator)

Nobody else in this space has real HA. Jellyfin is architecturally
single-instance (SQLite single-writer, in-memory transcode sessions — its
community bolts on keepalived + rsync and calls it done), and no Rust media
server is both maintained and clustered. The specific hard problem worth solving
is **replicated playback/transcode session state** — the thing that turns "a
node died, my movie is gone, restart it" into "playback hiccuped for two
seconds."

### 2.1 Consensus & storage — embed the store, don't run a database

**Decided (Phase 3 spike): embed [hiqlite](https://github.com/sebadob/hiqlite)
0.14** — raft-replicated SQLite built on openraft, purpose-built for the exact
"1 node or 3+ nodes, no external infra" shape (production-proven as Rauthy's
default store; SQL + replicated KV cache + distributed locks + listen/notify in
~65 MB RAM for an HA cluster). The spike confirmed its `execute`/`query_map`/
`txn` API maps directly onto the existing rusqlite row mappers, it compiles
clean in the workspace, and a live node ran a migration + raft insert + typed
read-back; its own suite proves 3-node replication and self-heal. See
[PHASE3-SPIKE.md](PHASE3-SPIKE.md). **Fallback (not needed):** hand-rolled
[openraft 0.9.x](https://github.com/databendlabs/openraft) with a redb raft log
and a rusqlite state machine. Nothing before Phase 4 depends on the choice
because all cluster access goes through the one internal `Store` trait.

Single-node mode is the same code path with a 1-voter raft (a supported
openraft/hiqlite pattern) — no "cluster edition" fork, and any single node can
later grow into a cluster by adding voters.

### 2.2 Replication classes — not everything needs consensus

Data is sorted by how much its loss hurts, because paying raft's cost for a
regenerable thumbnail cache would be waste:

| Class | Examples | Storage | Loss tolerance |
|---|---|---|---|
| **Replicated-durable** | Users, auth tokens, settings, library metadata, watch state, playlists | Raft → SQLite | None once acked |
| **Replicated-ephemeral** | Playback sessions (item, decision, position, segment index), node membership/health | Raft KV/cache with TTL | Seconds of staleness OK |
| **Node-local, regenerable** | Transcode segment cache, image cache, thumbnails/trickplay | Local disk (optionally shared) | Free to lose |
| **Operator-owned** | The media files themselves | Shared storage | plurx never writes media |

Write rates are safe for raft: watch-state progress ticks batch to ~1 write /
10 s / stream server-side regardless of how often the client pings; session
state updates on segment boundaries, not per-chunk. This matters because raft
commits every write to a quorum — a naive "save position on every timeupdate"
would put hundreds of writes/second through consensus and melt it.

### 2.3 The failover mechanic — any node can serve segment N

The Phase 3 spike ([PHASE3-SPIKE.md](PHASE3-SPIKE.md)) measured the
deterministic-segment idea against constant-frame-rate, **sparse-keyframe**, and
VFR sources. The load-bearing property — *any node can produce a valid segment
N* — holds even in the sparse-keyframe worst case (accurate input-seek), and
independently-produced segments sequence to the correct total via the HLS
playlist.

```
 normal playback            node A dies mid-stream        client recovers
 ───────────────            ─────────────────────         ───────────────
 client ─▶ node A           client ─▶ node A  ✗           client ─▶ node B
   HLS seg 0,1,2,3            (request fails)                │
   one ffmpeg session         session recipe is in          ├ reads recipe from raft
   writing forward            replicated raft state          ├ restarts ffmpeg,
                                     │                        │   input-seek to seg 4
                                     ▼                        ├ emits EXT-X-DISCONTINUITY
                             recipe: {file, args,             └ serves seg 4,5,6…
                              seg=4s, keyframes@4s}          buffered 0–3 still valid
                                                            cost: a few seconds, once
```

1. Every transcode session pins its full recipe in replicated state: source
   file, ffmpeg arg set, segment duration `d` (4 s), forced keyframes at
   multiples of `d`. The **primary** path is one sequential ffmpeg session
   (Phase 2) — clean, no per-segment resets.
2. **Failover:** a surviving node restarts the session seeked to the last-served
   segment boundary. Accurate input-seek guarantees a valid segment N from any
   node, so the client keeps its already-buffered segments and continues; an
   `EXT-X-DISCONTINUITY` is emitted at the failover boundary so the player
   remaps its timeline cleanly. This is the roadmap's "restart-at-position" — the
   spike showed it *is* the clean design, not a fallback.
3. HLS playlists are generated (not stored), identical on every node.
4. Client-side failover: clients hold the node list (REQ-HA-6); on request
   failure they retry the next node, which restarts the session from the
   replicated recipe. Direct play and remux failover are the same minus ffmpeg
   (stateless range requests / deterministic remux).
5. ffmpeg always runs as a child process — a codec crash kills a session, never
   a raft voter. Process isolation is load-bearing for HA, not a convenience.
6. **Optional optimization:** x264 `threads=1` makes segments byte-identical
   across nodes (measured), so a replicated/shared segment cache serves
   re-requested segments for free.

Scanner and metadata-refresh jobs are leader-scheduled singletons (distributed
lock), so three nodes don't triple-hit TMDB or thrash shared storage.

## 3. Playback pipeline — get out of the way first

The whole pipeline is built around one belief: the server's best move is to send
the file untouched. Everything else is a fallback the server is forced into, and
it says so out loud in `/decision`.

**The decision engine is a pure function.** `(file streams + HDR/audio detail,
device profile, client-reported caps, user prefs, bandwidth) → Decision`. Device
profiles are TOML data shipped with the server and hot-fixable; the file-side
facts come from the scanner (§4). It's a pure function so it's unit-testable
without ffmpeg, a server, or a network — the correctness of "will this play?"
never depends on runtime state.

```
                 ┌───────────────────────────────┐
 file streams ──▶│  video codec in profile?      │── no ──┐
 device profile  │  resolution ≤ max?            │        │  any hard
 client caps     │  bitrate ≤ max?               │── no ──┼─▶ video/res/
 user prefs      │  HDR ok, or tone-map needed?  │        │  bitrate/HDR
                 └──────────────┬────────────────┘── no ──┘  mismatch
                    all ok │                     │
                           ▼                     ▼
                 ┌──────────────────┐      ┌───────────┐
                 │ container ok AND  │─no─▶ │ TRANSCODE │  hardware first,
                 │ audio codec ok?   │      │  → HLS    │  tone-map, sub burn-in
                 └────────┬──────────┘      └───────────┘
                     yes  │        └── container/audio only ──┐
                          ▼                                   ▼
                   ┌─────────────┐                      ┌──────────┐
                   │ DIRECT PLAY │                      │  REMUX   │  -c:v copy,
                   │  HTTP range │                      │  → fMP4  │  audio maybe re-enc
                   └─────────────┘                      └──────────┘
```

**Serve paths:**

- **Direct play** — HTTP range serving of the file with correct caching headers;
  zero transcode CPU. This is the goal state, not the consolation prize.
- **Remux** — on-the-fly restream (MKV → fMP4/HLS) with `-c:v copy`; audio
  re-encoded only when the target can't take the source codec. Solves "right
  codecs, wrong container" (the tvOS/Roku staple) for pennies of CPU.
- **Transcode** — hardware first: QSV / VA-API (Linux), NVENC, VideoToolbox
  (macOS); software x264/x265 fallback. HDR→SDR tone mapping via `libplacebo`
  (Vulkan, `tonemapping=bt.2390`) as the cross-vendor path, with vendor filters
  (`tonemap_opencl`, `vpp_qsv`, `tonemap_videotoolbox`) as alternates — mirroring
  Jellyfin's proven matrix. Image subs and ASS burn-in happen here.
- **Audio** — passthrough per device profile (TrueHD/DTS-HD where the chain
  allows), else transcode to EAC3/AC3/AAC with correct downmix.

**A stalled hardware session repairs itself.** Hardware encoders can initialize
cleanly and then stall under concurrency (two QSV sessions on one iGPU is the
classic case). The transcode manager arms a watchdog: if the first HLS segment
hasn't landed within a grace window (8 s), it kills the session, clears its
directory, and respawns on software x264. The user sees a few extra seconds of
the loading overlay, not a permanent gray screen.

ffmpeg is orchestrated as a **spawned CLI** (thin tokio process code), never
linked: crash isolation, license cleanliness, and drop-in support for the user's
ffmpeg build — **jellyfin-ffmpeg explicitly supported** and recommended for its
extra hwaccel/tone-mapping patches (and required for GPUs newer than the distro's
VA driver, e.g. Intel Arrow Lake on the `xe` kernel driver).

This section is the *server's* verdict. The client half — which transport each
browser actually uses to play a verdict, why Safari and Chromium diverge on
remux, and the copy-video HLS path that keeps Safari at source resolution — is
[PLAYBACK.md](PLAYBACK.md).

## 4. Scanner & metadata — ffprobe is ground truth

```
 library root
     │  inotify + periodic reconcile (incremental: skip unchanged by size+mtime)
     ▼
 identify ──▶ filename/structure parse (movie · show S/E · anime absolute #)
     │
 inspect  ──▶ ffprobe -print_format json  ─┐  codecs, profiles, bit depth,
     │        (pure-Rust pre-scan skips     │  HDR10/HDR10+/DV profile+level,
     │         unchanged files cheaply)     │  audio layouts, subtitle tracks
     ▼                                      └─▶ fed VERBATIM to the decision engine
 match    ──▶ TMDB (movies/TV)  ·  AniList (anime, no key)
     │        anime detection routes to absolute numbering, not TVDB seasons
     ▼
 cache    ──▶ provider JSON + artwork cached (replicated / shared)
              a scanned library works offline forever
```

The scan result is published *before* metadata enrichment starts, so the UI
shows real file counts and any problems while posters are still fetching — a scan
that found nothing tells you immediately, instead of looking like it's still
working. Enrichment runs as a second phase and is leader-coordinated so a cluster
doesn't triple-hit the providers.

## 5. API design — one service, two façades

**Native API** (`/api/v1`) — JSON over HTTP, OpenAPI-specified from day one
(clients across five platforms need generated types), WebSocket for push
(now-playing, scan progress, cluster events). Auth: opaque bearer tokens from
local login; optional OIDC (Google/Apple) code flow mapping to local accounts
(REQ-USER-2). Argon2id at rest, SHA-256 token lookup.

**Plex-compat façade** (Tier 1, REQ-PLEX-1) — a stateless translation layer over
the *same* services, plus a GDM responder (UDP 239.0.0.250:32414, LAN-only).
Implements the endpoint set the Kodi-family clients actually use: `/identity`,
`/library/sections...`, `/library/metadata/...`, `/photo/:/transcode`, part
serving, `/video/:/transcode/universal/decision|start.m3u8`, `/:/timeline`,
`/:/scrobble`, `/:/progress`, `/hubs/search`, `/playlists`. XML `MediaContainer`
by default, JSON on `Accept: application/json`; `X-Plex-Token` values are plurx
tokens. plex.tv is never contacted (REQ-PLEX-3); plex.tv *emulation* for
Infuse/official apps is deferred Tier 2 (see [CLIENTS.md](CLIENTS.md) §3).

It's a façade over shared services rather than a fork so that a bug fixed in the
decision engine is fixed for both a native client and Kodi at once — there is one
source of truth for "how does this file play," not two.

## 6. Tech stack (verified 2026-07)

| Concern | Choice | Notes |
|---|---|---|
| Language | Rust (stable, pinned toolchain) | Single static binary; cross-compile amd64/arm64 |
| HTTP | axum 0.8 + tower-http | Streaming bodies, range serving; hyper 1.x |
| Cluster | hiqlite 0.14 (spike) → else openraft 0.9 + redb + rusqlite | §2.1 |
| Local DB | SQLite (rusqlite), STRICT tables + FTS5 search | Relational metadata, append-only migrations |
| Transcode | ffmpeg CLI spawn; jellyfin-ffmpeg supported | §3 |
| Inspection | ffprobe JSON; `symphonia` / `matroska` pre-scan | §4 |
| HLS | generated playlists (`m3u8`) | §2.3 |
| Discovery | mDNS `_plurx._tcp` + Plex GDM responder | LAN only |
| Passwords / tokens | Argon2id (at rest) · SHA-256 (token lookup) | §5 |
| Observability | `tracing` + Prometheus exporter | REQ-OPS-1 |
| Web app | embedded single-file SPA (no build step, no framework) | served by `plurxd` |
| Avoided | sled (stalled), rocksdb (C++ dep), external DBs, ffmpeg linking | — |

The web app is a deliberate non-choice: one hand-written `index.html` with inline
CSS and JS, compiled into the binary. No npm, no bundler, no framework churn — the
admin UI ships in the same static binary as the server and can never version-skew
against the API it talks to.

## 7. Key decisions, each with its reason

1. **One binary, one code path for 1 or N nodes.** A "cluster edition" fork
   doubles the test surface and rots the single-node path. Cost accepted: the
   single-node case carries a 1-voter raft it doesn't strictly need. Worth it —
   there is exactly one code path to keep correct.
2. **The `Store` trait from commit one.** Everything touches storage through it,
   so Phase 4 swaps SQLite for hiqlite behind the trait. The scar this avoids:
   media servers that grew a database assumption into 500 call sites and could
   never cluster without a rewrite.
3. **ffmpeg is spawned, never linked.** A codec crash must not take down a raft
   voter, the license stays clean, and users can drop in jellyfin-ffmpeg. Cost:
   process-spawn overhead per session and parsing ffmpeg's stderr for progress.
4. **Direct play is the goal, transcode is the failure.** The decision engine is
   biased toward sending the file untouched and reports every reason it couldn't.
   This is why `/decision` exists as a first-class endpoint: the server can always
   explain itself.
5. **ffprobe output is treated as ground truth and stored verbatim.** The raw
   JSON is kept so a future decision-engine rule can use a field we didn't parse
   yet, without a re-scan of the whole library.
6. **Chapters, not fingerprinting, for skip intro/credits.** Real chapter titles
   (MakeMKV, anime OP/ED, hand-authored) are honest and cheap — one ffprobe at
   playback start. We do *not* guess an intro from a model, because a "Skip Intro"
   button that jumps into the middle of a scene is worse than no button. A
   duration-based end-credits estimate is the one exception, and the API marks it
   `chapter:false` so the UI can hedge.
7. **Watch state is judged on the probed duration, not the stream's.** A
   progressive stream's `video.duration` grows as it buffers; trusting it marked
   partially-watched items as fully watched. The file's `ffprobe` duration is the
   authority. Scar: this bug shipped once and is why the rule is now explicit.

## 8. Non-goals (what the architecture deliberately refuses)

Named here because an architecture is defined as much by what it won't do — every
one of these is a door we're keeping shut on purpose:

- **No external database, broker, or cache service.** The moment plurx needs
  Postgres or Redis to run, "lean and boring to operate" is dead. The embedded
  raft store is the whole point.
- **plurx never writes to media storage.** Media is operator-owned and mounted
  read-only. No "organize my files," no renaming, no deleting — a media server
  that edits your files is one bug away from eating them.
- **No cloud dependency, no phone-home.** Everything works on a LAN that never
  touches the internet. There is no plurx.tv and there never needs to be.
- **No linking GPL/ffmpeg into the process.** See decision 3.
- **Not an everything-server (yet).** Music and photos are out of scope for v1;
  the data model won't preclude them, but they are not bolted on speculatively.
- **No transcode-by-default.** The server will not "optimize" a library into
  pre-baked renditions; it transcodes on demand, only when a client forces it.

## 9. Risks & mitigations

| Risk | Mitigation |
|---|---|
| hiqlite is a small project (bus factor) | `Store` trait isolation; openraft fallback is the same shape; both MIT/Apache |
| Deterministic-segment failover has sharp edges (VFR, keyframe drift) | Spiked at the Phase 3 gate; worst case = session restart-at-position, still ahead of everyone |
| Plex-compat drift / client quirks | Tier 1 targets a small, testable client set; contract tests against recorded Composite/PKC traffic; official API docs exist now |
| DV/HDR correctness is genuinely hard | Profiles are data; a test-file corpus per DV profile (P5/P8) from day one; HDR10 base-layer + tone-map fallbacks |
| Concurrent hardware transcode stalls | Software-fallback watchdog (§3); QSV-preferred on Arc-class GPUs |
| Solo-dev scope creep | Roadmap phases are gates; anything not in [REQUIREMENTS.md](REQUIREMENTS.md) is a "later" by default |
