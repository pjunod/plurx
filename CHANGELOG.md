# Changelog

All notable changes to plurx are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and plurx uses
[semantic versioning](https://semver.org/) under the 0.x rules described in
[docs/RELEASING.md](docs/RELEASING.md): while the major number is 0, a **minor**
bump may break compatibility and a **patch** bump never does.

## [Unreleased]

### Added

- **Home video & photos.** A fourth library kind, `home`, turns a folder tree of
  camera files — phone clips, camcorder dumps, scanned photos — into a library
  you can browse, play, and curate. The directory tree *is* the organization:
  `2019/Beach Trip/clip.mp4` becomes **2019** ▸ **Beach Trip** ▸ the clip, and
  titles are the filename verbatim, so "Christmas 2019.mp4" stays *Christmas
  2019* rather than being tidied into *Christmas* by the movie parser. Photos
  are first-class items with a full-screen lightbox (←/→, Esc, swipe), and are
  kept out of *Recently added* so a 2,000-photo import can't bury the home
  screen. Clips play through the ordinary pipeline — direct play, resume,
  quality menu, continue-watching — because a home video is just a video.
- **Capture dates.** Home items carry `recorded_at`, filled from the first
  source that has one: a Kodi `.nfo`, then the container's `creation_time`
  (EXIF `DateTimeOriginal` for photos), then a date on the filename, then the
  file's mtime. "Date recorded" is the default sort for home libraries.
- **`.nfo` sidecars, read exactly once.** A `<basename>.nfo` seeds an item the
  first time it is scanned and is then dead to plurx: never re-read, never
  written. Add a sidecar later and it still seeds on the next rescan; edit one
  after seeding and nothing happens, because the database owns the metadata by
  then and your own edits can never be clobbered by a file on disk. A junk
  sidecar complains once, by name, instead of on every scan.
- **Metadata editing** for home libraries (admin): title, date recorded,
  description, and tags, from a pencil on the item page. Tags are searchable.
  Movies and shows are still owned by their metadata agent and refuse the edit,
  with a message saying why — a hand edit there would vanish on the next
  refresh.
- **Local artwork for home libraries.** No provider is ever called for a camera
  file; art you put beside the media wins (`<stem>-thumb.jpg`, `poster.jpg` in
  a folder), otherwise ffmpeg grabs a frame 20% in, and folders inherit their
  first child's poster. Generated art lives in the artwork cache — plurx still
  never writes anything into your media folders.

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
- Durations under a minute now read in seconds. A six-second phone clip
  labelled "0m" was worse than saying nothing.
- Schema v6 rebuilds the `libraries` and `items` tables to drop their
  kind CHECK constraints — SQLite cannot alter one, and every future media type
  would otherwise repeat the same dance. Kind validation already lives in one
  place per enum. The migration runner now disables foreign keys around each
  migration and runs `PRAGMA foreign_key_check` afterwards, failing loudly
  rather than quietly leaving dangling rows. Existing databases upgrade in
  place; the upgrade is covered by a fixture test that builds a v5 database and
  asserts every row survives.

### Added

- **A file whose probe failed can be read again.** A movie added while its
  permissions were wrong showed `Video —  Audio —` forever: the scan is
  incremental on size and mtime, and `chmod` moves neither, so every later scan
  skipped the file and nothing in the UI offered a way to ask again. Three
  things now close that: the scan retries any file with no media details (in
  place, against the item it already belongs to — re-deriving the item from the
  filename would orphan a home video renamed by an NFO or by hand); a
  **⟳ Reanalyze** button appears on the item page for admins, next to a plain
  explanation of why the dashes are there; and `POST /api/v1/items/:id/reanalyze`
  does the same over the API, reporting per file whether it was repaired, still
  failing, or gone. A scan that fixes something says so — *repaired* is its own
  count, inside `unchanged`, because nothing about the file changed on disk.
- **ffprobe now says why it refused.** It ran under `-v quiet`, so a failure
  produced an exit code and nothing else, and "Permission denied" was
  indistinguishable from "Invalid data found when processing input" — a
  permissions problem and a corrupt file, which have opposite fixes. Failures
  now carry ffprobe's own last line of stderr into the scan report and the log.

### Fixed

- The crammed `102` form is understood. DVD-era rips write season and episode
  as three digits — `drawn.together.102-med.avi` is S01E02 — and plurx skipped
  every one of them. It is tried only after `S01E02` and `1x02` fail, on a
  standalone three-digit token, seasons 1–9 only: four digits are a year or a
  resolution far more often than they are season 10, and inventing an episode is
  worse than skipping one. Resolution and codec numbers are excluded by name
  (`264` would otherwise be season 2, episode 64), and `x264`, `1080p` and
  `480p` can't match at all because their digits touch a letter. No episode
  title is taken from this form — what follows the digits is the release group,
  and *Episode 2* beats *med*. Anime is deliberately exempt: there, three digits
  after a dash are the absolute episode number, so `One Piece - 102` stays
  episode 102 rather than becoming season 1, episode 2.
- Samples and other extras say that's what they are, instead of claiming the
  episode couldn't be identified. A skip note prints the whole path, so
  "no season/episode marker … in the file name or on the folder holding it"
  under `Drawn.Together.S01E02.DVDRip-MEDiEVAL/sample.drawn.together.102-med.avi`
  was the same self-contradiction as before, one layer down: the file was
  excluded on purpose and the message described a different rule. Skip reasons
  are now produced where the decision is made rather than reconstructed after
  the fact, so they cannot drift again. Extras are recognized two ways: the
  Plex/Jellyfin `-trailer` / `-featurette` / `-sample` suffix convention, and
  the words *sample*, *trailer*, *proof*, *screens* where an episode title
  cannot be — before the marker, or anywhere in a hash-named file that borrowed
  its folder's marker. *sample* counts anywhere, since no real episode title
  contains it; the others are left alone inside a title, so
  `Show.S01E02.The.Trailer.Park` is still an episode. This also catches extras
  that carry a perfectly good marker of their own (`Show.S01E02.sample.avi`),
  which previously attached to the episode as a second version and, tying on
  resolution, could be picked ahead of the real file.
- Episodes whose filename is a hash are found by their release folder. A common
  torrent shape puts every identifying token on the directory —
  `Drawn.Together.2004.S01E06.Dirty.Pranking.Number.2.480p.DVD.x265.Panda/` —
  and names the file inside `956a4a82d3e71a92e95bc3658e6978d7.mkv`. The parser
  only ever regexed the *filename*, so every one of these was skipped, and the
  skip note printed the full path while claiming there was no `S01E06` in it —
  a message that contradicted itself on screen. The marker is now read from the
  immediate parent directory when the filename hasn't got one, which is where
  the show title has always been read from anyway; the folder above the release
  still wins the show name, so a file under `Drawn Together/Season 1/` is still
  *Drawn Together* and not *Drawn.Together.2004*. Two things deliberately do not
  inherit a folder's marker: a `Season 02` directory, which carries a season but
  no episode, and a file whose own name says it isn't the episode (`sample`,
  `trailer`, `proof`, `screens`, `rarbg`) — a 30-second sample beside the real
  file would otherwise attach to the same episode and, on a resolution tie, sort
  ahead of it. The skip message now names both places that were checked.
