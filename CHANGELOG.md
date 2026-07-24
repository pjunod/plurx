# Changelog

All notable changes to plurx are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and plurx uses
[semantic versioning](https://semver.org/) under the 0.x rules described in
[docs/RELEASING.md](docs/RELEASING.md): while the major number is 0, a **minor**
bump may break compatibility and a **patch** bump never does.

## [Unreleased]

### Changed

- Every drill-in page — item, library, category, search — now shares one header.
  The back control sits on the same line as the breadcrumb trail instead of
  floating above it on its own, and always points at the crumb one level up, so
  the button and the trail can no longer disagree about where "up" is.
- The breadcrumb trail records the list you arrived from. Opening a movie from
  the full Movies page now reads **Home / Movies / The Menu** rather than
  **Home / The Menu**, and the middle crumb returns you to the list — including
  for categories and search results. Show → season → episode hops keep it, so
  the trail stays complete all the way down. A deep link with no such history
  still degrades to **Home / Title**.
- 2160p content is labelled `2160p` rather than `4K`, matching every other
  resolution chip in the app.

### Fixed

- On a phone, the playback-info panel no longer buries the player controls. It
  had no size limit and no close affordance — touch has no `i` key to press —
  so opening it covered both control bars with no way back. It is now bounded
  to the band between the two bars, scrolls internally, carries a close button,
  is measured at open time against the top bar's real height (it wraps to three
  rows on a narrow screen), and repositions on rotate. The quality menu is
  bounded the same way, and the two panels take turns rather than stacking.
- Remux streams are no longer delivered as fast as the link allows. A `-c copy`
  remux was a disk-to-socket pipe braked only by the browser's buffer, measured
  here at over 200× real time; because every seek opens a fresh stream, seeking
  meant repeated line-rate bursts. On Wi-Fi that monopolises airtime for the
  duration, and a client whose DHCP lease came up for renewal during one could
  lose the lease and then struggle to reacquire it: the client's own requests
  are 802.11 unicast to the AP and so are ACKed and retried by the MAC layer,
  but the server's reply has to cross a downlink queue that the burst is busy
  filling, and if it is group-addressed it gets no ACK, no retry, the lowest
  basic rate, and a wait for the next DTIM. Streams now burst 30 seconds
  flat-out
  (so starting and seeking stay instant) and then settle to 4× real time,
  configurable in **Settings → Playback → Delivery speed**.
- Seeking no longer stacks transcode sessions. Each seek started a new session
  and left the old one for the idle reaper, so up to ~75 seconds of a second
  ffmpeg kept encoding after every seek, without bound. A new session now
  supersedes that viewer's previous session on the same file immediately.
- The web player no longer strands hls.js instances. Overwriting the live
  instance on seek, audio switch, or fallback left the old one polling a
  playlist that never ends, forever; each one also re-fetched segments from the
  session it was supposed to have left behind.
- Fixed a progress-timer leak on auto-next: each episode transition left the
  previous episode's 5-second progress poster running, so after N episodes N+1
  reported concurrently.
- `POST /api/v1/client-log` is rate-limited to 30 reports/minute. A browser
  looping on an error could otherwise flush the entire 2000-line log ring in
  seconds, erasing the history an operator opens the page to read. Suppressed
  reports are counted and reported rather than silently dropped.

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
