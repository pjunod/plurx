# Clients code review — capable players that under-ask the server

**Reviewed:** 2026-08-02
**Scope:** `clients/android` and `clients/apple` at commit `f8655c166` plus
the working tree (two uncommitted Apple files: `PlurxApp.swift`,
`PlayerController.swift`). §11 additionally reviews the two server commits
that landed after that snapshot (`ab5438ca2`, `a2f5239f9`).
**Outcome:** review only; no code was changed

**Assessment:**
[CLIENTS-CODE-REVIEW-ASSESSMENT.md](CLIENTS-CODE-REVIEW-ASSESSMENT.md)
accepts the review as the remediation roadmap, narrows its few overstatements,
and turns the priority order into observable acceptance checks.

This review complements [PLAYBACK.md](PLAYBACK.md) (the delivery contract the
clients execute), [ANDROID-CLIENT-PARITY.md](ANDROID-CLIENT-PARITY.md) and
[APPLE-CLIENT-PARITY.md](APPLE-CLIENT-PARITY.md) (what the clients are meant
to do), and the server-side
[CODE-QUALITY-PERFORMANCE-REVIEW.md](CODE-QUALITY-PERFORMANCE-REVIEW.md). It
covers code quality, architecture, performance, stability, and — with the
most weight — the playback decisions: streaming type, transcode/remux,
subtitles, and direct play.

**How it was reviewed.** Every playback-path file (caps, API clients, both
player controllers and screens, models, session/discovery) was read line by
line and cross-checked against the server's actual behavior in
`crates/plurxd/src/http/{stream,hls}.rs` and `crates/plurx-core/src/playback`
— server behavior is cited from server source, not assumed. The UI/app
layers, tests, and build configs were covered by two additional scoped review
passes whose findings were evidence-checked before inclusion. What could
*not* be verified here is in §9.

**Severity legend:** HIGH = crash, broken feature, security, or a first-order
quality loss · MEDIUM = edge-case bug, real performance or UX cost,
maintainability hazard · LOW = polish. Findings cite `file:line` in the
reviewed tree.

## 1. Verdict — the delivery plan is executed right; the inputs to it are not

Both clients get the hard part right. They execute the server's delivery
plan instead of re-deriving policy from the verdict, they carry supersession
keys (`playback_id` + per-attempt `request_id`), they DELETE their HLS
session the moment playback ends, and the code is unusually well-commented —
the *why* is on the line that needs it. The Apple client in particular has
absorbed lessons the web player learned the hard way: burn-over-remux keeps
source height, a refused stream gets exactly one automatic
compatibility-transcode retry, and text subtitles switch in place through
native HLS renditions.

The weaknesses cluster in three places:

1. **The clients under-report device capability** (§2). The whole pipeline is
   built on "the server's best move is to send the file untouched" — but the
   server only sends the file untouched when the client tells the truth
   about what it can play. Android never mentions Dolby Vision and never
   mentions the audio formats its HDMI sink can bitstream, so on exactly the
   hardware bought for quality (Shield + AVR, DV television), plurx strips
   DV and re-encodes lossless audio to AAC.
2. **Subtitles are the most expensive menu in both apps** (§3). Both clients
   can turn a 4K HDR remux into a 1080p and/or SDR transcode by selecting —
   or on Apple, *auto*-selecting — a subtitle. This is the web player's
   original PGS-burn cascade (see `docs/STUTTER-4K.md` era fixes) alive in
   the native clients.
3. **Android's player has two first-order stability holes** (§4): no player
   error handling of any kind, and a settings toggle that releases the live
   ExoPlayer and then reuses it.

| Area | Android | Apple |
| --- | --- | --- |
| Delivery-plan execution | Good — direct/remux/HLS all honored | Good — plus VOD-cache and native-subtitle handling Android lacks |
| Capability reporting | Weakest point — no DV, no passthrough audio, broken `force` | Good — DV profiles, honest containers |
| Subtitles | Burns everything, at the wrong height | Native text subs done right; auto-select can trigger burns |
| Stability | Two HIGH player bugs, solid app shell | One HIGH discovery hang, one uncommitted build break |
| Code clarity | Good; comments explain intent | Good; same house voice |
| Tests | Sharp pure-function tests; silent on the stateful paths | Same pattern, same gap |

---

## 2. Capability reporting and the decision — where quality is decided

The server's decision engine (`crates/plurx-core/src/playback`) is a pure
function of the file and the reported caps. Every finding in this section is
a case where the caps under-claim, so the pure function — working correctly —
picks a worse stream than the hardware deserves.

### 2.1 Android never reports Dolby Vision, so DV is stripped for every Android device — HIGH (quality)

`Caps.query` (`clients/android/.../data/Caps.kt:19-49`) sends `vcodec`,
`acodec`, `container`, `hdr` — and nothing else. The server is explicit about
what an absent `dv` means (`crates/plurxd/src/http/stream.rs:130-135`):

> `Absent means no — an old client that never sends it is exactly one that
> has not been taught to ask.`

So for every DV title, every Android/Google TV device gets the
`DvHandling::Strip` path: **remux, never direct play** (the strip requires
ffmpeg), and the delivered stream is the HDR10 base layer. Consequences, in
order of cost:

- A Shield/Sony/TCL with a DV display plays HDR10 where it could play DV —
  the quality ceiling is lowered on the exact devices bought to raise it.
- Every DV file costs a server ffmpeg session (the `dovi_rpu` strip) where a
  direct play would have cost zero CPU — on a library where DV is common,
  that is the difference between "NAS serves bytes" and "a transcode slot
  per viewer."

The Apple client already does this correctly (`clients/apple/Sources/
Caps.swift:34-38`): it sends `dv=1&dvprofile=5,8` when hardware HEVC and HDR
eligibility hold, and the server honors it end to end
(`crates/plurx-core/src/playback/mod.rs:339` sets `preserve_dolby_vision`
from `profile.allows_dolby_vision`).

