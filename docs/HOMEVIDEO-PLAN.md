# Home video & photos — implementation plan (`home` libraries)

**Status:** ✅ **built** (2026-07-25, M1–M8) · **Decided:** 2026-07-25 (founding
Q&A with Paul) · **Verified against:** the 2026-07-23 tree (HEAD ebcb8a2).

This is now the *record of why home video works the way it does*, not a
work order. What shipped matches the plan; the deltas worth knowing:

- **Kind CHECKs were dropped, not extended** (§3.2 flagged the choice; Paul
  chose drop). Migration v6 landed with the FK-off runner change and the FTS
  rebuild exactly as designed.
- **Artwork sidecars are excluded from the scan** — `poster.jpg`,
  `folder.jpg`, `<stem>-thumb.jpg` are artwork for something else, not photos
  to browse. The plan didn't say so; local enrichment adopting a file that had
  also become an item made it obvious.
- **`ScanReport` gained `seeded`** so a late-arriving NFO shows up in the scan
  tally instead of hiding inside `updated`.
- **Home videos carry `runtime_ms`** from their own probe, so a clip card can
  wear a duration badge without loading its files.
- **Durations under a minute read in seconds** app-wide; "0m" on a six-second
  clip was worse than nothing.
- **Doc screenshots were not regenerated:** the docs carry no home-library
  shot, and a page of ffmpeg test-pattern tiles would be worse than none. A
  real screenshot wants a real (or fictional but plausible) library.

Read [REQUIREMENTS.md](REQUIREMENTS.md) §2 and [ARCHITECTURE.md](ARCHITECTURE.md)
§4 + §8 before starting — this plan amends both, and the amendments are part
of the work (§12). Work milestone by milestone (§11); every milestone ends
`make check` green (fmt --check + clippy + test — clippy and tests alone miss
the fmt gate, which has broken CI before). Standing instruction: **if a step
seems to require writing into a library path, or re-reading an NFO after an
item is seeded, stop and flag it** — that contradicts the founding decisions
below and is a design problem, not an implementation detail.

## 1. Objective & founding decisions

One new library kind, `home`, that turns a folder tree of camera files —
phone clips, camcorder dumps, scanned photos — into a browsable, playable,
curatable library. There is no metadata provider for home video; the source
of truth is what's on disk: the folder layout, an optional Kodi-style `.nfo`
sidecar per video, and the files' own embedded dates.

Four decisions, from the owner, each with its consequence:

1. **The NFO is a one-time seed, not a data backend.** The scanner reads
   `<basename>.nfo` when it first ingests a video and uses it to build the
   item's metadata in the plurx DB. From then on the DB owns the metadata:
   edits in the UI change the DB only, plurx never writes an `.nfo`, and it
   never re-reads one it has already consumed (§4.3 has the exact contract).
   Consequence: ARCHITECTURE §8's "plurx never writes to media storage"
   survives byte-for-byte — there is no write-back machinery in this feature,
   anywhere. Accepted trade-off: a rebuilt-from-scratch DB re-seeds from
   NFOs and loses post-seed edits; that's what DB backups are for.
2. **Folders are the organization.** The directory tree is mirrored as
   browsable folder items ("2019/Beach Trip" → nested groups). No albums,
   no playlists-as-events, no config — how you arranged the files is the
   answer. Loose files at the library root are standalone videos.
3. **Kodi-compatible NFO dialect.** Per-video `<basename>.nfo` with a
   `<movie>` root element — the shape Kodi, Jellyfin, Emby, and
   tinyMediaManager all read and write, so existing tooling can author the
   sidecars plurx consumes (§5 is the parsing contract).
4. **v1 scope: frame-grab thumbnails, in-UI metadata editing, and photos**
   living alongside videos in the same folder tree. Explicitly out (§2):
   Plex-façade exposure of home libraries, NFO write-back or export, and any
   provider lookup.

## 2. Non-goals (guardrails — do not do these)

An executing agent without explicit guardrails will helpfully "improve"
things out of scope. Do not:

- **Write anything under a library path.** No `.nfo` writes, no thumbnails
  next to media, no `.plurx` marker files. Generated artwork goes in the
  artwork cache dir exactly like TMDB posters do today. This is the
  project's load-bearing non-goal (ARCHITECTURE §8) and this feature was
  explicitly designed to not need an exception.
