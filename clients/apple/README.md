# plurx for Apple (iOS + tvOS)

A native **SwiftUI + AVPlayer** client for [plurx](../../README.md). One shared
source tree builds two apps — **iPhone/iPad** and **Apple TV** — from the same
screens and networking, wired to plurx's native `/api/v1`.

Playback goes through **AVPlayer/AVKit**, so it takes the Apple-native path the
project's [client plan](../../docs/CLIENTS.md) describes: files AVPlayer can take
(MP4/MOV, H.264/HEVC, AAC/AC3/E-AC3) **direct-play** the original untouched;
anything it can't (MKV, DTS/TrueHD, …) is delivered as the server's on-the-fly
**HLS** remux/transcode. The client probes the device's VideoToolbox decoders and
HDR display at runtime and sends that to `/decision`, so the server transcodes
only what this hardware genuinely can't play.

> Status: **v0.2.7**, build `64` in [`project.yml`](project.yml) — working
> development client. Browse, resume, discover, and play on both iOS and
> tvOS. Both targets compile against the iOS/tvOS 26.5 SDKs and share the
> same regression suite. Build 64 stops the delivery watchdog firing on a
> healthy player: a full forward buffer looks exactly like a wedge from the
> server's delivery meter, so the film clock and the buffered runway now have
> to agree before it recovers. Build 63 makes stall recovery un-foolable: one
> shared no-progress clock that regime flapping cannot reset, a longer
> bounded leash (then a visible error) where recovery used to disarm
> forever, a server-truth delivery watchdog fed by the 2-second status
> poll, and a rolling budget that turns automatic reopen storms into the
> failure screen. Build 62 shows the audio and subtitle tracks a file
> actually has on the detail screen and lets a viewer choose both before
> pressing Play. Build 61 corrects copy-HLS recovery to seek past the
> preceding keyframe and adds correlated AVPlayer, access-log, buffer, and
> server-supply evidence to every stall report. Build 60 requires a concrete
> PGS overlay track before
> treating a subtitle as an active overlay, so ordinary playback with no
> selected subtitle keeps its AVKit PiP controller attached. A physical H.264/
> AAC baseline on iPhone Air (iOS 26.6) and iPad Pro 13-inch (M4, iPadOS 26.6)
> reached active PiP and returned or stopped cleanly; the separate in-app PGS
> refusal still needs physical acceptance. Build 59 keeps the iOS Picture in
> Picture control
> reachable when AVKit cannot start yet or an in-app PGS overlay blocks system
> output, bounds the not-ready explanation to five seconds, and rejects stale
> availability from a detached PiP controller instead of sending a start
> command through a nil optional. Build 58 labels a
> recognized `pgs-v1` subtitle as an
> Overlay instead of the legacy Burn-in fallback, so the physical HDR/Dolby
> Vision run in
> [APPLE-PGS-OVERLAY-ACCEPTANCE.md](../../docs/APPLE-PGS-OVERLAY-ACCEPTANCE.md)
> can distinguish the path it exercised. Build 57 makes the tvOS progress bar
> an ordinary focus stop until Select engages scrubbing, so left/right can cross
> the transport row without seeking. Build 56 restores bidirectional tvOS focus between
> show/season header actions and their non-empty child shelves. Build 53 splits
> season episode cards into Play artwork and Details copy on iPhone/iPad; tvOS
> keeps one lifted card whose Select action plays. Build 52 preserves completed
> iPhone and iPad offline download
> locations across the equivalent `/private/var` and `/var` container spellings
> returned by the system. Build 51 divides a long final audio tail into bounded,
> sample-preserving HLS segments. A repeated near-end boundary now completes at
> the actual media position; a genuinely early repeat stops with telemetry and
> **Try Again** / **Close** actions instead of reopening forever. build `49`
> adds first-class audiobook details and
> playback through the shared audio player and progress path; physical-device
> acceptance remains pending. Build 46 adds a one-shot, session-joined TTFF
> beacon
> when the online player first advances, with separate cold-start and resume
> reasons; passing physical-device evidence remains pending. Build 29 added
> app-managed offline viewing on iPhone and iPad; the
> action remains hidden on tvOS. The
> default-off `pgs-v1` bitmap-overlay client draws PGS over the existing Dolby
> Vision, HDR, or SDR video
> without changing its bytes or reopening playback. It is ready for the paired
> iPhones, iPads,
> and Bedroom Apple TV, but has not been uploaded to TestFlight.
> It is not yet at full web parity; the exact boundary is in
> [Feature parity](#feature-parity).

## What works

- **Find servers automatically** through Bonjour (`_plurx._tcp`), with manual
  host entry as a fallback. A bare host uses the standard port `32400`.
- **Prompt for local-network access before sign-in** and hold requests while
  the iOS permission sheet is open, so first sign-in no longer fails behind it.
- **Connect & sign in**; the session is remembered and reconnects silently on
  next launch. The bearer token lives in the Keychain
  (`TokenVault.swift`) rather than `UserDefaults`, so installing a new build
  does not sign the viewer out; ordinary preferences stay in `UserDefaults`.
- **Add a server by scanning the QR code** the web sign-in screen shows, for
  the networks Bonjour cannot cross. The code carries the server address
  only — you still sign in inside the app.
- **Home** with Continue Watching / Next Up / Recently Added and your libraries.
- **Browse** libraries as a poster grid; open show → season → episode.
- **Detail** pages with backdrop, overview, Resume / Start-over.
- **App-managed offline viewing on iPhone and iPad.** Download queues are
  durable, transfer through two stable `AVAssetDownloadURLSession` background
  sessions for Wi-Fi-only and any-network policies, stay in app-private
  storage, and resume after app termination. A restored completed transfer
  still releases its server package and quota, even when Media3 finished while
  the process was dead. Downloads opens before server
  reconnection, plays a validated local asset without making a playback
  decision, and syncs the newest local progress timestamp after reconnect.
  Another signed-in profile cannot play the saved title but can remove its
  device storage from the Other Profiles section. tvOS exposes none of these
  controls.
- **On-demand player** with explicit play/pause, ±10 seconds, full-film seek,
  Skip Intro/Credits, a real runtime in iOS Now Playing instead of `LIVE`,
  audio/subtitle/quality menus, and a playback-info panel fed by the server's
  live encoder and delivery telemetry. In a full-screen iOS window, the status
  bar and Home indicator remain while any player overlay is visible and retire
  after the last overlay leaves; a windowed iPad status bar remains
  system-owned.
- **Subtitles**, split by what the format can survive. Text tracks
  (SRT/SubRip/WebVTT) are native HLS WebVTT renditions: selecting one, or
  Off, applies inside the running player item — no restart, no new session,
  no quality change, and no video encoder — and the cues stay with the
  picture through a resume and a seek. When the default-off server advertises
  `overlay: "pgs-v1"`, PGS is fetched as authenticated, positioned PNG objects
  and rendered in a layer synchronized to the existing player item. Selection,
  seek, switch, and Off do not restart or re-encode the video, so Dolby Vision
  and HDR remain on the original delivery. VobSub and styled ASS/SSA remain
  burn-only on SDR and are refused on HDR instead of silently replacing it.
- **Connect & sign in**; the bearer token lives in the **Keychain** (so a new
  development build does not sign you out) and the session reconnects silently
  on next launch. Address and token are written together, so changing servers
  cannot leave the previous server's credential on disk.
- **Home** with Continue Watching / Next Up / Recently Added and your
  libraries. Hubs, libraries, and Coming Soon are fetched in parallel and each
  shelf paints as it arrives; a refresh never blanks a populated dashboard, and
  returning from the player or from the background refreshes it.
- **Browse** libraries as a poster grid, with sort and watch-status filters —
  including for shows and seasons, which classify from the server's watch
  rollup. Pages paint as they arrive rather than after the last one.
- **Search** across the library from its own tab.
- **Detail** pages with backdrop, overview, Resume / Start-over, and a metadata
  badge row (resolution, codec, dynamic range).
- **On-demand player** with explicit play/pause, ±10 seconds, full-film seek,
  Skip Intro/Credits, a real runtime in iOS Now Playing instead of `LIVE`,
  audio/subtitle/quality menus, iOS Picture in Picture, and a playback-info
  panel fed by the server's live encoder and delivery telemetry. In a
  full-screen iOS window, the status bar and Home indicator remain while any
  player overlay is visible and retire after the last overlay leaves; a
  windowed iPad status bar remains system-owned.
- **Subtitle output limits**: the custom PGS layer is part of the in-app player
  surface, not the video frames. Picture in Picture and external playback are
  therefore unavailable while it is active, with a visible explanation. The
  app never falls back to an SDR burn merely to make those output modes work.
- **Honest dynamic-range badge.** The chip names what the *file* carries; when
  the session is delivering something else, its source half dims while the
  rendered result stays legible (`DV → HDR10`, `HDR → SDR`). The split answers
  both questions at once: which source capability was unavailable, and which
  dynamic range is functioning on screen. The playback-info panel spells out
  the server's reason.
- **Autoplay next episode**, including rollover to the next season; it is on
  by default to match the web client and can be toggled in the player or
  Settings.
- **Resume and progress** every 10 seconds and on exit, driving watch state and
  the server-side **Trakt** scrobble.
- **Default audio/subtitle language** (English out of the box) in Settings, mapped
  to AVPlayer's media-selection criteria.
- **tvOS**: D-pad focus via the SwiftUI focus engine (posters lift on focus),
  Menu-button to exit playback.

## Requirements

- **Xcode 26+** for App Store uploads; the deployment target remains iOS 17 /
  tvOS 17.
- [**XcodeGen**](https://github.com/yonaskolb/XcodeGen) to generate the project:
  `brew install xcodegen`.
- An Apple Developer account for signing (free personal team is fine for
  sideloading to your own devices).
- A reachable plurx server (default port `32400`). Because home servers are
  usually plain `http` on the LAN, both targets set
  `NSAppTransportSecurity → NSAllowsArbitraryLoads`.

## Build and install on your own devices

TestFlight delivery is automated through the macOS Ansible controller; it
tests both schemes, then archives and uploads each platform. See
[`docs/PUBLISHING.md`](../../docs/PUBLISHING.md#ansible-owns-the-repeatable-mobile-deploy).

```bash
cd clients/apple
xcodegen generate        # writes plurx.xcodeproj from project.yml
open plurx.xcodeproj
```

In Xcode, select the project, select each app target, and confirm **Signing &
Capabilities → Team** is your developer team. The committed project currently
uses Paul's team ID; another developer should replace it in `project.yml` and
regenerate.

### Pair an Apple TV

1. Put the Mac and Apple TV on the same network.
2. On Apple TV, open **Settings → Remotes and Devices → Remote App and
   Devices** and leave that screen open.
3. In Xcode, use **Window → Devices and Simulators**. This is a Window menu
   item; it is not under **Xcode → Open Developer Tool**.
4. Select the discovered Apple TV and enter the pairing code shown on the TV.
5. Choose the **plurx-tvOS** scheme and the paired Apple TV in Xcode's run
   destination menu, then press Run.

The first install can take longer while Xcode copies device support. Later
installs work wirelessly while the Mac and Apple TV can see each other.

### Install on iPhone or iPad

1. Connect the device by USB for the first install, unlock it, and accept
   **Trust This Computer** if prompted.
2. In **Window → Devices and Simulators**, confirm the device appears. Enable
   **Connect via network** there if wireless installs are wanted later.
3. If iOS asks, enable **Settings → Privacy & Security → Developer Mode** and
   restart the device.
4. Choose the **plurx-iOS** scheme and the phone or tablet as the run
   destination, then press Run.

On first launch, allow **Local Network** access. The app starts its Bonjour
browser before attempting saved-session or login requests, and those requests
wait for the choice instead of failing underneath the prompt. If access was
denied, re-enable plurx under **Settings → Privacy & Security → Local Network**.

Both targets share everything under `Sources/`; `project.yml` is the single place
that defines them (bundle id `tv.plurx.app`, deployment target 17.0). The native
subtitle design, deployment record, and remaining physical Dolby Vision failure
are documented in
[`docs/APPLE-NATIVE-SUBTITLES-HANDOFF.md`](../../docs/APPLE-NATIVE-SUBTITLES-HANDOFF.md).

### Live validation harnesses: the fencing rule

Ad-hoc "play this exact file against this exact server and watch what happens"
harnesses are useful, and one has already broken the tree once. Any live
validation entry point added to `Sources/` must obey all three of these, or it
does not get added:

1. **Platform- and DEBUG-fenced.** Wrap the entry point *and the view or type it
   references* in `#if os(tvOS) && DEBUG` (or whichever platform it targets).
   A `#if DEBUG` around the call site alone is not enough: the referenced type
   must not exist in a Release build either, or the Release configuration stops
   compiling. Both configurations of both schemes are part of the gate below
   precisely because this is the failure that got shipped.
2. **No credentials from launch arguments or the environment, ever in Release.**
   `ProcessInfo.processInfo.arguments` and `environment` are readable by anything
   that can launch the app; a bearer token or password arriving that way is a
   credential in a place the Keychain exists to avoid. Inside the DEBUG fence,
   read them if it helps; outside it, there must be no such code path at all.
   As of this writing the tree contains no reads of either — keep it that way.
3. **No writes to persisted settings.** The reverted `--live-scary-movie`
   harness overrode the saved `subLang`, so running it once silently changed the
   viewer's subtitle language for every later launch. A harness may pass its
   overrides down as parameters; it may not put them through `SettingsStore`.

The same rule is recorded as a standing non-goal in
[CLIENTS-REMEDIATION-PLAN.md](../../docs/CLIENTS-REMEDIATION-PLAN.md) §10.

## Test

The iOS and tvOS schemes run the shared XCTest source. Coverage includes origin
and Bonjour URL normalization, Bonjour resolution completing rather than
hanging, authenticated media URLs, bearer headers, the origin/token write
invariant, expired-session classification, HLS request semantics, in-place
AVPlayer media selection, the automatic-subtitle policy and the stream-reopen
queue, watch-status filtering for both leaves and rollup-bearing containers,
poster downsampling, the dynamic-range badge's three states and its
playback-info wording, PGS manifest/path validation, source-to-item timeline
conversion, authored-canvas layout, item replacement, and the invariant that
PGS selection never burns or reopens direct video, plus the playback duration
and progress passed into progress reporting.

```bash
cd clients/apple
xcodegen generate

# Debug: build and run the tests on each platform.
xcodebuild -project plurx.xcodeproj -scheme plurx-iOS  -configuration Debug \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' test

xcodebuild -project plurx.xcodeproj -scheme plurx-tvOS -configuration Debug \
  -destination 'platform=tvOS Simulator,name=Apple TV 4K (3rd generation)' test

# Release: build only — the configuration a DEBUG-fenced harness can break
# without any Debug build noticing. Run both, every time.
xcodebuild -project plurx.xcodeproj -scheme plurx-iOS  -configuration Release \
  -destination 'generic/platform=iOS Simulator' build

xcodebuild -project plurx.xcodeproj -scheme plurx-tvOS -configuration Release \
  -destination 'generic/platform=tvOS Simulator' build
```

Four green runs mean the shared Swift source and both platforms' conditional
branches compile in **both** configurations, the app launches in each simulator,
and the client contracts above still hold. It does not replace real-device
playback testing. Substitute whatever simulator names `xcrun simctl list
devicetypes` offers on your Xcode; the `generic/platform=…` destinations avoid
naming one at all, and building for a simulator rather than a device keeps
code signing out of the loop.

## How playback decides

`Caps.swift` builds the device capability set:

- **Video**: `h264` always; `hevc` and `av1` when VideoToolbox reports hardware
  decode for them.
- **Audio**: `aac, ac3, eac3, alac, mp3` (AVPlayer's set — DTS/TrueHD excluded).
- **Container**: `mp4, mov, m4v` (what AVPlayer direct-plays).
- **HDR**: on when `AVPlayer.eligibleForHDRPlayback` says the current output
  path can present HDR.
- **Dolby Vision**: profiles 5 and 8 only, and only when the generic HDR gate
  is open and `AVPlayer.availableHDRModes` names Dolby Vision for the current
  device/display path. Apple deprecated that format-specific property in OS
  26 without providing a format-specific replacement; it remains the signal
  that prevents an HDR10-only HDMI path from being over-claimed as DV.

On play the client calls `GET /files/{id}/decision?<caps>` and executes the
server's **delivery plan**:

- `direct` → AVPlayer streams `/files/{id}/direct?token=…`, seeking natively
  over HTTP range. A file that merely *contains* native text tracks stays on
  this path: since 2026-08-02 the client enters a session only when a native
  subtitle is actually selected, which costs one reopen at that moment
  instead of taxing every play of every subtitled file with a segmenter
  session.
- `remux` → `POST /files/{id}/hls/sessions` with `copy: true`: the source video
  repackaged into HLS **untouched**, so a 4K HEVC/HDR MKV reaches the screen at
  full quality with no encoder running (audio re-encodes only when the plan
  says so). The playlist is capability-authed by its own session id, so no
  token is needed on it — which is also what lets an Apple TV fetch it itself
  during AirPlay.
- `transcode` → the same `POST` with **no height named**: Auto is the server's
  choice, because the rung depends on which encoder wins and only the create
  response knows that.

Dolby Vision is preserved on the first attempt. Before real playback, if an
HDR10- or HLG-compatible Profile 8 title produces no film-time progress for
roughly 12 seconds, the client retries once with only the Dolby Vision metadata
removed. The 10-bit HEVC base picture and HDR grade remain intact; a full
compatibility transcode is reserved for a second startup failure. Once an HDR
item has advanced for at least five seconds, a later interruption reconnects
the same HDR delivery once and then stops visibly on an immediate repeat — it
never hides a transport fault by switching established playback to SDR.

Both the decision and the session response also carry
`delivered_dynamic_range` (`dolby_vision` / `hdr10` / `hlg` / `sdr`) — what the
bytes on the wire actually hold, as opposed to what the file holds. The player
compares it with the source grade and with `AVPlayer.eligibleForHDRPlayback` to
dim the unavailable source half while keeping the rendered result fully lit.
It is a readout only: nothing in the client's decision, capability, or session
request reads it back.

Either session starts at the resume point, carries this player's `playback_id`
(the server's supersession key) plus a per-attempt `request_id` (so a replayed
create recovers the same session), and is released with a `DELETE` the moment
playback ends instead of waiting out the server's idle reaper.

For a growing HLS session, AVPlayer still sees an EVENT playlist because the
server cannot honestly publish `ENDLIST` until every segment exists. The Apple
client therefore publishes the known title duration and `isLiveStream = false`
to iOS Now Playing, provides its own transport, and implements non-native seeks
by reopening the same stream at the requested film position. Faking an HLS VOD
playlist would truncate the movie; the client-side timeline is the safe fix.
Growing copy and transcode items also request a 60-second forward buffer. That
preference is not a cap: a physical iPad fetched about 120 seconds ahead. The
server therefore retains 180 seconds behind the download frontier — the
measured 120-second lead · 30 seconds of back buffer · 30 seconds for
retry/reload. A shorter window let AVPlayer's download frontier outrun the
contract, so the server could prune a fetched segment before AVPlayer presented
or retried it.

## Project layout

```
clients/apple/
  project.yml            XcodeGen spec — the iOS + tvOS targets
  Sources/               all shared Swift (compiled into both apps)
    Session, Models, PlurxAPI, Caps, SettingsStore, AppModel   (core)
    TokenVault                                        (Keychain bearer token)
    ServerDiscovery                                            (Bonjour)
    Theme, Components, AuthImage                               (UI support)
    PlurxApp + RootView, AuthViews, HomeView, LibraryView,
    DetailView, SearchView, SettingsView                       (screens)
    PlayerController, PlayerView, PlayerSurface     (AVPlayer, incl. iOS PiP)
  Tests/                 shared iOS + tvOS unit tests
  Resources/Assets.xcassets   iOS app icon
  Resources/tvOS.xcassets     tvOS layered Brand Assets
```

One authenticated path backs everything: API/image requests carry the bearer
header, and AVPlayer/image URLs that can't set headers carry `?token=` inline —
both accepted by the server's `AuthUser` extractor.

## Feature parity

The implementation and prioritized remaining work are tracked in
[Apple client feature parity](../../docs/APPLE-CLIENT-PARITY.md). In short,
browse/search/play/resume, sorting and watch filters, full-film seek, explicit
transport controls, Now Playing, native text subtitles, track and quality
selection, playback stats, markers, PiP, and next-episode autoplay are present.
Copied Dolby Vision is green on the physical Bedroom Apple TV: the client
selects DV from the current output's format modes, the server preserves the
RPU, the master advertises `SUPPLEMENTAL-CODECS`, and AVPlayer reaches
`readyToPlay`. These are still outstanding:

- **Automatic** intro/credits skipping. Manual Skip buttons are present.
- Continuous adaptive quality changes and audio-sync controls.
- Playlists and **AirPlay** polish. Search, iOS **PiP**, and app-managed
  iPhone/iPad offline downloads are present; the offline flow still needs the
  physical-device acceptance recorded in the implementation plan.
- **tvOS launch storyboard** — cosmetic only (a black frame at launch), not a
  blocker for upload. The layered Brand Assets themselves are committed under
  `Resources/tvOS.xcassets` and wired up via
  `ASSETCATALOG_COMPILER_APPICON_NAME: tvOS`; see
  [docs/PUBLISHING.md](../../docs/PUBLISHING.md) §4 for what each store-facing
  key buys and what breaks without it.

## License

Same as the plurx workspace.