**Fix:** probe `MediaCodecList` for a `video/dolby-vision` decoder and the
display for `HdrCapabilities.HDR_TYPE_DOLBY_VISION`; derive `dvprofile` from
the decoder's `CodecProfileLevel`s (dvhe.st → 8, dvhe.dtr → 4, etc.) and
send `dv`/`dvprofile` exactly as the Apple client does. ExoPlayer plays DV
profiles 8/4 as HEVC-with-metadata on capable devices; profile 7 (dual
layer) should stay unclaimed so the server keeps stripping it.

### 2.2 Android audio caps only see decoders, never the HDMI sink — HIGH (audio quality)

`Caps.kt:35-38` claims `ac3/eac3/dts/truehd` only when `MediaCodecList` has
a *decoder* for them. Passthrough is not a decoder: a Shield or TV box wired
to an AVR/soundbar bitstreams TrueHD/Atmos and DTS-HD via `AudioTrack`
encodings, and `MediaCodecList` on most such devices has no `audio/true-hd`
entry. The result: a TrueHD MKV that should **direct-play with lossless
audio passthrough** instead comes back as the remux verdict with
`transcode_audio` — the server re-encodes TrueHD/DTS-HD to AAC 256k
(`docs/PLAYBACK.md`, copy-pipe args). Lossless Atmos becomes lossy AAC on
the one client platform whose pitch (docs/CLIENTS.md §2) is "true MKV direct
play … DTS per device."

**Fix:** merge sink capabilities into the probe — Media3's
`AudioCapabilities.getCapabilities(context)` reports the HDMI sink's
supported encodings (`ENCODING_AC3/E_AC3/E_AC3_JOC/DTS/DTS_HD/
DOLBY_TRUEHD`); claim a codec when *either* a decoder exists or the sink
accepts the bitstream. Caveat worth a comment when doing it: sink caps
change when the user re-plugs HDMI, so query per `decision`, not once.

### 2.3 Android sends `force=<rung>` values the server parses as Auto — MEDIUM (broken feature)

`AppViewModel.decision` (`clients/android/.../ui/AppViewModel.kt:306-309`)
sends the raw preference value as `force`:
`auto | original | 2160 | 1440 | 1080 | 720 | 480`. The server's parser
(`crates/plurx-core/src/playback/mod.rs:85-91`) recognizes exactly
`original` and `transcode`; **everything else is `Force::Auto`**. So a user
who picks "720p" (say, over Tailscale on a phone) still gets a direct-play
or remux verdict at the source's full bitrate — the rung only takes effect
in the rare case where the natural verdict was already a transcode (then
`Controller.openSession` sends `height`, `Controller.kt:220`). The quality
preference is silently a no-op for most files.

**Fix:** map explicit rungs to `force=transcode` at `/decision` and keep
sending `height` at session create (the server snaps strays onto the ladder,
`crates/plurxd/src/http/hls.rs:160-162`). "Auto" and "Original" already
parse correctly.

### 2.4 Android's "Original" doesn't carry source height into forced transcodes — MEDIUM (quality)

