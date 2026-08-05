# Offline viewing plan review — right product, wrong package, wrong queue

**Status:** review complete · **Reviews:**
[OFFLINE-VIEWING-PLAN.md](OFFLINE-VIEWING-PLAN.md) (written against
`83be7dc481` on `codex/fix-apple-subtitle-warning`) · **Verified against:**
`origin/main` @ `2ff661da` · **Written:** 2026-08-05 · **Outcome:** review
only; no code was changed

Companion to [PLAYBACK.md](PLAYBACK.md) (how online delivery works),
[CLIENTS.md](CLIENTS.md) (the native-client boundary), and
[SECURITY.md](SECURITY.md) (who may read media). This document says what must
change in the plan before an implementing agent treats it as a handoff
contract. It does not replace the plan and it does not authorize M0.

**How it was reviewed.** Every code claim was checked against a pinned
`origin/main` (`2ff661da`) extracted with `git archive` — never the working
tree, never local `main`. Seven scoped passes (server production, persistence
and API, media package, Apple client, Android client, process/validation,
design adversary) produced findings; each finding was then re-verified by a
second pass whose default posture was *refuted*. Platform claims are cited to
Apple and Android documentation, not to memory. Findings that survived
verification with a corrected mechanism are marked as such.

**Severity legend:** **BLOCKING** = the plan as written specifies something
the code or platform will not do, and building it would produce a wrong
contract or a failed acceptance check · **SHOULD-FIX** = real cost, wrong
estimate, or a contract that permits unsafe behavior · **NIT** = polish.
Findings cite `file:line` at `2ff661da`.

## 1. Verdict — accept the product model, revise four contracts before M0

The product model is right and should not be relitigated. App-owned opaque
storage, one portable output, prepare-then-transfer with two named progress
domains, a hashed expiring capability for headerless child fetches, live
playback outranking preparation, a final-state progress outbox, and an honest
"no DRM" statement are all correct choices for this codebase. §16's decision
list is the strongest part of the document.

The plan is not buildable as written. Four contracts describe things that do
not exist at `2ff661da`, and two more describe platform behavior that Apple
and Google document differently:

| Contract | Verdict | Why |
|---|---|---|
| §3.2 · §4.4 · §5.4 · §7.4 media package | **Rewrite** | The transcode path emits MPEG-TS, not fMP4. There is no `init.mp4` and no `.m4s`. |
| §3.2 · §7.1 cache reuse | **Rewrite the premise** | The pre-transcode cache only covers 4K/HDR/HEVC/AV1 sources at default tracks and zeroed A/V offset. Most packages are a fresh encode. |
| §4.3 · §7.2 queue and priority | **Rewrite** | The "shared one-producer permit" is a 6-hour skip-if-busy flag with no yield. As written the plan *guarantees* the inversion it claims to prevent. |
| §5.4 · §5.5 · §12 lease lifecycle | **Rewrite** | Rotating the secret changes every media URL, which is a from-zero re-download on both platforms — and past day 7 it is a from-zero re-encode. |
| §8 Apple client | **Correct and expand** | Force-quit cancels downloads; automatic media selection may skip the chosen subtitle; a backgrounded app cannot start the transfer when preparation finishes. |
| §9 Android client | **Correct and expand** | `dataSync` is capped at 6 h/24 h at this targetSdk, cannot start from boot, and the described cache-only player still reaches the network. |
| §5 · §6 persistence and API | **Amend** | Migration v14 and the subtrait pattern are right; the cascade, the error vocabulary, and the route shapes are not. |
| §10 progress outbox | **Amend** | Sound design, but `watched` is monotone server-side and the wire type carries neither `watched` nor `recorded_at`. |
| §14 · §15 milestones and tests | **Amend** | The validation point as specified fails lint, and the store build-number bump cannot happen at M5. |
| §2 · §11 · §16 product and security | **Keep** | The user contract, the security boundary, and the trade-off record are the parts worth defending. |

Recommended disposition:

> **Product model approved · package, queue, lease, and both client contracts
> require revision · M0 authorized only after §2's B1, B3 and B11 are
> resolved on paper.**

M0 is the right first milestone and its instinct — prove the platform
downloaders against real plurx bytes before any schema — is correct. But M0
as written would prove the wrong fixture: it names files the server does not
produce.

## 2. Blocking findings

### B1 — The offline package format does not exist (§3.2, §4.4, §5.4, §7.4)

The plan specifies "finished fMP4 HLS", `init.mp4`, `{segment}.m4s`, and
`#EXT-X-VERSION:7`. The transcode path emits MPEG-TS:

```text
crates/plurx-core/src/transcode/mod.rs:968   "-hls_segment_type", "mpegts"
crates/plurx-core/src/transcode/mod.rs:971   "{out_dir}/seg%05d.ts"
crates/plurxd/src/produce.rs:126             format!("seg{:05}.ts", …)
crates/plurxd/src/produce.rs:143-153         #EXT-X-VERSION:3 … #EXT-X-ENDLIST
```

fMP4 exists only on the **copy** path (`transcode/mod.rs:1395-1400`), which
§3.2 explicitly excludes. The muxer is part of recipe identity
(`crates/plurx-core/src/transcode/recipe.rs:71`, `field(h, "muxer",
b"hls/mpegts")`), so switching it for offline would put offline packages in a
different cache namespace from playback — deleting §4.2's stated reason for
the whole design — and bumping `CACHE_FORMAT_VERSION` (`recipe.rs:95`)
invalidates every existing cache entry on the fleet.

Both platforms accept TS VOD, so this is a specification defect, not a
platform blocker. Every route, readiness condition and M0 fixture in the plan
is written against filenames the server will never produce.

**Correction.** Specify v1 as MPEG-TS VOD: `index.m3u8` + `segNNNNN.ts`,
`#EXT-X-VERSION:3`, no `#EXT-X-MAP`, no `.m4s` routes. If fMP4 is wanted, make
"switch the transcode muxer" its own milestone with the cache-invalidation
cost stated, and do not fold it into an offline feature.

### B2 — The cache-reuse premise is false for most titles (§3.2, §7.1)