- A scan that reports errors now says which files and why. The scan report has
  always carried two things — a count and a list of human-readable problems —
  but only the two library-wide failures (a missing root, an unreadable
  directory) ever filled the list. The two *per-file* failures did not, so a
  scan that hit a bad file showed a red **2 errors** in Settings with nothing
  underneath it and no way to find out which two of three hundred files were
  meant. Worse, one of the two logged nothing either, so the server log couldn't
  answer the question the UI had raised. Both now name the path and the cause:
  a file the walk listed but couldn't stat says whether it's permissions, a
  broken symlink or a file moved out from under the scan, and notes that the
  existing record was kept; a file ffprobe refuses says it was added without
  codec or duration, which is what makes its later playback decisions guesses.
  The rule is now an invariant on the report — no error is counted without a
  line describing it *and* a log line at ERROR level — and it is what the new
  tests check. ERROR rather than WARN so that setting the log view to **Errors**
  narrows it to exactly the files a scan complained about, which also means they
  are still findable after a few hundred lines of metadata fetching have rolled
  through the log's ring buffer.
- The scan counts no longer read as more events than there were files. A file
  ffprobe refused was counted as both *added* and an *error*, so two bad new
  files produced "2 added … 2 errors" — four numbers for two files, with nothing
  saying they were the same two. The counts are now explicitly two different
  kinds of thing. What happened to a file's record — added, updated, unchanged,
  skipped, or the new *unreadable* — is a partition: every file the walk found
  lands in exactly one, and they sum to the total, which a test now asserts.
  Whether it came through cleanly is a flag on top: the new *degraded* count is
  a subset of added and updated, and is displayed attached to them — "2 added
  (2 incomplete)" — rather than as an unrelated number further down the line.
  Moving those files out of *added* was the other option and would have been
  worse: the scan would report nothing added while two new items sat in the
  library, so the overlap is kept and simply stated.
- Scan problems are boxed and scroll in place instead of printing into the
  libraries table. A TV scan reporting 187 skips pushed every other library off
  the screen and turned the settings page into a log dump; the list is now a
  fixed-height panel that scrolls, so a row is the same size whether its scan
  found one problem or a thousand. Long release-group paths wrap inside the box
  rather than stretching the column, and a panel holding real failures gets a
  red left spine that a skips-only panel doesn't — bad scans stay spottable at a
  glance now that the text itself is contained.
