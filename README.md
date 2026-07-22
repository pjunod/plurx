# plurx

[![ci](https://github.com/pjunod/plurx/actions/workflows/ci.yml/badge.svg)](https://github.com/pjunod/plurx/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/pjunod/plurx/branch/main/graph/badge.svg)](https://codecov.io/gh/pjunod/plurx)

A self-hosted media server and player family in the spirit of **old-school Plex** — before the streaming tiles, live TV, ads, and cloud accounts. Your media, your hardware, your network. One lean server binary, clients everywhere, and something no media server has ever shipped: **real high-availability clustering**.

## Principles

1. **Your media, your rules.** No cloud dependency, no phone-home, no accounts hosted by anyone else. Everything works on a LAN that never touches the internet.
2. **Direct play first.** The server's job is to get out of the way. Every client uses hardware decoding; the server only remuxes or transcodes when a device truly can't handle the file — and when it must, it uses hardware encoders.
3. **Lean and boring to operate.** One static Rust binary. No external database, no message broker, no sidecar services. Run one node, or run three identical binaries and they form an HA cluster.
4. **HA is a feature, not an ops project.** Active-active nodes over shared storage. Settings, users, watch state, and playback sessions replicate across the cluster; a node dying mid-movie costs seconds, not your evening.
5. **Meet clients where they are.** A native API for our own apps, plus a Plex Media Server-compatible API so existing third-party Plex clients can point at plurx and just work.

## What it is (v1 scope)

- Movies, TV shows, and anime (with anime-correct metadata and episode ordering)
- 4K HDR10 / Dolby Vision and lossless audio (TrueHD/Atmos, DTS-HD MA) as first-class citizens
- Local user accounts and profiles, optional Google/Apple OIDC sign-in, bring-your-own remote access
- Clusters of 1 or 3+ nodes (active-active, shared media storage, embedded raft — no external infra)
- Clients: web app first (doubles as the admin UI), then Samsung Tizen / LG webOS (shared web core), Apple TV (native Swift), Android/Google TV (Kotlin, covers Sony), Roku (SceneGraph)
- Plex-compat API so Kodi-family Plex clients and plexapi-based tooling work out of the box

## What it is not

- Not a streaming aggregator. No ad-supported movies, no live TV, no "discover" feeds, no rentals.
- Not a cloud service. There is no plurx.tv and there never needs to be.
- Not an everything-server. Music and photos are explicitly out of scope for v1 (the data model won't preclude them later).

## Status

**Phase 2 complete** (see [docs/ROADMAP.md](docs/ROADMAP.md)): plurx is old Plex, honestly replaced for movies, TV, and anime on the LAN — any file plays on the web app or a Kodi/Plex-compat client, HDR looks right on SDR screens, and watch state is trustworthy.

- **Scanner** — identifies movies/episodes (Plex/Jellyfin + scene naming, and anime absolute numbering), probes with `ffprobe` (codecs, HDR, audio/subtitle tracks), incremental, reconciles vanished files.
- **Metadata** — TMDB for movies/TV (optional key) and **AniList for anime** (no key); cached artwork; fully offline once enriched.
- **Playback** — data-driven device profiles + decision engine (direct / remux / transcode), HTTP-range direct play, MKV→fMP4 remux, and full **hardware transcode** (validated NVENC/QSV/VA-API/VideoToolbox with software fallback) delivering **HLS** with **HDR→SDR tone-mapping** and subtitle burn-in.
- **Anime** — absolute episode numbering, AniList metadata, and dual-audio default-track selection (prefer original audio + subs).
- **Plex-compat (Tier 1)** — a Plex Media Server API façade + GDM discovery, so Kodi (Composite/PKC), python-plexapi, and Home Assistant browse and play directly. Validated end-to-end with python-plexapi.
- **Web app** — login, browse, an in-modal player (native + hls.js, **AirPlay** from Safari/iOS) with resume, continue-watching, **next-up**, search, multiple versions per item, and a **Settings** area: server diagnostics, libraries with live scan status + loud scan problems, TMDB key, and a live log viewer. A global activity pill shows what the server is doing on every page.
- **Ops** — `/healthz`, `/readyz`, Prometheus `/metrics`; Docker/Compose, bare-metal systemd, and Unraid deploy templates in [`deploy/`](deploy/).

```sh
cargo run -p plurxd            # serves http://localhost:32600 — open it in a browser
```

First launch walks you through creating an admin account and adding a library. Configuration: copy `plurx.example.toml` to `plurx.toml`, or use `PLURX_*` env vars (`PLURX_FFMPEG`/`PLURX_FFPROBE`, `PLURX_HWACCEL`, `PLURX_TONEMAP`).

Deferred to a Phase 2.x fast-follow: TVDB agent (TMDB already covers TV), movie collections, playlists, and bitmap-subtitle burn-in.

**Phase 3 (cluster spike) is complete** — the decision gate for HA. Both risks were spiked with real experiments (see [docs/PHASE3-SPIKE.md](docs/PHASE3-SPIKE.md)): the replicated store backend is **hiqlite** (raft-replicated SQLite; its API maps onto plurx's existing `Store` mappers, verified with a live node), and the transcode-failover mechanic is a session restart-at-boundary that any node can serve (validated against sparse-keyframe and VFR sources, with byte-identical segments). Next: **Phase 4** — HA for real, adding `HiqliteStore` behind the unchanged `Store` trait.

| Document | Contents |
|---|---|
| [docs/REQUIREMENTS.md](docs/REQUIREMENTS.md) | Product requirements — scope, playback contract, HA contract, users, metadata, deployment |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Server design — components, cluster/replication model, streaming pipeline, tech stack |
| [docs/CLIENTS.md](docs/CLIENTS.md) | Client strategy — platform matrix, Plex client compatibility tiers, per-platform constraints |
| [docs/ROADMAP.md](docs/ROADMAP.md) | Phased plan sized for a solo developer + AI pair, always-shippable increments |

## Development

All developer tasks go through the `Makefile` — CI runs the same targets, so
"green locally" means "green in CI":

```sh
make            # list every target
make run        # serve http://localhost:32600
make check      # fmt-check + clippy + test (the CI gate)
make test       # just the tests
make coverage   # line coverage via cargo-llvm-cov → lcov.info
make hooks      # install a pre-commit hook that runs `make check`
```

`make hooks` installs a git pre-commit hook so a commit can't land unless
`make check` passes (bypass a single commit with `git commit --no-verify`).

CI (`.github/workflows/ci.yml`) runs `make check`, uploads coverage, and
cross-builds amd64 + arm64 on every push and PR. Pushing a version tag
(`git tag v0.1.0 && git push --tags`) additionally builds and publishes a
multi-arch image to `ghcr.io/pjunod/plurx`.

The two badges above need one-time linking to render live: the **ci** badge
works as soon as the workflow runs on GitHub; the **coverage** badge needs the
repo added at [codecov.io](https://codecov.io) and its upload token stored as
the `CODECOV_TOKEN` repository secret (CI is configured to not fail if it's
absent).

## License

Private for now. Licensing will be decided if/when the project is shared.