§3.2 says the package "reuses the pre-transcode cache's content-addressed HLS
rather than introducing a second media pipeline". The cache is a genuine VOD —
publication happens only at `PartEnd::Finished` (`transcode.rs:2657`), and
pruning is confined to live sessions, which cached sessions skip entirely
(`transcode.rs:4025-4027`). That part of the premise survives. Hit rate does
not:

- The scheduled producer only runs for 4K, HDR, or HEVC/AV1 sources
  (`produce.rs:226-232`, gated at `state.rs:1335`), and a test pins the
  exclusion: `assert!(!worth_producing(&media(1080, "h264", None, true)))`
  (`produce.rs:508`). A 1080p H.264 SDR film has no cache entry at all.
- `produce` clones the file and forces `playback_file.audio_offset_ms = 0`
  (`transcode.rs:2378-2380`) and selects tracks with `select_tracks(file,
  None, None)` (`:2389`) — defaults only.
- Recipe identity includes `aindex`, `burn`, `height` and `audio_offset`
  (`recipe.rs:101-139`), so any explicit audio choice, any burn, or a rung the
  scheduler did not produce is a miss.

There is also a trap hiding in the reuse: if `ensure_offline` zeroes
`audio_offset_ms` to share the cache, every offline package ships without the
operator's per-file A/V correction — and it cannot be fixed on a disconnected
device.

**Correction.** State that reuse applies only to default-track, no-burn
packages whose rung matches an already-produced entry, and that the ordinary
case is a fresh encode. Decide explicitly, in the plan, whether offline
honours `audio_offset_ms`; if it does, say that those packages never share the
playback cache.

### B3 — The queue as specified guarantees the inversion it forbids (§4.3, §7.2)

The plan says both background paths "share the same one-producer permit", with
priority live · offline · speculative. The permit is an `AtomicBool`:

```text
crates/plurxd/src/state.rs:1243  if self.producing.swap(true, Ordering::Relaxed) { … return; }
crates/plurxd/src/state.rs:427   PRODUCE_MAX_PER_PASS: usize = 12
crates/plurxd/src/state.rs:433   PRODUCE_WINDOW = 6 * 3600 s
```

The holder runs up to six hours across twelve candidates. There is no yield to
a higher-priority waiter; the only escape is the operator flag
`stop_producing` (`state.rs:539`), checked between candidates
(`state.rs:1328`). Sharing today's permit means a user's tap can wait six
hours behind speculative work.

Preemption is also not what §4.3 describes. On `live_is_waiting()` the
producer kills the child immediately, mid-segment
(`transcode.rs:2707-2712`); the incomplete trailing `#EXTINF` is dropped on
read (`produce.rs:41-46`). Resume is from the last *published* boundary, not
"at a published segment boundary". Two further limits go unmentioned:
`PRODUCER_MAX_PARTS = 64` (`transcode.rs:1763`) caps preemptions per run, and
on `PartEnd::Failed` the caller deletes staging **and** forgets the claim
(`transcode.rs:2485-2491`) — an `encoder_failed` retry restarts from zero,
contradicting §7.2's "staged work remains resumable".

**Correction.** Make the priority-aware permit an explicit deliverable of M2
(replace the `AtomicBool`, or have `OfflineManager` raise `stop_producing` and
await the guard). Reword §4.3 to "resumes from the last published segment
boundary", and document the 64-part cap and the failure-path reset in §12.

### B4 — A parked package loses its staged work after 24 hours (§7.3, §12)

§7.3's pin only covers the budget step of the sweep. Step 1 forgets claims
older than `STALE_CLAIM_SECS = 24 * 3600` (`cachekeep.rs:41`, `:106-119`), and
the orphan pass then deletes any staging directory whose recipe nothing claims
(`cachekeep.rs:331-336`, "removed staging for a recipe nothing claims").
`touch_cache_claim` has exactly one caller — the end of a produce attempt
(`transcode.rs:2482` → `:2528`) — so a package sitting in the queue never
refreshes it.

Combined with §16.5's accepted "a download may pause for hours on a busy
server", §12's "Resume from staged segment boundary" is false past one day,
silently.

**Correction.** Exclude claims referenced by a nonterminal package row from
the stale-claim step, or touch the claim on every queue pass. Say which.

### B5 — A library rescan deletes package rows mid-transfer (§6, §12)

`offline_packages.file_id … ON DELETE CASCADE` meets
`DELETE FROM files WHERE id = ?1` (`store/sqlite/media.rs:1119`), with foreign
keys enforced at runtime (`store/sqlite/mod.rs:623`). A rescan of an unmounted
share drops the package row and cascades its leases. §12's "Source disappears
→ Failed · Media unavailable on server" becomes unreachable: the client gets a
404 mid-transfer instead of a typed `source_unavailable`, and the pinned cache
bytes lose the row that would have driven their cleanup.

**Correction.** Do not cascade from `files`. Keep the row (nullable `file_id`,
or no FK) and transition to the typed failure explicitly.

### B6 — Rotating a lease is a full re-download, and after day 7 a full re-encode (§5.4, §5.5, §12)

The secret is in the path, so a new lease changes the URL of the master, the
media playlist, and *every segment*.

