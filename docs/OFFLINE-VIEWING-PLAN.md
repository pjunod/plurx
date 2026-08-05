# Offline viewing — one-tap, app-managed downloads

**Status:** reviewed · revised · implementation candidate complete · draft PR
review and physical-device acceptance pending ·
**Executes:** offline viewing requested 2026-08-05 · **Written:** 2026-08-05 ·
**Revised:** 2026-08-05 against review base `2ff661da` · **Implementation
target:** `origin/main` at `7874b37b`

Companion to [PLAYBACK.md](PLAYBACK.md) (how online delivery works),
[CLIENTS.md](CLIENTS.md) (the native-client boundary), and
[SECURITY.md](SECURITY.md) (who may read media). Read §2 and §4 before
implementing anything: they define the user contract and the package contract
that every milestone preserves. Build milestone by milestone and stop after
each acceptance check. If a step appears to require exposing files through the
Files app, teaching the user about HLS, recording a download as playback, or
weakening media authentication, stop and flag it instead.

This is a plan for Netflix-style downloads, not a Save File feature. You tap
Download on a movie or episode, watch its progress inside Cinema, and play it
from Cinema with the server shut down and the device in airplane mode. The app
owns the package, artwork, metadata, storage, resume state, and deletion. There
is no file picker, destination chooser, Open In action, exported `.mp4`, or
user-visible media path.

The independent review in `target/offline-review-drop/` approved that product
model and rejected several implementation assumptions. This revision treats
the following decisions as binding:

1. Version 1 packages are the MPEG-TS VOD that the current transcode producer
   actually emits. fMP4 is a later, cache-invalidating format migration.
2. Offline preparation honours `audio_offset_ms`; a non-zero offset prevents
   reuse of the narrower scheduled-pretranscode cache entry.
3. Server preparation continues after the app backgrounds. If preparation
   finishes after iOS suspends the app, transfer starts on the next foreground
   visit; background catch-up is best effort and never presented as guaranteed.
4. A package has one stable, renewable lease URL. Polls and media reads extend
   the same lease; a retry never rotates a still-valid media URL.
5. Android accepts the Android 15 `dataSync` six-hour ceiling in version 1,
   persists progress, and resumes through the scheduler or an explicit tap.
6. Offline bytes have their own operator and per-user quotas. They do not
   consume the ordinary playback-cache budget or evict playback artifacts.
7. One-tap selection never auto-burns a recommended bitmap subtitle. It falls
   back to no subtitle and explains that limitation in download options.

The correction ledger is intentionally in the plan: format, queue, lease,
Apple lifecycle, Android service, persistence, progress, and validation claims
must not drift back to the rejected version during implementation.

## 1. Objective

Ship offline viewing for the iPhone/iPad and Android phone/tablet apps with one
predictable flow:

1. Tap **Download** on a playable movie or episode.
2. Cinema queues it immediately using the saved download quality and language
   preferences.
3. The server prepares or reuses a finished portable mobile HLS package. The
   UI says **Preparing on server**; it does not expose transcoding terminology.
4. If preparation finishes while the app can start a platform transfer, the
   device downloads in the background. Otherwise the UI says **Prepared —
   open Cinema to finish downloading** and starts it on the next visit.
5. The item appears under **Downloads** and plays with no network, DNS, server,
   bearer token, or provider API.
6. Offline playback writes progress locally. Cinema syncs the newest pending
   position after the server becomes reachable again.
7. **Remove Download** deletes the app-owned package and artwork and reclaims
   the space. It does not delete or modify server media.

Success is observable, not architectural: after force-quitting the app,
turning on airplane mode, and stopping `plurxd`, an already downloaded title
still appears, starts, seeks, shows its downloaded subtitle, records progress,
and can be deleted without leaving bytes behind.

## 2. Product contract — it behaves like a streaming app

### 2.1 One tap starts the work

The primary detail action is **Download**, next to Play/Resume on phones and
tablets. It does not open a filesystem UI. On tap, the button becomes a compact
state indicator and cancellation control:

```
Download ─▶ Queued ─▶ Preparing on server ─▶ Downloading 42% ─▶ Downloaded
    ▲          │                 │                    │              │
    └──────────┴──── Retry ◀─────┴──── failure ───────┘              │
                                                                  Remove
```

The default is deliberately one tap. Settings owns the ordinary choices:

| Setting | Default | Reason |
|---|---:|---|
| Download quality | Standard (up to 720p) | A flight library should not silently consume tens of gigabytes. |
| Download network | Wi-Fi / unmetered only | A background transfer must not surprise a mobile-data plan. |
| Audio | Current preferred audio language | It matches ordinary playback selection. |
| Subtitles | Current preferred subtitle policy | The downloaded title behaves like the same title online. |

An optional **Download options** action may override quality, audio, or
subtitle for one title. It is secondary; ordinary downloading cannot depend on
opening it. High quality means up to 1080p. A source below the requested rung is
never upscaled, and the UI shows the actual prepared height.

### 2.2 Downloads is a real offline library

iOS adds a **Downloads** tab. Android adds a Downloads destination reachable
from the Home top bar and from an empty/offline Home state. Both show only
app-managed records for the last authenticated server instance and user.

Each row carries a locally saved poster, title, episode context, duration,
quality, size, and state. The screen must render without asking the server for
metadata or artwork. Tapping a completed row plays it; tapping an in-progress
row shows progress, pause/resume where the platform supports it, and Cancel.

The launch path must not make a traveller wait through today's reconnect
attempts. If at least one completed download belongs to the saved profile, the
app exposes Downloads immediately while server validation continues in the
background. Online Home may show a connection error; Downloads may not be held
behind it.

### 2.3 Storage remains inside Cinema

Cinema stores an opaque HLS package in its application sandbox. Users can see
how much space each title and all downloads consume, but not a path or media
filename. There is no share sheet, document-provider registration, Android
`FileProvider`, iTunes file sharing, or Files-app document declaration.

The package remains until the user removes it, the app is uninstalled, or the
OS reclaims managed storage. A reconciliation pass on every launch verifies
that the package still exists; if the OS removed it, Cinema says **Download
missing — download again** instead of claiming it is available.

### 2.4 Account and server boundaries survive offline mode

Every local record is scoped by `(server_instance_id, user_id, file_id)`.
Both clients persist the numeric `user.id` returned by login/`me()`, in
addition to the existing username. A non-nil server instance id and user id
are required before a download can be queued. If either is missing, the action
is disabled with **Connect once before downloading**. Changing servers or
signing in as another user hides, but does not silently
delete, a different profile's packages. Explicit sign-out asks whether to keep
or remove that profile's downloads; **Keep** is the default because deletion is
irreversible and a sign-out may be accidental. Kept downloads remain hidden
until that same server/user profile is active again. Downloads always exposes
an **Other profiles** row with item count, total bytes, and a delete action so
hidden packages never become unreclaimable storage.

Offline access necessarily trusts the last authenticated local profile. A
server cannot revoke bytes that are already on a disconnected device. plurx
does not have DRM, device attestation, rental expiry, or remote wipe, and this
feature must not pretend otherwise. §11 defines the security boundary.

## 3. Scope — mobile first, one portable output

### 3.1 Version 1 platforms

| Surface | Version 1 | Why |
|---|---|---|
| iPhone / iPad | Yes | `AVAssetDownloadURLSession` provides system-managed background HLS downloads. |
| Android phone / tablet / foldable | Yes | Media3 `DownloadService` and `DownloadManager` persist background HLS downloads. |
| tvOS | No | Television storage is small and reclaimable, and offline travel is not its use case. Keep shared Swift fenced with `#if os(iOS)`. |
| Android TV / Google TV | No | Hide download actions on the television form factor; external-storage policy is a separate product decision. |
| Web / installed PWA | No | Browser quota, eviction, background execution, and authenticated HLS persistence do not provide the same promise. |
| Plex-compatible clients | No | plurx cannot control their download UI, storage, or authentication lifecycle. |

The server contract is client-neutral. A future desktop or television client
may consume it, but no version 1 acceptance check depends on those surfaces.

### 3.2 Version 1 media contract

Every offline package is a complete HLS VOD presentation:

| Component | Contract |
|---|---|
| Video | H.264, maximum 1080p, the existing rung bitrate and encoder pipeline. |
| Dynamic range | SDR. HDR/Dolby Vision sources use the existing tone-map path. |
| Audio | One selected track, AAC through the existing transcode recipe. |
| Text subtitle | One selected SRT/SubRip/WebVTT-class track as native segmented WebVTT, plus Off. |
| Bitmap/styled subtitle | Never selected automatically. Version 1 falls back to Off; an explicit future burn choice requires confirmation. |
| Container | Finished MPEG-TS HLS (`segNNNNN.ts`) with `#EXT-X-PLAYLIST-TYPE:VOD` and `#EXT-X-ENDLIST`. |
| Start | Always source time zero; resume is a local player seek. |

This intentionally does not download the raw source. Raw MP4 would work for
some phones, but MKV, DTS, TrueHD, unsupported Dolby Vision profiles, and
bitmap subtitles would make the same Download button succeed or fail according
to the title and platform. One portable output makes the promise total. It also
reuses the transcode cache's content-addressed MPEG-TS HLS when the full recipe
matches rather than introducing a second media pipeline. Reuse is narrow:
source identity, output rung, audio, burn track, tone map, pipeline version,
and audio offset must all match.

The trade-off is explicit: even a directly playable source is prepared as the
portable H.264/AAC package in version 1. That spends server time and gives up
offline HDR in exchange for one background-download and playback contract on
both client families. A later fast path may copy compatible video only after it
proves byte-for-byte parity with §4; it must not be smuggled into this release.

### 3.3 Non-goals are guardrails

- **No Save File or export.** The app owns opaque media storage because users
  asked for offline viewing, not file management.
- **No original-quality / 4K / HDR offline output.** Storage cost, device
  compatibility, and thermal behavior need physical evidence first.
- **No multiple downloaded audio tracks.** Each extra track costs bytes and
  complicates selection. Redownload with another language in version 1.
- **No PGS object bundle or automatic burn.** A recommended bitmap/styled
  subtitle falls back to Off. The online `pgs-v1` overlay is many authenticated
  objects and is not yet an offline package format.
- **No whole-season or smart-download automation.** Individual movie/episode
  downloads establish the lifecycle first. Bulk queueing can compose later.
- **No play-while-preparing.** Apple only persists finished VOD HLS; an EVENT
  playlist is not an offline asset.
- **No download expiry, DRM, remote revocation, or remote wipe.** OS sandboxing
  is the boundary; [SECURITY.md](SECURITY.md) must say so plainly.
- **No offline library browsing beyond Downloads.** Server-backed Home,
  Search, Libraries, and full metadata remain online surfaces in version 1.
- **No change to source media.** The read-only media mount promise remains
  absolute; all generated bytes live under plurx's cache/data directories and
  the client's sandbox.

## 4. Architecture — prepare once, transfer in the background, play locally

### 4.1 End-to-end flow

```
       tap Download
             │
             ▼
   native client saves local intent + metadata snapshot
             │  authenticated JSON
             ▼
   POST /files/{id}/offline-packages
             │
             ├── cache hit ───────────────────────────────┐
             │                                            │
             └── queued producer                          │
                   │  same recipe + admission controls    │
                   ▼                                      │
             complete HLS VOD in transcode cache          │
                   │                                      │
                   └──────────────────────────────────────┘
                                      │
                         package state = ready
                                      │
                 POST /offline/packages/{id}/leases
                                      │  scoped capability URL
                     ┌────────────────┴────────────────┐
                     ▼                                 ▼
        AVAssetDownloadURLSession            Media3 DownloadService
        stores managed local package         stores non-evicting cache
                     │                                 │
                     └───────────┬─────────────────────┘
                                 ▼
                    local-only player source
                                 │
                                 ▼
                    durable local progress outbox
                                 │  next connection
                                 ▼
                         POST item progress
```

There are two progress domains and the UI names both. **Preparing** means the
server is creating a finished portable package. **Downloading** means the
device is transferring that already-finished package. Combining them into one
percentage would show long stalls and make failures impossible to diagnose.

### 4.2 Existing systems to reuse

| Existing owner | Reuse |
|---|---|
| `TranscodeManager::options_for` | Build the exact H.264/AAC/tone-map/subtitle recipe. |
| `transcode::ladder` and `bitrate_for_height` | Offer real 720p/1080p rungs and estimate bytes. |
| `transcode::Recipe` | Deduplicate equal requests and invalidate on source/pipeline changes. |
| `TranscodeManager::produce` / `produce_into` | Produce resumable, preemptible, complete VOD assets. |
| `transcode_cache_*` tables | Publish atomically and retain reusable artifacts. |
| HLS master/subtitle helpers | Generate native WebVTT renditions against the completed timeline. |
| hardware admission | Let live viewers preempt background preparation. |
| native player surfaces | Keep controls, seek, subtitles, skip markers, and progress UI. |

Reuse means extracting a shared internal primitive, not calling the public
playback endpoint. `POST .../hls/sessions` records playback start, feeds Trakt,
creates an idle-reaped live session, and may return an unfinished EVENT
playlist. An offline preparation doing any of those is a correctness bug.

### 4.3 New server owners

Add `crates/plurxd/src/offline.rs` for the durable preparation coordinator and
`crates/plurxd/src/http/offline.rs` for the API and capability-served VOD.
Keep recipe/output construction in `TranscodeManager`; keep persistence behind
the `Store` trait.

`OfflineManager` owns a serial durable request queue, but the priority permit
does not exist yet. The existing speculative producer is a six-hour
`AtomicBool` skip-if-busy loop, while live playback can terminate its child
immediately and later resume at the last published boundary. Milestone M2b
therefore introduces a shared priority-aware permit and converts both
background producers to it. Until M2b lands, offline production stays disabled.

Priority is live playback, user-requested offline work, then speculative
pre-transcode. Live admission may terminate an active encoder mid-segment; the
next producer resumes from the last atomically published segment. Encoder
failure may still discard staging and retry from zero, and the UI/ETA must be
honest about that. A download may take longer while somebody watches a film;
it must never make that film stall.

### 4.4 Finished means every dependent byte is finished

A package does not enter `ready` until all of these are true:

- The video cache location is complete and contains a VOD playlist ending in
  `#EXT-X-ENDLIST`.
- Every named `segNNNNN.ts` media segment exists with its final name.
- The actual duration assembled from `#EXTINF` entries covers the source within
  the current segment-duration tolerance.
- A selected native text subtitle has completed `ensure_vtt`; offline routes
  may never return the temporary empty WebVTT segment used by live playback.
- The offline master names the output rung's real resolution and bandwidth,
  not the source file's values used by today's generic HLS master.
- The package's actual byte count includes media segments, video playlist,
  offline master, and selected VTT rendition. Readiness re-parses duration from
  the completed playlist's `#EXTINF` entries rather than assuming a cache
  duration column that does not exist.

Only VOD can be saved by Apple's offline HLS APIs. A `ready` response for a
growing playlist would therefore be a platform failure disguised as success.

## 5. Server contract — exact JSON and lifecycle

All metadata/status endpoints require `AuthUser` and enforce package ownership.
Only the final media paths use a scoped capability because AVFoundation and
Media3 autonomously fetch child HLS resources.

### 5.1 `GET /api/v1/files/{file_id}/offline-options`

This is the single truth for the one-tap defaults and optional override sheet.
Clients pass their saved ISO language preferences as `audio_lang` and
`subtitle_lang` query values; the server feeds those through the shared core
track selector rather than making Swift and Kotlin duplicate it. Re-verify DTO
names against
[`http/stream.rs`](../crates/plurxd/src/http/stream.rs) at build time.

```json
{
  "file_id": 42,
  "qualities": [
    { "height": 1080, "label": "High", "estimated_bytes": 4831838208 },
    { "height": 720, "label": "Standard", "estimated_bytes": 2466250752 }
  ],
  "audio": [
    { "index": 0, "codec": "truehd", "channels": 8,
      "language": "eng", "title": "Atmos", "default": true }
  ],
  "subtitles": [
    { "index": 2, "codec": "subrip", "language": "eng",
      "title": "English", "default": true, "offline_mode": "native" },
    { "index": 3, "codec": "hdmv_pgs_subtitle", "language": "eng",
      "title": "English SDH", "default": false, "offline_mode": "burned" }
  ],
  "recommended_audio_index": 0,
  "recommended_subtitle_index": 2
}
```

