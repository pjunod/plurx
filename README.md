# plurx

[![ci](https://github.com/pjunod/plurx/actions/workflows/ci.yml/badge.svg)](https://github.com/pjunod/plurx/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/pjunod/plurx/branch/main/graph/badge.svg)](https://codecov.io/gh/pjunod/plurx)

A self-hosted media server and player in the spirit of **old-school Plex** —
before the streaming tiles, live TV, ads, and cloud accounts. Your media, your
hardware, your network: one lean Rust binary, a web app that doubles as the admin
UI, a Plex-compatible API so existing clients just work, and the thing no media
server has ever shipped — **real high-availability clustering**. Music, photos,
and live TV are out of scope on purpose (see [non-goals](#non-goals)).

> **Self-hosted and pre-1.0.** plurx runs on your LAN with no cloud dependency and
> never phones home. It mounts your media **read-only** and never writes, renames,
> or deletes a file. Today it runs as a **single node** — the HA cluster is
> decided and validated (Phase 3 spike) but not yet wired up (Phase 4). Treat it
> as a capable daily driver, not a backup of your only copy.

![The plurx home screen — continue watching, next up, and recently added](docs/img/home.png)

## Start here

New to the project? Read in this order. [docs/FEATURES.md](docs/FEATURES.md) is
the shortest answer to *what does this actually do* — the exhaustive inventory,
including what it deliberately doesn't. Then [docs/OPERATIONS.md](docs/OPERATIONS.md)
for running it day to day and reading every status and log line it shows you, with
[docs/CHEATSHEET.md](docs/CHEATSHEET.md) as the copy-paste quickstart beside it.
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) has the diagrams and the founding
decisions (why one binary clusters without external infra). Scope and the phased
plan live in [docs/REQUIREMENTS.md](docs/REQUIREMENTS.md) and
[docs/ROADMAP.md](docs/ROADMAP.md); client strategy in
[docs/CLIENTS.md](docs/CLIENTS.md); deploy recipes in
[deploy/README.md](deploy/README.md).

## What it looks like

A borderless, projection-style player: the title and controls auto-hide during
playback, a staged loading overlay replaces the mystery gray screen, and an ⓘ
stats overlay shows source → what your browser is actually decoding — plus Skip
Intro / Skip Credits from real chapter markers.

![The player with the stats overlay open](docs/img/player-stats.png)

Every item shows its versions with labeled specs — a 2160p Dolby Vision remux and
a 1080p encode on one title, each with full video/audio/file detail:

![Movie detail with multiple versions and labeled specs](docs/img/item-detail.png)

<details>
<summary>More screenshots — Skip Intro, Settings, and a second theme</summary>

The Skip Intro button appears when playback enters a marked region (chapters, or a
conservative end-credits estimate); auto-skip is an opt-in preference:

![Borderless player showing the Skip Intro button](docs/img/player-skip-intro.png)

The admin Settings page: server + hardware at a glance (green pills are encoders
that passed a real startup probe), libraries with **live** scan status, users,
metadata key, and a live log viewer:

![Settings — server diagnostics, hardware pills, and live library scan status](docs/img/settings.png)

Theming follows the system light/dark and offers named themes — here the **Terminal**
theme (true-black, monospace):

![The Terminal theme — true black, monospace, green accents](docs/img/theme-terminal.png)

</details>

## Principles

1. **Your media, your rules.** No cloud dependency, no phone-home, no externally
   hosted accounts. Everything works on a LAN that never touches the internet.
2. **Direct play first.** The server's job is to get out of the way — every
   client uses hardware decoding, and plurx only remuxes or transcodes when a
   device truly can't handle the file, using hardware encoders when it must.
3. **Lean and boring to operate.** One static binary. No external database,
   message broker, or sidecar. Run one node, or run three identical binaries and
   they form an HA cluster.
4. **HA is a feature, not an ops project.** Active-active nodes over shared
   storage; settings, users, watch state, and playback sessions replicate — a
   node dying mid-movie costs seconds, not your evening.
5. **Meet clients where they are.** A native API for our own apps, plus a Plex
   Media Server-compatible API so existing third-party Plex clients point at
   plurx and just work.

## Install

```bash
# Docker / Compose — recommended for homelabs; builds from source the first time
cd deploy
cp docker-compose.override.example.yml docker-compose.override.yml  # your media mounts + GPU
docker compose up -d --build                                        # then open http://<host>:32600

# Bare metal — one binary; needs ffmpeg/ffprobe (or point PLURX_FFMPEG/PLURX_FFPROBE at them)
plurxd run                                                          # serves :32600

# From source (development)
cargo run -p plurxd                                                 # or: make run
```

First launch walks you through creating an admin account and adding a library.
Library paths are what the **server process** sees — under Docker, the
container-side mount path. Configuration is defaults → `plurx.toml` → `PLURX_*`
env (full table in [docs/OPERATIONS.md](docs/OPERATIONS.md#configuration-surface)).

Developer commands (CI runs the same targets, so green locally means green in CI):

```bash
make            # list every target
make run        # serve http://localhost:32600
make check      # fmt-check + clippy + test  (the CI gate — the single quality bar)
make coverage   # line coverage via cargo-llvm-cov → lcov.info
make hooks      # install a pre-commit hook that runs `make check`
```

## Usage

**Add a library.** Settings → Libraries → *Add & scan*. Pick Movies, TV Shows, or
Anime; give it one or more paths. The scanner identifies files, probes them with
`ffprobe`, and enriches from TMDB (movies/TV, optional key) or AniList (anime, no
key). Watch the live status: `scanning… N / M files` → `fetching metadata…` →
`idle`. A scan that finds nothing almost always means the path isn't what the
server sees — see [OPERATIONS.md](docs/OPERATIONS.md#reading-library-scan-status).

**Play something.** Press play; plurx decides direct-play / remux / transcode for
your device and reports it. Open the **Stats** overlay (ⓘ or press `i`) to see the
method, source, and what your browser is actually decoding. *Direct play* is
ideal, *Remux* is cheap, *Transcode · QuickSync* means the GPU is working,
*Transcode · software* means it fell back to CPU (the logs say why).

**Point a Plex client at it.** Kodi-family Plex clients (Composite, PKC),
`python-plexapi`, and Home Assistant work against the Plex-compat façade + GDM
discovery — no plex.tv contact. See [docs/CLIENTS.md](docs/CLIENTS.md).

**Operate it.** `/healthz`, `/readyz`, and Prometheus `/metrics`; a global
activity pill shows what the server is doing on every page; Settings → Logs is a
live, filterable log viewer. Full guide: [docs/OPERATIONS.md](docs/OPERATIONS.md).

## Layout

| Path | What's inside |
|---|---|
| [`crates/plurx-core`](crates/plurx-core) | Domain model · the `Store` trait · scanner · metadata agents · playback decision engine |
| [`crates/plurxd`](crates/plurxd) | The HTTP daemon (axum) · transcode orchestrator · the embedded single-file web app |
| [`crates/plurx-compat-plex`](crates/plurx-compat-plex) | Plex Media Server API façade + GDM discovery responder |
| [`docs/`](docs) | Architecture · features · operations · cheat sheet · requirements · roadmap · clients |
| [`deploy/`](deploy) | Docker/Compose, systemd, and Unraid templates |

## Status

Phases are gates — each ends with something you actually use. Full detail in
[docs/ROADMAP.md](docs/ROADMAP.md).

- [x] **Phase 0 — Skeleton.** Workspace, CI (fmt/clippy/test, cross-build), Docker
  image, `Store` trait boundary from commit one.
- [x] **Phase 1 — It plays.** Scanner, TMDB metadata, native API, direct play +
  remux, resume, embedded web app.
- [x] **Phase 2 — Old-Plex parity.** Hardware transcode (NVENC/QSV/VA-API/
  VideoToolbox + software fallback) with HDR→SDR tone-mapping and HLS; anime
  (AniList, absolute numbering, dual-audio); multi-version items; subtitles;
  Plex-compat Tier 1 (validated with `python-plexapi`); ops (metrics, logs,
  deploy templates).
- [x] **Phase 3 — Cluster spike.** The HA decision gate: store backend (hiqlite)
  and transcode-failover mechanic decided and validated against real sources. See
  [docs/PHASE3-SPIKE.md](docs/PHASE3-SPIKE.md).
- [~] **Playback experience.** Borderless player, staged loading, rich stats, skip
  intro/credits with auto-skip — shipped. Public ratings and multi-server
  dashboard still to come.
- [ ] **Phase 4 — HA for real.** `HiqliteStore` behind the unchanged `Store`
  trait, replicated sessions, client-retry failover, Helm chart, failure-drill
  tests.
- [ ] **Phase 5 — Native clients.** Android/Google TV → Apple TV → Tizen/webOS →
  Roku, each with a device profile and the shared correctness corpus.

## Non-goals

Deliberate, with reasons — the full list and rationale is in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#8-non-goals-what-the-architecture-deliberately-refuses)
and [docs/FEATURES.md](docs/FEATURES.md#9-what-plurx-does-not-do):

- **No cloud, no phone-home.** There is no plurx.tv and there never needs to be.
- **plurx never writes your media.** Read-only mounts; no organizing, renaming, or
  deleting.
- **Not a streaming aggregator.** No ads, no live TV, no rentals, no "discover"
  feeds.
- **No music or photos** in v1 (the data model won't preclude them later).
- **No transcode-by-default.** On demand only, when a device forces it.

## License

Private for now. Licensing will be decided if/when the project is shared.
