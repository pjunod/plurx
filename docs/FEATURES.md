# Features — everything plurx does, exhaustively

Companion to [ARCHITECTURE.md](ARCHITECTURE.md) (how it's built) and
[OPERATIONS.md](OPERATIONS.md) (how to run it) — this is the complete inventory
of behaviour. **If a capability isn't listed here, plurx does not do it.** The
last section lists what it deliberately does *not* do, so an absence is never
ambiguous.

Everything below is implemented and shipping as of Phase 2 (movies, TV, anime,
playback, Plex-compat) with the Phase 3 cluster spike complete. Anything still
on the roadmap is called out inline as *planned* with its phase, or lives in the
[not-yet](#9-what-plurx-does-not-do) section — it is never listed as if it
works. Scope and phase gates live in [REQUIREMENTS.md](REQUIREMENTS.md) and
[ROADMAP.md](ROADMAP.md).

---

## 1. Libraries & scanning — "find my media and keep up with it"

**What it does:** turns folders of files into browsable movies, shows, and
anime.

- Three library kinds: **Movies**, **TV Shows**, and **Anime** (a shows library
  flagged for anime rules). One library spans multiple root paths
  (comma-separated).
- **Identification** from filename and folder structure: Plex/Jellyfin layouts
  and scene naming for movies and `S01E02` episodes; anime **absolute
  numbering** (episode 137, no season) routed by anime detection rather than
  forced into TVDB season shapes.
- **Inspection** with `ffprobe` as ground truth: container, video codec/profile,
  width/height, bit depth, HDR type (HDR10 / HDR10+ / HLG / Dolby Vision +
  profile/level), overall bitrate, every audio track (codec, channels,
  language, title, default), every subtitle track (codec, language, forced,
  default), and chapters.
- **Incremental rescan:** unchanged files are skipped by size + mtime, so a
  rescan of a large library is cheap. Vanished files are reconciled (the item
  reflects what's actually on disk).
- **Multiple versions per item:** two files of the same movie (a 2160p remux and
  a 1080p encode) attach to one item, ordered best-first (height, then bitrate).
- **Live scan status** per library: `scanning… N / M files`, then `fetching
  metadata…`, then `idle` — with the file count and any errors surfaced loudly,
  not swallowed. The scan result publishes *before* enrichment so counts and
  problems appear immediately.
- **Refresh art:** re-fetch all metadata and artwork for a library, including
  backfilling season posters onto shows scanned before a poster existed.
- *Planned (fast-follow):* live inotify watching (today: on-demand + create/
  update rescan); manual fix-match UI.

**How to read it:** a library stuck at `scanning… 0 / 0 files` with an error
means the path isn't visible to the **server process** (the usual cause is a
Docker mount that doesn't match the path you typed). `idle` with a low item
count after a scan that reported many files means enrichment matched little —
check the TMDB key.

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
    the target can't take it. Fixes "right codecs, wrong container."
  - **Transcode** — hardware-first (NVENC / QSV / VA-API / VideoToolbox) with a
    software x264 fallback, delivered as **HLS**.
- **HDR → SDR tone-mapping** on transcode so 4K HDR looks right on an SDR screen
  (zscale default, libplacebo opt-in).
- **Resume everywhere:** client-seek for direct play, server fast-seek for remux,
  offset-based session for transcode.
- **Multi-track audio:** pick any audio track from the player; a non-default pick
  forces a remux so the chosen track is the one delivered. Anime dual-audio
  defaults to original audio + subtitles.
- **Subtitles:** text tracks (SRT/ASS) extracted to WebVTT on the fly and shown
  as a selectable native track for direct/remux. Bitmap subs (PGS/VobSub) are
  identified and can only be burned in during transcode (that burn-in is
  *planned*, 2.x).
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
  decoding (the browser's actual resolution, dropped frames, buffer), and Network
  (HLS bandwidth + stream rate) when transcoding.
- **Skip Intro / Skip Credits** buttons appear when playback enters a marked
  region. Markers come from real **chapters** (MakeMKV, anime OP/ED, hand-
  authored titles); a conservative duration-based end-credits estimate is the
  fallback when a file has no chapters. Skipping credits that run to the end
  finishes the item.
- **Auto-skip** intro & credits — an opt-in, per-user, persisted toggle in the
  preferences menu (default off).
- **Method-aware seek:** direct play seeks natively; remux and transcode restart
  the server stream at the new offset.

**How to read it:** a "Skip Credits" button that reads as an estimate exists
because that file had no end-credits chapter — it's a guess and the timeline
knows it. Chapter-derived buttons are exact.

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
  **Classic** (the original look); **Terminal** (a real unix box — `user@plurx:~$`
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

## 9. What plurx does NOT do

Listed so the inventory above is unambiguous — these are deliberate, with reasons:

- **Does not write to your media.** Libraries are read-only; no rename, move,
  organize, or delete. A media server that edits files is one bug from eating
  them.
- **Does not phone home or need the cloud.** No accounts hosted elsewhere, no
  plex.tv contact, no telemetry. It runs on a LAN with no internet.
- **Does not do music or photos** (v1 scope). The data model won't preclude them;
  they are not bolted on speculatively.
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
- **Does not burn in bitmap subtitles yet** (PGS/VobSub) — identified, but
  burn-in is 2.x. Text subs (SRT/ASS) work today.
- **Does not emulate plex.tv** for Infuse/official Plex apps (Tier 2, deferred).
