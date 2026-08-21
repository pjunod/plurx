# Ebook reader — Cinema reads what Curator acquires

**Status:** M0–M1 complete · M2a publication proof complete · M2b–M6 ready
to build · **Written:**
2026-08-20 · **Verified against:** `plurx` `43b7cb68` and `monarr` `a3f5fb8`

Companion to [FEATURES.md](FEATURES.md) (what Books libraries do today),
[INTEGRATION.md](INTEGRATION.md) (the Curator → Cinema handoff),
[CLIENTS.md](CLIENTS.md) (native-client boundaries),
[OFFLINE-VIEWING-PLAN.md](OFFLINE-VIEWING-PLAN.md) (Cinema-owned mobile
storage), and [SECURITY.md](SECURITY.md) (who may read media). Read §2–§4
before changing code, then work milestone by milestone (§7). Every milestone
ends with an observable acceptance check. If a step appears to require
writing beside a book, treating a page as milliseconds, allowing publication
scripts to run, or making Runner understand reading state, stop and flag it:
that contradicts the contracts below.

This plan finishes the Books feature that already exists. Curator discovers,
grabs, and imports the edition. Runner transfers and unpacks the payload.
Cinema indexes the result, renders it, owns per-user reading state, and keeps
an app-managed offline copy when requested. A separate Reader application
would duplicate pairing, authentication, browsing, downloads, and progress
without creating a new owner for any of them.

## 1. Objective — one shelf for watching, listening, and reading

An imported EPUB appears in Cinema without a manual scan. On web, iPhone,
iPad, and Android phone/tablet, **Read** opens it inside Cinema at the newest
saved locator. The reader provides a table of contents, page/chapter
navigation, typography and theme controls, search, and an explicit **Open
in…** escape hatch. Reading state follows the signed-in user across devices.
An app-managed download remains readable after Cinema's server is stopped and
the device enters airplane mode.

Success is a user-visible chain, not a collection of internals:

```text
Curator imports EPUB
        │ targeted scan
        ▼
Cinema Books shelf ──▶ Read ──▶ chapter 4, 42%
        ▲                          │
        └──── another device ◀────┘ newest locator wins
                                   │
                                   └── Download ─▶ airplane-mode reading
```

Six decisions define the work:

1. **Cinema owns consumption.** Curator owns acquisition and library intent;
   Runner owns transfer; Cinema owns watching, listening, reading, and the
   associated user state. The existing three-application boundary survives.
2. **A reader is not the A/V player.** It reuses authentication, catalogue,
   profiles, and offline-storage patterns, but not playback sessions,
   transcoding, now-playing, Trakt scrobbles, millisecond progress, or the 95%
   watched threshold.
3. **EPUB is the first rendered format.** It is reflowable, carries a spine
   and navigation model, and is Curator's default ebook quality. PDF keeps its
   platform/browser viewer in v1. MOBI, AZW/AZW3, FB2, CBZ, and CBR retain
   **Open in…** until a later format milestone earns each renderer.
4. **Online and offline use the same publication bytes and locator.** An
   offline reader must not fork pagination or invent a second progress model.
5. **Publication content is hostile input.** Book scripts and remote network
   loads never run. Rendering lives behind an iframe/WebView boundary with a
   restrictive content policy; malformed archives fail closed.
6. **The current exit stays.** **Open in…** and **Download original** remain
   available. A built-in reader that mishandles an unusual book must not trap
   the user inside Cinema.

## 2. Starting point — the library exists; the reading loop does not

| Surface | What exists on 2026-08-20 | Missing contract |
|---|---|---|
| Curator | Ebook and audiobook editions, metadata, profiles, search, import | M0 replaced its stale book decline with exact-path `hint:"book"` targeted scans. |
| Runner | Downloads and unpacks Curator's payload | Nothing. Runner must remain unaware of reading. |
| Cinema scanner | `books` library kind; `book` and `audiobook` items; author-preserving path identity | M0 proved Curator's targeted scan reaches the Books library. Author is identity context, not a first-class display field. |
| Cinema server | Authenticated, range-capable original bytes plus revision-bound, per-user reading-state storage and API | No reader surface. |
| Cinema web | Books shelf, detail, **Open book**, **Download** | Opens another browser/platform handler; no in-app EPUB renderer. |
| Cinema native | Book detail on Apple and Android; external URL intent | No phone/tablet reader or ebook offline record. |
| User state | Timed `watch_state` for movies, episodes, videos, and audiobooks | Text books are correctly excluded; a locator-shaped sibling is needed. |