`Controller.openSession` sends
`height = playbackQuality.storageValue.toIntOrNull()` (`Controller.kt:220`)
— `"original".toIntOrNull()` is null, which the server documents as *its*
Auto rung: `min(source, 1080)` on hardware (`hls.rs:152-157`). The server
comment states the contract the client is missing (`hls.rs:158-159`): "The
source's own height is the Original/forced-burn promise." So under Quality =
Original, selecting a subtitle that burns (§3.1) restarts the 4K stream as a
**1080p** transcode — the exact downgrade the web player fixed in its
`sessionHeight()` pass, and which the Apple client implements
(`PlayerController.swift:324-328`: `burnHeight = source.height` whenever the
verdict wasn't a real transcode).

**Fix:** on any forced session (burn, or Original with a session), send the
source height from the decision/file metadata; keep null only for genuine
Auto-transcode verdicts.

### 2.5 Smaller, deliberate, or fine

- **Apple's DV claim** couples to `hevc && AVPlayer.eligibleForHDRPlayback`
  (`Caps.swift:48-50`). On an HDR10-only television this still claims DV
  5/8, which is safe in practice — tvOS converts DV output to the display's
  format — but it is a claim about the OS, not the panel; worth one comment.
  LOW.
- **Neither client sends `maxheight`.** That matches the stated policy
  ("4K-on-small-screen → direct-play", founding decision 2026-07-23) — this
  review records it as intent, not omission.
- Android's `hdr` probe (display HDR types non-empty, gated on HEVC/AV1)
  mirrors the server's tone-map-on-SDR rule correctly.

---

## 3. Subtitles — the most expensive menu in both apps

The server offers three subtitle mechanisms, in ascending cost: native
WebVTT renditions inside the HLS master (`native_subtitles` +
`subtitle`, `crates/plurxd/src/http/hls.rs:88-91,165-178`); on-demand VTT
extraction (`GET /files/{id}/subs/{idx}`, cached server-side); and burn-in
(`subtitle_burn`, a video transcode). The web player uses `<track>` VTT for
text and burn for bitmap; the Apple client uses native renditions for
SRT/VTT (machinery hardened right after this review's snapshot — §11) and
burn for bitmap/styled; **the Android client burns everything.**

### 3.1 Android: selecting any server-listed subtitle restarts playback as a transcode at the Auto rung — HIGH (quality)

`Controller.switchSubtitle` (`clients/android/.../player/Controller.kt:
164-173`) sets `activeMode = "transcode"` for every non-null selection and
`openSession` sends `subtitle_burn` (`Controller.kt:222`). Two compounding
losses on, say, a 4K HDR remux with an SRT track:

- **Resolution:** the session is created with `height = null` under Auto
  (§2.4), so the server picks `min(source, 1080)` — 4K becomes 1080p.
- **Dynamic range:** the transcode chain is H.264 SDR (tone-mapped), so HDR
  is gone too — for a *text* subtitle that the server would have handed over
  as a WebVTT rendition or a side-loaded VTT for free.

The client uses neither of the two free mechanisms: its
`CreateSessionReq` (`data/Models.kt:227-241`) has no
`native_subtitles`/`subtitle` fields, and `PlurxApi` has no
`/files/{id}/subs` call. On direct play the damage is smaller —
`TrackMenu` filters server tracks to bitmap-only there
(`Controller.kt:374`) and embedded text tracks ride ExoPlayer's own
selector — but remux and transcode sessions expose every text track
straight into the burn path.

**Fix (either works; the first matches Apple):**

1. Send `native_subtitles: true` (+ `subtitle` ordinal) on HLS sessions and
   toggle via ExoPlayer text-track selection — the server machinery exists
   and is already exercised by the Apple client; or
2. Side-load `/files/{id}/subs/{idx}` as a `SingleSampleMediaSource`
   (`MergingMediaSource`) for remux/direct, keeping burns for bitmap only.

Either way, a bitmap burn must carry source height per §2.4.

### 3.2 Android: the decision's default/forced subtitle is never auto-applied on server-controlled streams — MEDIUM (correctness)

`Controller.selectedSubtitle` initializes to null and nothing consults
`plan.subtitles` flags. On direct play the ExoPlayer track selector honors
`preferredTextLanguage` and forced flags from the container, so foreign-
language forced subs appear; on remux/transcode (where the server strips
subtitle tracks from the stream) they silently don't. A film with forced-sub
dispositions plays remuxed with no forced subs and no hint. Fix together
with §3.1 — once text subs are free, applying the decision's
forced/default track at start is the same one-liner the web player has.

### 3.3 Apple: the auto-subtitle rule falls through to "any same-language track", which can burn on cold start — HIGH (quality/policy)

`automaticSubtitleIndex` (`clients/apple/Sources/PlayerController.swift:
621-635`) picks, within the preferred language: forced → flagged-default →
**`matching.first`**. With `subLang` defaulting to `"eng"`
(`SettingsStore.swift:72`), that last arm means:

- A file whose English tracks carry no forced/default flags — the common
  case — auto-enables **full subtitles** on every play of an
  English-audio film.
- When the only English track is PGS — the standard 4K disc-remux shape —
  the auto-pick is a bitmap, `subtitleRequiresBurn` is true
  (`PlayerController.swift:548-550`), and the **cold start becomes a burn
  transcode**: an encoder slot per play, H.264 SDR output, HDR gone. This
  is the web player's original "default PGS sub burns every session"
  cascade (fixed there in the 2026-07-31 arc) reintroduced by policy. The
  height is right (source height, §2.4 done correctly) — the dynamic range
  and the encoder cost are not.

The unit test pins only the forced-title case
(`Tests/AppleClientTests.swift:164-203`); the `?? matching.first` tail —
the expensive one — is untested, which is usually the sign it wasn't the
intended behavior.

**Fix:** stop the chain at flagged tracks
(`forced || title-says-forced → default-in-language → nil`), or at minimum
never auto-select a track that requires burn. If "subs always on in my
language" is genuinely wanted, make it an explicit setting — it should not
be the default price of `subLang = eng`.

### 3.4 Apple: any native-text track forces HLS even with subtitles off — MEDIUM (design tradeoff to confirm)

`open()` computes `direct = normalMode == "direct" && … &&
!hasNativeSubtitles` (`PlayerController.swift:298-302`): a direct-play
verdict file that merely *contains* an SRT/VTT track never direct-plays —
it opens a copy-HLS session so the renditions exist if the viewer toggles
subs later. The stream quality is identical (copy), but every such play
consumes a server session + segmenter where the web and Android clients
serve raw bytes, and start-up takes the session-create path. If that
tradeoff is intentional, document it in APPLE-CLIENT-PARITY.md; the
alternative is to direct-play until the first subtitle selection and accept
one restart at that moment (the code already restarts cleanly for burns).

---

## 4. Session lifecycle, seeking, and player stability

### 4.1 Android: no player error handling at all — HIGH (stability)

The only `Player.Listener` (`clients/android/.../player/PlayerScreen.kt:
414-443`) implements `onIsPlayingChanged`, `onPlaybackStateChanged`,
`onVideoSizeChanged` — there is **no `onPlayerError`** anywhere in the app.
`playFailed` is set only when the HLS *create call* throws
(`Controller.kt:226-228`). So a stream ExoPlayer rejects — a decoder that
claims a profile and fails at level 5.1, an HTTP error mid-stream, a source
error on a direct play the decision was optimistic about — leaves a black
frozen surface with no message, no retry, and no exit besides Back.

PLAYBACK.md's contract has a name for what should happen: the error
fallback — "any direct/remux stream the browser rejects gets exactly one
automatic rescue: restart as a guaranteed-compatible transcode." The Apple
client implements exactly that (`PlayerController.swift:477-497`:
`shouldRetryWithCompatibilityTranscode`, once per item, then a real failure
screen). Android needs the same: `onPlayerError` → if the mode was
direct/remux and no retry yet, reopen as a forced transcode at the current
position; else surface `PlaybackFailed`.

### 4.2 Android: toggling "Autoplay next episode" mid-play releases the live player, then uses it — HIGH (crash)

The listener effect is keyed on the preference:
`DisposableEffect(controller, preferences.autoplayNext)`
(`PlayerScreen.kt:413`). Its `onDispose` runs `controller.release()` —
which releases the ExoPlayer and the MediaSession (`Controller.kt:150-156`)
— and the effect body then re-runs `controller.startAt(startMs)` on the
**released** player (the `Controller` itself is `remember(plan)` and does
not rebuild). The toggle sits in the in-player settings panel
(`PlayerScreen.kt:895`), so this is one click away during any playback:
best case a dead surface, worst case an `IllegalStateException`; either way
playback also restarts from the original `startMs`, not the current
position.

**Fix:** key the effect on `controller` alone and read the preference
through `rememberUpdatedState` inside the listener (or hoist autoplay
handling out of the listener entirely).

### 4.3 Android: `vod` is ignored — cached transcodes resume at 0:00 and seek by session churn — MEDIUM

The server marks fully-cached sessions `vod: true` with
`start_seconds: 0.0` and expects "the player seeks into it"
(`crates/plurxd/src/transcode.rs:1980-1985`). Android's `HlsStart`
(`data/Models.kt:213-219`) doesn't decode `vod`; `openSession` sets
`baseMs = start_seconds` and never seeks (`Controller.kt:239-242`), so a
resume into a cache-hit transcode **starts the film from the beginning**,
and every in-film seek re-creates a session that native seeking would have
handled. Apple handles both (`PlayerController.swift:347-355`:
`isVOD → baseMs = 0` + seek-after-attach; `usesDirectTimeline` for seeks).

### 4.4 Apple: seeks and track changes during a stream change are dropped — MEDIUM

`reopen` bails while a change is in flight
(`PlayerController.swift:271`: `guard … !isChangingStream`). On tvOS the
progress bar seeks in 30 s steps (`PlayerView.swift:916-924`), and any step
pressed while the previous reopen is mid-create is silently discarded — the
bar snaps back. Supersession exists precisely so the newest request can
safely replace the older (`playback_id` keying); the guard should record
the latest requested position and issue one trailing reopen when the
in-flight change lands, instead of dropping intent.

### 4.5 Apple: the old session is released before the new one is created — MEDIUM

`open()` awaits `releaseCurrentSession()` before `createHlsSession`
(`PlayerController.swift:284-338`). If the create then fails (server busy,
network blip), the viewer is left with an AVPlayerItem pointed at a
playlist the client just deleted — buffered runway plays out, then it
stalls, and `fail()` deliberately doesn't show the failure screen while an
item exists (`PlayerController.swift:406-410`). Server-side supersession
already retires the predecessor when the new create lands, so the explicit
pre-release is not needed for correctness: release *after* a successful
create (or not at all, letting supersession do it) turns a failed change
into "still watching the old stream" instead of "stalls in N seconds."

### 4.6 Android: an in-player quality change resets audio and subtitle choices — MEDIUM

The quality rows call `onReload`, which rebuilds the plan and a fresh
`Controller` (`PlayerScreen.kt:273-277,862-875`); position and audio-sync
offset survive via saved state, but `selectedAudio`/`selectedSubtitle`
re-initialize to defaults (`Controller.kt:84-86`). Watching with French
audio and picking 1080p silently returns to the English default. Hoist the
two selections next to `playbackAudioOffset` in `PlayerScreen`.

### 4.7 Android: no audio focus, no becoming-noisy handling — MEDIUM

`buildPlayer` (`Controller.kt:280-293`) never calls
`setAudioAttributes(attrs, /* handleAudioFocus = */ true)` or
`setHandleAudioBecomingNoisy(true)`. Film audio plays *over* an already-
playing music app instead of pausing it, nothing ducks for
notifications/calls, and unplugging headphones keeps playing on the
speaker. Both are one-line builder options.

### 4.8 Android: TV pipeline options worth taking — LOW

Two Media3 switches matter on TV hardware: tunneling
(`DefaultTrackSelector.buildUponParameters().setTunnelingEnabled(true)` on
television devices) for smooth 4K/HDR A/V sync on SoCs that need it, and
`DefaultRenderersFactory.setEnableDecoderFallback(true)` so a flaky
hardware decoder falls back to software instead of erroring (which today,
per §4.1, shows as a silent black screen).

### 4.9 Parity seams — LOW, recorded so they're chosen rather than drifted

- Android's quality menu is a hardcoded enum (2160→480,
  `data/ViewerPreferences.kt:51-58`) offering rungs above the source;
  the server now advertises a real per-source ladder with bitrates in both
  `/decision` and the session response (`ladder`), which the Apple menu
  renders (`PlayerView.swift:1024-1036`). Android ignores it.
- Android sends no `stream=` id on `/stream.mp4`
  (`Controller.kt:253-266`), so remux delivery telemetry
  (`/stream/{id}/status`) is unavailable; its info panel has no server-side
  rows. Apple polls `/hls/{session}/status` every 2 s and shows encode
  speed/ahead/delivery — the panel Android's overlay should eventually
  match.
- Apple's `playableEpisode`/`nextEpisode` pick `files?.first` with no
  availability check (`AppModel.swift:499-517,552`; the Swift `MediaFile`
  model lacks `available`), so a missing-on-disk file can be chosen where
  Android filters `it.available` (`AppViewModel.kt:340,368`).
- Progress cadence is 10 s + exit + natural end on both — fine, matches the
  parity doc; the web's 5 s is not worth copying.

---

## 5. Android app shell — architecture, stability, performance

One `AndroidViewModel` owns session lifecycle and home state; screens fetch
their own data in composition via `produceState`. That is a reasonable v0.2
shape, and the session-recovery design (instance-id rediscovery, the
retry-not-logout `validateSavedSession`) is genuinely good. The findings:

### 5.1 Switching servers keeps the old bearer token — HIGH (security; both platforms)

`changeServer()` clears only the in-memory token
(`AppViewModel.kt:258-261`); the DataStore token survives, and
`connectToOrigin` saves the new origin without touching it
(`AppViewModel.kt:427`, `SettingsStore.kt:70-76`). Connect to server B, get
killed before login, relaunch: init sees `origin(B) + token(A)` and sends
server A's bearer token to server B over plain HTTP
(`AppViewModel.kt:118-131`) — and when B answers 401 it also destroys A's
still-valid session (`clearToken`, line 143). The Apple client has the
identical defect (`AppModel.swift:243-247` vs `131-132`). **Fix on both:**
clearing/changing the origin clears the persisted token in the same write.

### 5.2 The D-pad focus chain references disposed FocusRequesters — HIGH (TV crash/dead nav)

Every shelf card wires `up`/`down` to the *first card* of the neighboring
shelf (`ui/components/Common.kt:233-243`), but LazyRow disposes scrolled-out
items: scroll "Continue watching" to item 20, press down then up, and the
target requester is no longer attached — current Compose throws
`FocusRequester is not initialized` (at best the press dies). This is the
primary TV browsing surface (`HomeScreen.kt:91-101`). Attach the requesters
to the row containers (focusGroup per shelf) rather than a specific card.
Related: the home "Group by" picker is unreachable by D-pad because the
chain skips over it (`HomeScreen.kt:139-155`), and
`.clickable(...).focusable()` creates doubled focus targets on cards and
pickers (`Common.kt:102-103,279-281`).

### 5.3 The watch filter is broken for show libraries — MEDIUM (both platforms + a server gap)

Both clients filter on `item.watch` alone (Android
`LibraryScreen.kt:137-145`; Apple `AppModel.swift:625-634`), but watch rows
exist for leaves only, and the library listing attaches **neither** watch
nor rollup to containers — `list_items` maps
`with_watch + with_resolution + with_child_count` and never `with_rollup`
(`crates/plurxd/src/http/browse.rs:88-118`; rollups appear only on
`item_detail`). So in a TV library, "Watched"/"In progress" are always
empty and "Unwatched" includes fully-watched shows — on Android and Apple
alike. The client-side fallback (`rollup.watched == leaves` → watched,
`0 < watched < leaves` → in progress, as `orderedSeasonCandidates` already
reasons) only works once the server attaches rollups in `list_items` — it
already batch-fetches `child_counts` there, so a batched rollup is the
same shape. Fix both halves together.

### 5.4 Serial request cascades gate first paint — MEDIUM (performance)

- Home: `hubs` → `libraries` → one `libraryItems` per library, strictly
  sequential (`AppViewModel.kt:233-237`) — 2+N round trips before the
  landing screen renders, every launch and refresh.
- Library: the *entire* library pages 200-at-a-time behind a spinner before
  the grid appears, and changing Sort refetches everything even though
  `sortMerged` re-sorts client-side anyway (`LibraryScreen.kt:59,153`,
  `AppViewModel.kt:291-300`). "Added" sort across a merged collection is
  also a no-op concatenation (`LibraryScreen.kt:153` falls to
  `else -> items`) because the Android `Item` model never decodes the
  `added_at` the server sends (`dto.rs:51`) — Apple decodes it and
  interleaves correctly.
- Detail: `itemDetail` + `seriesPlayback` (which walks seasons and probes
  episodes serially) both complete before anything renders
  (`DetailScreen.kt:93-103`, `AppViewModel.kt:344-381`) — a show with
  missing files walks every episode before first paint.

Parallelize with `coroutineScope`/`async`, render what arrived, and resolve
the play target after paint.

### 5.5 Lifecycle and network-edge bugs — MEDIUM

- NSD discovery started for saved-server recovery is never stopped
  (`AppViewModel.kt:443` → `ServerDiscovery.kt:177-185`); after any
  `ServerUnavailable` launch, multicast runs for the whole app session —
  on always-on TV boxes, indefinitely. Wrap in `try/finally { stop() }`.
- `serverBrowser.resolve` has no timeout (`ServerDiscovery.kt:147-174`);
  a swallowed NSD callback wedges `busy = true` forever and every connect
  control is `enabled = !busy` (`AppViewModel.kt:181-194`,
  `AuthScreens.kt:164`). `withTimeout(5s)` at the call site.
- Launch validation against an unreachable saved server can hold the bare
  splash spinner for 80+ seconds with no escape (4 × 20 s connect timeouts
  plus backoff plus rediscovery, `AppViewModel.kt:118-131,474-499`,
  `Net.kt:24`). Cap it, or show the shell in a connecting state with a
  visible way out.
- Default "safe" insets use `statusBars` only, so landscape corner cutouts
  aren't cleared (`ui/components/SafeAreas.kt:22,37`;
  `HomeScreen.kt:198`) — the androidTests inject synthetic insets and so
  can't catch it. Union with `displayCutout`.