- **Apple.** The asset is bound at `AVAssetDownloadConfiguration(asset:title:)`
  and `AVAssetDownloadTask.urlAsset` is read-only; the task inherits
  `URLSessionTask`, not `URLSessionDownloadTask`, so there is no resume-data
  path. The only documented reuse points at the *local* bundle ("to augment an
  existing download, initialize a new task with an `AVURLAsset` whose URL
  references the downloaded asset on disk").
- **Android.** Cache keys come from `CacheKeyFactory.DEFAULT` —
  `dataSpec.key != null ? dataSpec.key : dataSpec.uri.toString()` — and
  `HlsDownloader`/`SegmentDownloader` build every `DataSpec` with URI only.
  `DownloadRequest.customCacheKey` is rejected for HLS
  (`checkArgument(customCacheKey == null …)`).

So §12's "keep any platform-safe partial cache" is unachievable on both
platforms. Worse, the seven-day absolute TTL composes badly with §7.3: on day
8 the lease expires, the pin releases, the sweep evicts the recipe, and the
retry is a multi-hour re-encode — which §12 presents as "mint a new lease and
restart transfer".

**Correction.** Make media paths stable and content-addressed
(`/offline/media/{recipe_hash}/…`) and make the lease *renewable in place*
(`POST …/leases/{id}/renew` extends `expires_at`; never a second secret for
the same package). Renew on every client status poll. On Android, install a
`CacheKeyFactory` that strips the volatile component and use the identical
factory for download and playback. Add a typed `package_evicted` error whose
UI copy says the title must be prepared again.

### B7 — "App is killed → no state loss" is wrong on iOS (§12, §14.1, §15.3)

Apple: "If the system terminates your app while downloads are in progress, it
relaunches the app and calls
`application(_:handleEventsForBackgroundURLSession:completionHandler:)`. **If a
person force-quits your app, the system cancels all active downloads and
doesn't relaunch the app.**"

M0 step 2 ("suspend and kill the app mid-transfer, relaunch, and finish") and
the §12 recovery row therefore describe behavior iOS does not provide for the
common case — the user swiping the app away. There is also no
`UIApplicationDelegateAdaptor` anywhere under `clients/apple` (zero hits), so
the background-session completion handler has no delivery point today.

**Correction.** Split the §12 row into system-terminated (restores) and user
force-quit (task cancelled, partial bytes discarded, Retry required). Test both
in M0. Add the delegate adaptor to §8.1's new owners, fenced iOS-only.

### B8 — The chosen subtitle may not be downloaded at all (§8.3, §14.4)

`AVAssetDownloadContentConfiguration.mediaSelections`: "If your configuration
doesn't indicate a media selection, the system uses the asset's **automatic**
media selection" — which follows the device's region and localization
settings, not the server's choice. §8 never mentions media selections, so on
any device whose system preferences differ from the package's languages, the
subtitle rendition is simply not fetched. §14.4's "the selected subtitle works"
would fail on real devices and pass on the developer's.

The completion check is also wrong. `AVURLAsset.isPlayable` means only "you can
build an `AVPlayerItem`"; the offline gate is `AVAssetCache.isPlayableOffline`,
and Apple states plainly that `true` there "doesn't indicate that all of the
asset's associated media selection options are available for offline playback.
Instead, call `mediaSelectionOptions(in:)`."

**Correction.** §8.3 sets `primaryContentConfiguration.mediaSelections`
explicitly from the package's audio and subtitle options, and gates
`Downloaded` on `isPlayableOffline` **and** a non-empty
`mediaSelectionOptions(in: subtitleGroup)`.

### B9 — "Tap Download and close the app" does nothing on iOS (§5.3, §8.3)

There is no push or background-launch story anywhere in the plan. Apple: "You
can only start a non-discretionary download task while your app is in the
foreground", and for transfers started while backgrounded "the system always
starts transfers at its discretion". A force-quit app is not relaunched at
all. Background pushes are throttled and explicitly not guaranteed;
`BGProcessingTask` "runs only when the device is idle".

The honest end-to-end for the plan's own example is: tap → server encodes for
90 minutes → **nothing happens** until the user reopens Cinema → the transfer
then starts. That is a materially different product from the one §2.1
describes, and a traveller who taps Download at the gate and pockets the phone
lands with nothing.

**Correction.** Either (a) create the platform task at tap time against a URL
that becomes valid when preparation completes — which requires the lease to
exist before `ready`, a real design change — or (b) state the contract plainly
in §2 and in the UI ("we'll finish this the next time you open Cinema"), and
add a best-effort catch-up path (silent push / `BGProcessingTask` on iOS,
`WorkManager` on Android). Do not ship the promise without one of these.

### B10 — Android's foreground service cannot legally carry the download (§9.1, §14.5)

The app is `targetSdk = 37` (`clients/android/app/build.gradle.kts:15`), so
every Android 15+ foreground-service restriction applies:

- `dataSync` is capped at **6 hours per 24-hour period**, app-wide, and
  `Service.onTimeout()` fires. Media3 1.10.1's `DownloadService` already
  overrides `onTimeout()` with `stopSelf()`, so a long transfer does not crash
  — it silently stops and waits for the `Scheduler`.
- A `BOOT_COMPLETED` receiver **may not start** a `dataSync` foreground
  service; it throws `ForegroundServiceStartNotAllowedException`. §9.1's
  "Media3 restart intent" boot path is illegal as written.
- `mediaProcessing` carries the same cap. Google's current recommendation for
  a user-triggered download with visible progress is a user-initiated data
  transfer job (`JobInfo.setUserInitiated(true)` + `RUN_USER_INITIATED_JOBS`,
  API 34+), and **Media3 1.10.1 has no UIDT support** — zero occurrences of
  `setUserInitiated` in `androidx/media` at that tag.

**Correction.** §12 gains a "paused by the system — tap Resume" row. §9.1
states the 6 h ceiling per foreground visit, implements `onTimeout` + resumable
restart, drops the boot-start claim, and records the UIDT decision (custom
`Scheduler`/`JobService`, or accept the ceiling) as an explicit trade-off.

### B11 — The cache-only player as specified still reaches the network (§9.3)

`setCacheWriteDataSinkFactory(null)` makes the cache read-only; a miss still
goes upstream. `FLAG_BLOCK_ON_CACHE` is about locked cache keys, not misses.
As written, an airplane-mode test would pass on Wi-Fi too — precisely the
failure §9.3 says it prevents.

**Correction.** Leave the upstream factory **unset**. `CacheDataSource` then
uses `PlaceholderDataSource.INSTANCE`, whose `open()` throws — this is what
Media3 itself does in `createDataSourceForRemovingDownload()`. Make the §15.2
"Offline play" case assert against that throwing factory rather than "packet
inspection".

### B12 — The profile key does not exist on either client (§2.4, §8.2, §9.2)

Every local record is to be scoped by `(server_instance_id, user_id,
file_id)`. Neither client persists a user id:

```text
clients/apple/Sources/SettingsStore.swift:18-31   origin · instanceId · token · username
clients/android/.../data/SettingsStore.kt:34-46   ORIGIN · INSTANCE_ID · TOKEN · USERNAME
```

`instanceId` is nullable and backfilled only when the server is reachable
(`AppModel.swift:372-377`, `AppViewModel.kt:169`), so an install that has
never reached a current server has no instance id — exactly the first-launch
offline case §15.2's "Wrong profile" fixture is supposed to test.

**Correction.** Persist `user.id` at login/`me()` on both clients, require a
non-nil `server_instance_id` before any download is queued, and define the
behavior when it is null.

## 3. Server contract corrections

**S1 · `ensure_offline`'s signature cannot express today's outcomes (§7.1).**
`produce` returns `Result<Option<Produced>, String>` and `Ok(None)` covers
already-cached, claimed-elsewhere, no-cache-configured, deadline-reached and
preempted-too-often (`transcode.rs:2415`, `:2455`, `:2470`, `:2563`, `:2671`).
`Result<Produced, OfflineProduceError>` with no deadline or cancellation
argument forces a fake error for "yielded, resume later". Return a three-way
outcome and take a deadline/cancel token.

**S2 · the shared-primitive refactor is larger than implied (§7.1).**
`options_for` (`transcode.rs:2144`) and `produce_into` (`:2535`) are private,
and `produce`'s head is offline-hostile (B2). The shareable part is the tail —
digest → `Recipe::hash` → `cache_hit` → `ensure_text_subtitle` →
`staging_dir` → `claim_cache_entry` → `produce_into` → rename →
`complete_cache_entry` (`:2402-2517`). Say "parameterize track selection and
hoist the tail", plus the visibility change.

**S3 · `actual_bytes` undercounts, and duration has no accessor (§4.4, §6).**
`Produced.bytes` sums segments plus the playlist only
(`transcode.rs:1869-1884`); subtitle VTTs live in a separate cache
(`:2221`) and the offline master does not exist yet. `CachedTranscode` has no
duration column (`domain.rs:498-509`; `store/sqlite/mod.rs:348-359`), and
`Published.duration_ms` is discarded. Compute `actual_bytes` at publication as
segments + playlist + master + VTT renditions, and state that readiness
re-parses `#EXTINF` from the cached playlist (or add the column in v14).

**S4 · `CODECS="avc1.640034"` is an unsafe hardcode (§7.4).** Only
`Encoder::Software` pins a profile (`encoder.rs:153-177`); nvenc, VideoToolbox,
VAAPI and QSV set neither `-profile:v` nor `-level`. Today's master
deliberately omits `CODECS` for H.264 and a test pins that
(`http/hls.rs:1670`, `assert!(!master.contains("CODECS="))`). Asserting
High@L5.2 risks AVFoundation rejecting the variant before a byte transfers,
and on the API 23 devices §14.5 promises to test, `MediaCodec` capability
filtering can reject the only track. Derive `CODECS` from the produced
bitstream, or pin `-profile:v high -level 4.0` across encoders first.

**S5 · `estimated_bytes` is built on the average, not the ceiling (§5.1).**
`Rung` exposes both `total_kbps` and `peak_kbps = video*3/2 + audio`
(`transcode.rs:4477-4494`), and rate control is `-b:v` with `maxrate 1.5×`. A
5% headroom on an average under-reserves against the 507 threshold. Say the
estimate is an average-rate estimate, and note it excludes the 10% platform
package overhead the client later demands (§12).

**S6 · the error vocabulary does not exist (§5.2, §5.3, §11).** `ApiError`
(`http/error.rs:11-25`) has no 507 and no 429, and serializes
`{"error": "<string>"}`, not `{code, message}`. `Forbidden` is the fixed string
`"admin privileges required"` (`error.rs:33`) — a cross-user 403 would say
that. Budget the `ApiError` extension as work; for ownership follow the house
pattern instead (pass `user.id` into the lookup and 404 on miss, as
`stream.rs:1148-1157` does).

**S7 · route shapes should follow the house pattern (§5.4).** No route in
`crates/` uses a parameter followed by a literal suffix; the existing subtitle
route takes a whole segment and validates the suffix in the handler
(`http/mod.rs:131-133`, `stream.rs:837-841`). Specify `{segment}` whole-segment
params with handler-side validation — which is also where §11's "typed names
only" check belongs — rather than `{segment}.m4s`, and re-verify against axum
0.8 at build time.

**S8 · the per-user row cap races (§7.2).** Each store call re-acquires the
connection mutex independently (`store/sqlite/mod.rs:741-755`), so a
count-then-insert pair admits unbounded rows under a tap loop. Specify cap
check + idempotent lookup + insert as one store operation in one transaction.
Note also that the cap counts nonfailed rows, so a permanently failing source
allows unbounded `failed` rows.

**S9 · trace redaction is right but under-scoped (§11).** `TraceLayer` is
installed with no `make_span_with` (`http/mod.rs:201`), so the hook exists and
is free. But capability secrets already reach the admin-readable log ring by
hand — `session_id = %session` at `hls.rs:470`, `:556`, `:579`, `:1429`, where
the session id *is* the credential (`hls.rs:5-11`) — and bearer tokens are
accepted in query strings (`extract.rs:110-114`). Make the redaction a
router-wide span builder that strips `token=` and both capability path shapes.

**S10 · metrics have no machinery to hang on (§13).** `/metrics` is an
unauthenticated hand-formatted string (`http/mod.rs:197`, `system.rs:1392+`)
with no counter or histogram type, so `plurx_offline_prepare_seconds` needs
new plumbing the plan does not budget. The bounded-label rule is correct and
load-bearing precisely because the endpoint is unauthenticated. Also: §13
emits `result="cancelled"` but §5.3's state machine has no cancelled state.

**S11 · the outbox cannot say what §10 claims (§10).** The wire type is
`ProgressRequest { position_ms, duration_ms }` (`http/watch.rs:14-19`) — no
`watched`, no `recorded_at` — and the store never un-watches
(`store/sqlite/watch.rs:136-147`, `watched = watch_state.watched OR
excluded.watched`). Replaying a download from zero, which §8.4 explicitly
permits, syncs the lower position but leaves the item finished. Separately,
"the existing server-side watch/Trakt path runs once" overstates Trakt: with no
session for `(user_id, item_id)` the scrobble path returns early
(`trakt.rs:206-208`), so only the monarr watched outbox fires. Say both
plainly.

**S12 · add `recorded_at` and refuse older writes (§10).** Last-writer-wins
clobbers a newer position set by another device while the phone was offline
(watch on the flight at T0, another device at T+3 h, phone reconnects at
T+5 h). §16.8 defers cross-device conflict resolution, but ignoring an update
older than the server's `updated_at` costs one field and one comparison.

**S13 · never auto-burn from the one-tap default (§5.1, §3.2).**
`select_tracks` returns *forced-only* when audio language matches the subtitle
preference (`plurx-core/src/tracks.rs:277-280`, test at `:354`), and
`forced_or_default` never inspects the codec (`:219-228`). On a typical disc
rip that forced track is PGS — so `recommended_subtitle_index` →
`offline_mode: "burned"` → an irreversible burn with no Off, from a button
whose premise is "one tap, no decisions". Today burn is far narrower: it
happens only through the prefer-original path (`transcode.rs:2107-2121`).
Either fall back to `none` for a bitmap recommendation or require explicit
confirmation, and allow forced + full as the one documented two-track
exception (the one-track contract otherwise loses forced-subtitle behavior
that online playback has).

**S14 · "SDR always" contradicts "the current tone-map preference" (§3.2,
§7.1).** `PLURX_TONEMAP=off|none|passthrough` yields `ToneMap::None`
(`transcode.rs:969-975`), gating the whole tone-map chain
(`plurx-core/src/transcode/mod.rs:584`). On such a node an HDR source produces
a PQ-graded H.264 package that §7.4 ships with no `VIDEO-RANGE` and §3.2 calls
SDR. Offline must force the tone-map regardless of the node preference.
(Forcing SDR conflicts with nothing else: `delivered_dynamic_range` already
returns `"sdr"` for every transcode, `playback/mod.rs:338-340`.)

**S15 · the offline subtitle route needs its own timeline (§4.4, §7.4).**
`ensure_vtt` exists (`subtitles.rs:220`) and `subtitle_media_playlist` mirrors
the video playlist's `PLAYLIST-TYPE` and `ENDLIST` (`http/hls.rs:1279-1299`),
so the mirroring claim holds. But `subtitle_vtt` resolves through
`session_file`/`segment_window` and slices against
`context.media_origin_seconds` (`hls.rs:546-609`) — all live-session state.
The plan should say the offline route needs its own timeline source, not
"reuse the helper". The "temporary empty WebVTT" it warns about is real
(`hls.rs:583-585`).

**S16 · node affinity is assumed and never stated (§4.3, §6).** Packages are
`node_id`-scoped and `manifest_url` is a relative path with no node identity.
A child request routed to another node hashes the lease fine and then has no
local cache directory — a 404 mid-download, which per B6 is unrecoverable on
iOS. The same hazard applies to LAN-versus-remote base URLs when the user
walks out of the house mid-transfer. Return an absolute, node-pinned URL, or
declare sticky routing a hard precondition and refuse package creation
otherwise.

**S17 · the playback-cache race the plan mentions is real (§7.3).**
`serve_cached` touches `last_used_at` exactly once, at session start
(`transcode.rs:2282` — the only non-test caller), while the budget step evicts
complete entries coldest-first with no reader check (`cachekeep.rs:130-141`),
deleting the directory before the row (`:199`). A viewer three hours into a
film can lose the directory underneath them. Good catch by the plan — but per
§8 below it is a corrective change with its own regression obligation, so it
should not ride along inside M2.

## 4. Apple client corrections

**A1 · the launch change is a state-machine rewrite, not a tab (§8.5).**
`RootView` is a five-way switch on `model.phase` and reaches `HomeView` — which
owns the iOS `TabView` — only in `.ready` (`PlurxApp.swift:27-43`,
`HomeView.swift:26-50`). Phase is set at the *end* of `bootstrap()`
(`AppModel.swift:84-115`), which awaits `me()` on a session with
`waitsForConnectivity = true` and 30-second timeouts (`PlurxAPI.swift:30-36`),
then runs Bonjour rediscovery of up to 3 s browse plus 5 s resolve per
candidate plus a `serverInfo()` each (`ServerDiscovery.swift:98,170`). Nothing
can render during `.loading`. Specify a new phase published *before*
bootstrap's first await, hoist the tab shell out of `.ready`, and bound the
reconnect path.

**A2 · a nil decision breaks seek, markers and subtitles (§8.4).** `issueSeek`
reopens the stream unless `usesDirectTimeline || isVOD`
(`PlayerController.swift:480`), and `reopen` early-returns on
`guard let decision, started` (`:690`) — so an offline branch with no decision
turns every seek into a silent no-op, failing §14.4's one-second seek check.
`activeMarker` reads `decision?.markers` (`:344-345`) and `selectNativeSubtitle`
maps decision-derived ordinals (`:1837-1866`). §8.2's `OfflineItem` has no
markers field. The offline branch must synthesize a decision-shaped snapshot
(tracks, markers, source) and set `isVOD`/`usesDirectTimeline`.

**A3 · persist the relative path, not a bookmark (§8.2).** Apple's documented
pattern is `UserDefaults.standard.set(location.relativePath, forKey:)`
restored against `NSHomeDirectory()`, because the container path changes across
launches and updates; no Apple document recommends bookmark `Data` for HLS
downloads. "Don't move the downloaded asset" is documented and the plan is
right about it. Replace the bookmark with the relative path (or keep both, with
the path as the fallback).

**A4 · network policy is per session, not per task (§8.3).**
`allowsCellularAccess`/`allowsExpensiveNetworkAccess` are configuration
properties copied at session creation; "once configured, the session object
ignores any changes". A Wi-Fi-only toggle therefore needs two background
sessions with distinct identifiers, or invalidate-and-recreate. Say which.

**A5 · delegate availability at deployment target 17 (§8.3, §17).**
`AVAssetDownloadConfiguration` and `makeAssetDownloadTask(downloadConfiguration:)`
are iOS 15+, so "where available" should be "unconditionally".
`urlSession(_:assetDownloadTask:willDownloadTo:)` is **iOS 18+**, and
`didFinishDownloadingTo` is deprecated at iOS 27 — an iOS 17 target must
implement the deprecated callback and add the new one under
`if #available(iOS 18, *)`. Also worth planning for: on iOS 26+ a
non-discretionary asset download surfaces a Live Activity the user can cancel,
failing the task with `NSUserCancelledError`.

**A6 · use the eviction lever the plan describes but never pulls (§2.3,
§11.3).** `AVAssetDownloadStorageManager` (iOS 11+) sets per-asset priority and
expiry; `.important` is a relative ordering hint, not a guarantee, and Apple
documents that "in extreme low-disk-space conditions, the operating system may
automatically delete downloaded assets". Set it for user-requested downloads
and keep §2.3's honest detection path.

**A7 · the tab list is test-pinned (§8.5).**
`HomeLayoutPolicy.topLevelTabs` is asserted equal to
`["Home","Libraries","Search","Settings"]` by a tvOS test
(`HomeView.swift:216`, `Tests/AppleClientTests.swift:2583-2586`). Adding
Downloads needs a platform-conditional value or a test update; say which.

**A8 · "the four Apple build/test configurations remain the gate" is false
(§8.1, §14.4).** `make apple-test` runs exactly two commands — iOS and tvOS
Debug `test` (`Makefile:265-274`) — and that is the only Apple check in
validation (`validation/points.toml:101-109`). The four-configuration rule is a
README convention (`clients/apple/README.md:206-218`), unenforced. Either add
the Release builds to `apple-test` in M3 or stop calling them the gate.

**A9 · the outbox needs a throwing progress path (§10).** The plan's claim that
clients drop failed progress is correct (`AppModel.swift:778-780`, `try?`), but
`reportProgress` returns `Void` and swallows the error, so "clear only on 2xx;
401/403 waits" cannot be implemented through it. `PlurxAPI.progress` does throw
(`PlurxAPI.swift:88-93`); the outbox must call it directly or `AppModel` gains
a throwing variant.

## 5. Android client corrections

**N1 · `currentFormFactor().television` does not exist (§9.4).** The API is
`enum class FormFactor { Compact, Expanded, Television }` with
`currentFormFactor(): FormFactor` (`ui/Layout.kt:10-17`); the correct spelling
is `currentFormFactor() == FormFactor.Television`, as used at
`ui/AuthScreens.kt:309`. It is also `@Composable`/`LocalConfiguration`-bound,
so it cannot gate a service, a manifest entry, or anything outside
composition — non-UI code uses `isTelevision(context)`
(`player/Controller.kt:717`).

**N2 · "Android TV: No" is a UI hide, not a fence (§3.1).** One module
(`settings.gradle.kts:26`), one activity, one manifest carrying both `LAUNCHER`
and `LEANBACK_LAUNCHER` (`AndroidManifest.xml:47-51`). The download service,
the foreground-service permissions, the boot receiver and the cache all ship to
television. State that, and add an instrumentation assertion that the service
is never started when `isTelevision` is true.

**N3 · the dependency list is partly a no-op, partly a hazard (§9.1).**
`media3-database` and `media3-datasource` are already compile-scope transitive
deps of `media3-exoplayer`, so adding them changes nothing. Conversely
`media3-exoplayer-workmanager:1.10.1` pulls `androidx.work:work-runtime:2.8.1`,
whose `SystemForegroundService` declares no `android:foregroundServiceType` —
in every WorkManager version, by design, because the app must merge the type
in. At this targetSdk that is a live crash surface. Prefer `PlatformScheduler`,
which ships inside `media3-exoplayer` and needs only
`RECEIVE_BOOT_COMPLETED` plus a `BIND_JOB_SERVICE` service registration.

**N4 · empty `streamKeys` downloads everything the master advertises (§9.2).**
`DownloadRequest.streamKeys`: "If empty, all streams will be downloaded";
`HlsDownloader.getSegments()` enumerates every `mediaPlaylistUrls`. That is
survivable only because §7.4's master carries one variant — the moment it
carries two, the device silently downloads bytes §3.3 forbids. Write explicit
stream keys from the package manifest and assert them in the §15.2 "Ready +
lease" fixture. (`Download.request.toMediaItem()` is correct and preserves
stream keys.)

