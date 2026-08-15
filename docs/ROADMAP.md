# plurx — Roadmap

Sized for one developer + AI pair at a steady cadence. Every phase ends with something you actually use in your own living room — no phase is "infrastructure only" except where the infrastructure *is* the product (Phase 4). Phases are gates: a phase's exit criteria must hold before the next starts, but item order inside a phase is flexible.

## Phase 0 — Skeleton (small) ✅ DONE

Repo scaffolding: cargo workspace (`plurxd`, `plurx-core`, `plurx-compat-plex`), CI (fmt/clippy/test, cross-compile amd64+arm64), Docker image build, `docs/` as the source of truth. The `Store` trait boundary exists from the first commit (single-node SQLite behind it) so Phase 4 is a backend swap, not a rewrite.

**Exit:** `plurxd` runs, serves `/healthz` and an empty native API, in Docker and as a bare binary. ✅

## Phase 1 — It plays (the old-Plex kernel) ✅ DONE

- ✅ Scanner v1: movies + TV naming (Plex/Jellyfin + scene styles), ffprobe inspection (codecs/HDR/audio/subs), incremental rescan by size+mtime, vanished-file reconcile. (Live inotify watch deferred to a fast-follow; on-demand + create/update rescan covers the flow.)
- ✅ Metadata v1: TMDB agent (title+year matching, movie/show/episode), artwork caching, offline-safe when no key set. (Manual fix-match UI → Phase 2.)
- ✅ Native API v1: auth (local accounts, Argon2id, tokens, first-run setup), library CRUD, browse (grids, detail, hubs), item detail with files, search (FTS5), watch progress/scrobble, artwork, scan status.
- ✅ Playback v1: decision engine + data-driven device profiles, direct play (HTTP range), on-the-fly MKV→fMP4 remux (`-c:v copy`, audio-only transcode fallback), watch state + resume (client-seek for direct, server fast-seek for remux).
- ✅ Web app v1: embedded SPA — login/setup, library grid/detail, in-modal `<video>` player, continue-watching, search, admin (libraries, scan, TMDB key).

**Exit:** ✅ a real library, browsed and played (direct/remux) in a browser, resume working — verified end to end in a real (Playwright) browser: WebM streamed from the direct endpoint decoded and played to completion; MKV→fMP4 remux produces valid h264+aac; 49 tests green. Multi-user accounts exist, so "someone other than the developer" is supported. (OpenAPI description and live inotify watching carry into Phase 2.)

## Phase 2 — Old-Plex parity ✅ DONE

- ✅ Transcode pipeline: hardware encode (validated NVENC/QSV/VA-API/VideoToolbox + software x264 fallback), bitrate ladder, HDR→SDR tone-map (zscale default, libplacebo opt-in), text/ASS sub burn-in, HLS session lifecycle + reaper. (Bitmap-sub burn-in and zero-copy GPU filter graphs → 2.x.)
- ✅ Audio policies: downmix + per-profile codec handling in transcode.
- ✅ Anime: AniList agent (no key), anime detection + absolute-numbering ordering, dual-audio default-track rules (tracks module). (ASS *rendering* on clients is Phase 5.)
- ✅ On-deck refinement: next-up episodes hub. **Deferred to 2.x:** TVDB agent (TMDB covers TV), collections, playlists.
- ✅ **Plex-compat Tier 1**: GDM responder + PMS endpoint subset; validated end-to-end with python-plexapi (the reference client). (Composite/PKC contract-recording tests → 2.x.)
- ✅ Ops v1: Prometheus `/metrics`, structured logs, config file + env, Docker/Compose + bare-metal systemd + Unraid templates. (TrueNAS app → 2.x.)

**Exit:** ✅ old Plex, honestly replaced for movies/TV/anime on LAN — verified: HEVC/HDR10 transcodes to tone-mapped SDR HLS and plays; python-plexapi browses + direct-plays + marks watched; anime scans with absolute numbering and AniList metadata; watch state (resume, continue-watching, next-up) is trustworthy. 84 tests, clippy clean.