- `TightTvButtonFocusBounds` disables the 48 dp minimum touch target on
  *all* form factors, not just TV (`ui/components/TvFocus.kt:158-163`),
  and its justifying comment ("Android still expands touch hit testing")
  describes the very mechanism the Local disables.

### 5.6 Smaller items — LOW

`runCatching` swallows `CancellationException` and can navigate after Back
(`DetailScreen.kt:248-256`, also 98/386-408; same class in
`AppViewModel.kt:173,221,244` — `validateSavedSession` at 484 shows the
correct pattern) · stale `authError` shown on the connect screen after
"Use a different server" (`AppViewModel.kt:222,258`) · session identity as
non-reactive plain `var`s read in composition (`AppViewModel.kt:89-100`) ·
`remember` instead of `rememberSaveable` for search query and library
sort/filter (`SearchScreen.kt:53`, `LibraryScreen.kt:57`) · `loadHome` has
no in-flight cancellation, slowest response wins (`AppViewModel.kt:229`) ·
`Caps.query` (binder IPC) on the main thread per decision
(`AppViewModel.kt:106,306`) · PhotoScreen has no loading/error state and a
dead double-tap handler (`PhotoScreen.kt:21-34`) · `allowBackup` includes
the token DataStore in cloud backups (`AndroidManifest.xml:21`) · search
field lacks the TV select-to-edit guard the auth fields have
(`SearchScreen.kt:84-95`) · duplicate filename line in single-file Media
cards (`DetailScreen.kt:446-455`) · every user-facing string is hardcoded
(`strings.xml` has only `app_name`) · legacy-only launcher icons and a
1× TV banner; `tools:targetApi="34"` is stale.