**N5 · say where the cache lives (§9.1).** Use `getFilesDir()`, not
`getCacheDir()` — the latter is OS-reclaimable, which would silently destroy
downloads and make §2.3's "OS reclaims managed storage" an Android reality
rather than an iOS one. Only one `SimpleCache` instance may hold a directory at
a time (`IllegalStateException: Another SimpleCache instance uses the folder`),
so the singleton must be process-wide and shared with playback — which §9.3
does correctly; say why.

**N6 · the player coupling is deeper than §9.3 implies.** `PlayerContent` takes
the concrete `Plan` (`player/PlayerScreen.kt:465`), not the `PlanLike`
interface (`Controller.kt:106`); `Controller`'s constructor takes the view
model and calls `vm.endHlsSession`/`vm.createHlsSession`/`Session.url(...)`
(`:403`, `:509`, `:515`, `:543`, `:640`, `:651`); the PGS overlay controller is
an unconditional property initializer over `vm.api()`, which throws when not
connected (`:275-276`, `AppViewModel.kt:113`); every subtitle and seek route is
derived from `planMode` (`:192`, `:226-240`, `:478-485`); and `buildPlayer`
hardwires `DefaultMediaSourceFactory(Net.dataSourceFactory())` (`:729`, `:738`).
Name the four real edits: re-type `PlayerContent` to `PlanLike`, add an offline
`planMode`, parameterize `buildPlayer`'s data-source factory, make the PGS
controller optional.

