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

**Phase 1 complete** (see [docs/ROADMAP.md](docs/ROADMAP.md)): plurx is a working single-node media server. Point it at a folder of movies or TV, open the web app, and play.

- **Scanner** — walks libraries, identifies movies/episodes (Plex/Jellyfin + scene naming), probes each file with `ffprobe` (codecs, HDR, audio/subtitle tracks), incremental by size+mtime, reconciles vanished files.
- **Metadata** — optional TMDB agent (title/year matching, overviews, cached posters/backdrops/stills); works fully offline without a key.
- **Accounts** — first-run admin setup, Argon2id logins, token auth.
- **Native API** — libraries, browse (grids, detail, home hubs), FTS search, watch progress/resume, artwork, scan status.
- **Playback** — data-driven device profiles + a decision engine (direct/remux/transcode), HTTP range direct-play, and on-the-fly MKV→fMP4 remux via ffmpeg.
- **Web app** — a self-contained SPA embedded in the binary: login, browse, an in-modal player with resume, continue-watching, and an admin panel.

```sh
cargo run -p plurxd            # serves http://localhost:32600 — open it in a browser
```

First launch walks you through creating an admin account and adding a library. Configuration: copy `plurx.example.toml` to `plurx.toml`, or use `PLURX_*` env vars (`PLURX_FFMPEG`/`PLURX_FFPROBE` to point at a specific build, e.g. jellyfin-ffmpeg).

Next: **Phase 2** — hardware transcode, HDR tone-mapping, anime metadata (AniDB/AniList), and the Plex-compat façade. Then **Phase 3–4**: the HA cluster.

| Document | Contents |
|---|---|
| [docs/REQUIREMENTS.md](docs/REQUIREMENTS.md) | Product requirements — scope, playback contract, HA contract, users, metadata, deployment |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Server design — components, cluster/replication model, streaming pipeline, tech stack |
| [docs/CLIENTS.md](docs/CLIENTS.md) | Client strategy — platform matrix, Plex client compatibility tiers, per-platform constraints |
| [docs/ROADMAP.md](docs/ROADMAP.md) | Phased plan sized for a solo developer + AI pair, always-shippable increments |

## License

Private for now. Licensing will be decided if/when the project is shared.