---

## 6. Apple app shell — architecture, stability, performance

One `@MainActor` `AppModel` drives a phase machine; value-based navigation;
platform forks are narrow and mostly in Theme/layout. Token storage is done
properly (Keychain `TokenVault` behind a protocol, with a careful
UserDefaults migration — the parity doc's ask is complete). The findings:

### 6.1 Bonjour resolution runs on a run-loop-less thread — resolve never completes — HIGH

`BonjourResolver.resolve()` is nonisolated async, so its body runs on the
cooperative pool; `NetService` schedules its callbacks — including the 5 s
timeout — on the *current thread's* run loop, which never spins there
(`ServerDiscovery.swift:151-157`). Neither delegate method fires; the
continuation leaks; the `await` never returns. Reachable two ways: tapping
a discovered server leaves `resolving` set and every row disabled
(`AuthViews.swift:224-236`), and saved-session recovery hangs bootstrap on
the spinner whenever the saved server is down but any `_plurx._tcp`
service is visible (`AppModel.swift:278`). **Fix:** schedule the
`NetService` on the main run loop (or make the resolver `@MainActor`), or
replace it with an `NWConnection` to the browsed endpoint — `NetService`
is deprecated and this is exactly the trap it sets.

### 6.2 Server switch keeps the old token — MEDIUM (security)

The twin of §5.1: `changeServer()` never calls `settings.clearToken()`
(`AppModel.swift:243-247`), `connect()` persists the new origin
(`AppModel.swift:131-132`), and the next `bootstrap()` sends the old
server's bearer token to the new host (`AppModel.swift:79-90`).

### 6.3 No session-expiry handling after bootstrap — MEDIUM

401/403 flips to `.needLogin` only during bootstrap
(`AppModel.swift:95-98,263-266`). Once `.ready`, a revoked/rotated token
degrades every screen to "Server returned 401" strings (or silence where
cached content suppresses the error) with no path back to login except
finding Settings → Sign out. Centralize `APIError.http(401|403)` →
`clearToken(); phase = .needLogin`.

### 6.4 Home goes stale after playback; tvOS has no refresh at all — MEDIUM

`loadHome()` runs on bootstrap/login, watched-toggles, and iOS
pull-to-refresh only; the on-appear task is guarded by `homeLoading`
(`HomeView.swift:69,123`). `PlayerView.onPlaybackStopped` patches only the
local detail snapshot (`DetailView.swift:331-337`). Finish an episode, come
Home: Continue Watching still shows the old state — on tvOS until relaunch.
Refresh from the player-dismissal path and/or on `scenePhase == .active`
(there is currently no scenePhase handling anywhere).

### 6.5 Performance seams — MEDIUM

- Pull-to-refresh blanks the whole populated dashboard to a spinner
  (`AppModel.swift:181` sets `homeLoading` unconditionally;
  `HomeView.swift:105-116` renders spinner-or-content) — contradicting the
  file's own cached-content policy. Spinner only when `!hasHomeContent`.
- `AuthImage` decodes full-resolution artwork per cell with no downsample
  and no decode cache, and retries never (`AuthImage.swift:24-33`) — on
  tvOS grids with `.extraLarge` posters this is a scroll-hitch and memory
  risk; a failed first load is a placeholder forever. Downsample via
  ImageIO to the known cell size + `NSCache`; route through the
  `waitsForConnectivity` session like the API does.
- `libraryItems` pages the entire merged collection before showing anything
  and re-runs per sort change (`AppModel.swift:409-429`,
  `LibraryView.swift:123-132`) — same shape as Android §5.4; publish pages
  incrementally.
- Detail `.task(id:)` refires on every pop-back and then yanks tvOS focus
  to the header Play button 120 ms later (`DetailView.swift:305-321`) —
  keep the refetch if freshness matters, drop the focus grab after the
  first appearance.

### 6.6 Uncommitted validation scaffolding: iOS build break now, a backdoor if committed — HIGH (working tree only)

The two dirty files add the `--live-scary-movie` acceptance harness. As the
tree stands, `PlurxApp.body` references `LiveScaryMovieValidationView`
unguarded (`PlurxApp.swift:10-14`) while the type is `#if os(tvOS)`
(`PlurxApp.swift:23-26`) — **the iOS target does not compile**. And the
mechanism itself (launch-argument entry that injects a bearer token from
the environment, flips `model.phase = .ready`, and overrides the saved
`subLang`; plus the matching `print` hook in
`PlayerController.swift:471-476`) would ship in Release builds if
committed as-is. HEAD is clean — this is in-flight work. When the log
capture is done, delete it, or fence the whole thing in
`#if os(tvOS) && DEBUG`.

### 6.7 Smaller items — LOW

`LibraryView.load` renders `CancellationError` as a user-facing failure
(`LibraryView.swift:128-130`; SearchView and `homeErrorMessage` do it
right) · a transient refresh failure wipes Coming Soon
(`AppModel.swift:192`) · `connect()` mutates `Session.shared.origin` before
the probe succeeds (`AppModel.swift:124`) · dead code:
`EmptyLibraryCategory`, `DetailView.metaLine`, `AppModel.collection(kind:)`
· login fields lack `.textContentType(.username/.password)`, so iCloud
Keychain and the tvOS nearby-iPhone flow don't engage
(`AuthViews.swift:315-323`) · tvOS search is a bare `TextField` instead of
`.searchable` (`SearchView.swift:18-22`) · guarded force-unwraps
`Double(durationMs!)` (`DetailView.swift:401,488`) · `Session` is
`@unchecked Sendable` with unsynchronized vars — fine today, a trap under
future concurrency · three separate language-name maps
(`PlayerController.swift:606-614,639-653`, `PlayerView.swift:1054-1063`)
should be one table.

---

## 7. Tests, build, and documentation honesty

**Tests.** Both suites share a pattern: sharp, hermetic tests for pure
policy (origin normalization, instance-id matching, retry semantics, series
ordering, wire-contract decoding; Apple adds real layout-regression tests
and WCAG contrast assertions — 45 tests, well above hobby grade) — and
nothing on the stateful paths where this review's HIGHs live. Neither
platform tests its ViewModel/AppModel state machine (login/changeServer —
the token bug of §5.1/§6.2 sits exactly there), the player controllers'
session lifecycle, D-pad traversal between shelves (§5.2 is invisible to
the existing focus tests), or the discovery async bridges (§6.1 lives in
the untested `BonjourResolver`; its pure helpers are tested instead).
Two test-quality nits: Android's `AuthTextFieldKeyboardTest` can only pass
on a TV-uiMode device, and several Apple tests assert constants
(`width ≤ 54`) rather than behavior; Apple unit tests also run hosted in
the full app, so `AppModel.init` starts live discovery/bootstrap during
test runs — a flakiness hazard.

**Build/packaging.** Android release is unminified while depending on
`material-icons-extended` (`app/build.gradle.kts:22-25`,
`libs.versions.toml:29`) — thousands of unused icons shipped in every APK
for the ~10 used; either enable R8 (+shrinkResources), or switch to
`material-icons-core` + inlined icons. `proguard-rules.pro` is dead config
until minification exists. The rest is in good shape: version-catalog
setup, wrapper pinned to the documented toolchain, a Dockerfile that
reproduces the documented SDK exactly, TV-correct manifest
(`LEANBACK_LAUNCHER`, banner, `touchscreen/leanback required=false`,
justified cleartext), PiP declared. Apple's project.yml is coherent
(iOS/tvOS 17 floors match API usage, schemes/test hosts wired, ATS
`NSAllowsArbitraryLoads` deliberately argued in comments, Bonjour/local-
network/camera strings present, accurate PrivacyInfo).

**Documentation drift.** Both clients out-run their docs, which in this
repo is the failure mode docs exist to prevent:

| Doc claim | Reality |
| --- | --- |
| Both READMEs: "Status: v0.1.0" | `versionName`/`MARKETING_VERSION` 0.2.0 |
| Apple README: token in UserDefaults, "Keychain-free" | Keychain `TokenVault` with migration (`SettingsStore.swift:38-62`) |
| Apple README: "the same six unit tests" | 45 tests |
| Apple README: search, sort/filter, PiP listed as missing | All implemented and tested |
| Apple README + APPLE-CLIENT-PARITY.md: "burns every selected track" / "Keychain move remaining" | Native WebVTT path shipped; Keychain done |
| Android README: quality menu "Auto/Original/1080/720/480" | Also 2160p and 1440p (`ViewerPreferences.kt:51-58`) |

One documentation pass against the code, same commit as the next behavior
change.

---

## 8. What is genuinely good

Worth naming, because the fix list above shouldn't read as the whole story:

- **Delivery-plan discipline.** Both clients execute `delivery` instead of
  re-deriving policy from `method`, and both carry the comment explaining
  the bug that rule prevents. This is the core contract, done right twice.
- **Session hygiene.** POST-create with `playback_id` + fresh `request_id`
  (the supersession contract the server-side review said clients needed),
  DELETE on exit with the reaper as backstop, stale-response version
  guards on Android (`sessionRequestVersion`), stale-session end on Apple.
- **Apple's playback maturity:** native text subtitles via HLS renditions
  switched in place, burn-at-source-height, the one-shot compatibility
  transcode retry, VOD-cache handling, `preserve_dolby_vision`, the
  2 s `hls/status` telemetry panel that can tell a link stall from an
  encoder stall — several of these are ahead of the parity doc.
- **Session recovery design on both platforms:** only an explicit 401/403
  discards a token; transport failures retry then degrade to a retryable
  state; rediscovery matches servers by mDNS instance id with a legacy
  hostname migration. The Android version is unit-tested per branch.
- **One authenticated HTTP client** feeding API, artwork, and media on
  Android (`Net.kt`), with the deliberate public-probe exception for
  `/server` so LAN probing never leaks a token — mirrored on Apple in
  `PlurxAPI.serverInfo`.
- **TV chrome that takes tvOS/Android TV seriously:** custom focus rings
  with semantics keys tests can assert, select-to-edit text fields,
  contrast-ratio assertions, the tvOS progress bar built as a focusable
  non-Button for a documented reason.
- **Comments that explain why, everywhere.** The codebase reads like the
  server's: the reviewer rarely had to guess intent, which is why this
  review can be specific.

## 9. What this review could not verify

- **Nothing was compiled.** The sandbox has neither an Android SDK nor
  Xcode; `gradlew … assembleDebug`, `lintDebug`, `xcodegen` + both schemes,
  and both test suites should be run before acting on §5/§6 line numbers.
- **No device playback.** Every claim about what a stream *does* is derived
  from code + the server contract, not from pixels. The five-case
  real-hardware matrix in APPLE-CLIENT-PARITY.md §1 (direct MP4 / copy MKV
  / transcode / text + PGS subtitle / episode rollover) remains the release
  gate for both platforms, and §2–§3 fixes here change what those cases
  exercise (a DV title and a TrueHD-over-AVR title should join the matrix).
- Line numbers reference tree `f8655c166` plus the two dirty Apple files;
  re-verify against the current tree before patching.

## 10. Recommended order

**P0 — breaks, hangs, leaks (small, isolated fixes):**

1. §6.6 fence or remove the `--live-scary-movie` scaffolding (iOS target
   compiles again).
2. §4.2 autoplay-toggle player release (Android crash, one-line rekey).
3. §4.1 `onPlayerError` + one-shot transcode rescue (Android parity with
   PLAYBACK.md and Apple).
4. §6.1 Bonjour resolver run loop (Apple connect/bootstrap hang).
5. §5.1/§6.2 clear the persisted token when the origin changes (both).

**P1 — the quality ceiling (the reason for this review):**

6. §2.1 Android DV caps (`dv`/`dvprofile`).
7. §2.2 Android passthrough audio caps (TrueHD/DTS/E-AC3-JOC via sink).
8. §3.1 + §3.2 + §2.4 Android subtitles: native/side-loaded text tracks,
   burn only bitmap, carry source height on burns and Original.
9. §3.3 Apple auto-subtitle: stop at flagged tracks; never auto-burn.
10. §2.3 Android `force` mapping so explicit rungs mean themselves.
11. §4.3 Android `vod` handling (cached-transcode resume + native seek).

**P2 — comfort, performance, hygiene:**

12. §5.2 TV focus graph; §5.3 shows watch filter; §5.4/§6.5 serial loads,
    image pipeline, refresh-blanking; §6.3 401 handling; §6.4 home
    refresh; §4.4–§4.8 seek queueing, release-after-create, track-state
    retention, audio focus, tunneling; §7 R8/icons, doc pass. LOWs as
    touched.

After P1, re-run the device matrix with a DV P7 title, a DV P8 title, a
TrueHD Atmos title over an AVR, and a PGS-subbed 4K remux on both
platforms — those four are the cases this review predicts will change.

---

## 11. Addendum (2026-08-02) — the two commits that landed after the snapshot

While this review was being written, two server-side commits landed on the
same branch, both in §3's territory: the native-subtitle machinery the
Apple client executes. They are reviewed here at the same depth. Neither
changes any client-side finding or the §10 order — and both belong to
exactly the class §9 said code reading cannot reach: AVPlayer rejecting a
master playlist is invisible until a real device plays it, which is
evidently where these came from.

### 11.1 `ab5438ca2` — Make HLS subtitle autoselection valid. Sound.

**What it does:** enforces RFC 8216's rendition-group uniqueness rule.
Every `AUTOSELECT=YES` member of the `subs` group must be unique by
(LANGUAGE, FORCED, CHARACTERISTICS); AVPlayer rejects the whole master
when two plain same-language tracks collide — the common two-English-SRT
mux, which previously made the entire native path fail on device.
Duplicate tuples now emit `AUTOSELECT=NO` (still listed, still manually
selectable), the client-selected track carries `DEFAULT=YES` which forces
`AUTOSELECT=YES` (`autoselect = default || tuple_copies == 1` — the rule's
own precondition), and SDH/CC tracks gain the standard accessibility
`CHARACTERISTICS` pair. The tuple count is self-inclusive, so a selected
duplicate stays valid while its twin goes manual — checked against each
combination. Two focused tests pin the behavior.

**Consequences and one follow-up:** duplicate plain tracks no longer
OS-autoselect — plurx-driven selection (`DEFAULT` via `?subtitle=`) is
unaffected, and the code comment says so; deliberate. The SDH detection
sniffs titles ("sdh", "closed caption", …) because `SubtitleStream`
(`domain.rs:301-308`) doesn't carry ffprobe's
`disposition.hearing_impaired` — LOW: add the disposition to the probe and
demote the title sniff to a fallback.

### 11.2 `a2f5239f9` — Advertise complete native subtitle codecs. Sound; three edges worth one more pass.

**What it does:** the native master's `#EXT-X-STREAM-INF` now carries a
`CODECS` attribute — the primary rendition's formats plus `,wvtt` when
subtitle renditions are present, which AVPlayer requires before it will
play an otherwise-valid copy. Sessions record `hls_codecs`: copy sessions
derive it from the file (`dvh1` when DV is preserved, `hvc1` for HEVC,
`avc1` otherwise; audio maps the copied codec, or `mp4a.40.2` when the
session transcodes audio), transcode and cached sessions are hardcoded
`avc1,mp4a.40.2` (correct — that chain is H.264+AAC). Separately,
`shift_webvtt` now always stamps
`X-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:0` into the VTT header, so cue
times sync to the media timeline at zero offset too.

**Verified while reviewing:** `dvh1` agrees with the sample entry the copy
pipeline actually muxes (`-tag:v dvh1` for preserved DV,
`plurx-core/src/transcode/mod.rs:285-289`, asserted by its tests);
`"dolby_vision"` is the domain's real `hdr` vocabulary (`domain.rs:325`);
and the now-unconditional VTT rewrite preserves non-cue blocks — the block
loop pushes NOTE/STYLE blocks through untouched — so removing the
zero-offset early-return loses nothing.

**The edges** — none block today's consumers (only the Apple client
requests `?native=1`, and AVPlayer parses these strings leniently):

