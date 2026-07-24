# Changelog

All notable changes to plurx are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and plurx uses
[semantic versioning](https://semver.org/) under the 0.x rules described in
[docs/RELEASING.md](docs/RELEASING.md): while the major number is 0, a **minor**
bump may break compatibility and a **patch** bump never does.

## [Unreleased]

## [0.1.0] — 2026-07-22

First numbered release. Everything before this point was developed under the
placeholder version `0.0.1`, which never moved and never corresponded to a tag;
the entries below describe what that work amounts to rather than reconstructing
a hundred commits of history.

### Added

- **Media server.** Library scanning for movies and TV (filename parsing,
  incremental rescans, filesystem watching), TMDB and AniList metadata with
  cached artwork, and per-user watch state including resume positions.
- **Playback.** Direct play where the client can already decode the file,
  remux into a browser-friendly container where only the container is wrong,
  and HLS transcode where it isn't — chosen per client from advertised codec
  support. Hardware encode via NVENC, QuickSync, VA-API, and VideoToolbox, each
  validated by a test encode at startup so only paths that actually work get
  used.
- **Web app.** A single-file SPA that doubles as the admin UI: browse, search,
  a custom player overlay with subtitle and audio track selection, quality
  override, intro/credit skip, autoplay, and Chromecast/AirPlay handoff.
- **Plex compatibility.** A Plex Media Server API façade plus GDM discovery, so
  existing Plex clients connect directly without knowing plurx exists.
- **Clients.** Android and iOS applications alongside the web app.
- **Operations.** Container image with a hardware-transcode-capable ffmpeg,
  Prometheus metrics at `/metrics`, an in-process log buffer surfaced in
  settings, and health probes.

### Changed

- Version numbering is now real: the workspace carries a semantic version, the
  binary is stamped with the git commit it was built from, and `/api/v1/server`
  reports both. See [docs/RELEASING.md](docs/RELEASING.md).

[Unreleased]: https://github.com/pjunod/plurx/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/pjunod/plurx/releases/tag/v0.1.0