**N7 · `DownloadIndex` bytes are not byte-accurate for HLS (§9.2).** It
persists `bytes_downloaded` and survives process death, but for HLS
`contentLength` is typically `LENGTH_UNSET`, so `getPercentDownloaded()` falls
back to segment counts. Drive the UI from segment progress, or say the byte
figure is approximate.

**N8 · notification denial is benign; say so (§9.1).** The foreground service
still starts and runs without `POST_NOTIFICATIONS`; the notification is hidden
from the drawer but visible in the task manager. The plan's "a hidden
notification is not permission to let the process die silently" is the right
instinct with the wrong mechanism.

**N9 · the launch fix is necessary and sufficient in shape (§9.4).**
`LAUNCH_VALIDATION_TIMEOUT_MS = 12_000` is real (`ui/AppViewModel.kt:561`,
used at `:150`) and `MainNav` renders only in `Phase.Ready`
(`MainActivity.kt:62-67`), so entering `Ready` early is the right change —
unlike Apple, this one is small.

**N10 · the outbox must not reuse `postProgress` (§10).** `postProgress`
launches on `viewModelScope` and `reportProgress` swallows every failure
(`AppViewModel.kt:480-496`) — accurate as the plan describes, but
`viewModelScope` dies with the process, so the pending row must be written to
DataStore synchronously at teardown rather than enqueued through that path.