## Phase 3 — Cluster spike (decision gate) ✅ DONE

Time-boxed spike, not a feature phase. Full findings in [PHASE3-SPIKE.md](PHASE3-SPIKE.md).

- ✅ **Store backend decided: hiqlite 0.14.** API (`execute`/`query_map`/`txn`) maps onto the existing rusqlite row mappers; compiles clean in the workspace; a live node ran migration + raft insert + typed read-back; upstream's suite proves 3-node replication + self-heal. openraft fallback not needed.
- ✅ **Deterministic-segment behavior measured** against CFR, sparse-keyframe, and VFR sources: any node produces a valid segment N (accurate seek even with keyframes only at 0/10 s); independently-produced segments sequence to the correct 12.000 s via the playlist; same segment is byte-identical across runs (`threads=1`).
- ✅ **Sharp edges + fallback scoped:** PTS resets and one audio-boundary discontinuity at a *failover* (not normal playback) → emit `EXT-X-DISCONTINUITY`. The "restart-at-position" contract turned out to be the clean primary design, not a fallback.

**Exit:** ✅ written decision recorded (this doc + ARCHITECTURE §2 updated). Phase 4 adds `HiqliteStore: Store` behind the unchanged trait, replicated session recipes, and client-retry failover.

## Home video & photos ✅ DONE

Its own shippable slice, between Phase 3 and Phase 4 — HA-neutral (no new
replication classes; scan and local artwork ride the already-planned
leader-scheduled singleton). Design record: [HOMEVIDEO-PLAN.md](HOMEVIDEO-PLAN.md),
scope in [REQUIREMENTS.md](REQUIREMENTS.md) §5a.