- **Re-read an NFO after seeding.** No mtime-watching of sidecars, no
  "NFO changed on disk" reconciliation, no re-import button in v1. Seed
  once, then the DB owns it (§4.3).
- **Call any metadata provider for home libraries.** No TMDB/AniList
  lookups — there is nothing to match against, and a false match on
  "Christmas 2019.mp4" would be worse than nothing.
- **Expose home libraries through the Plex façade** (`plurx-compat-plex`).
  The façade should skip `home` libraries entirely in v1 — a half-mapped
  section type breaks Kodi clients harder than an absent one.
- **Touch the movies/shows scan or enrichment paths.** The `home` arm is
  additive. Existing parse behavior (including its year/codec token
  stripping) must not change — 90 core tests stay green untouched.
- **Build photo editing, rotation, favorites, or face/object anything.**
  v1 photos are: scanned, dated, thumbnailed, viewable. Nothing else.
- **Allow metadata edits outside home libraries.** The new PATCH endpoint
  (§8.2) refuses items whose library kind isn't `home` — movie/show items
  are owned by their provider agents until the REQ-META-5 fix-match UI
  exists, and a hand-edit would be silently clobbered by the next refresh.

## 3. Data model & migration v6

### 3.1 New enum values (`plurx-core/src/domain.rs`)

`LibraryKind` gains `Home` (`"home"`). `ItemKind` gains `Folder`
(`"folder"`), `Video` (`"video"`), and `Photo` (`"photo"`). Extend both
`as_str`/`parse` pairs. Anime stays a shows-only flag (`libraries.rs`
already forces `anime = false` for non-shows kinds — verify it treats
`home` the same).

The `Item` struct gains three fields (and `MetadataPatch` mirrors the first
two):

```rust
/// ISO-8601 date or datetime ("2019-06-14" / "2019-06-14T18:22:03") of when
/// the footage/photo was captured. TEXT, lexicographically sortable; see
/// §4.4 for the precedence ladder that fills it.
pub recorded_at: Option<String>,
/// Free-form labels ("beach", "kids"). JSON array in SQLite, like
/// audio_streams. Seeded from NFO <tag>/<genre>, edited in the UI.
pub tags: Vec<String>,
/// Unix seconds when an NFO sidecar was consumed for this item.
/// None = never seeded (and eligible for seeding if a sidecar appears).
pub nfo_seeded_at: Option<i64>,
```

`ItemSort` gains `Recorded` (`"recorded"`): `ORDER BY (recorded_at IS NULL),
recorded_at DESC, sort_title` — nulls last, newest first.

Update `ITEM_COLS`, `item_from_row`, and every `INSERT`/mapper in
`store/sqlite/` **by appending columns at the end** so existing positional
offsets stay valid — `item_from_row` indexes by position.

### 3.2 Migration v6 — the CHECK-constraint rebuild (the risky part)

The v2 schema bakes the allowed kinds into CHECK constraints:

```sql
kind TEXT NOT NULL CHECK (kind IN ('movies','shows'))            -- libraries
kind TEXT NOT NULL CHECK (kind IN ('movie','show','season','episode')) -- items
```

SQLite cannot alter a CHECK, so v6 must rebuild both tables. Three traps,
each of which eats data if missed:

1. **`ALTER TABLE … RENAME` rewrites child FK references** (default
   `legacy_alter_table = OFF`): renaming `libraries` to `libraries_old`
   would repoint `items.library_id`'s FK at `libraries_old`. So use the
   documented create-new → copy → drop-old → rename-new order, never
   rename-old-first.
2. **`PRAGMA foreign_keys` is a no-op inside a transaction**, and
   `SqliteStore::migrate` wraps each migration in `BEGIN … COMMIT`
   (`store/sqlite/mod.rs`). With FKs ON, `DROP TABLE items` cascades and
   `DROP TABLE libraries` fails against children. Extend the migration
   runner: set `PRAGMA foreign_keys=OFF` before applying a migration and
   restore `ON` after (it already runs before the loop in `init`), and run
   `PRAGMA foreign_key_check` after each migration, failing loudly on any
   row. This is a runner change, not a v6 special case — comment why.