## 6. Platform facts the plan states incorrectly

| Plan claim | Verdict | Reality |
|---|---|---|
| §12 "App is killed → no state loss" | **Wrong** | User force-quit cancels all active downloads and the app is not relaunched. |
| §8.3 `AVURLAsset.isPlayable` as the completion check | **Wrong** | Use `AVAssetCache.isPlayableOffline` **plus** `mediaSelectionOptions(in:)`. |
| §8.3 subtitle downloads with the package | **Wrong by omission** | Without `mediaSelections`, automatic selection follows device settings. |
| §8.2 bookmark `Data` as the local identity | **Wrong** | Apple documents the path relative to `NSHomeDirectory()`. |
| §8.3 Wi-Fi policy applies "to newly created tasks" | **Wrong** | Session configuration is copied at creation; use two sessions. |
| §8 "use `AVAssetDownloadConfiguration` where available" | **Understated** | iOS 15+, i.e. unconditional at target 17; `willDownloadTo` is iOS 18+. |
| §12 "keep any platform-safe partial cache" on lease rotation | **Impossible** | New URL = new asset (Apple) and new cache keys (Android). |
| §9.1 boot restart intent | **Illegal** | `BOOT_COMPLETED` cannot start a `dataSync` FGS at this targetSdk. |
| §9.1 `dataSync` for a multi-GB transfer | **Capped** | 6 h per 24 h, app-wide; Media3 1.10.1 has no UIDT path. |
| §9.3 "no upstream fallback" via read-only cache | **Wrong mechanism** | Leave upstream unset so `PlaceholderDataSource` throws. |
| §9.2 empty `streamKeys` | **Risky, not safe** | Empty means *all* renditions. |
| §14.5 API 23 acceptance | **Feasible** | Media3 1.10.1's minSdk is 23; the app's floor matches. |
| §4 "Apple only persists finished VOD" | **Correct** | Saving a live stream throws. |
| §9.1 `NoOpCacheEvictor` + shared cache | **Correct** | Documented pattern. |