- Skipped files say which ones. "1 skipped" meant a file the library kind
  couldn't identify, most often an episode with no `S01E02` in the name sitting
  in a Shows library, and the only way to find it was to guess. Skips are listed
  by name, with what was expected, in quieter ink than the errors they sit
  beside — they are usually a stray trailer, not a problem. Both lists are
  capped so a pathological library can't produce a page-long status line, with
  a trailing count of what was left out; errors claim the cap before skips do.
- Closing the player while it is fullscreen now hands the screen back. Fullscreen
  belongs to the browser rather than to the page, so hiding the player's own UI
  did not end it: `#player` stayed the fullscreen element and the tab kept the
  whole display with nothing drawn on it. On a desktop that reads as a black
  screen you press Esc on out of reflex; an iPad has no Esc, only Safari's own ✕
  in the top-left corner, so until you find it the browser is unreachable and
  the app looks hung. Every exit runs through this path, including the automatic
  one when a film ends with nothing queued behind it. A picture-in-picture window
  is closed on the same occasion and for the same reason — it is the browser's
  window, and emptying the video element leaves it open, playing nothing. The
  fullscreen button can also now turn *off* iPhone-style video fullscreen, which
  it was able to enter but not see: `document.fullscreenElement` never reports
  that mode, so pressing the button a second time did nothing.
- The Docker deployment no longer shares a Compose project with anything else
  on the host that happens to live in a directory called `deploy`. Compose
  names a project after the directory holding its file, so plurx's was
  `deploy` — and two stacks that resolve to the same project name *are* one
  project: `docker compose ps` in either lists the other's containers, and `up
  --remove-orphans` or `down` in either **deletes** the other's, on the
  reasoning that they aren't in the compose file it just read. The container
  disappears rather than exiting, so there is no exit code, no restart, and no
  log left to read; `restart: unless-stopped` cannot help something that no
  longer exists. `container_name: plurxd` hid the collision rather than causing
  it, by keeping the service off the `<project>-<service>-1` naming that would
  have made it obvious. The project is now pinned to `plurx`, and the data
  volume to `plurx-data` rather than `<project>_plurx-data`. Read **Renaming
  the data volume** in `deploy/README.md` before upgrading: pointing plurxd at
  a volume that doesn't exist yet is not an error — Docker creates an empty one
  and plurxd initialises a fresh database in it, which looks exactly like
  losing your library.
- A `docker stop` no longer becomes a SIGKILL. plurxd drained in-flight
  requests without a deadline, and a playback response is a stream: a paced
  remux holds its connection open for a quarter of the film's runtime, and an
  HLS playlist poll is only ever moments from the next one. Waiting for all of
  them to finish meant never finishing, so Docker spent its ten-second grace
  period achieving nothing and then killed the process — which surfaces as exit
  137, indistinguishable at a glance from an out-of-memory kill. The drain is
  now capped at five seconds, comfortably inside that grace period, and logs
  what was still open when it gave up.
- Items are no longer marked watched minutes in. The web player treated the
  `ended` event as "the film finished", but `ended` fires at the end of the
  media the browser *has*, which is not the same thing: a remux is a
  one-directional pipe that stops early whenever its session is reaped or
  ffmpeg exits, and an HLS playlist that stops growing reads as complete. On
  that signal the player posted the full runtime as the position, which cleared
  the server's 95% threshold — and since the watched flag is deliberately
  sticky, one truncated stream marked a film watched for good and told Trakt so.
  `ended` now only counts as completion when the playhead really is at the end;
  short of that the true position is recorded and playback resumes from there,
  giving up after three failures at the same spot rather than looping. The
  player also no longer offers a growing stream's `duration` as the runtime —
  only direct play's own duration is the whole file — and the server clamps a
  reported position to the runtime so a resume point can't land past the last
  frame. Anything already mis-marked can be cleared with **Mark unwatched** on
  the item.
- On a touchscreen, the playback-info panel no longer buries the player
  controls. It had no size limit and no close affordance — touch has no `i` key
  to press — so opening it covered both control bars with no way back. It now
  carries a close button, scrolls internally, and is capped against the top
  bar's real height (it wraps to three rows on a narrow screen), remeasured on
  rotate. It stays a card rather than stretching corner to corner, and is only
  as tall as what it holds, so on a tablet it covers about a sixth of the screen
  instead of most of it; it fills the width only where the screen is too narrow
  for the card. The quality menu is bounded the same way, and the two panels
  take turns rather than stacking.
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
  flat-out (so starting and seeking stay instant) and then settle to 4× real
  time,
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