Rules:

- Qualities are the existing ladder restricted to at most 1080p. Standard is
  the highest rung at or below 720p; High is the highest at or below 1080p.
  Duplicate answers collapse for low-resolution sources.
- `estimated_bytes = duration_seconds × total_kbps × 1000 / 8` is an
  average-rate estimate. Admission uses the rung's `peak_kbps` and reserves
  platform overhead separately; neither number is a storage guarantee.
- Recommendations come from the same `select_tracks` and language preferences
  as playback. No client duplicates the language algorithm.
- `offline_mode` is `native` for simple WebVTT-convertible text. Bitmap or
  styled recommendations become `none`; explicit burn is deferred until the
  UI can require confirmation. Unknown explicit selections are rejected.
- A missing source returns the same `409` availability explanation as
  `/decision`; an unprobed file returns `409` because package sizing and track
  selection would be guesses.

### 5.2 `POST /api/v1/files/{file_id}/offline-packages`

```json
{
  "request_id": "8d86cb9d-67d7-4d74-92bc-8a77ef430df4",
  "height": 720,
  "audio_index": 0,
  "subtitle_index": 2
}
```

`request_id` is generated and persisted by the client before the request. The
same user plus request id and same normalized choices returns the original
package; the same id with different choices returns `409`. This makes an app
kill or uncertain response safe to retry.

Created or in-progress response (`202 Accepted`):

```json
{
  "id": "503c4d67-0cde-43ec-bb5b-15978c56f328",
  "state": "queued",
  "phase": "waiting_for_encoder",
  "status_url": "/api/v1/offline/packages/503c4d67-0cde-43ec-bb5b-15978c56f328",
  "progress": null,
  "bytes_ready": 0,
  "estimated_bytes": 2466250752,
  "output": {
    "height": 720,
    "video_codec": "h264",
    "audio_codec": "aac",
    "dynamic_range": "sdr",
    "subtitle_mode": "native"
  }
}
```

A cache hit may return `200 OK` with `state: "ready"`. Creation validates the
selected file, rung, and tracks before inserting anything. `507 Insufficient
Storage` says how much server cache space is required when the configured
offline ceiling or per-user quota cannot hold the peak-rate reservation.
`offline.max_gb = 0`, `offline.enabled = false`, and a disabled user return
typed errors before hours of work begin. `ApiError` gains structured
`{code,message}` bodies plus explicit `429` and `507` variants; ownership
lookups take `user.id` and return `404` on a miss instead of reusing the
admin-only `Forbidden` text.

### 5.3 `GET /api/v1/offline/packages/{id}`

The owning client polls while preparing. States are
`queued · preparing · ready · failed`; phases are
`waiting_for_encoder · transcoding · extracting_subtitles · publishing ·
ready`. `progress` is nullable until the first complete segment, then equals
completed presentation duration divided by source duration and never moves
backward. `bytes_ready` counts only published/staged complete files.

Failure is typed:

```json
{
  "id": "503c4d67-0cde-43ec-bb5b-15978c56f328",
  "state": "failed",
  "phase": "transcoding",
  "error": {
    "code": "source_unavailable",
    "message": "The media file is no longer available on the server."
  }
}
```

Expected codes are `source_unavailable · invalid_track · insufficient_storage
· encoder_failed · subtitle_failed · package_expired · package_evicted ·
offline_disabled · quota_exceeded`. Internal ffmpeg paths and stderr do not
cross the API.

Clients poll after 1 s, then 2 s, then every 5 s. The app may stop polling in
the background; the durable server job continues and the next GET catches up.

### 5.4 `PUT /api/v1/offline/packages/{id}/lease`

Only a ready package can create or renew its one transfer lease. The client
generates and durably stores a 256-bit token before sending the idempotent body
`{"token":"<64 lowercase hex characters>"}`. The first call returns
`201 Created`; retries with the same token return `200 OK` with the same URL
and an extended expiry:

```json
{
  "manifest_url": "/api/v1/offline/media/4af0...e927/master.m3u8",
  "expires_at": 1786579200,
  "bytes": 2381040331,
  "duration_ms": 7198123
}
```

The path contains that client-generated 256-bit secret. Store only its SHA-256
hash. The
lease grants read access to this package and nothing else, lasts seven days,
survives a server restart, and pins the matching offline artifact. A successful
status poll, renewal, or media read extends that same lease in place. The
server never rotates a valid URL: AVFoundation has no asset resume data and
Media3 HLS cache keys include child URIs, so rotation means a from-zero
redownload. Neither the secret nor full manifest URL is logged.

Typed child routes prevent path traversal:

```text
GET /api/v1/offline/media/{secret}/master.m3u8
GET /api/v1/offline/media/{secret}/index.m3u8
GET /api/v1/offline/media/{secret}/{segment}
GET /api/v1/offline/media/{secret}/subs/{index}/index.m3u8
GET /api/v1/offline/media/{secret}/subs/{index}/{segment}
```

`{segment}` is a whole path segment; the handler accepts only exact
`segNNNNN.ts` or `segNNNNN.vtt` names. This follows the existing subtitle-route
pattern and is re-verified against axum 0.8 at build time. Every request hashes
the secret, verifies the unexpired lease and complete
cache location, and touches `last_access_at` at most once per minute. Segment
responses keep `Content-Length`, correct HLS MIME types, and immutable private
cache headers. The VOD playlists themselves are immutable too; unlike live
playlists they may use a seven-day private max age.

### 5.5 Completion, cancellation, and expiry

```text
POST   /api/v1/offline/packages/{id}/complete  client has a verified local copy
DELETE /api/v1/offline/packages/{id}           cancel preparation / release lease
```

Both require the owning user and are idempotent. Completion or cancellation
removes this package's lease and pin, but leaves the content-addressed VOD in
the ordinary cache for reuse and normal LRU eviction. If another package row
references the same recipe, its lease and preparation interest remain.

The client calls `complete` only after the platform download callback and a
local playability check. If that acknowledgement cannot reach the server, the
local download is still valid; a server sweep expires untouched package rows
and leases seven days after their last access. Expiry never reaches into a
device or deletes a completed local package.

## 6. Persistence — durable jobs without turning the server into a library

Add migration v14 after the current v13 in
[`store/sqlite/mod.rs`](../crates/plurx-core/src/store/sqlite/mod.rs). Re-verify
the number at build time.

```sql
CREATE TABLE offline_packages (
    id                 TEXT PRIMARY KEY,
    request_id         TEXT NOT NULL,
    user_id            INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    file_id            INTEGER NOT NULL,
    node_id            TEXT NOT NULL,
    source_path        TEXT NOT NULL,
    source_size        INTEGER NOT NULL,
    source_mtime       INTEGER NOT NULL,
    recipe_hash        TEXT,
    target_height      INTEGER NOT NULL,
    output_width       INTEGER,
    output_height      INTEGER,
    audio_index        INTEGER,
    audio_offset_ms    INTEGER NOT NULL DEFAULT 0,
    subtitle_index     INTEGER,
    subtitle_mode      TEXT NOT NULL
                       CHECK (subtitle_mode IN ('none', 'native', 'burned')),
    state              TEXT NOT NULL
                       CHECK (state IN ('queued', 'preparing', 'ready', 'failed')),
    phase              TEXT NOT NULL,
    progress_millis    INTEGER NOT NULL DEFAULT 0,
    estimated_bytes    INTEGER NOT NULL DEFAULT 0,
    reserved_bytes     INTEGER NOT NULL DEFAULT 0,
    actual_bytes       INTEGER,
    error_code         TEXT,
    error_message      TEXT,
    created_at         INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at         INTEGER NOT NULL DEFAULT (unixepoch()),
    last_access_at     INTEGER NOT NULL DEFAULT (unixepoch()),
    expires_at         INTEGER NOT NULL,
    UNIQUE (user_id, request_id)
) STRICT;

CREATE INDEX offline_packages_queue
    ON offline_packages(node_id, state, created_at);
CREATE INDEX offline_packages_recipe
    ON offline_packages(node_id, recipe_hash, state);

CREATE TABLE offline_package_leases (
    token_hash      TEXT PRIMARY KEY,
    package_id      TEXT NOT NULL UNIQUE
                    REFERENCES offline_packages(id) ON DELETE CASCADE,
    created_at      INTEGER NOT NULL DEFAULT (unixepoch()),
    last_access_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    expires_at      INTEGER NOT NULL
) STRICT;

CREATE INDEX offline_package_leases_package
    ON offline_package_leases(package_id);
```

