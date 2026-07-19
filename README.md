# plurx

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

**Phase 0 complete** (see [docs/ROADMAP.md](docs/ROADMAP.md)): cargo workspace (`plurx-core`, `plurx-compat-plex`, `plurxd`), the `Store` trait with a single-node SQLite backend behind it, and a running daemon with `/healthz`, `/readyz`, and the seed of `/api/v1`. CI covers fmt/clippy/tests, amd64+arm64 release builds, and the Docker image. Next: Phase 1 — scanner, TMDB metadata, and playback.

```sh
cargo run -p plurxd            # serves http://localhost:32600
curl localhost:32600/api/v1/server
```

Configuration: copy `plurx.example.toml` to `plurx.toml`, or use `PLURX_*` env vars.

| Document | Contents |
|---|---|
| [docs/REQUIREMENTS.md](docs/REQUIREMENTS.md) | Product requirements — scope, playback contract, HA contract, users, metadata, deployment |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Server design — components, cluster/replication model, streaming pipeline, tech stack |
| [docs/CLIENTS.md](docs/CLIENTS.md) | Client strategy — platform matrix, Plex client compatibility tiers, per-platform constraints |
| [docs/ROADMAP.md](docs/ROADMAP.md) | Phased plan sized for a solo developer + AI pair, always-shippable increments |

## License

Private for now. Licensing will be decided if/when the project is shared.