The existing `/content` separation is load-bearing. Opening an EPUB is not a
playback start, so it must not announce a viewer, allocate a playback session,
or manufacture audio progress. The reader continues to fetch bytes through
that route rather than teaching `/direct`, `/decision`, or HLS about books.

## 3. Product contract — reading behaves like reading

### 3.1 The primary action is `Read`, and it resumes honestly

An available EPUB shows **Read** as the primary action. A saved locator changes
the label to **Resume reading · 42%**. The same action appears on web and
phone/tablet detail screens. Television surfaces keep **Open book** hidden:
there is no ten-foot reader in this plan.

Inside the reader:

- Back returns to the same item detail and leaves the newest locator queued
  for sync.
- Previous/next moves through the publication spine; the table of contents
  jumps to an authored destination.
- Font family, font size, line height, margins, light/dark/sepia, and
  paginated/scroll preference are device-local presentation settings. They
  do not alter the shared locator.
- Search is publication-local. Search text and results never leave the
  device/server installation.
- **Open in…** and **Download original** remain secondary actions.

"Page 87" is not durable state. Page counts change with screen size, type,
font, and margins; Cinema stores a publication locator plus total progression
and derives whatever page label the current renderer can honestly show.

### 3.2 Completion is explicit, not a borrowed 95% rule

The reader may suggest **Mark finished** near the end, but it does not
auto-finish at the video player's 95% threshold. End matter, indexes, notes,
and choose-your-own-path publications make that inference unreliable.
`completed` changes only through **Mark finished**, **Mark unfinished**, or an
explicit renderer end action the user confirms.

Finished books may appear in a future Books filter, but they do not enter
Continue Watching, Trakt, now-playing, or the Curator watched webhook. A later
recommendation feature can consume reading history only after its privacy and
product contract is decided separately.

### 3.3 Unsupported and protected books fail without pretending

| Input | Version-1 action | Reason |
|---|---|---|
| Valid EPUB without DRM | Read in Cinema · Open in… · Download original | The built-in contract. |
| PDF | Open in platform/browser viewer · Download original | Fixed-layout PDF is already handled well by platform surfaces; consistent in-app PDF pagination is not required for EPUB launch. |
| MOBI · AZW/AZW3 · FB2 · CBZ · CBR | Open in… · Download original | Detection is not a renderer promise. |
| DRM/encrypted publication | Explain **This book is protected and Cinema cannot open it** · Open in… | Cinema does not acquire keys, remove DRM, or imply that unreadable bytes are corrupt. |
| Malformed or over-limit EPUB | Explain the parse/security failure · Open in… · Download original | The original remains available even when the built-in boundary refuses it. |

## 4. Server contract — reading state is a locator, not a clock

The implementation lives in `crates/plurx-core/src/store/mod.rs`,
`crates/plurx-core/src/store/sqlite/reading.rs`,
`crates/plurx-core/src/store/hiqlite_reading.rs`, and
`crates/plurxd/src/http/reading.rs`. M1 advanced SQLite from schema v19 to
v20 and the authoritative Hiqlite schema from v5 to v6 without changing the
replicated protocol version.

### 4.1 Durable model

```sql
CREATE TABLE reading_state (
    user_id            INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    item_id            INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    file_id            INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    file_size          INTEGER NOT NULL,
    file_mtime         INTEGER NOT NULL,
    locator_json       TEXT NOT NULL,
    progression_millis INTEGER NOT NULL
                       CHECK (progression_millis BETWEEN 0 AND 1000000),
    completed          INTEGER NOT NULL DEFAULT 0,
    updated_at         INTEGER NOT NULL,
    PRIMARY KEY (user_id, item_id, file_id)
) STRICT;
CREATE INDEX reading_state_recent
    ON reading_state(user_id, updated_at DESC);
```

The primary key includes `file_id` because two editions of one work can have
different spines. `file_size` + `file_mtime` bind the locator to the exact
revision that produced it. A replacement at the same path keeps its file row
but invalidates the saved locator rather than jumping into an unrelated
chapter. The stale row may remain for diagnosis, but the API returns no
resume locator until the current revision records one.