3. **The FTS index is contentless-external** (`content='items'`) with
   AFTER triggers. Dropping/recreating `items` orphans it. v6 must drop
   the three `items_fts_*` triggers, rebuild `items`, recreate the FTS
   table and triggers, then `INSERT INTO items_fts(items_fts)
   VALUES('rebuild');`.

v6 contents (one migration string, append-only as always):

- Rebuild `libraries`: identical columns, **CHECK dropped** (see below),
  copy all rows, recreate nothing else (no indexes on it today).
- Rebuild `items`: existing columns + `recorded_at TEXT`,
  `tags TEXT NOT NULL DEFAULT '[]'`, `nfo_seeded_at INTEGER`, **CHECK
  dropped**; copy rows (`SELECT *, NULL, '[]', NULL`); recreate
  `idx_items_library_kind`, `idx_items_parent`, `idx_items_added`.
- Recreate `items_fts` with columns `title, overview, tags` (+ updated
  triggers listing `tags`) and run the `'rebuild'` command. Search now
  matches tags for free.
- `files` needs no change — a photo is a row with image `container`,
  width/height from probe, null duration, empty stream arrays.

**Why drop the CHECKs instead of extending them:** every future media type
(music is a named Phase 6 bet) repeats this whole dance if the CHECK stays.
Kind validation already lives in one place per enum (`ItemKind::parse` /
`LibraryKind::parse`, and `item_from_row` fails loudly on unknown kinds),
so the constraint is redundant with app-level validation that must exist
anyway. STRICT typing stays. If Paul prefers belt-and-suspenders, extending
the CHECK with the three new kinds is the same rebuild — flag it, don't
decide silently; the default is drop.

**Replication/HA note (Phase 4 neutrality):** everything here is ordinary
replicated-durable rows in existing tables; thumbnails land in the artwork
cache (regenerable class). No new session state, no new replication class.
Scan + local enrichment ride the already-planned leader-scheduled
singleton. Nothing in this feature complicates the hiqlite swap.

### 3.3 Acceptance for the migration

A test that builds a v5 database (open a store at current HEAD's schema in
a tempfile — or apply `MIGRATIONS[..5]` directly), inserts a movies library
+ show/season/episode tree + watch state + a settings row, then reopens
through the new binary and asserts: all rows survive with identical values,
`PRAGMA foreign_key_check` returns zero rows, FTS finds a pre-migration
title, an item with `kind='video'` inserts cleanly, and `user_version` = 6.
This is the single most important test in the feature.

## 4. Scanner — the `home` arm (`plurx-core/src/scan/`)

### 4.1 Candidates

`is_video` and `VIDEO_EXTS` stay as-is. Add `PHOTO_EXTS`: `jpg jpeg png gif
webp heic heif` and an `is_photo` check used **only when
`library.kind == Home`** — photo files in movie/show libraries stay
invisible, exactly as today. The walk in `scan_library_with_progress`
collects photo candidates for home libraries alongside videos; the "no
video files found" problem message becomes kind-aware ("no video or photo
files…" for home).

### 4.2 Folder mirroring (`place_item` arm)

For a file at `<root>/2019/Beach Trip/clip.mp4`, ensure a `Folder` item
"2019" (parent NULL) → `Folder` "Beach Trip" (parent 2019) → the
`Video`/`Photo` item (parent Beach Trip). Loose files directly under a root
get parent NULL. Folder identity is `(library_id, parent_id, kind='folder',
title = directory name)` — add a `find_child_folder(library_id,
Option<parent_id>, name)` store method (§7). Multiple roots merge at the
top level by folder name (same rule shows use today for titles).

Folder items have no files, no year, no NFO. Their `recorded_at` stays
NULL in v1 (a "span of dates" display is a later nicety). `sort_title` is
the folder name lowercased — do **not** strip articles from folder names
("The Lake House 2021" is a place, not a movie title): pass the raw name,
or bypass `sort_title_for` for folders.

`prune_empty_items` must remove empty folder *chains* after reconcile
(delete a whole subtree of files → its nested empty folders all go).
Verify the current SQL prunes recursively (it already handles
season→show); if it's single-pass, loop until zero rows pruned.

### 4.3 NFO seeding — the seed-once contract

At most once per item, ever. Exact rules:

- When the scanner records a `Video` file (new or changed) and the item's
  `nfo_seeded_at` IS NULL, look for a sidecar: same directory, same file
  stem, extension `.nfo` (case-insensitive on the extension only).
- If present and parseable (§5): apply the parsed fields as a
  `MetadataPatch` and set `nfo_seeded_at = now`.
- If present but junk (§5's leniency rules): record a scan problem naming
  the file, set `nfo_seeded_at` anyway — a broken sidecar should complain
  once, not on every scan.
- If absent: leave `nfo_seeded_at` NULL. **This is what makes
  add-the-NFO-later work:** write the sidecar next week, rescan (the file
  is unchanged but the item is unseeded — the seeding check runs on the
  item, not inside the size+mtime skip), and it seeds then. Note this
  means the incremental-skip path gains a cheap "unseeded video item +
  sidecar exists?" stat check — one `fs::metadata` per unseeded item, no
  probe.
- Once `nfo_seeded_at` is set, the sidecar is dead to plurx: edits, file
  changes, even a rewritten NFO change nothing. User edits can therefore
  never be clobbered — seeding and editing can't both happen after it.

Photos never seed from NFO (Kodi has no photo NFO convention); their
metadata comes from EXIF + filename (§4.4).

### 4.4 `recorded_at` — the precedence ladder

The whole browse experience sorts on this field, so fill it from the best
available source, first hit wins:

| Priority | Videos | Photos |
|---|---|---|
| 1 | NFO `<premiered>` (else `<aired>`) | EXIF `DateTimeOriginal` |
| 2 | Container `creation_time` (ffprobe `format.tags.creation_time` — phones and camcorders set it) | — |
| 3 | Filename date prefix (§4.5) | Filename date prefix |
| 4 | File mtime (date only — mtime lies after copies, but it beats nothing) | File mtime (date only) |

Implementation: add `creation_time: Option<String>` to `ProbeResult`,
parsed in `probe::parse_probe_json` from the raw JSON (normalize to ISO
8601, strip the `Z`/offset to local-naive — sorting is all we need). EXIF
via the `kamadak-exif` crate (pure Rust, small, permissive license) for
JPEG/TIFF; HEIC EXIF extraction is a rabbit hole — v1 lets HEIC fall
through to filename/mtime and says so in a code comment.

### 4.5 Title rules — do NOT reuse the movie parser

`parse::parse_movie` strips year and codec-ish tokens ("Movie H264
(2024).mp4" → "Movie") — correct for scene-named movies, destructive for
home video, where "Christmas 2019.mp4" must stay "Christmas 2019". Known
scar; don't step on it. Home-video titling, new function in `parse.rs`:

- Title = file stem, untouched, except:
- If the stem starts with an ISO-ish date (`YYYY-MM-DD` / `YYYYMMDD`,
  optional ` - `/`_`/space separator), lift it into the recorded-date
  ladder (priority 3) and strip it from the title: `"2019-06-14 -
  Beach.mp4"` → title "Beach", recorded 2019-06-14. If stripping leaves an
  empty title, keep the stem whole and just record the date
  (`"2019-06-14.mp4"` → title "2019-06-14").
- Camera junk names (`IMG_4021`, `MVI_0033`, `DSC01234`) stay as-is —
  they're honest, and the fix is the edit UI, not a guess.

Unit-test the lot in `parse.rs` alongside the existing parser tests.

## 5. NFO parsing contract (`plurx-core/src/scan/nfo.rs`, new module)

Parse the Kodi `<movie>` dialect, leniently. Field map (everything else is
ignored without complaint — Kodi NFOs carry dozens of tags we don't want):

| NFO element | Item field | Notes |
|---|---|---|
| `<title>` | `title` | Trimmed; empty → ignore (keep filename title) |
| `<plot>` | `overview` | |
| `<premiered>` | `recorded_at` | Wins over `<aired>`; accept `YYYY-MM-DD` or full ISO datetime |
| `<aired>` | `recorded_at` | Fallback when no `<premiered>` |
| `<year>` | `year` | Also derived from `recorded_at` when absent |
| `<tag>` (repeating) | `tags` | |
| `<genre>` (repeating) | `tags` | Folded into tags — home video has tags, not genres |

Explicitly ignored, with a comment saying why: `<fileinfo>`/`<streamdetails>`
(ffprobe is ground truth — ARCHITECTURE §7 decision 5 applies verbatim),
`<actor>`/`<director>` (no people model in v1 — §13), ratings/IDs/artwork
tags (no providers here).

Leniency rules — real-world NFOs are filthy:

- Not XML at all (the classic: a bare IMDB URL, or empty file) → `None`,
  one scan problem, still seeds-as-consumed per §4.3.
- Valid XML, wrong root → same treatment.
- Valid `<movie>` with unknown children → parse what's known, ignore rest.
- Encoding junk / BOM → tolerate (read as UTF-8 lossy).

Parser: check `Cargo.toml`/`Cargo.lock` first — `plurx-compat-plex/src/xml.rs`
is a hand-rolled *writer*, so there is likely no XML *parser* in-tree yet.
Add `quick-xml` (the boring standard, MIT) to `plurx-core` rather than
hand-rolling; pin whatever major is current.

Tests (table-driven, in-module): full NFO · minimal (`<title>` only) ·
`<aired>`-only date · repeated tags+genres · junk URL file · malformed XML ·
empty file · unknown-tag soup. Eight cases minimum.

## 6. Local enrichment — thumbnails (`plurx-core/src/metadata/local.rs`, new)

Home libraries never call a provider; their "enrichment" is local artwork.
Runs in the same second-phase slot where `enrich_library` /
`enrich_anime_library` run today — find the per-kind dispatch in `plurxd`
(the jobs path behind `state.jobs.trigger_scan/trigger_refresh`) and add
the `Home` arm. Same leader-singleton story later; it's just ffmpeg instead
of HTTP.

For each item lacking `poster_path` (new store query, §7):

1. **Local art wins** (REQ-META-4 spirit): for a video,
   `<stem>-thumb.jpg/png` or `<stem>-poster.jpg/png` beside the file; for a
   folder item, `poster.jpg/png` or `folder.jpg/png` inside the directory.
   Found art is *copied into the artwork cache* (`{item_id}-poster.jpg`,
   same convention as `cache_image`) — never referenced in place, so the
   cache stays the only thing the image server touches.
2. **Videos — frame grab:** `ffmpeg -ss <t> -i <file> -frames:v 1 -vf
   "scale=w=500:h=-2" -q:v 4 <cache>/{item_id}-poster.jpg` where `t` =
   20% of probed duration, clamped to [1 s, 300 s]; duration unknown → 5 s.
   `-ss` before `-i` (fast input seek — a 2 GB file must not decode from
   zero). Failure is non-fatal: log, count, move on — same posture as
   `cache_image`.
3. **Photos:** same command minus `-ss` (scale-only) for the thumb. The
   full-size view serves the original file (§8.3), so this is grid-only.
4. **Folder posters:** after children are thumbed, a folder without local
   art copies its first child's cached poster (first by `recorded_at`).
   Cheap, deterministic, good enough.

HDR phone footage will frame-grab washed-out (no tone-map in the grab
pipeline). Accepted for v1 — it's a thumbnail; note it in the code. If it
grates, the existing tone-map filter args from the transcode path can be
reused later.

## 7. Store trait additions (`plurx-core/src/store/mod.rs` + `sqlite/`)

New/changed methods — all plain SQL against existing tables, all trivially
hiqlite-compatible. Exact signatures to be adjusted to local style:

```rust
// find-or-place support for the scanner's home arm
async fn find_child_item(&self, library_id: i64, parent_id: Option<i64>,
    kind: ItemKind, title: &str) -> Result<Option<Item>, StoreError>;

// local enrichment work list (folders, videos, photos with no poster)
async fn items_needing_artwork(&self, library_id: i64, force: bool)
    -> Result<Vec<Item>, StoreError>;

// the §8.2 edit endpoint — distinct from apply_metadata because edits must
// be able to CLEAR fields (MetadataPatch's None means "leave as is")
async fn update_item_fields(&self, item_id: i64, edit: &ItemEdit)
    -> Result<Option<Item>, StoreError>;

// mark NFO consumption (called by the scanner after seeding/junk per §4.3)
async fn set_nfo_seeded(&self, item_id: i64) -> Result<(), StoreError>;
```

`ItemEdit` (domain.rs): `title: Option<String>` (non-empty when Some),
`overview: Option<Option<String>>`, `recorded_at: Option<Option<String>>`,
`year: Option<Option<i32>>`, `tags: Option<Vec<String>>` — outer Option =
"field present in the PATCH", inner = the new value, None-inner clears.

Also: `MetadataPatch` gains `recorded_at`/`tags` (used by NFO seeding
through the existing `apply_metadata`), `items_needing_metadata` must not
return home-library items (guard by library kind — TMDB enrichment loops
must never see them), and `ItemSort::Recorded` lands in `list_top_items`.

## 8. HTTP API (`plurxd/src/http/`)

### 8.1 Browse & DTOs

- `dto.rs`: `ItemDto` + `recorded_at` + `tags` (+ serialize `duration_ms`
  for video cards if not already exposed via files).
- `browse.rs item_detail`: the files match arm becomes
  `ItemKind::Movie | ItemKind::Episode | ItemKind::Video | ItemKind::Photo`.
  Bump the ancestor-walk guard 8 → 16 (deep folder trees are legitimate
  here; the guard is only an anti-cycle backstop).
- Child ordering for folder detail: subfolders first, then videos+photos by
  `recorded_at` (nulls last), then title. Implement in the children query
  or sort in the handler — either, but deterministic.
- `list_items`: accept `sort=recorded`. Extend the resolution-badge lookup
  (`item_max_heights`) filter from `Movie` to `Movie | Video`.
- `hubs`: **exclude `kind='photo'` from `recently_added`** — a 2,000-photo
  import must not flood the home screen (the videos and folders of that
  import still surface it). Folder-level collapse is a later nicety (§13).
  Verify `continue_watching` picks up a partially-watched `Video` (the
  query looks kind-generic; the acceptance test in §11/M6 pins it).

### 8.2 Metadata editing

```
PATCH /api/v1/items/:id            (admin)
{ "title": "Beach day", "overview": "…", "recorded_at": "2019-06-14",
  "year": 2019, "tags": ["beach","kids"] }
```

Semantics: omitted field = unchanged; `null` = clear (title excepted —
empty/null title is a 400); `tags: []` = clear tags. `recorded_at`
validated as `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM:SS` (400 otherwise). The
handler loads the item, checks its library kind is `home`, else 400 with a
message saying why (§2, last guardrail) — same admin-gating pattern as
`libraries.rs`. Returns the updated `ItemDto`. Watch-state, files, FTS
(via the recreated triggers) all follow automatically.

### 8.3 Photo serving

```
GET /api/v1/items/:id/photo               → original bytes
GET /api/v1/items/:id/photo?size=thumb    → artwork-cache thumbnail
```

Original path: correct `Content-Type` from the extension, range-serving
reused from the direct-play file server (`http/stream.rs` has the range
helper — re-verify its name), auth like every media route. Browsers honor
EXIF orientation on `<img>` natively (`image-orientation: from-image` is
the default), so serve bytes untouched — no rotation pipeline. HEIC:
serve as-is with `image/heic` (Safari renders it); non-Safari fallback is
the JPEG thumbnail. Good enough for v1; note it (§13).

No decision engine, no transcode, no session — photos are static bytes and
must not touch the playback pipeline.

### 8.4 Libraries

`LibraryInput` already round-trips arbitrary kinds through
`LibraryKind::parse`, so `"home"` works once the enum lands. Confirm the
anime flag stays forced-off for `home` and the create/scan flow needs no
other change.

## 9. Web app (`plurxd/src/web/index.html`)

Reminder: the SPA is `include_str!`-embedded — UI edits need `cargo build`
+ restart to show up. One hand-written file, no framework: follow the
existing patterns (hash routes like `#/activity`, theme variables, the
existing modal player) rather than inventing structure.

- **Library admin:** kind selector gains "Home videos & photos".
- **Grid:** folder cards (name + poster, subtle "N items" count), video
  cards (thumb + duration badge + recorded date), photo cards (square-ish
  thumb). Sort control gains "Date recorded" (default for home libraries;
  title default stays for everything else).
- **Folder view:** existing detail route works (ancestors → breadcrumb,
  children → grid). Subfolders first, then media by date (§8.1).
- **Video detail/playback:** unchanged player modal; a `Video` item plays
  exactly like a movie (files, decision, resume, ◆ Quality menu — all
  inherited).
- **Photo lightbox:** clicking a photo opens a full-screen overlay of the
  original (`/photo` URL), ←/→ walk the folder's photos (skip videos),
  Esc closes, swipe on mobile (the player's touch handling is precedent).
  Preload next/prev. No zoom/pan in v1.
- **Edit modal:** pencil icon on detail pages of home-library items (admin
  only — hide otherwise). Fields: title, date recorded, description, tags
  (chip input). PATCH on save, update in place. Also reachable from folder
  items (folders are retitlable — DB title only; the directory on disk is
  never renamed).

## 10. Playback notes & test-corpus additions

`Video` items ride the existing ladder untouched — direct play → remux →
transcode, capability probing, HDR handling all apply. Phone footage is
exactly where the sharp edges live, so add to the standing corpus (the
roadmap rule says a red corpus blocks release):

- **Portrait/rotated clip** (mp4 with rotation side-data): direct play must
  display upright in-browser; transcode must auto-rotate (ffmpeg default —
  do not pass `-noautorotate`).
- **iPhone HEVC + Dolby Vision profile 8.4 clip:** the HDR-on-SDR →
  tone-map policy decided 2026-07-23 must hold for it.
- **AVCHD `.m2ts` camcorder clip** (interlaced): remux/transcode path
  sanity; worth a deinterlace note if it looks combed, not a v1 fix.

Sandbox scar (recorded in project memory): no GPU + headless Chromium lacks
H.264 decode — verify these via ffprobe/decision-API assertions and the
network trail, not pixels; clicking Play headless triggers the phantom
transcode fallback. Same drill as the 07-23 work.

## 11. Milestones

Sequential; each is a coherent commit (or few) ending `make check` green,
with the milestone's tests in the same commit as the behavior. Deliver per
the established bundle workflow — commit in the cloud clone, ship
`plurx-full.bundle`, never loose files into the working tree.

**M1 — Migration v6 + domain + store plumbing.** §3 in full: runner
FK-off/check change, both table rebuilds, FTS recreate with tags, enum +
struct + mapper updates, `ItemSort::Recorded`, new store methods (§7)
with unit tests.
*Accept:* the §3.3 upgrade-fixture test passes; full suite green; a `home`
library and `folder/video/photo` items round-trip through the store.

**M2 — NFO parser.** §5's module + the eight leniency tests. Pure, no I/O
beyond reading the given path.
*Accept:* `cargo test -p plurx-core nfo` green, junk inputs produce `None`
+ problem strings, never a panic or Err that would fail a scan.

**M3 — Scanner home arm.** §4: candidates, folder mirroring, seed-once,
date ladder, title rules, folder-chain pruning, kind-aware problems.
*Accept:* an integration test builds a temp tree (nested folders, loose
root video, a photo, one video with NFO, one with a junk NFO, one with a
date-prefixed filename), scans, and asserts the exact hierarchy, titles,
`recorded_at` values, and seed flags; then (a) edits a title via the store,
rewrites the NFO, rescans → title survives; (b) adds an NFO beside a
previously-scanned unseeded video, rescans → it seeds exactly once.

**M4 — Photos.** EXIF dates (`kamadak-exif`), photo probe path, §8.3
serving (original + thumb), photos excluded from `recently_added`.
*Accept:* test with a real tiny JPEG (EXIF `DateTimeOriginal` set) asserts
`recorded_at` from EXIF, `/photo` returns the bytes with the right
`Content-Type` + range support, and the hubs response omits photo items.

**M5 — Local enrichment.** §6: local-art-wins, frame grab, photo thumbs,
folder posters, wired into the jobs dispatch for `home`.
*Accept:* test generates a 2 s clip with ffmpeg, scans, enriches: cache
contains `{id}-poster.jpg`, `poster_path` set; a video with a sidecar
`-thumb.jpg` uses it (byte-compare) instead of grabbing a frame; folder
item inherits first child's poster; second run is a no-op.

**M6 — Edit API + browse polish.** §8.1 + §8.2: PATCH with full validation
matrix, `sort=recorded`, folder child ordering, ancestor guard bump,
res-badge extension, continue-watching pin.
*Accept:* endpoint tests cover edit/clear/reject (non-home item → 400,
empty title → 400, bad date → 400); a partially-watched home video shows
in continue-watching.

**M7 — Web UI.** §9 in full, screenshots regenerated if the docs show them
(the 07-23 docshots harness exists for this).
*Accept:* manual pass on the four flows — create home library → browse
folders → play a video (resume works) → lightbox photos → edit metadata
and see it stick after reload. `make check` still green (UI is embedded —
rebuild to verify include_str compiles).

**M8 — Docs + corpus + changelog.** §12, plus the §10 corpus clips wired
into CI.
*Accept:* every doc listed in §12 updated in the same PR; corpus green.

## 12. Docs to update (same commits as the behavior — standing rule)

- **REQUIREMENTS.md:** §2 media-types table — Photos/home video row flips
  to "Yes — `home` libraries (added 2026-07)". Add a short REQ-HOME block
  (numbered like the rest): folder-tree organization; NFO one-time seed,
  Kodi dialect, never written; local-only artwork; in-UI editing scoped to
  home libraries; photos scanned/dated/thumbed/viewed only. This doc is
  the scope police — the guardrails in §2 above should be recognizable in
  it.
- **ARCHITECTURE.md:** §4 scanner diagram gains the home branch (folder
  mirror · NFO seed · local thumbs — no provider); §7 key decisions gains
  "**The NFO is a seed, not a store**" with the §1 reasoning; §8
  non-goals: "plurx never writes to media storage" stands **unchanged** —
  say explicitly that home video was designed around it, and amend "Music
  and photos are out of scope" to "Music is out of scope (photos: in
  `home` libraries since 2026-07)".
- **FEATURES.md:** a Home-video section in the house pattern (what it
  does / what it considers / how to read it), including the seed-once
  semantics table from §4.3 — this is where a future operator will look
  first when an NFO edit "doesn't do anything" (by design).
- **ROADMAP.md:** insert as its own shippable slice ("Home video &
  photos", after Phase 3, before or parallel to Phase 4 — Paul's call to
  slot; the feature is HA-neutral per §3.2) with the M1–M8 exit line:
  browse folders, play clips, view photos, edit metadata, thumbs
  everywhere.
- **CHEATSHEET.md** (if library-setup commands/flows are listed there) and
  **CHANGELOG.md:** entry per house format.

## 13. Defaults chosen where the founding answers didn't reach

Each is decided (build it as stated) but cheap to reverse — flag before
deviating:

1. **Tags join FTS** — the triggers are rebuilt in v6 anyway; searching
   "beach" should find tagged clips. Cost: none beyond the rebuild.
2. **`movie.nfo` / `folder.nfo` are NOT read** — only `<basename>.nfo`.
   A folder here is an *event containing many videos*, so a folder-level
   movie.nfo is ambiguous by construction. Kodi's own home-video guidance
   uses per-file sidecars.
3. **HEIC:** scanned + thumbed via ffmpeg when the build decodes HEIF
   (jellyfin-ffmpeg does); otherwise skip the file with a named scan
   problem. Full-size serving is native-Safari-only (§8.3). No conversion
   pipeline in v1.
4. **People/`<actor>` tags: dropped**, not stored-and-hidden. A people
   model deserves design (it touches search, browse, maybe faces later),
   not a JSON column smuggled in now.
5. **Recently-added folder collapse** (one card per import batch): later.
   The photo exclusion (§8.1) removes the flood; collapse is polish.
6. **No re-seed/re-import button in v1.** The one-time contract stays
   absolute until real usage demands an explicit, user-initiated re-read.
   If it comes, it's a per-item admin action — never automatic.
7. **Edit endpoint is admin-only**, matching every other mutation
   (`libraries.rs` pattern). Per-user metadata is a non-feature; there is
   one family library, one truth.

---

*Companion docs: [ARCHITECTURE.md](ARCHITECTURE.md) (how plurx is built) ·
[REQUIREMENTS.md](REQUIREMENTS.md) (scope police) ·
[ROADMAP.md](ROADMAP.md) (where this slots). Suggested resting place for
this file once executed: `docs/HOMEVIDEO-PLAN.md`, alongside
[PHASE3-SPIKE.md](PHASE3-SPIKE.md) as the record of why home video works
the way it does.*