`file_id` deliberately has no foreign key. Rescans replace file rows and must
not cascade-delete an active package, its lease, or its diagnostic. The source
identity snapshot lets the producer either reopen the same bytes or return
typed `source_unavailable`. `recipe_hash` is nullable only between validated
insertion and recipe construction and deliberately does not reference the
recipe table.

Add an `OfflinePackageStore` subtrait and compose it into `Store`, following
the existing cache subtrait pattern. Required operations are create/idempotent
lookup · owner lookup · queue claim · monotonic progress · ready/failure ·
single lease create/renew/hash lookup/touch · complete/cancel · expired sweep · active
recipe pins. No SQL may live in the HTTP handlers.

The per-user quota check, idempotent request lookup, and insert are one store
operation in one SQLite transaction. Counting and inserting through separate
mutex acquisitions is a tap-loop race. Failed rows are retained for a bounded
diagnostic window and count toward a separate registry limit until swept.

On startup, rows left `preparing` by a process death become `queued`; the
deterministic staging directory and cache claim let the producer resume. A
missing source or irrecoverable failed staging area becomes a typed failure,
not an infinite retry.

Migration v14 is forward-only. Rollback means restoring the pre-upgrade
database backup with the older binary; a v13 binary is not expected to open a
database after v14 has committed. The migration test records that boundary.

## 7. Package production — extend the cache, do not fork playback

### 7.1 One normalized internal specification

Add these shapes next to `SessionRequest` in
[`transcode.rs`](../crates/plurxd/src/transcode.rs); exact visibility may change
during implementation, but the information must stay singular:

```rust
pub struct OfflineSpec {
    pub file_id: i64,
    pub target_height: i64,
    pub output_width: Option<i64>,
    pub output_height: Option<i64>,
    pub audio_index: Option<i64>,
    pub subtitle: OfflineSubtitle,
}

pub enum OfflineSubtitle {
    None,
    Native(i64),
    Burn(i64),
}

pub async fn ensure_offline(
    &self,
    file: &MediaFile,
    spec: &OfflineSpec,
    deadline: Instant,
    cancelled: CancellationToken,
    progress: impl Fn(OfflineProgress) + Send,
) -> Result<OfflineProduceOutcome, OfflineProduceError>;

pub enum OfflineProduceOutcome {
    Ready(Produced),
    Yielded,
    ClaimedElsewhere,
}
```

`ensure_offline` builds a time-zero `SessionKind::Transcode`, honours the
source's `audio_offset_ms`, uses the selected audio, and forces SDR tone mapping
even when the node-wide playback preference is passthrough. A native subtitle
does not change video bytes, so it is not added to the transcode recipe; the
package row records it and readiness separately gates on its extracted VTT.

The current `produce` head is not reusable: its eligibility and track choices
are scheduled-pretranscode policy. Parameterize track selection, change the
needed private visibility, and hoist the shared tail: source digest, recipe
hash, cache hit, text-subtitle preparation, staging, cache claim,
`produce_into`, atomic rename, and cache completion. `Yielded` is a queue
outcome, not a fake encoder error.

### 7.2 Queue and resource rules

- Add one priority-aware background permit per node across scheduled and
  requested work; this is a new primitive, not an existing queue tweak.
- Priority is live playback · offline request · speculative pre-transcode.
- FIFO within offline requests, with identical recipe hashes coalesced.
- A live waiter may terminate the child mid-segment and receives the slot
  inside the existing bounded admission window. Resume begins at the last
  published segment boundary.
- Cancellation removes one package's interest. It stops production only when
  no other package or scheduled candidate wants the recipe; staged work remains
  resumable.
- A process restart requeues durable requests oldest-first.
- Admission uses a per-user pinned-byte quota, not a nominal 100-row promise.
  A secondary total-row ceiling bounds failed-row spam and is enforced inside
  the same transaction as idempotent create.
- Active package claims are either excluded from the existing 24-hour stale
  claim sweep or touched on every queue pass. Version 1 uses explicit exclusion
  so a long preempted encode cannot be mistaken for abandoned staging.

### 7.3 Cache budget and pins

Create an offline artifact budget separate from `cache.max_gb`. Operator
settings are `offline.enabled`, `offline.max_gb`,
`offline.max_gb_per_user`, and optional per-user enable/override. A default
install reserves 25 GiB globally and 15 GiB per user; operators may set either
to zero to disable admission. Offline sweeps protect preparing, ready, and
actively leased recipes and never evict ordinary playback cache to make room.

Before accepting work, compare the conservative estimate with filesystem free
space and the configured cache ceiling after unpinned eviction candidates. A
package may reuse a complete matching recipe for zero new bytes. Do not delete
another active transfer to make room.

The existing playback-cache reader/eviction race is a separate corrective
change with its own regression test and commit. Active readers expose a recipe
pin while segments are served so housekeeping cannot delete their directory;
that change does not ride invisibly inside the offline milestone.

### 7.4 Offline HLS is a separate presentation wrapper

The cached `index.m3u8` and MPEG-TS media segments are reused without copying.
The new immutable master wrapper must describe the output rather than the
source:

```text
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-STREAM-INF:BANDWIDTH=<rung peak>,AVERAGE-BANDWIDTH=<rung total>,
  RESOLUTION=<scaled output>,
  CLOSED-CAPTIONS=NONE[,SUBTITLES="subs"]
index.m3u8
```

The actual line is unwrapped. `CODECS` stays absent until it is derived from
the produced bitstream or all encoders pin an asserted profile/level.
`VIDEO-RANGE` is absent because every v1 package is forced SDR. Native subtitle
playlists mirror the completed video timeline and end with `#EXT-X-ENDLIST`.
The offline subtitle route uses a package-owned timeline parsed from the VOD;
it does not reuse live `session_file`/`segment_window` state or its temporary
empty WebVTT fallback.

## 8. Apple client — system-managed HLS, local-first launch