`locator_json` uses one versioned, renderer-neutral envelope:

```json
{
  "version": 1,
  "href": "OEBPS/chapter-04.xhtml",
  "type": "application/xhtml+xml",
  "locations": {
    "progression": 0.42,
    "totalProgression": 0.19,
    "position": 56
  }
}
```

`href` names a manifest resource after archive-path normalization.
`locations.progression` is the fraction within that resource;
`totalProgression` is the fraction through the full publication; `position`
is optional renderer output, never the sole resume authority. New fields are
additive. A future incompatible representation increments `version` and keeps
the v1 decoder until all shipped clients have crossed the migration window.

Add a `ReadingStore` sibling to `WatchStore`, with SQLite and Hiqlite
implementations and backend-neutral contract tests. Reading state is
replicated catalogue/user truth in Hiqlite, not node-local telemetry. Add it
to authoritative dumps and import parity so cluster activation cannot report
success while silently dropping the shelf position.

### 4.2 Native API

```text
GET /api/v1/items/{item_id}/reading-state?file_id={file_id}
    → 200 {state:null, stale:false}
    → 200 {state:{file_id,revision,locator,progression,completed,updated_at},
           stale:false}
    → 200 {state:null, stale:true}       # file revision changed

PUT /api/v1/items/{item_id}/reading-state
{
  "file_id": 88,
  "revision": {"size": 481921, "mtime": 1787225300},
  "locator": { ...versioned locator... },
  "progression": 0.19,
  "completed": false,
  "recorded_at": 1787225400
}
    → 200 {the durable winning state}

DELETE /api/v1/items/{item_id}/reading-state?file_id={file_id}
    → 204                              # Start over / clear progress
```

All three routes require a user token. The handler verifies that the item is
`book`, the file belongs to it, the revision matches, the locator version is
supported, progression is finite and in `[0,1]`, and the locator JSON stays
within a bounded request body. A dated offline write older than `updated_at`
returns the existing winning row instead of rewinding another device, exactly
the conflict policy offline watch progress already uses.

The detail DTO may attach the newest non-stale reading-state summary so cards
and detail screens do not issue an N+1 merely to label **Resume reading**.
Do not reuse the `watch` field: old clients understand that field as timed
playback and calculate a duration-based bar from it.

### 4.3 Original bytes remain the publication boundary

The reader fetches `GET /api/v1/files/{id}/content`. M1 does not add an
unauthenticated resource route, unpack EPUBs into the media library, or cache
derived publication files beside the source. If the renderer needs extracted
resources, they live in memory, the browser's private blob space, or Cinema's
app/cache directory and are scoped to the authenticated file revision.

## 5. Renderer boundary — one publication model, hostile content inside

### 5.1 M2 starts with a proof, not a dependency guess

The preferred shape is a locally bundled web publication core shared by the
server web UI and native WebViews. It promises one pagination/locator model and
keeps native work to lifecycle, downloads, accessibility, and bridge plumbing.
Do not commit to it until an M2 spike proves all of these on fixtures checked
into `target/fixtures` or generated by tests:

| Proof | Pass condition |
|---|---|
| Navigation | EPUB 2 NCX and EPUB 3 nav both produce the authored table of contents and spine order. |
| Locator stability | Reopening after font, margin, viewport, and paginated/scroll changes lands in the same paragraph, not merely the same nominal page. |
| Security | Inline scripts do not run; remote images/fonts do not make a network request; `../` archive paths cannot escape publication storage. |
| Scale | A 500 MiB illustrated EPUB opens without holding two full decompressed copies in memory. |
| Accessibility | VoiceOver and TalkBack traverse headings and text in reading order; Dynamic Type/text scaling remains usable. |
| Offline identity | The same file revision and locator reopen from an app-local file with the server absent. |

If the shared core fails accessibility, memory, or locator stability, stop the
spike and use native publication toolkits for Apple/Android while keeping the
wire locator envelope from §4.1. Do not paper over a failed proof with a
platform-specific locator hidden inside `locator_json`; that only postpones
the incompatibility until a user changes devices.

#### M2a verdict — stream the publication; do not expand it