- MEDIUM (latent, nearest-term real case): the video arm falls through
  everything non-HEVC to `avc1`. An AV1 copy session — an AV1 MKV on
  hardware whose caps claim `av1` (A17-class iPhones today) — would
  advertise `avc1` over `av01` samples: actively wrong metadata on the
  newest devices. Add an `av01` arm before AV1-capable hardware meets the
  native path.
- LOW-MEDIUM: the strings are bare FourCCs, not RFC 6381 (`avc1` without
  `.PPCCLL`; `hvc1`/`dvh1` without profile.tier.level). AVPlayer
  tolerates; `mediastreamvalidator` will flag it; and any future consumer
  that feeds `CODECS` into an `isTypeSupported`-class check breaks —
  Chromium rejects a bare `hvc1`. The probe already holds
  profile/bit-depth/level, so full strings are derivable when convenient.
- LOW: `copied_audio_codec`'s `_ → mp4a.40.2` mislabels a genuinely copied
  FLAC/Opus/DTS track. Unreachable from current clients (none both claims
  those codecs and takes the copy path) — worth a comment saying so, or it
  bites silently when one does. (Nit, same file: the header stamp keys on
  `starts_with("WEBVTT")`, so a BOM'd VTT would skip it; ffmpeg-extracted
  VTT carries no BOM, unreachable today.)

### 11.3 Standing state after the addendum

Both commits carry their own unit tests and were gated natively if the
pre-commit hook ran (the bridge cannot run `make check`; this addendum is
a reading, not a build). The working tree still carries the two
uncommitted scaffolding files, so §6.6 — the iOS build break — remains the
first P0, and nothing here reorders §10. The three §11.2 edges slot into
P2.