## 7. Feasibility — one encoder, one flight

The plan's premise is "tap Download before a flight". Nothing in it states the
encode throughput that premise depends on, and the numbers are not kind.

A user queues twenty 45-minute episodes: fifteen hours of content, prepared by
one serial producer, at `libx264 -preset veryfast` on the software path, with a
tone-map or a PGS burn forcing full decode-filter-encode. At an optimistic 2×
realtime that is **7.5 hours serialized**; at 1× — an ordinary small-form-factor
box doing HDR tone-mapping — **fifteen hours**, before any live viewer preempts
(which §16.5 accepts "may pause for hours").

Storage compounds it. Twenty 720p packages at the plan's own estimate is
≈ 48 GB pinned against `DEFAULT_MAX_GB = 50` (`cachekeep.rs:46`). Package 21
returns 507, and the online transcode cache is evicted to make room for
downloads. Meanwhile §7.2 advertises a cap of **100** rows per user — five
times what a default install can hold.

None of this makes the feature wrong. It makes three things mandatory in the
plan:

1. A measured encode-throughput figure per source class, stated in §16, with
   the queue sized against it.
2. An honest ETA in the UI ("ready in about 6 hours") instead of a percentage
   that will sit near zero for an hour.
3. A per-user pinned-byte quota and an offline budget distinct from
   `cache.max_gb`, so downloads cannot evict the playback cache — plus an
   operator control to disable offline entirely.

## 8. Process gates the plan will fail

**P1 · the `offline.viewing` point as specified fails lint (§14.2).** `paths`
is mandatory (`validation/runner.py:318`), `crates/**` is audited
(`validation/points.toml:16`), and every audited file must match some point
(`runner.py:360-367`). `crates/plurxd/src/offline.rs` and
`http/offline.rs` match nothing — `server.api`'s globs are explicit brace lists
(`points.toml:241-242`). `make check` fails at `validation-lint` the moment M1
lands a file. Write the point in full per [VALIDATION.md](VALIDATION.md).

**P2 · the field is `depends_on`, and the direction is inverted (§14.2).**
`runner.py:162`; impact walks provider → consumer
(`runner.py:409-427`, "`A depends_on B` means a change to B can break A").
Declaring `depends_on = [apple.client, android.client]` on a server point means
a change to `offline.rs` selects *neither* client check — the opposite of the
cascade the point exists to create. There is also no "server/client checks"
concept: `checks` must name declared check ids, and the plan proposes none.

**P3 · the store build-number bump cannot happen at M5 (§14.6).**
`mobile-version` runs in the commit profile (`points.toml:53-59`) and in
staged mode requires `CURRENT_PROJECT_VERSION`/`versionCode` to increase above
HEAD whenever `clients/apple/Sources/**` or `clients/android/app/src/main/**`
change (`validation/mobile_versions.py:179-197, 243-255`), enforced by
`scripts/pre-commit`. M3 and M4 each bump their own counter in their own
commits.

**P4 · the history gate will reject M2's cache-race commit (§7.3, §15).**
`history-check` is inside `make check` (`Makefile:51-59`), and a subject
matching the corrective vocabulary — which includes `race`, `fix`, `prevent`,
`correct`, `recover` (`validation/history.py:19-31`) — must carry test evidence
in the same patch or a `validation/regressions.toml` entry
(`history.py:246-253`). §15 explicitly defers that test. Land the eviction-race
fix as its own commit with its own regression test.