M2a adopts Readium Web's server boundary without importing an unfinished web
navigator: Cinema turns the packaged EPUB into a manifest/resource API, then
M2b's browser navigator consumes that API. This keeps the original file as the
source of truth, lets web and native WebViews share normalized hrefs, and keeps
the account bearer out of publication markup. The response shape follows the
[Readium Web Publication Manifest](https://github.com/readium/webpub-manifest)
names — `metadata`, `readingOrder`, `resources`, and `toc` — while remaining a
Cinema API rather than claiming full Readium conformance.

The only new parser dependency is
[`zip` 8.6.0](https://github.com/zip-rs/zip2/releases/tag/v8.6.0), built with
default features off and Deflate only. Cinema parses the OCF container,
package, EPUB 3 navigation document, and EPUB 2 NCX itself with the existing
`quick-xml` dependency. No renderer bundle or build step has been selected
yet; M2b has to earn that choice with the browser proofs below.

| Proof | M2a evidence | Remaining gate |
|---|---|---|
| Navigation | Generated EPUB 2 NCX and EPUB 3 nav fixtures preserve spine order, authored labels, fragments, and nested TOC entries. | Browser TOC interaction in M2b. |
| Locator stability | Manifest hrefs are decoded, normalized, and revision-bound. | M2b must restore the same paragraph after font, margin, viewport, and mode changes. |
| Security | Absolute, drive-prefixed, control, backslash, duplicate, encoded traversal, and container-escaping paths fail closed. Encrypted publications get a distinct protected-book refusal. Resource responses block script, connect, form, object, frame, and remote image/font/media loads. | Browser test proves scripts stay inert and remote requests never leave the page. |
| Scale | The ignored/nightly 620 MiB EPUB (one 500 MiB refused child plus one 120 MiB streamed child) peaks at 19,382,272 bytes RSS on the M2a macOS proof run; child resources stream in 64 KiB chunks, stay below 128 MiB each, and share eight node-wide reader slots. | M2b observes the same ceiling while navigating/searching. |
| Accessibility | Publication order and headings remain in authored XHTML rather than being flattened server-side. | VoiceOver/TalkBack and keyboard browser acceptance in M2b/M3. |
| Offline identity | Manifest hrefs and M1 locators use the same file revision and normalized archive path. | M4 reopens those bytes and locators with the server absent. |

The measured parser/stream ceiling is 256 MiB peak RSS for the 620 MiB fixture.
The proof used 18.5 MiB while draining the 120 MiB child; the wider ceiling
leaves room for allocator and platform variance without permitting a second
expanded copy of the book.

### 5.2 The security boundary is stricter than the ordinary web app

Publication markup renders in a sandboxed document without scripts, top-level
navigation, forms, downloads, popups, or same-origin access to Cinema. Remote
HTTP(S) resources are blocked. The reader UI owns navigation and supplies only
normalized local publication resources. Archive parsing rejects absolute
paths, drive prefixes, NULs, and any normalized `..` escape; entry count,
uncompressed bytes, nesting, and per-resource reads are bounded before the
reader allocates them.

The M2 spike records the chosen concrete limits against its large-book fixture
instead of inventing numbers in this plan. Those limits become code constants,
tests, and a user-facing failure message in the same milestone.

M2a established the limits:

| Boundary | Limit |
|---|---:|
| Archive entries | 20,000 |
| Declared total uncompressed bytes | 1 GiB |
| One served resource | 128 MiB |
| Decompressed resource chunk | 64 KiB |
| Concurrent resource reads per node | 8 |
| Container/package/navigation XML | 8 MiB each |
| Compression ratio | 1,000:1 |
| Live publication sessions per node | 64 |
| Sliding session lifetime | 2 hours |

`POST /api/v1/files/{file_id}/publication` requires the user token, validates
the current book revision, parses only bounded metadata, and mints a random
session capability. Nested resources use
`GET /api/v1/publication/{session}/{resource}` without the account token.
Those path credentials are redacted from HTTP traces, slide while in use, can
be closed early by their owner, and refuse a resource if the source size or
mtime changes after the manifest was parsed.

## 6. Offline contract — save the original, not a video package

Ebook downloads reuse the account/server isolation and catalogue discipline
from [OFFLINE-VIEWING-PLAN.md](OFFLINE-VIEWING-PLAN.md), not its HLS preparation
API. An EPUB already is the portable output; asking the server to transcode or
package it would add work while weakening its identity.

Each native offline record is scoped by
`(server_instance_id, user_id, item_id, file_id, file_size, file_mtime)` and
stores title · author when available · cover · original filename · local path
· byte count · newest pending locator · sync timestamp. It lives in Cinema's
application sandbox. There is no destination chooser or public Files-provider
registration. **Open in…** is a deliberate export of a copy; **Download** is
Cinema-managed storage.

When online, the client downloads `/content` with its authenticated session to
a temporary app-local path, verifies byte count/revision, then atomically
publishes the final record. A launch reconciliation turns a missing local file
into **Download missing — download again**. Removal deletes only app-local
bytes and its catalogue row; plurx's read-only media remains untouched.

Offline locator writes are durable locally. Reconnection sends only the newest
pending state per `(item,file,revision)` with `recorded_at`; the server's
timestamp comparison prevents an old tablet from rewinding a phone that read
further online.

## 7. Milestones — each leaves a usable boundary

### 7.1 M0 — close the Curator → Cinema book handoff

**Status:** complete 2026-08-20. Curator's full Go suite and Cinema's full
`make check` gate are green.

Remove Curator's explicit `book` decline. `scanBody` emits `hint:"book"` with
no movie/series ids; Cinema resolves the Books library from the path and its
library kind remains authoritative. Extend Cinema's `ScanRequestBody` hint
documentation to list `book`, even though only episode/season hints affect id
placement. Replace the decline test with a contract test proving one book
directory produces one authenticated targeted scan, carries the correlation
id, and records Cinema's returned item id in the delivery trace.

Update Curator's `docs/settings.md`, `docs/integration.md`, and historical
`docs/plan-integration.md` so none retains the now-false guardrail. Update
Cinema's integration docs with the book request example and explicit format
ownership.

**Accept:**

```bash
go test ./internal/adapters/notify    # book import posts hint=book and reports item id
make test                             # Curator repository baseline
cargo test -p plurxd scan             # Cinema targeted-scan contract
make check                            # Cinema repository baseline
```

### 7.2 M1 — replicated reading state and API

**Status:** complete 2026-08-20. SQLite and three-voter Hiqlite run the same
reading-state contract; full repository, cluster, Apple, and Android model
gates are green.

Add §4's `ReadingState` domain type, `ReadingStore`, SQLite v20 migration,
Hiqlite catalogue table/dumps/import parity, three user-token routes, DTO
summary, OpenAPI/native models, and backend-neutral tests. The server accepts
state only for current `book` file revisions and ignores stale dated writes.

**Accept:** the same store contract passes SQLite and Hiqlite; an offline state
at `t1` cannot rewind an online state at `t2`; replacing a file at the same
path returns `stale:true`; movie/audiobook item ids are rejected; cluster
import parity includes every `reading_state` row.

```bash
make check
make cluster-check
```

### 7.3 M2 — EPUB proof and web reader

**Status:** M2a publication API complete 2026-08-20; M2b reader UI remains.

M2a ships §5's bounded OCF/package/nav parser, Readium-shaped manifest,
revision-bound resource capability, security headers, generated EPUB 2/3
fixtures, and 620 MiB memory proof. It deliberately stops before calling the
feature a reader: locator stability, accessibility, search, and actual browser
network refusal require a rendered page.

Run §5.1's dependency/architecture proof and record the verdict in this plan.
Then ship the authenticated web reader, **Read/Resume** detail actions, table
of contents, typography/theme controls, search, state heartbeat/close flush,
explicit finish controls, security limits, and **Open in…** fallback. Keep
reader assets outside the hand-written `index.html` when their lifecycle or
size warrants it; preserving "no build step" is useful, but not if it hides an
unauditable minified blob in the application shell.

**Accept:** browser tests cover open · locator restore · style change · TOC ·
search · finish/unfinish · stale revision · script/network refusal. The large
fixture stays inside the memory ceiling established by the spike.

```bash
make playback-smoke               # existing playback remains unchanged
make validate                     # point-selected reader/browser contracts
```

### 7.4 M3 — Apple and Android online readers

Add phone/tablet reader destinations using M2's proven publication model and
§4's API. App navigation owns dismissal and profile changes; a server switch
or sign-out clears live publication credentials and cannot leave another
profile's book visible. Television builds compile but expose no reader action.

Advance both mobile build counters in the same changes that alter release
paths. Add simulator/emulator tests for URL/auth handoff, locator flush,
profile isolation, and TV refusal before physical-device claims.

**Accept:** iPhone/iPad and Android phone/tablet resume the same paragraph
after a font and orientation change; VoiceOver/TalkBack traverse headings in
order; tvOS/Google TV do not show **Read**.

```bash
make apple-test
./clients/android/gradlew -p clients/android test
python3 -m validation.mobile_versions
```

### 7.5 M4 — Cinema-managed ebook downloads

Implement §6 on Apple and Android: app-private original download, atomic local
publication, profile-scoped catalogue, launch reconciliation, remove/retry,
offline reader startup, and newest-state replay on reconnect. No server-side
preparation queue is added.

**Accept:** download an EPUB, force-quit Cinema, stop plurxd, enable airplane
mode, relaunch, read and change chapters, force-quit again, resume locally,
then reconnect and observe the same locator on web. Removing the download
reclaims local bytes without touching the server file.

### 7.6 M5 — author, cover, and edition metadata

Make author and edition medium first-class Cinema facts. Prefer an explicit
Curator handoff keyed by its book identifiers when paired; retain local EPUB
metadata/cover extraction so a standalone Cinema install remains useful.
Provider data is cached in Cinema's DB/artwork cache, never written beside the
book. Text and audiobook editions may link under one work only when identifiers
or Curator's explicit relation prove it; matching title+author alone is not
strong enough to merge user state.

**Accept:** a Curator-imported work displays author and cover immediately; a
standalone EPUB gains the same fields from its package; two authors' identical
titles remain separate; replacing artwork never mutates the library path.

### 7.7 M6 — formats, acceptance, and release truth

Use measured demand and fixtures to choose the next renderer: PDF in-app,
fixed-layout EPUB, or comics. Each format needs its own locator, security,
accessibility, and offline acceptance before the action changes from **Open
in…** to **Read**. Update FEATURES, ROADMAP, STATUS, client docs, screenshots,
and release notes to claim only the formats and physical devices actually
verified.

**Accept:** the documented support matrix is generated or contract-tested
against the same extension/action registry clients use; no detected format is
accidentally labeled built-in merely because `/content` can serve its bytes.

## 8. Non-goals — doors this initiative keeps shut

- **No DRM removal or key service.** Cinema reads unprotected local files. A
  protected publication is explained and handed off, not cracked.
- **No writes under Books library roots.** Covers, extracted resources,
  indexes, and offline copies live in Cinema-owned storage.
- **No reader in Runner or Curator.** They may expose acquisition and file
  facts; user reading state has one owner.
- **No timed playback semantics for text.** No HLS, transcode, playback
  session, now-playing viewer, Trakt scrobble, or 95% watched threshold.
- **No television reader.** Apple TV and Google TV continue to support
  audiobooks; text reading is phone, tablet, and web in this plan.
- **No annotations, highlights, social shelves, dictionaries, or cloud
  accounts in v1.** They multiply sync and privacy contracts before the core
  read/resume/offline loop has evidence.
- **No title-only edition merging.** A false merge can attach a locator to the
  wrong text; explicit identifiers or relations are required.
- **No promise that every detected ebook format renders in-app.** Detection,
  authenticated serving, and rendering are three different capabilities, and
  the UI names them separately.

## 9. Status ledger — keep the plan honest as it lands

| Milestone | State | Evidence |
|---|---|---|
| M0 · Curator handoff | Complete | Curator `make test`; Cinema `make check`; focused cross-seam tests |
| M1 · Reading state/API | Complete | `make check`; `make cluster-check`; Apple/Android native model contracts |
| M2 · EPUB proof/web | Planned | §5 proof matrix |
| M3 · Native online | Planned | Simulator/emulator + accessibility acceptance |
| M4 · Offline EPUB | Planned | Physical airplane-mode drill |
| M5 · Metadata | Planned | Paired + standalone fixtures |
| M6 · More formats/release | Planned | Contract-tested support matrix |

Update this table, the status header, [FEATURES.md](FEATURES.md), and
[ROADMAP.md](ROADMAP.md) in the same change that advances a milestone. A plan
whose checkboxes outrun its evidence is not progress; it is a future bug with
good typography.
