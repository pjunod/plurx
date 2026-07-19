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

## Phase 2 — Old-Plex parity

- Transcode pipeline: hardware encode (QSV/VA-API/NVENC/VideoToolbox), quality/bitrate ladder, HDR→SDR tone mapping (libplacebo bt.2390), image-sub burn-in, session lifecycle + seek-while-transcoding
- Audio policies: passthrough vs downmix per device profile
- Anime: AniDB/AniList agents, anime detection + absolute-numbering ordering, dual-audio default-track rules (data model from day one; ASS rendering itself is Phase 5)
- TVDB agent; collections; on-deck refinement; playlists
- **Plex-compat Tier 1**: GDM responder + PMS endpoint subset; contract tests against recorded Composite/PKC traffic; validated with Composite for Kodi + python-plexapi + Home Assistant
- Ops v1: metrics, structured logs, config file + env, Unraid template + TrueNAS app

**Exit:** old Plex, honestly replaced for movies/TV/anime on LAN: any file plays on web + a Kodi/Plex-compat client, HDR files look right on SDR screens, watch state is trustworthy.

## Phase 3 — Cluster spike (decision gate)

Time-boxed spike, not a feature phase: hiqlite embedded vs openraft+redb+rusqlite behind the `Store` trait; 3-node testbed; kill-a-node drills for API reads/writes; deterministic-segment prototype — two nodes serving one HLS transcode session interchangeably (VFR and odd-keyframe sources included).

**Exit:** written decision (this doc + ARCHITECTURE.md updated): chosen backend, measured failover behavior, sharp edges list. If deterministic segments prove unreliable for some source class, the fallback contract (restart-at-position) is scoped here.

## Phase 4 — HA for real

- Cluster membership: join tokens, node add/remove, health, single logical server identity
- Replication classes wired: durable (users/settings/metadata/watch state) + ephemeral (sessions) + leader-scheduled scanner singleton
- Streaming failover: client node-list + retry (web client first), session takeover per ARCHITECTURE §2.3; VIP/keepalived + k8s patterns documented
- Helm chart (3-replica StatefulSet, anti-affinity); rolling-upgrade support; cluster admin UI (node status, raft health)
- Failure drills as CI-able integration tests: kill leader mid-transcode, kill follower mid-scan, netsplit, disk-full node

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