**P5 · the doc list is incomplete (§14.6).** Missing:
[STATUS.html](STATUS.html) — the pinned ledger, whose standing rule is that it
updates in the same commit as the behavior — plus [ROADMAP.md](ROADMAP.md),
[ARCHITECTURE.md](ARCHITECTURE.md), [CHEATSHEET.md](CHEATSHEET.md), and
[VALIDATION.md](VALIDATION.md), whose current-inventory table goes stale the
moment M1 adds a point.

**P6 · M2 is oversized and hides a separate fix (§14.3).** Eight workstreams,
including a refactor of existing scheduled production and the playback-cache
race correction. Split: M2a `ensure_offline` + shared recipe · M2b
queue/preemption/restart · M2c pins/budget, with the race fix as its own
corrective commit.

**P7 · `make validate-full` is the wrong ceiling (§14.6).** The target is real
(`Makefile:102-104`), but the `full` profile excludes `playback-recovery` and
`resource-bounds` (`points.toml:186`, `:196` — nightly only), which are the
closest existing evidence for §15.1's "no slot/permit leaks" and preemption
claims, and `android-device` skips silently without `PLURX_ANDROID_SERIAL`.
`make release-check` and `scripts/ship --check` run `--profile ci --strict`;
name that.

**P8 · `make check` is too weak for M1 (§14.2).** It is
`validation-lint history-check operations-check rust-check` (`Makefile:58-59`),
the Rust half only. M1 adds fields to `tests/contracts/native-api.json`, whose
purpose is Swift/Kotlin decoding. Use `make validate-staged`.

**P9 · M0's harness has two guardrails to restate (§14.1).** The Apple README's
fencing rule is stricter than the plan's nod to it: wrap the entry point *and
the type it references* in `#if os(tvOS) && DEBUG`, and never write through
`SettingsStore` from a harness (`clients/apple/README.md:167-185`) — M0's
harness selects quality, audio and subtitle. And M0's "temporary capability
route" is an unauthenticated media route, the exact class recorded as a past
incident in [SECURITY.md](SECURITY.md).

## 9. What the plan gets right

Worth recording so the revision does not churn it:

- Two named progress domains instead of one fused percentage (§4.1). This is
  the single best decision in the document.
- VOD-only, `#EXT-X-ENDLIST` before `ready`, and the refusal to serve a growing
  playlist as an offline asset (§4.4) — correct, and correctly reasoned.
- Not calling `POST .../hls/sessions` for preparation (§4.2), and the whole
  §10 rule that downloads never touch watch state, Trakt, or Watching Now —
  verified against the code: `note_playback_started` has three call sites, none
  of which a preparation path would reach.
- 256-bit lease stored only as SHA-256, scoped to one recipe, with the trace
  layer redacting the URI *before* span creation (§11.1). The span detail is
  the part most plans miss.
- Migration v14 is the right number (`store/sqlite/mod.rs:382`), the DDL
  matches house style, and `OfflinePackageStore` drops straight into the
  existing subtrait pattern (`store/mod.rs:675`, `TranscodeCacheStore`).
  "No SQL in HTTP handlers" is already true, not aspirational.
- `UNIQUE(user_id, request_id)` plus the stored choices genuinely supports
  §5.2's "same id, different choices → 409".
- The `cache.max_gb = 0` → 507 instinct is right for the right reason:
  `budget_bytes` returns `None` for 0 and the sweep's ceiling is
  `unwrap_or(0)` (`cachekeep.rs:88-92`, `:124`), so a zero budget evicts
  everything.
- A final-state outbox instead of replayed beats (§10), and the honest §11.3
  sharp-edges list. Do not soften either.

## 10. Decisions to make before the revision

1. **TS or fMP4 for v1?** TS keeps the cache namespace and ships sooner; fMP4
   is a separate milestone that invalidates every cache entry. (§B1)
2. **Does an offline package honour `audio_offset_ms`?** Honouring it is
   correct and costs cache sharing. (§B2)
3. **Foreground-start or catch-up?** Create the platform task at tap against a
   URL that becomes valid later, or accept "we'll finish it next time you open
   Cinema" and say so in the UI. (§B9)
4. **Renewable lease, or stable content-addressed media paths?** One of the two
   is required; the second is cheaper on Android. (§B6)
5. **Accept Android's 6 h ceiling per foreground visit, or build a UIDT
   scheduler?** (§B10)
6. **What is the offline storage budget**, separate from `cache.max_gb`, and
   what is the per-user quota that replaces the 100-row cap? (§7)
7. **Forced-bitmap default:** confirm-before-burn, or fall back to no
   subtitle? (§S13)

Also missing from the document entirely and worth adding as short sections: a
feature flag and kill switch (server-side `offline.enabled`, per-user enable);
admin controls (per-user quota, force-expire a package); a migration-rollback
statement (v14 is forward-only, and nothing tests a v14 database opened by a
v13 binary after a bad release); pause/resume as a real contract rather than
"where the platform supports it"; and battery/thermal, which §3.3 cites as a
reason to defer 4K and then never measures for the multi-hour encode this plan
does ship.

Two smaller notes. §16's revisit criteria depend on production telemetry that a
self-hosted product does not have — reword as "operator-visible metrics" and
make each question answerable from one admin's Prometheus. And §2.4's
keep-on-sign-out default creates invisible, unreclaimable gigabytes when an
instance id changes; Downloads should always show an "Other profiles (N items,
X GB)" section that can be deleted regardless of the active profile.

## 11. What this review could not verify

- **Swift and Kotlin do not compile here.** Every client finding is source
  reading plus vendor documentation, not a build. The Apple findings in
  particular should be confirmed by a compile before they drive a rewrite.
- **The axum route constraint (§S7) is unproven from this tree.** No route uses
  a parameter with a literal suffix and the house pattern validates suffixes in
  the handler, but nothing in the repository asserts the router rejects the
  plan's shape. Verify at build time.
- **Encode throughput (§7) is arithmetic, not measurement.** The 1–2× realtime
  band is an estimate from the preset and filter chain; the plan needs real
  numbers from the fleet, not these.
- **The plan's own base commit.** It was written against `83be7dc481` on
  `codex/fix-apple-subtitle-warning`, which is not an ancestor of the reviewed
  `origin/main`. Findings that name a symbol as missing are true at
  `2ff661da`; if that branch adds any of them, the finding narrows to "state
  the dependency".