Apple's supported offline path is
[`AVAssetDownloadURLSession`](https://developer.apple.com/documentation/avfoundation/avassetdownloadurlsession),
which uses a background session, persists across launches, and downloads HLS
VOD into an app-managed local package. The current Apple deployment target is
iOS 17, so `AVAssetDownloadConfiguration` is unconditional. The delegate keeps
the iOS 17 completion callback and adds the iOS 18+ callback under an
availability check. The tvOS target must continue compiling in Debug and
Release.

### 8.1 New owners

- `Sources/OfflineModels.swift` — Codable local record and pure state mapping.
- `Sources/OfflineCatalog.swift` — an actor that atomically writes
  `Application Support/Offline/index.json`, reconciles missing packages, and
  owns pending progress.
- `Sources/OfflineDownloadManager.swift` — iOS-only
  `AVAssetDownloadURLSession`, delegate callbacks, task restoration, network
  policy, and server preparation polling.
- `Sources/DownloadsView.swift` — iOS-only library and storage UI.
- `Sources/OfflineAppDelegate.swift` — iOS-only
  `UIApplicationDelegateAdaptor` delivery for background-session completion.

`AppModel` owns or exposes these services but must not absorb their file and
delegate logic. Shared files fence iOS-only types and call sites with
`#if os(iOS)`. `make apple-test` is expanded to run iOS/tvOS Debug tests and
Release builds; until then the four configurations are a documented manual
gate, not an enforced claim.

### 8.2 Local record

Persist the intent before the first network request:

```swift
struct OfflineItem: Codable, Identifiable {
    var id: String                 // stable local UUID / Media3-equivalent id
    var requestId: String          // idempotent server request
    var serverInstanceId: String
    var userId: Int
    var itemId: Int
    var fileId: Int
    var packageId: String?
    var title: String
    var context: String?           // show · SxxExx, when applicable
    var durationMs: Int?
    var posterFile: String?
    var requestedHeight: Int
    var actualHeight: Int?
    var audioLabel: String?
    var subtitleLabel: String?
    var state: OfflineState
    var bytesDownloaded: Int64
    var bytesTotal: Int64?
    var localAssetRelativePath: String?
    var markers: [OfflineMarker]
    var positionMs: Int
    var recordedAt: Date?
    var pendingProgress: Bool
    var updatedAt: Date
}
```

Use an atomic temp-write plus rename and recover the last valid index after a
crash. Persist the delegate destination relative to `NSHomeDirectory()` and
restore it against the current container root; absolute container paths change
across launches and updates. Do not move the downloaded asset.
Artwork is copied into the same private tree before the item is marked
downloaded. A missing poster is a cosmetic failure, not a failed movie.

### 8.3 Background transfer

Create two background asset sessions with stable bundle-qualified identifiers:
one unmetered-only and one any-network. Session configuration is copied at
creation, so a preference change applies by creating future tasks in the other
session. Restore both sessions' tasks on launch and map `taskDescription` to
the local item id. Use the system task's `Progress` for the device-transfer
percentage. The App delegate/SwiftUI lifecycle retains and calls each
background-session completion handler after delegate events are drained.

Wi-Fi-only maps to the unmetered session. A user switching to Any Network
applies to newly created tasks; the UI states when an existing task is waiting
for Wi-Fi. Do not emulate background work with an ordinary `Task` that dies
when iOS suspends the app. If server preparation completes after suspension,
transfer starts at the next foreground visit; `BGProcessingTask` is only a
best-effort catch-up and the UI says so.

Set `primaryContentConfiguration.mediaSelections` explicitly from the
package's audio and subtitle options. Save the delegate-provided local URL
immediately. On completion, require `asset.assetCache?.isPlayableOffline ==
true`, a nonzero duration, and the selected subtitle in
`mediaSelectionOptions(in:)`; generic `AVURLAsset.isPlayable` is insufficient.
Set `AVAssetDownloadStorageManager` priority to `.important` and a long expiry
as an eviction hint, not a guarantee. Then save artwork/index, mark Downloaded,
and best-effort acknowledge the server package.

System termination restores tasks and delivers background events. A user
force-quit cancels active downloads and does not relaunch the app; the catalog
maps that cancellation to **Paused — tap Resume**, discards unusable partial
asset bytes, and never promises continuation. iOS 26+ user cancellation from
the system transfer UI maps to the same state.

### 8.4 Offline playback

Refactor `PlayerController` to accept a source enum rather than manufacturing a
remote decision for every play:

```swift
enum PlaybackSource {
    case remote(PlayContext)
    case offline(OfflinePlayContext) // local AVURLAsset + snapshot + position
}
```

The offline branch creates `AVPlayerItem` from the local asset and never calls
`/decision`, creates an HLS session, polls status, or builds a token-bearing
URL. It installs a decision-shaped local snapshot with tracks, markers, source,
`isVOD`, and `usesDirectTimeline` so seek, markers, and subtitle ordinals do not
silently depend on a remote decision. It keeps the same PlayerSurface, Now
Playing metadata, native subtitle selection, and Picture in Picture where the
local presentation supports it. Quality is fixed and the info panel says
`Offline · 720p · H.264 · AAC`.

Progress goes to the local record every 10 seconds and on exit. When online,
`OfflineCatalog` submits the newest pending value through the existing progress
endpoint and clears the flag only after a 2xx. Replaying a completed download
from the beginning may lower progress deliberately; the local event most
recently created by this profile wins when it reconnects.

### 8.5 Offline launch and UI

Add Downloads to the iOS `TabView` with a platform-conditional
`HomeLayoutPolicy.topLevelTabs` value and update its pinned test. Publish an
offline-capable phase before `bootstrap()` performs its first await and hoist
the iOS tab shell out of the current `.ready`-only switch. Bound rediscovery in
the background. If bootstrap cannot reach the server but a saved profile has
downloads, it does not strand the user on `ReconnectView`. Home can retain a
retry banner, while Downloads stays functional. `ReconnectView` also gets an
explicit **View Downloads** escape hatch for defensive reachability.

The detail screen shows Download only on iOS and only for a playable leaf.
Queued/downloaded state comes from `OfflineCatalog`, not a fresh server query.
Series and seasons keep their ordinary play action; bulk downloads are outside
v1.

## 9. Android client — Media3 owns transfer and cache

Use Media3's documented offline stack:
[`DownloadService`](https://developer.android.com/media/media3/exoplayer/downloading-media)
wraps a persistent `DownloadManager` and `DownloadIndex`; a `SimpleCache` with
`NoOpCacheEvictor` holds completed downloads; playback uses the same cache
through a read-only `CacheDataSource.Factory`. Never open files from the cache
directory directly.

### 9.1 New owners and dependencies

- `data/offline/OfflineDownloads.kt` — singleton database provider, non-evicting
  cache, data-source factories, and `DownloadManager`.
- `data/offline/PlurxDownloadService.kt` — foreground service and progress
  notification.
- `data/offline/OfflineCatalog.kt` — maps `DownloadIndex` plus request metadata
  to UI state and owns pending progress/artwork.
- `ui/DownloadsScreen.kt` — phone/tablet library and storage controls.

Use the already pinned Media3 1.10.1 family and its `PlatformScheduler`;
`media3-database` and `media3-datasource` are already transitive. Do not add
the WorkManager artifact, whose merged foreground-service declaration would
need additional type configuration at this target SDK.

The manifest adds `FOREGROUND_SERVICE`,
`FOREGROUND_SERVICE_DATA_SYNC`, the unexported service with
`foregroundServiceType="dataSync"`, `RECEIVE_BOOT_COMPLETED`, and the
`BIND_JOB_SERVICE` scheduler service. A boot receiver never starts the
`dataSync` service directly. At target SDK 37, Android 15+ caps app-wide
`dataSync` foreground work at six hours per 24 hours; `onTimeout()` persists
state, stops cleanly, and leaves **Paused by system — tap Resume**. Version 1
accepts this ceiling instead of building a custom UIDT scheduler.

Android 13+ asks for notification permission when the first background
download needs it. Denial is benign: the service still runs and the transfer
appears in system task management even if the drawer notification is hidden.

The `NoOpCacheEvictor` cache lives under `filesDir`, not `cacheDir`, and its
singleton is process-wide because only one `SimpleCache` may own a directory.
Playback and download share that same instance.

### 9.2 Download identity and metadata

Use the local UUID as `DownloadRequest.id`; put the server/user/file identity,
display snapshot, package id, actual quality, selected tracks, and duration in
versioned JSON inside `DownloadRequest.data`. The manifest capability URI is
the content URI and the MIME hint is HLS. Populate explicit `streamKeys` from
the package manifest; empty means every rendition and is not a safe
single-rendition assertion.

The app persists its local intent before server work, then creates the Media3
request only after a lease exists. `DownloadIndex` is authoritative for bytes
and transfer state, but HLS content length is often unknown, so UI percentage
may be segment-based and byte totals are labelled approximate. The catalog
reconciles orphaned intent, failed requests,
missing artwork, and completed downloads at launch.

Set requirements to unmetered network by default and maximum parallel device
downloads to two. Server preparation remains serial; two transfers may run
because their bytes already exist and parallelism helps a pre-flight queue.

### 9.3 Cache-only playback

The existing online player continues using its authenticated OkHttp source.
An offline route passes `Download.request.toMediaItem()` into an ExoPlayer built
with the shared download cache and **no upstream factory**. Media3 then uses
`PlaceholderDataSource`, whose `open()` throws. A cache miss must
fail locally with **Download incomplete**; it must not quietly reach the server
and make an airplane-mode test pass only on Wi-Fi.

Refactor the player entry to a sealed source:

```kotlin
sealed interface PlaybackSource {
    data class Remote(val itemId: Long, val fileId: Long, val startMs: Long) : PlaybackSource
    data class Offline(val downloadId: String, val startMs: Long) : PlaybackSource
}
```

The implementation re-types `PlayerContent` from concrete `Plan` to `PlanLike`,
adds an offline `planMode`, parameterizes `buildPlayer`'s data-source factory,
and makes the API-backed PGS controller optional. The offline controller
bypasses `decision`, HLS creation, telemetry polling, session deletion, and
online next-episode lookup. It retains the same controls, PiP, local track
selection, and progress cadence. Autoplay may continue only
when the next episode is also in the local catalog; otherwise it returns to
Downloads.

### 9.4 Phone/tablet UI and offline launch

Hide the detail Download action and Downloads navigation when
`currentFormFactor() == FormFactor.Television`; non-composable service guards
use `isTelevision(context)`. The shared APK still contains the service and
permissions, so an instrumentation assertion proves no television path starts
it. Phone/tablet Home adds a Downloads
icon with an active-count badge. An empty online Home caused by a connection
failure includes **Open Downloads** rather than a dead end.

If saved-profile downloads exist, launch `Phase.Ready` immediately and validate
the server in the background instead of waiting today's 12-second launch cap.
Home reports connectivity independently; the Downloads route remains usable.

## 10. Local progress — offline work is not best-effort

Today's clients intentionally drop a failed progress POST. Offline viewing
cannot: the failure is the expected state for the whole session. Each local
catalog therefore keeps the last unsynced position per downloaded item:

```json
{
  "item_id": 91,
  "position_ms": 5368123,
  "duration_ms": 7198123,
  "watched": false,
  "recorded_at": 1785967200,
  "pending": true
}
```

Version 1 extends `POST /items/{id}/progress` with optional `recorded_at` and
refuses a mutation older than the server row's `updated_at`. The current wire
has only position and duration, and server watched state is monotone: replaying
from zero does not un-watch a completed item. The local `watched` field is a UI
snapshot, not a promise that sync can clear server watched state.

Send at most once per item, newest only, after saved-session validation
succeeds. Clear only on 2xx; 401/403 waits for reauthentication; transport/5xx
backs off without losing the row. Apple calls the throwing `PlurxAPI.progress`
path (or adds a throwing `AppModel` variant); Android writes the pending row
synchronously rather than launching a `viewModelScope` request that dies with
the process.

This is intentionally a final-state outbox, not an event history. It avoids a
hundred ten-second beats flooding the server after a flight. `recorded_at`
handles the common cross-device stale-write case; a richer revision protocol
remains deferred.

Downloading and package transfer never write progress, call
`note_playback_started`, scrobble, or show **Watching now**. Only opening the
local player creates the local progress record. On reconnection the ordinary
monarr watched outbox runs from the applied final state. Trakt scrobbling may
be absent when no live playback session exists, matching current behavior.

## 11. Security and privacy — bounded capabilities, honest local limits

### 11.1 Server media access

- Package create/options/status/complete/cancel require a valid bearer and the
  owning `user_id`.
- HLS child fetches use only the random package lease because platform media
  engines cannot reliably attach bearer headers to every autonomous child
  request.
- A lease is 256 random bits, stored only as SHA-256, scoped to one completed
  recipe/track selection, read-only, and renewable in place for seven days
  from its last authenticated or media access.
- The HTTP router uses one redacting span builder before span creation. It
  strips `token=` query credentials and both existing HLS and offline
  capability path shapes, including hand-written session-id fields that
  currently reach the admin log ring.
- Logs include package id, file id, user id, phase, recipe prefix, and byte
  counts. They never include the lease, bearer, full manifest URL, or local
  client path.
- Typed resource routes accept only exact playlist/TS/subtitle names. No
  arbitrary relative path is joined to the cache root.
- Expired/cancelled/complete packages revoke the server lease.
- `manifest_url` is a stable relative capability that each client resolves
  once against its signed-in server origin. Version 1 runs on the current
  single-process SQLite server, so that origin and the cache-owning node are
  the same deployment boundary. A future multi-node store may not enable
  offline delivery until it adds an absolute node endpoint or enforced sticky
  routing. Changing from LAN to remote routing mid-transfer is presented as
  Pause/Retry, never a transparent guarantee.

### 11.2 Device storage

Downloaded media is not encrypted by plurx and has no DRM. It receives the
platform's application sandbox and device-at-rest protection. The apps do not
export it, include it in ordinary cloud backups, or place it in shared/external
storage. Capability URLs may remain in platform download databases until the
record is removed; their scope and expiry make that an accepted trade-off.

Record enough metadata to render the Downloads screen, and no more. Do not
copy the bearer token into an offline record. Apple keeps the login token in
Keychain as today; Android keeps the existing session store. The transfer lease
cannot call JSON APIs or read another title.

### 11.3 Sharp edges to document

- Revoking a user or token does not erase an already downloaded package.
- A rooted/jailbroken device can read application storage.
- App uninstall deletes local downloads.
- iOS may reclaim managed assets under storage pressure; Cinema detects this.
- Changing a server address during an active transfer may require Retry. The
  server does not rotate the valid lease URL; a routing change that cannot
  reach its pinned node is explicit.

Update [SECURITY.md](SECURITY.md) in the implementation commit that first
serves a lease. Silence here would imply a DRM promise plurx does not make.

## 12. Failure behavior — every interruption has one recovery

| Failure | User-visible result | Recovery |
|---|---|---|
| Server offline before create | Queued · Waiting for server | Retry automatically when the saved server validates; Cancel remains local. |
| Live playback takes encoder | Preparing pauses without losing percent | Resume from staged segment boundary after the live waiter clears. |
| Server restarts while preparing | Preparing returns to Queued | Startup requeues durable row and resumes its claim. |
| Source disappears | Failed · Media unavailable on server | Restore/rescan source, then Retry creates a fresh request. |
| Server cache lacks space | Not started · Needs N GB on server | Admin frees/increases cache; no partial client task exists. |
| Device lacks space | Not started · Needs N GB on device | Remove downloads/free storage, then Retry. |
| Wi-Fi disappears | Waiting for Wi-Fi | Platform background manager resumes under requirements. |
| Lease nears expiry during transfer | Downloading | Media reads and status polls renew the same stable lease URL. |
| Lease expired after inactivity | Failed · Authorization expired | Restart from zero with a new package lease; no unsafe partial-cache promise. |
| iOS system terminates app | Downloading / restored | Background session relaunches the app and maps callbacks to local intent. |
| User force-quits iOS app | Paused · tap Resume | iOS cancels active tasks; discard unusable partial asset and resume explicitly. |
| Android hits six-hour ceiling | Paused by system · tap Resume | Persist index state; scheduler or foreground Resume continues cached segments. |
| Preparation finishes while iOS is suspended | Prepared · open Cinema to finish | Start transfer on next foreground; background catch-up is best effort. |
| Package validation fails | Failed · Download incomplete | Delete corrupt local package and Retry. |
| OS reclaims local asset | Download missing | Remove stale catalog entry or Download Again. |
| Progress sync fails | Playback remains complete locally | Durable final state retries after authenticated connectivity. |
| User cancels | State becomes Not downloaded | Stop task, delete partial local bytes, release package best-effort. |

Before starting a device transfer, compare actual ready bytes with available
capacity. Require `actual_bytes × 1.10 + 256 MiB` free; the 10% covers platform
package overhead and 256 MiB prevents a successful movie from leaving the
device unusably full. If capacity APIs return unknown, show the actual package
size and allow the platform to decide rather than inventing free space.

## 13. Observability — downloads are work, not viewers

The activity surface gains two non-playback kinds:

- **Preparing offline · title · 63%** — encoder work and whether it is waiting
  behind playback.
- **Sending offline · title · 1.8 GB / 2.4 GB** — bytes served through an active
  lease, when the client is currently fetching.

Neither appears under Watching Now. Metrics use bounded labels only:

```text
plurx_offline_packages{state="queued|preparing|ready|failed"}
plurx_offline_prepare_seconds{result="ok|failed|cancelled"}
plurx_offline_prepared_bytes_total
plurx_offline_transfer_bytes_total
plurx_offline_active_leases
plurx_offline_failures_total{code="..."}
plurx_cache_pinned_bytes{reason="playback|offline"}
```

The current `/metrics` response is hand-formatted and has no counter or
histogram abstraction. M2c adds the minimal bounded counter/histogram plumbing
before naming these series; it does not scatter ad-hoc atomics through HTTP
handlers. Admin surfaces also expose queue ETA, measured encode speed by source
class, global/per-user quota use, and force-expire/disable controls.

Do not label metrics with user, title, file, package, recipe, or device. Logs
carry correlation ids for diagnosis; metrics stay bounded.

## 14. Milestones — each leaves a reviewable, tested boundary

### 14.1 M0 — prove both platform downloaders against plurx VOD

Before schema or UI work, use an existing completed cached transcode and a
temporary capability route/test harness to prove the riskiest dependency:

1. Serve a complete one-variant master plus native WebVTT from plurx.
2. Download it with `AVAssetDownloadURLSession` on a physical iPhone/iPad.
   Test system termination restores, and separately prove user force-quit
   cancels with the documented Resume state.
3. Download the same presentation with Media3 `DownloadService` on a physical
   Android phone, kill the process, relaunch, and finish.
4. Stop `plurxd`, enable airplane mode, play and seek both local packages.
5. Verify plain-HTTP LAN ATS/cleartext settings, MIME types, playlist shape,
   task restoration, and subtitle completeness.

Any Apple harness fences both entry point and referenced type under
`#if os(tvOS) && DEBUG` and never writes `SettingsStore`. A temporary media
route remains authenticated or capability-scoped; M0 may not introduce an
unauthenticated media route. Do not land a throwaway harness in Release source.

**Acceptance:** both physical devices play the complete fixture for at least
10 minutes, seek across its midpoint, show the selected subtitle, and produce
zero network requests after airplane mode. Record device/OS/build evidence in
this document. If Apple rejects the current MPEG-TS VOD, stop here and correct the
package contract before building product UI.

### 14.2 M1 — durable server contract and security boundary

Implement migration v14, `OfflinePackageStore`, DTOs, options/create/status,
lease hashing/ownership, expiry cleanup, and typed empty media routes. Add the
shared native API fixture fields. Define a complete `offline.viewing` point in
[`validation/points.toml`](../validation/points.toml), including mandatory
`paths`, declared checks, and correct provider-to-consumer `depends_on`
direction; extend existing globs so every new `crates/**` file is owned.

**Acceptance:** upgrade a v13 fixture without changing existing rows; create
and idempotently recover a package; reject cross-user status/cancel; prove the
database contains only a hash of the lease; prove wrong/expired secrets cannot
read a byte; `make validate-staged` passes so Swift/Kotlin fixture decoding is
selected too.

### 14.3 M2 — resumable portable package production

Land reviewable slices:

- **M2a:** parameterized shared production tail, `ensure_offline` outcome,
  forced SDR/audio-offset recipe, MPEG-TS wrapper, and offline subtitle timeline.
- **M2b:** priority permit, scheduled/offline queue conversion, live
  preemption, durable restart, and active-claim stale-sweep protection.
- **M2c:** separate offline budget, per-user quota, pins, activity, ETA, and
  metric plumbing.
- **Corrective cache-reader change:** protect active playback readers in a
  separate commit with its regression test; do not defer evidence.

**Acceptance:** prepare the synthetic HEVC/HDR + TrueHD case as 720p
H.264/AAC/SDR while a recommended PGS selection safely becomes Off; prepare an
explicit-burn fixture only if confirmation ships. Prepare an SRT case with a selectable
native subtitle; both playlists are VOD before `ready`; a duplicate request
encodes once; kill/restart mid-produce and resume; start live playback and see
the producer yield within the existing admission bound; cache sweep cannot
evict the leased recipe; watch state and Trakt outbox remain unchanged.

### 14.4 M3 — iOS app-owned downloads and offline playback

Implement the Apple catalog, background manager/lifecycle restoration,
Downloads tab, detail action, storage/network settings, local source branch,
offline-first bootstrap, local progress outbox, and deletion. Keep tvOS fully
fenced and unchanged.

M3 increments `CURRENT_PROJECT_VERSION` in the same commit as Apple source
changes. **Acceptance:** all iOS and tvOS Debug tests and Release builds pass;
a physical iPhone downloads in the background across suspension and system
termination, while force-quit reaches the documented Resume state;
with server stopped and airplane mode on, cold launch exposes Downloads within
2 seconds, local playback starts within 2 seconds, a midpoint seek lands within
1 second, and the selected subtitle works; deletion removes the package and
catalog bytes.

### 14.5 M4 — Android app-owned downloads and offline playback

Implement the Media3 singleton/cache/service, manifest and notification
permissions, catalog, Downloads route, detail action, cache-only player source,
offline-first launch, progress outbox, and deletion. Hide the feature on
television form factors.

M4 increments Android `versionCode` in the same commit as Android source
changes. **Acceptance:** JVM/lint and phone instrumentation pass; API 23 and current API
emulators restore download state; a physical Android phone continues across
background/process death; with server stopped and airplane mode on, the same
2-second launch/start and 1-second seek checks pass; packet/log inspection shows
no upstream attempt from offline playback; Android TV navigation and Release
build remain unchanged.

### 14.6 M5 — cross-client recovery, documentation, and release gate

Run the §15 matrix, fix the failures it finds, and update
[STATUS.html](../STATUS.html), [ROADMAP.md](ROADMAP.md),
[ARCHITECTURE.md](ARCHITECTURE.md), [CHEATSHEET.md](CHEATSHEET.md),
[VALIDATION.md](../VALIDATION.md), [README.md](../README.md),
[FEATURES.md](FEATURES.md),
[CLIENTS.md](CLIENTS.md), [PLAYBACK.md](PLAYBACK.md),
[OPERATIONS.md](OPERATIONS.md), [SECURITY.md](SECURITY.md),
[REQUIREMENTS.md](REQUIREMENTS.md), both client READMEs, and
[CHANGELOG.md](../CHANGELOG.md) in the behavior commits. Add operator guidance
for cache sizing, pinned bytes, failed preparations, and stale package cleanup.

**Acceptance:** `make release-check`, `scripts/ship --check`, the nightly
`playback-recovery` and `resource-bounds` checks, and an explicit
`PLURX_ANDROID_SERIAL` device run are green. The physical-device evidence table
is complete. A release
candidate downloads the same server-prepared package on Apple and Android,
plays both offline, syncs final progress after reconnect, and leaves no server
lease or local bytes after their respective cleanup paths.

## 15. Test strategy — prove the disconnected state, not just the button

Use the cheapest test that owns each fact, then keep a small physical-device
layer for behavior no simulator can promise:

| Tier | Owns | Target |
|---|---|---|
| Pure unit | Recipe identity, track/quality policy, state machines, size/storage math, progress merge | Every state transition and typed failure code has at least one direct case. |
| Store/API integration | Migration, idempotency, ownership, stable lease renewal, expiry, cache pins, HTTP media shapes | Every endpoint has success, unauthenticated, wrong-owner, invalid-input, and retry coverage where applicable. |
| Contract/component | Shared JSON, Swift/Kotlin catalog mapping, buttons, offline routing, accessibility | Both clients decode every representative wire state and expose no TV action. |
| Process/filesystem integration | Resume, atomic publish, eviction, cleanup, app/server death | No half-published package is observable and no completed deletion leaves owned bytes. |
| Physical end to end | Background OS transfer, storage pressure, airplane launch/playback, PiP, network transitions | One real Apple and Android pass per release candidate; §15.3 records the evidence. |

Do not chase a repository-wide line-coverage percentage. The coverage target is
100% of the package state graph, public error vocabulary, authorization
decisions, and destructive cleanup branches; framework delegate plumbing earns
one restoration integration test rather than mocks of every callback.

The current suite has three gaps this work must close: it has no durable native
download-task harness, no cache-only player assertion that fails on any
upstream access, and no server test where cache eviction races an active VOD
reader. Add those as named regression checks rather than relying on the final
manual flight simulation.

### 15.1 Server unit and integration coverage

- v13 → v14 migration, future-schema refusal, foreign keys, and expiry sweep.
- Options ladder, size estimate, recommended tracks, unavailable/unprobed file,
  and every subtitle classification.
- Transactional create idempotency/conflict/quota and ownership on every mutation.
- Recipe equality/difference for height, audio, burn track, source size/mtime,
  encoder/pipeline, and tone-map settings.
- Native text selection reuses video bytes but gates readiness on complete VTT.
- MPEG-TS VOD completeness, actual output master values, immutable typed resources,
  path/name rejection, secret hashing, expiry, and lease touch throttling.
- Same-recipe coalescing, live preemption, cancellation interest counting,
  crash recovery, failure cleanup, and no slot/permit leaks.
- Cache budget with ready/preparing/active-transfer pins and honest over-budget
  reporting.
- Download preparation and transfer do not change watch rows, watched outbox,
  Trakt state, playback registry, or Watching Now.
- Package/lease registry bounds and cleanup after client abandonment.

### 15.2 Client deterministic coverage

Both clients use the same JSON fixture cases:

| Case | Expected |
|---|---|
| New package queued | Local intent exists before POST; duplicate tap does not duplicate work. |
| Preparing progress | Monotonic server phase/percent separate from device percent. |
| Ready + lease | One platform task with stable local id, explicit selections/stream keys, and correct metadata. |
| Process death | Restored platform task maps to the same local record. |
| Completed | Local playability verified before Downloaded. |
| Missing local bytes | Catalog repairs to missing/failed; no phantom Downloaded state. |
| Wrong profile | Download hidden; bytes retained until explicit removal. |
| Offline launch | Downloads accessible without waiting for server validation. |
| Offline play | A throwing/placeholder upstream proves remote decision/session/status/progress clients are not invoked. |
| Offline progress | Newest local final state persists and retries once after reconnect. |
| Delete | Platform task/cache, artwork, metadata, and pending progress are removed. |

Apple tests must compile the iOS-only service out of tvOS and exercise both
schemes in Debug and Release. Android tests must prove the offline MediaItem has
no upstream data source and television layouts have no download affordance.

### 15.3 Physical-device matrix

| Scenario | iPhone/iPad | Android phone/tablet |
|---|---|---|
| App foreground for whole transfer | Required | Required |
| Screen locked / app backgrounded | Required | Required |
| System terminates app/process mid-transfer | Restore required | Resume required |
| User force-quits app mid-transfer | Cancel + Resume required | Persisted Resume required |
| Android six-hour service timeout | N/A | Simulated timeout required |
| Server restart during preparation | Required once | Required once |
| Live playback preempts preparation | Server-side shared check | Server-side shared check |
| Wi-Fi lost and restored | Required | Required |
| Airplane-mode cold launch | Required | Required |
| Full play, seek, subtitle, PiP | Required | Required |
| Low device storage refusal | Required | Required |
| Delete and measured space recovery | Required | Required |
| Offline progress sync after reconnect | Required | Required |

Run at least one iOS 17-class device/OS and one current iOS device, plus Android
API 23 and current API behavior (emulator is acceptable for the old API;
background/airplane evidence must include physical phones).

## 16. Decisions and trade-offs — what review should challenge

1. **App-owned HLS, never user-visible files.** This is the requested streaming
   product model and lets the OS manage background transfer. The accepted
   trade-off is that another player cannot open the download.
2. **One portable H.264/AAC/SDR output in v1.** It makes every title and both
   client families obey one contract. The cost is preparation time and no
   offline HDR/original quality.
3. **Completed cache artifact before device transfer.** Apple requires VOD and
   background retries need immutable bytes. The cost is temporary server disk
   roughly equal to the device package.
4. **A durable package row plus one hashed, renewable media lease.** Background
   media engines need headerless child fetches, and resume requires the URL to
   remain stable. The cost is migration v14 and expiry housekeeping.
5. **Live playback always outranks preparation.** Offline work is planned work;
   current viewing is latency-sensitive. The cost is that a download may pause
   for hours on a busy server.
6. **720p is the default.** It gives a useful phone/tablet picture without
   silently consuming 1080p-sized storage. High remains a saved setting.
7. **One selected audio and subtitle policy per package.** It bounds bytes and
   package complexity. Changing languages means removing/redownloading in v1.
8. **Offline progress is a durable timestamped final-state outbox.** It
   preserves the disconnected session without replaying hundreds of beats.
   Older writes are refused and server watched state remains monotone.
9. **Mobile only.** The server API is reusable, but tvOS, Android TV, and web
   cannot inherit a storage promise merely because they can display a button.
10. **No DRM claim.** OS sandboxing is honest and consistent with plurx's
    self-hosted model. Revocation after bytes leave the server is impossible.

Before enabling the feature by default, measure encode throughput for software,
hardware, HDR tone-map, and subtitle-burn source classes. Queue ETA uses those
operator-local measurements; it must not imply that 20 episodes can be encoded
before a flight by one serial producer. Track CPU temperature/throttling and
energy during the multi-hour cases.

Revisit copied HEVC/HDR packages, multiple languages, season queues, smart
downloads, and desktop/TV surfaces only after one operator can answer from
Prometheus: preparation latency and realtime factor · requested quality mix ·
package size · cancellation rate · offline-quota pressure · background failure
rate · redownloads for another language. Self-hosted plurx does not assume a
central telemetry service.

## 17. Implementation handoff — what the draft PR proves

The implementation candidate now covers the durable server package and lease,
shared producer/cache rules, timestamped progress merge, iOS and Android
catalogs/background downloaders, cache-only local playback, profile isolation,
settings, deletion, security documentation, and validation ownership described
above. The mobile build counters advance with their source changes.

Automated evidence recorded on 2026-08-05:

- `cargo test --workspace --all-targets` — passed.
- `cargo fmt --all -- --check` — passed.
- `make check` — passed, including strict Clippy, the complete Rust workspace,
  catalog/history policy checks, and documentation tests.
- iOS shared XCTest suite — 95 tests, passed; tvOS shared XCTest suite — 98
  tests, passed.
- iOS and tvOS generic Debug/Release builds — passed.
- Android `testDebugUnitTest`, `lintDebug`, and `assembleRelease` — passed.
- `scripts/validate lint` — 17 points, 20 checks, 322 audited files.
- The `offline.viewing` CI plan selects the store/API contract plus both native
  client suites in the correct provider-to-consumer direction.

The draft is deliberately not merge-ready until review reconciles these
remaining evidence gaps:

1. Physical iPhone/iPad: background transfer, system termination restoration,
   force-quit recovery, selected WebVTT, airplane-mode start and midpoint seek.
2. Physical Android phone: process death, foreground-service timeout/resume,
   airplane-mode start and midpoint seek, with an API 23 emulator compatibility
   pass.
3. Same real server-prepared package transferred to both client families, then
   removed with server lease and device bytes reconciled.
4. Operator-facing activity rows and the bounded Prometheus offline metric
   family in §13. Typed package states, quota settings, and diagnostic logs are
   present; the richer observability surface remains a named review follow-up.

Do not merge the draft PR before those findings are reviewed. Fable feedback is
expected to be reconciled on the same branch, with judgment calls recorded in
the PR and this ledger updated if the contract changes.

## 18. Platform references — primary documentation

- Apple,
  [Offline playback and storage](https://developer.apple.com/documentation/avfoundation/offline-playback-and-storage)
  and
  [Using AVFoundation to play and persist HTTP live streams](https://developer.apple.com/documentation/avfoundation/using-avfoundation-to-play-and-persist-http-live-streams)
  — `AVAssetDownloadURLSession`, background restoration, local destination
  persistence, and the VOD-only constraint.
- Android,
  [Downloading media](https://developer.android.com/media/media3/exoplayer/downloading-media)
  — `DownloadService`, `DownloadManager`, `DownloadIndex`, non-evicting cache,
  requirements, HLS track selection, and cache-backed playback.

Re-verify these APIs against the pinned Xcode 26 and Media3 1.10.1 toolchains at
M0. Platform documentation changes; the disconnected acceptance test is the
contract that does not.