- ✅ **Migration v6:** `libraries`/`items` rebuilt to drop the kind CHECKs (SQLite can't alter one); `recorded_at`, `tags`, `nfo_seeded_at`; FTS recreated over tags. The migration runner now disables FKs around each migration and runs `foreign_key_check` after.
- ✅ **Scanner `home` arm:** folder mirroring, photo candidates, filename-verbatim titles, the recorded-date ladder, folder-chain pruning.
- ✅ **NFO as a one-time seed:** Kodi `<movie>` dialect, lenient parser, seeded at most once per item — never re-read, never written.
- ✅ **Photos:** EXIF dates, thumbnails, range-served originals, a lightbox; excluded from *Recently added*.
- ✅ **Local artwork:** art beside the file wins, else an ffmpeg frame grab; folders inherit a child's poster.
- ✅ **Editing:** `PATCH /api/v1/items/:id` (admin, home libraries only) + the edit modal.

**Exit:** ✅ browse folders, play clips, view photos, edit metadata, thumbnails everywhere.

## Integrations — plurx's side of the monarr pipeline ✅ DONE

Another shippable slice, HA-neutral (targeted scans ride the same
leader-scheduled scanner singleton as full scans). Design record:
[INTEGRATION-PLAN.md](INTEGRATION-PLAN.md); the master plan lives in monarr's
repo. Behaviour in [FEATURES.md](FEATURES.md) §11.

- ✅ **Migration v8 — scoped API keys:** `plx_…`, SHA-256 at rest, shown once, scope list, admin CRUD. A second credential kind, so another application never needs a token that IS a user.
- ✅ **Targeted scan:** `scan_path` indexes one path and **never prunes** — the property that separates it from a full scan, and the one whose absence would eat a library.
- ✅ **`POST /api/v1/scan`:** path → library resolution (longest root wins), 200 with report + placed items, 202 + `request_id` when busy (queued, not dropped), `GET /api/v1/scan/requests/{id}` to poll, `correlation_id` echoed and logged on `plurxd::integrate`.
- ✅ **Scheduled reconcile scan:** per-library intervals with completion-stamped clocks — the slow sweep underneath the fast path, catching what nobody announced.
- ✅ **Migration v9 — enrich by id:** an id the caller supplied is used directly (movie/show detail; an IMDb id resolved in one lookup) and the title search is skipped, because the search is the step that can be wrong and a wrong TMDB id does not stay local — Trakt sync matches on it too. `metadata_at` replaces "has an id" as the enrichment marker: an item that *arrives* carrying an id needs enriching, not skipping.

**Exit:** ✅ monarr finishes an import and the file is in the library seconds
later, carrying the ids monarr already knew, with one `correlation_id`
traceable across both applications.

- ✅ **Coming-soon rail (master plan §11.2):** `GET /api/v1/coming-soon` proxies monarr's calendar server-side with a monarr API key from DB settings, cached 15 minutes, rendered as a home rail. Read-only, one endpoint, no monarr changes. The proxying *is* the feature: that key can edit monarr's library, so it never reaches a browser.

**Still to come from the master plan §11.1:** plurx pushing watch state back
to monarr. Deliberately not started — the plan lists three open questions
(per-profile policy shape, multi-user semantics, and whether "watched" should
ever gate deletion automatically) that are decisions, not implementation.

**Live inotify watching remains future work**, and is off the critical path
now. What shipped instead — scheduled scans plus targeted scans — covers both
"something changed and said so" and "something changed and didn't". A watcher
would improve only the case where a human moves files by hand *and* wants it
noticed sooner than the reconcile interval.

## Performance — start latency, 4K, pre-cache (IN PROGRESS)

Design record: [PERF-PLAN.md](PERF-PLAN.md) — where the seconds go when you
press Play, and the milestones that get them back: instrument first (M0),
single-node pacing/buffer/segment fixes (M1), GPU tone-mapping end to end
(M2), the pre-transcode cache (M3), and cluster transcode — placement,
restart-at-boundary failover, distributed pre-caching (M4). HA-neutral
through M3; M4 *is* Phase 4's transcode chapter and waits for its plumbing.

- ✅ **Weekend 1 shipped 2026-07-28:** telemetry (encode speed, TTFF/stall
  beacons, session status endpoint), chapters moved to scan time,
  burst-then-hold pacing replacing `-re`, the ahead-window suspend, and the
  progress-aware watchdog.
- 📝 **Reviewed 2026-07-28**, two rounds: [PERF-PLAN-REVIEW.md](PERF-PLAN-REVIEW.md)
  → [PERF-REVIEW-RESPONSE.md](PERF-REVIEW-RESPONSE.md) →
  [PERF-REVIEW-ASSESSMENT.md](PERF-REVIEW-ASSESSMENT.md). The plan carries
  the corrected contracts and a decisions ledger (PERF-PLAN §8.6).
- ✅ **M1 complete 2026-07-28:** the correction pass (PERF-PLAN §2.8), the
  fixture-matrix benchmark, 2-second segments, bounded rate control on every
  encoder family with a probe that runs the production arguments, and an Auto
  rung that means `min(source, 1080)` on hardware instead of 720p for
  everything. Measured on the corpus: the CPU tone-map chain costs a quarter
  of the pipeline's throughput (0.98× → 0.71× at 4K), which is what takes a
  4K HDR session below realtime.
- ✅ **M2 complete 2026-07-29:** GPU tone-map candidate graphs, the real-HDR
  boot probe, per-session routing and downgrade, and racing-safe hardware
  admission all shipped. The nynuc acceptance run measured the QSV path at
  4.89× the CPU chain while holding the bitrate bound.
- ✅ **M3 complete 2026-07-28:** the resumable pre-transcode producer, LRU
  budget, viewer preemption, cache-hit VOD serving, offline-package pinning,
  and active-reader eviction protection are shipped. Cached TTFF and seek
  measurements remain operational evidence, not unfinished implementation.

## Phase 4 — HA for real (IN PROGRESS)

The executable handoff is
[CLUSTERING-PLAN.md](CLUSTERING-PLAN.md): it separates node identity from the
logical server first, then brings up the one-voter replicated store before
membership, singleton jobs, session takeover, and failure drills.

M0–M1d and M2's source-backup plus row-import slices are merged: identity, the
one-/three-voter Hiqlite proofs, replicated auth/catalogue/media/search
contracts, the complete 120-method Store backend, and bounded exact import
parity are in-tree. M2 activation is next; SQLite remains the production
default until daemon selection lands.

- ✅ **M0 complete 2026-08-07:** local node identity, rollback-tolerant
  cluster configuration, Hiqlite decision/cost record, deterministic-SQL guard,
  and the semantic spike are merged.
- ✅ **M1a complete 2026-08-07:** the backend-neutral `Arc<dyn Store>` contract
  inventory and replicated transaction/CAS policy are merged.
- ✅ **M1b complete 2026-08-07:** settings, users/tokens, and API keys replicate
  across three voters with schema preflight, replica digests, and induced
  follower/leader/quorum-loss coverage.
- ✅ **M1c complete 2026-08-08:** libraries, media, watch state, node-local FTS,
  root identity, and atomic bounded reconciliation are merged in PR #88.
- ✅ **M1d complete 2026-08-08:** the 120-method backend, shared three-voter
  contract factory, multi-writer Trakt/outbox safety, offline/cache identity
  guards, and the progress coalescer passed the full PR #90 review and CI
  sequence. The bounded post-coalescer gate now drives 10,000 incoming beats
  across 80 streams, commits 160 physical writes, and records 1,017,640 bytes
  (101.764 bytes/beat) of growth after production-threshold compaction against
  a 256-byte/beat budget; `make cluster-growth` reproduces the measurement and
  rejects the 10,000-commit raw-write control.
- ✅ **M2 import complete 2026-08-09:** content-addressed backup and fresh-target
  import preserve v14 through current durable rows, rebuild derived FTS, and
  prove exact parity with bounded keyset pages, an incremental digest, and
  source reads isolated on one blocking worker.
- ○ **M2–M3 next:** activate one-voter Hiqlite, envelope-encrypt Trakt
  bearer credentials, then add join/remove/health and singleton job fencing.
  Removing a node must explicitly fail or safely re-home its offline work.
- ○ **M4–M7 remain:** replicated session takeover, client node-list retry,
  VIP/Kubernetes deployment patterns, rolling upgrades, the cluster admin UI,
  and the physical failure-drill corpus.

**Exit:** the demo that defines the project — pull the power on a node mid-movie; playback resumes within seconds; settings/watch state show zero loss; `docker compose up` a 3-node cluster from the README in under 10 minutes.

## Phase 5 — Native clients (order: cheapest coverage first)

1. **Android/Google TV (Kotlin/Media3)** — covers Sony/Shield/phones; true MKV direct play; trivial sideload
2. **Apple TV (Swift/AVPlayer)** — server-remux strategy; TestFlight cadence
3. **Tizen + webOS ports** of the web core — AVPlay interop where needed, dev-mode/cert tooling documented
4. **Roku (SceneGraph)** — last, leaning on the by-then-mature remux/transcode pipeline; beta-channel workflow

Each client ships with: device profile upstreamed, failover retry logic, and the same playback-correctness test corpus.

## Phase 6 — Polish & bets (unordered backlog)

- ASS/SSA styled subtitles (libass in clients; burn-in path already exists)
- OIDC sign-in (Google/Apple); parental controls; per-user library permissions UI
- Trickplay thumbnails (BIF/tiles), theme music, extras UX
- **Jellyfin-compat façade spike** (CLIENTS.md Tier 3 — the legitimate route to Infuse et al.)
- plex.tv-emulation Tier 2: revisit only if still worth it
- Synology/QNAP native packages; macOS/Windows server builds
- Music & photos: reopen the question with the data model that exists by then

## Standing rules

- Playback-correctness corpus (HDR10/DV P5/P8, TrueHD/Atmos, PGS, VFR, 10-bit anime) runs in CI from Phase 2 on; a red corpus blocks release.
- Every phase updates the docs it invalidates; REQUIREMENTS.md is the scope police — new ideas go to Phase 6 by default.
- Version discipline: nothing user-visible breaks within a minor version; the cluster protocol carries a version from its first byte.
