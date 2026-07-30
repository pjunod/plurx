# Features — everything plurx does, exhaustively

Companion to [ARCHITECTURE.md](ARCHITECTURE.md) (how it's built) and
[OPERATIONS.md](OPERATIONS.md) (how to run it) — this is the complete inventory
of behaviour. **If a capability isn't listed here, plurx does not do it.** The
last section lists what it deliberately does *not* do, so an absence is never
ambiguous.

Everything below is implemented and shipping as of Phase 2 (movies, TV, anime,
playback, Plex-compat), plus the home video & photos slice, with the Phase 3
cluster spike complete. Anything still
on the roadmap is called out inline as *planned* with its phase, or lives in the
[not-yet](#12-what-plurx-does-not-do) section — it is never listed as if it
works. Scope and phase gates live in [REQUIREMENTS.md](REQUIREMENTS.md) and
[ROADMAP.md](ROADMAP.md).

---

## 1. Libraries & scanning — "find my media and keep up with it"

**What it does:** turns folders of files into browsable movies, shows, and
anime.

- Four library kinds: **Movies**, **TV Shows**, **Anime** (a shows library
  flagged for anime rules), and **Home videos & photos** (§1a). One library
  spans multiple root paths (comma-separated).
- **Identification** from filename and folder structure: Plex/Jellyfin layouts
  and scene naming for movies and `S01E02` episodes; anime **absolute
  numbering** (episode 137, no season) routed by anime detection rather than
  forced into TVDB season shapes. An episode whose filename carries nothing —
  a hash-named `.mkv` inside `Show.S01E06.480p.x265-GROUP/` — takes its marker
  from the folder holding it, since that is where such releases put it. The
  crammed DVD-era form is read too (`drawn.together.102` → S01E02): standalone
  three-digit token, seasons 1–9, with resolution and codec numbers excluded —
  and never for anime, where `- 102` is absolute episode 102.
- **Extras are left out, by name.** A `Season NN` directory has no episode to
  inherit, and a file that declares itself an extra — the `-trailer` /
  `-featurette` / `-sample` suffix convention, or *sample* / *trailer* /
  *proof* / *screens* where an episode title can't be — is skipped rather than
  attached to the episode as a second version, where a 30-second clip tying on
  resolution can be served ahead of the real file. Every skip says which of
  these rules fired.
- **Inspection** with `ffprobe` as ground truth: container, video codec/profile,
  width/height, bit depth, HDR type (HDR10 / HDR10+ / HLG / Dolby Vision +
  profile/level), overall bitrate, every audio track (codec, channels,
  language, title, default), every subtitle track (codec, language, forced,
  default), and chapters.
- **Incremental rescan:** unchanged files are skipped by size + mtime, so a
  rescan of a large library is cheap. Vanished files are reconciled (the item
  reflects what's actually on disk).
- **Repairable inspection.** A file whose probe failed — bad permissions, an
  unmounted share, a half-copied download — is retried on every scan, because
  the fixes for all three leave size and mtime untouched and it would otherwise
  sit in the library forever with no codec and no duration. The item page says
  so in as many words and gives admins a **⟳ Reanalyze** button
  (`POST /api/v1/items/:id/reanalyze`); ffprobe's own reason for refusing is
  carried into the scan report and the log, since *Permission denied* and
  *Invalid data* need opposite fixes.
- **Multiple versions per item:** two files of the same movie (a 2160p remux and
  a 1080p encode) attach to one item, ordered best-first (height, then bitrate).
- **Live scan status** per library: `scanning… N / M files`, then `fetching
  metadata…`, then `idle` — with the file count and any errors surfaced loudly,
  not swallowed. The scan result publishes *before* enrichment so counts and
  problems appear immediately.
- **Refresh art:** re-fetch all metadata and artwork for a library, including
  backfilling season posters onto shows scanned before a poster existed — or for
  **one item**, from a **⟳ Refresh artwork** button on its page
  (`POST /api/v1/items/:id/refresh-artwork`, admin). Forced either way: an item
  that already has a poster is fetched again, since "the poster is wrong" is the
  reason someone presses it.
- **Self-healing artwork.** A poster download that fails leaves nothing on disk
  for a rescan to notice, and the enrichment queue keys on *has a provider
  answered*, not on *is there a picture* — so one rate limit used to mean a blank
  card forever. Every attempt is now recorded with its reason, and a sweep
  re-fetches anything matched but pictureless, on a per-item daily backoff so a
  film TMDB genuinely has no art for costs one request a day. TMDB calls
  themselves retry a 429 or a 5xx (honouring `Retry-After`); a 404 stays a fast
  permanent no.
- **Scheduled jobs**, off by default except the artwork retry. Per library: a
  **scan** interval and a **refresh art** interval (Settings → Libraries).
  Server-wide: **retry unreadable files**, **retry missing artwork** (every 30
  minutes unless turned off), and **clean transcode cache**. Intervals are
  minutes with a floor of 15 · a refresh beats a scan when both fall due · runs are stamped on
  completion, on the library row, so neither a slow scan nor a nightly reboot can
  put the schedule into a spin. **Scan at startup** covers what an interval
  can't: files that landed while the server was switched off.
- *Planned (fast-follow):* live inotify watching (today: on-demand, scheduled,
  and create/update rescan); manual fix-match UI.

**How to read it:** a library stuck at `scanning… 0 / 0 files` with an error
means the path isn't visible to the **server process** (the usual cause is a
Docker mount that doesn't match the path you typed). `idle` with a low item
count after a scan that reported many files means enrichment matched little —
check the TMDB key. A scheduled scan that never seems to fire: the clock starts
when the last run *finished*, and `last …` next to the schedule says when that
was; a library still scanning simply skips its turn and takes the next one.

---

## 1a. Home video & photos — "the shoebox, browsable"

**What it does:** turns a folder tree of camera files — phone clips, camcorder
dumps, scanned photos — into a library you can browse, play, and curate. There
is no metadata provider for home video, so everything here comes from the disk.

- **Folders are the organization.** The directory tree is mirrored as folder
  items: `2019/Beach Trip/clip.mp4` becomes **2019** ▸ **Beach Trip** ▸ the
  clip. Loose files at a root stand alone; multiple roots merge by folder name
  at the top level. Deleting a subtree prunes the whole empty chain above it.
- **Titles are the filename, verbatim.** "Christmas 2019.mp4" stays *Christmas
  2019* — the movie parser's year/codec stripping is deliberately not used
  here. The one exception: a leading `2019-06-14 - ` (or `20190614_`) moves
  into the capture date and off the title. Camera names (`IMG_4021`) are left
  alone; the edit UI is the fix, not a guess.
- **Photos** (jpg, jpeg, png, gif, webp, heic, heif) are first-class items —
  scanned, dated, thumbnailed, and viewable in a full-screen lightbox with
  ←/→, Esc, and swipe. They are invisible in movie/TV libraries, exactly as
  before, and `poster.jpg`/`folder.jpg`/`*-thumb.jpg` are treated as artwork
  rather than pictures to browse.
- **Capture date drives everything.** `recorded_at` is filled from the first
  source that has it: NFO `<premiered>`/`<aired>` → the container's
  `creation_time` (EXIF `DateTimeOriginal` for photos) → a date on the filename
  → the file's mtime. "Date recorded" is the default sort for home libraries.
- **Artwork is local only.** Art beside the file wins (`<stem>-thumb.jpg`,
  `poster.jpg` in a folder); otherwise ffmpeg grabs a frame 20% in. Folders
  inherit their first child's poster. Everything lands in the artwork cache —
  never beside your media.
- **Editing** (admin, home libraries only): title, date recorded, description,
  and tags, straight to the database. Tags are searchable. Folders are
  retitlable — the DB title changes, the directory on disk never does.
- **Playback is the ordinary pipeline:** a home video direct-plays, remuxes, or
  transcodes exactly like a movie, with resume, the ◆ Quality menu, and
  continue-watching.
- Photos are excluded from *Recently added* on purpose: a 2,000-photo import
  would otherwise bury the home screen. Its videos and folders still surface it.

**Seed-once semantics — why editing an `.nfo` "does nothing":**

| Situation | What plurx does |
|---|---|
| Clip scanned, sidecar present | Reads `<basename>.nfo` once, applies title/plot/date/tags, marks the item seeded |
| Sidecar added *after* the clip was scanned | Seeds on the next rescan (the check is on the item, not the file's size+mtime) |
| Sidecar edited after seeding | **Nothing.** The DB owns the metadata now — edit in the UI |
| Sidecar is junk (a bare URL, malformed XML) | One scan problem naming the file; item keeps its filename metadata and is marked seeded, so it complains once, not forever |
| Metadata edited in the UI | DB only. plurx never writes an `.nfo`, ever |
| Database rebuilt from scratch | Re-seeds from the sidecars; post-seed edits are gone (that's what backups are for) |

**How to read it:** a home library that scanned files but shows blank cards
means the local-artwork pass failed — check that ffmpeg is present in Settings
→ System. A clip whose date looks wrong is usually mtime (priority 4): the file
was copied, which resets it. Fix it with ✎ Edit; nothing will overwrite it.

---

## 2. Metadata & artwork — "make it look like a library, then work offline"

**What it does:** matches items to real metadata and caches everything.

- **TMDB** agent for movies and TV (title + year matching; movie/show/episode),
  optional API key. Without a key, the library still scans and plays — it just
  shows filenames and no posters.
- **AniList** agent for anime, **no key required**: absolute-numbering ordering,
  title variants, artwork.
- **Artwork cached locally** (posters, backdrops, season posters); provider JSON
  cached too. Once enriched, a library works **offline forever** — no provider
  is contacted to browse or play.
- Graceful art fallback: an item with no poster renders initials on a tinted
  card, and a season with no poster shows its season number — never a blank
  rectangle.
- *Planned (fast-follow):* TVDB agent (TMDB already covers TV), movie
  collections.

---

## 3. Browsing & discovery — "what do I watch"

**What it does:** the web app's home and library views.

- **Home hubs:** Continue watching, Next up (the next unwatched episode of a show
  in progress), and Recently added, plus a best-first grid per library.
- **Library view** with server-reported totals and client-side **sort** (added,
  title, year, rating) and **filter** (unwatched, HDR, 4K) on the loaded page.
- **Item detail:** hero backdrop, poster, breadcrumb trail (Home / Show / Season,
  every level clickable), title, spec chips (year, runtime, kind, resolution,
  HDR), overview, and a labeled spec block per version (Video / Audio / File).
- **Search** across the library (SQLite FTS5), debounced from the header on every
  page.
- **Progress + watched indicators** on posters: a glowing progress bar for
  partially-watched items, a ✓ badge for watched ones.
- *Planned (Phase 6):* public ratings (Rotten Tomatoes / IMDb / Metacritic) on
  the item page; multi-server switching in one dashboard.

---

## 4. Playback — "press play and it just plays"

**What it does:** decides how each file must be delivered to the current device,
and delivers it. Full decision logic is [ARCHITECTURE.md](ARCHITECTURE.md) §3.

- **Three methods, chosen automatically** and reported at `/decision`:
  - **Direct play** — HTTP range serving of the untouched file; zero transcode
    CPU. The goal state.
  - **Remux** — MKV → fragmented-MP4 with `-c:v copy`; audio re-encoded only when
    the target can't take it. Fixes "right codecs, wrong container." (HEVC copy
    is tagged `hvc1` so Safari accepts it; `delay_moov` lets AC-3/E-AC-3 copy
    through the fragmented muxer.)
  - **Transcode** — hardware-first (NVENC / QSV / VA-API / VideoToolbox) with a
    software x264 fallback, delivered as **HLS**.
- **Runtime capability probing — direct-play whenever the browser can.** The web
  player detects what *this* browser actually decodes (`canPlayType` /
  `MediaSource.isTypeSupported` — HEVC, AV1, VP9, AC-3/E-AC-3, Opus, FLAC) and
  sends it with the decision request, so a file only transcodes when this
  browser genuinely can't play it. Safari keeps HEVC (direct for HEVC MP4, a
  copy-remux for HEVC MKV); every browser keeps its own audio codecs instead of
  a needless AAC re-encode. Native/`?profile=` clients still use the named
  profiles — the caps are an override, fully back-compatible.
- **HDR → SDR tone-mapping** on transcode so 4K HDR looks right on an SDR screen
  (zscale default, libplacebo opt-in). HDR direct-plays *only* on an HDR display
  (probed via `matchMedia('(dynamic-range: high)')`); on an SDR screen it
  tone-maps rather than showing washed-out grey. A decodable 4K stream
  direct-plays on a smaller screen (the browser downscales) — resolution isn't
  capped to the display.
- **Manual quality override:** the player's **◆ Quality** menu — *Auto*
  (the automatic ladder), *Original* (never transcode video: direct/remux, with
  the error-path rescuing an undecodable pick), or a forced *1080p / 720p / 480p*
  transcode. Persists per browser; switching restarts at the current position.
- **Resume everywhere:** client-seek for direct play, server fast-seek for remux,
  offset-based session for transcode.
- **Multi-track audio:** pick any audio track from the player; a non-default pick
  forces a remux so the chosen track is the one delivered. Anime dual-audio
  defaults to original audio + subtitles.
- **Language defaults (global):** Settings → Playback defaults sets a preferred
  audio language, subtitle language, and a subtitle mode — *Auto* (subs appear
  only when the audio isn't the preferred language), *Always*, or *Off*.
  English/English/Auto out of the box. The same rule flags default tracks at
  `/decision` and picks the transcode burn-in, so every path agrees; 2- and
  3-letter language tags match each other (`de` = `ger` = `deu`).
- **Audio-sync correction (per file):** the player's **⇄ Sync** menu nudges
  audio ±50/±250 ms and persists the offset to the file (`PUT
  /files/{id}/audio-offset`), so a badly-muxed release stays fixed for
  everyone. ffmpeg applies it with a second `-itsoffset` input on both remux
  and transcode; a nonzero offset forces at least a remux for direct-play
  sources. The menu also shows the container's own declared audio/video
  start-time delta as a diagnostic (declared offsets are already honored and
  never double-applied).
- **Subtitles:** text tracks (SRT/ASS) extracted to WebVTT on the fly and shown
  as a selectable native track for direct/remux. Bitmap subs (PGS/VobSub) are
  a picture rather than text, so there is nothing to hand a `<track>` — picking
  one restarts the stream as a transcode with the subtitle drawn into the
  frames. The menu says `burned in` on those, because that restart is a cost
  worth knowing about before you choose between two English tracks.
- **Copy-video segments are cut where a player loses nothing:** on an HEVC or
  H.264 copy session, plurx does the segmenting itself and places a boundary
  only in front of a keyframe with no leading picture to discard — every
  ordinary boundary on an open-GOP disc remux costs exactly one frame
  ([STUTTER-4K.md](STUTTER-4K.md) §5.6). Where no such keyframe appears
  within 48 MB or 15 s the cut is taken anyway and counted; the decoded
  frames are bit-identical either way, and a stream the reader cannot follow
  falls back to ffmpeg's own muxer automatically.
- **A stalled hardware session self-repairs:** if no HLS segment lands within 8 s,
  the session is killed and respawned on software x264 (the concurrent-QSV-stall
  fix). The user sees the loading overlay a little longer, not a gray screen.
- **Missing-file guard:** if a file isn't on disk (unmounted share), `/decision`
  returns a clear error and the UI refuses to open a player that can never load —
  it shows why instead.
- **AirPlay** from Safari/iOS (native HLS path so an Apple TV can fetch segments
  itself).

**How to read it:** open the player **Stats** overlay (the ⓘ button, or press
`i`). *Method: Direct play* is ideal. *Remux* is cheap. *Transcode · QuickSync*
means the GPU is working; *Transcode · software* means it fell back to CPU —
check Settings → Logs for why the hardware path was rejected. Source vs Now-
decoding shows what the file is versus what your browser is actually rendering.

---

## 5. The player — "a real playback experience, not a gray box"

**What it does:** a borderless, projection-style player in the web app.

- **Borderless / true-black** playback surface; the title, option buttons, and
  cursor auto-hide during playback and reappear on mouse movement.
- **Staged loading overlay** before the first frame: *Reading media → Starting
  the transcoder → Preparing the stream / Buffering* — so a slow start looks like
  progress, not a hang.
- **Rich stats overlay:** Playback (method, encoder, position/duration), Source
  (codec, bit depth, HDR, resolution, bitrate, container, audio track), Now
  decoding (the browser's actual resolution, dropped frames, buffer), Network
  (HLS bandwidth + stream rate) when transcoding, and **Server** — the
  encoder's speed as a multiple of real time, how many seconds it has produced
  beyond your playhead, how long the stream took to start, and how many times
  it has stalled. It stays open until you close it, alongside the Quality,
  Audio, Subtitles and Sync menus — which slide clear of it rather than over
  it — so you can watch what a quality change does to the numbers. On a touch
  screen the two still take turns, there being room for one.
- **Skip Intro / Skip Credits** buttons appear when playback enters a marked
  region. Markers come from real **chapters** (MakeMKV, anime OP/ED, hand-
  authored titles), read from the scan-time probe rather than by re-probing the
  file every time you press Play; a conservative duration-based end-credits
  estimate is the fallback when a file has no chapters. Skipping credits that
  run to the end finishes the item.
- **Auto-skip** intro & credits — an opt-in, per-user, persisted toggle in the
  preferences menu (default off).
- **Method-aware seek:** direct play seeks natively; remux and transcode restart
  the server stream at the new offset.

**How to read it:** a "Skip Credits" button that reads as an estimate exists
because that file had no end-credits chapter — it's a guess and the timeline
knows it. Chapter-derived buttons are exact.

**How to read the Server block:** encode speed below 1× means the server
cannot produce the stream as fast as you are watching it, and a stall is only
a matter of time — that is a transcode problem (a heavier file than this
hardware can do at this quality), not a network one. Speed comfortably above
1× with stalls anyway points at the link instead. "Server ahead · held" is not
a fault: the transcoder has built the whole buffer it is allowed and paused
itself until you catch up.

---

## 6. Users & accounts — "more than one person, safely"

**What it does:** local multi-user accounts.

- **First-run setup** creates the admin account. Local accounts with **Argon2id**
  password hashing; opaque bearer tokens (SHA-256 lookup) per device login.
- **Admin vs standard** users; admin-only Settings (libraries, users, keys,
  logs). User management (add/remove, admin flag) and password handling are
  admin-gated.
- Per-user client preferences (theme, appearance, auto-skip) persist in the
  browser, not the server — one account looks different per device by design.
- *Planned (Phase 6):* OIDC sign-in (Google/Apple); per-user library permissions;
  parental controls.

---

## 7. Theming — "midnight by default, your call otherwise"

**What it does:** a CSS-variable theme engine in the web app.

- **Light/dark that follows the system** by default, falling back to dark when the
  OS expresses no preference — with a manual override that wins.
- **Three named themes** with a selector, each with a light and dark variant:
  **Classic** (the original look); **Terminal** (a real unix box — a `:~$ `
  prompt with a blinking block cursor, man-page section headers, getty-style
  login labels, `$ ls` empty states, syslog-tagged toasts, CRT scanlines; green
  phosphor in the dark, Solarized in the light); and **noirr** (the brand theme,
  built from `brand/` — exact midnight/matinee tokens, the `noirr_` wordmark with
  the cursor as status light (blinks while the server works), kit favicon, glow
  at midnight / red ink + shadows at matinee, film/paper grain on backdrops and
  the login room only).
- **JetBrains Mono + Inter ship embedded** as data-URL `@font-face` (latin
  subsets, ~260 KB total) so the brand type renders on every client with no CDN —
  Terminal and noirr use them; Classic keeps system fonts.
- No flash of the wrong theme on load (the theme resolves in a `<head>` script
  before first paint).
- *Still its own change:* the full product rename — see [ROADMAP.md](ROADMAP.md).

---

## 8. Plex compatibility & operations

**Plex-compat (Tier 1):** a Plex Media Server API façade + GDM discovery so
Kodi-family Plex clients (Composite, PKC), `python-plexapi`, and Home Assistant
browse and play directly against plurx — validated end-to-end with
`python-plexapi`. plex.tv is never contacted. Detail: [CLIENTS.md](CLIENTS.md),
[ARCHITECTURE.md](ARCHITECTURE.md) §5.

**Operations:** `/healthz` (liveness), `/readyz` (storage reachable), Prometheus
`/metrics` (uptime, active transcode sessions, library/user counts); structured
`tracing` logs with an in-app **live log viewer** (filter by level); a **global
activity pill** on every page showing what the server is doing right now (scan,
metadata, streams). Config via `plurx.toml` or `PLURX_*` env. Deploy templates
for Docker/Compose, bare-metal systemd, and Unraid in [`deploy/`](../deploy).
Detail: [OPERATIONS.md](OPERATIONS.md).

**High availability (Phase 3 spike complete, Phase 4 building):** the store
backend (hiqlite, raft-replicated SQLite) and the transcode-failover mechanic
(session restart-at-boundary, any node serves segment N) are **decided and
validated**, not yet wired into a running cluster. Today plurx runs as a single
node; the cluster is the next phase. Detail: [ARCHITECTURE.md](ARCHITECTURE.md)
§2, [PHASE3-SPIKE.md](PHASE3-SPIKE.md).

---

## 9. Trakt — "your history, everywhere"

**What it does:** an optional, admin-linked [Trakt.tv](https://trakt.tv)
integration: live scrobbling plus two-way watched-history and resume-point
sync. Off until keys are entered; plurx works fully without it.

- **Bring your own API app.** The admin creates an app at
  trakt.tv/oauth/applications (redirect URI `urn:ietf:wg:oauth:2.0:oob`) and
  pastes its client id + secret into Settings → Trakt — same pattern as the
  TMDB key, no shared/central credentials.
- **Device-code linking:** Connect shows an 8-character code to enter at
  trakt.tv/activate; the server polls in the background and the card flips to
  connected by itself. Tokens refresh automatically; a dead refresh token
  unlinks cleanly and says so.
- **Live scrobbling:** play → `scrobble/start`, crossing plurx's 95% watched
  threshold → `scrobble/stop` (Trakt records the play), abandoning a session →
  `scrobble/pause` after ~2½ quiet minutes. Your profile shows "watching now."
- **Two-way sync**, hourly and on demand (Sync now, or any manual
  watched/unwatched mark): watches from other Trakt apps mark items watched
  here with their original timestamps; local watches push to Trakt history;
  in-progress positions land both ways, so pausing in another Trakt-connected
  app resumes here. Movies match by TMDB id, episodes by show TMDB id +
  season/episode — items without ids are skipped, never guessed.
- **Conservative conflict rules:** a local un-watch that's newer than the
  remote watch wins (and removes on Trakt); Trakt-side history *deletions*
  don't propagate (a matching failure can never erase local history); a
  `last_activities` gate skips the heavy pulls when nothing changed.
- **First link = full import:** your entire Trakt history lands immediately,
  then plurx-only watches push back.
- **2026 Trakt limits, respected by design:** history (100k) and scrobbling
  are the safe surface; collection/"offline library" sync (100-item cap for
  third-party apps) is deliberately not implemented.

---

## 10. Activity — "what is the server doing right now"

**What it does:** the header's activity pill now lands on `#/activity`, a live
page (3-second refresh) instead of dumping you into Settings.

- **Now playing:** every transcode session with who's watching, what, the
  encoder and target height, and when it started; admins get a **Stop** button
  (`DELETE /activity/sessions/{id}`). Direct play and remux flow straight
  through without a server session and say so.
- **Library:** per-library scan/enrich state with the same live counters as
  Settings.
- **Trakt:** link state, last sync, and the last sync summary or error.

---

## 11. Integrations — "something else fills the library; plurx notices at once"

**What it does:** lets another application on the same box — monarr, today —
tell plurx that a file has landed, instead of plurx finding out on its next
sweep. Nothing here is required: plurx with no integration configured behaves
exactly as it did before, on scheduled and manual scans alone.

- **Scoped API keys** (Settings → an admin creates one). A key is what another
  application holds *instead of* a login token, and the distinction is the
  point: a token IS a user, so an admin token handed to a neighbouring app
  also hands over every secret in `GET /api/v1/settings`. A key carries a
  scope list — `scan:trigger`, `status:read` — and cannot widen itself. Stored
  as a SHA-256 hash; the secret (`plx_…`) is shown **once**, at creation, and
  is unrecoverable afterwards. Revoking one is a single delete and takes
  effect on the next request.
- **`POST /api/v1/scan`** — "index exactly this path", scope `scan:trigger`.
  The caller sends an absolute path (a file or a folder) and plurx works out
  which library owns it, so the caller never has to know plurx's library ids;
  a library nested inside another resolves to the more specific one. Answers
  **200** with the scan report and the items it placed when it can run now,
  **202** with a `request_id` when that library is mid-scan — queued, never
  dropped, because importing a season fires one request per episode and losing
  most of them would leave it half-indexed. `GET /api/v1/scan/requests/{id}`
  (scope `status:read`) reports how a queued one finished.
- **A targeted scan never prunes.** A full scan reconciles: files it did not
  see are gone. A targeted scan looked at one folder, so applying that rule
  would delete the rest of the library. It adds and updates only.
- **Ids from the caller are used, not guessed at.** A request may carry
  `tmdb` / `imdb` (and `series` for an episode, since an episode's own id is
  not what identifies its series). plurx stamps them on the item and
  enrichment then fetches **by id**, skipping the title search entirely — the
  search is the step that puts the 2015 remake's poster on the 1995 film, and
  a wrong id does not stay local: Trakt sync matches on TMDB id too. An IMDb
  id with no TMDB id is resolved with one lookup rather than a search. Items
  with no ids at all are matched by title exactly as before.
- **`correlation_id` is echoed and logged** on every request, so one grep
  reconstructs a single transfer across every application it passed through.
  Integration logging is on its own target (`plurxd::integrate`).
- **Counted, so a silent stop is visible.** `plurx_scan_total{trigger}` on
  `/metrics` splits scans into manual, scheduled, startup and targeted, and
  `plurx_notify_received_total` counts every inbound request — including the
  ones plurx rejects, because a rejected request still proves the caller
  reached it. Every trigger is listed even at zero: a counter that appears
  only once it fires cannot say "this has never happened", which is the most
  useful thing `trigger="targeted"` has to tell you. The same figures, plus
  who called last and when, are on `GET /api/v1/system` under `integration`.
- Pairing runbook and the two curl lines: [OPERATIONS.md](OPERATIONS.md) and
  [CHEATSHEET.md](CHEATSHEET.md); the credential model in
  [SECURITY.md](SECURITY.md).

- **Coming soon rail** — the one thing plurx asks monarr for. Settings →
  monarr takes a URL and a monarr API key; the home screen then carries a
  **Coming soon** rail of what monarr expects in the next four weeks
  ("Expected Friday", "Expected Aug 1"). plurxd makes the call and caches it
  for 15 minutes, so the key never reaches a browser — it can edit monarr's
  whole library, which is exactly why it stays on the server. Unpaired, the
  rail is absent rather than empty; a monarr that is down leaves it absent
  too, because the home screen must not depend on another application being
  up.

- **Watch state → monarr**, *off by default*. When enabled (Settings →
  monarr), finishing something — the 95% crossing or an explicit mark —
  queues a note to monarr saying what was watched, by id, **and which plurx
  user watched it**. monarr uses it to prefer upgrades for shows somebody is
  actually following. The per-user part is why this is opt-in and says so on
  the settings page: viewing history is personal, and this copies it into an
  application with no other reason to hold it. Queued in a table with
  5s/30s/2m retries, so a monarr that was restarting still hears; an item
  with no TMDB/IMDb id is not sent at all, because monarr matches on ids and
  a title would only make it guess. **Nothing is ever deleted as a result** —
  there is no delete path on either side.

- **The pairing says whether it works.** Settings → monarr has a **Test
  connection** button that asks monarr directly and reports what came back:
  connected with monarr's version, "cannot reach", or "rejected the API key"
  — three different problems with three different fixes, which a single
  "failed" would hide. Saving also tests, because saving without checking is
  how a typo survives a week. The watch-notification queue is shown beside
  it (sent / waiting / failed): a reachable monarr with a hundred waiting
  notifications is a different problem from an unreachable one, and both look
  identical without it.

**How to read it:** a 422 naming plurx's library roots means the path exists
for the caller but not for plurx — two containers with different mounts, which
is the likeliest cause by a wide margin; compare the roots in the response
with the path the caller sent. A 403 means the key lacks the scope, not that
it is invalid. Items that arrive but stay titled after their filenames mean
enrichment has no TMDB key, not that the scan failed.

---

## 12. What plurx does NOT do

Listed so the inventory above is unambiguous — these are deliberate, with reasons:

- **Does not write to your media.** Libraries are read-only; no rename, move,
  organize, or delete. A media server that edits files is one bug from eating
  them.
- **Does not phone home or need the cloud.** No accounts hosted elsewhere, no
  plex.tv contact, no telemetry. It runs on a LAN with no internet.
- **Does not push anything to other applications.** The integration in §11 is
  inbound, plus one read: other apps tell plurx to index, and plurx *asks*
  monarr for its calendar if you paired one. plurx never tells another
  application to do something. Pushing watch state back to monarr is on the
  roadmap and is not built.
- **Does not do music** (v1 scope). The data model won't preclude it; it is not
  bolted on speculatively. Photos *are* supported, in home libraries (§1a).
- **Does not expose home libraries through the Plex façade.** Plex has no
  honest section type for a folder tree of camera files, and a half-mapped
  section breaks Kodi clients harder than an absent one.
- **Does not write, export, or re-read `.nfo` sidecars.** One read, at first
  ingest, and never again (§1a).
- **Does not edit photos** — no rotation, cropping, favorites, or face/object
  detection. Browsers honor EXIF orientation on their own.
- **Does not allow metadata edits outside home libraries** — a provider agent
  owns those fields and would overwrite them on the next refresh. Manual
  fix-match is a separate, planned feature.
- **Does not fingerprint or ML-guess intros.** Skip markers come from chapters
  (plus one honest duration-based credits estimate). A wrong "Skip Intro" that
  jumps into a scene is worse than none.
- **Does not transcode by default or pre-bake renditions.** Transcode is on
  demand, only when a device forces it. There is no "optimize library."
- **Does not run a cluster yet.** HA is decided and spiked (§8) but Phase 4;
  today it's a single node.
- **Does not ship native TV apps yet.** Web app first (Tizen/webOS/tvOS/Android
  TV/Roku are Phase 5); Kodi-family Plex clients work today via the compat
  façade.
- **Does not emulate plex.tv** for Infuse/official Plex apps (Tier 2, deferred).
