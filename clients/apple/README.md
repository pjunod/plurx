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

> Status: **v0.1.0** — working development client. Browse, resume, discover,
> and play on both iOS and tvOS. Both targets compile against the iOS/tvOS
> 26.5 SDKs and share the same regression suite. It is not yet at full web
> parity; the exact boundary is in [Feature parity](#feature-parity).

## What works

- **Find servers automatically** through Bonjour (`_plurx._tcp`), with manual
  host entry as a fallback. A bare host uses the standard port `32400`.
- **Prompt for local-network access before sign-in** and hold requests while
  the iOS permission sheet is open, so first sign-in no longer fails behind it.
- **Connect & sign in**; the session is remembered (Keychain-free
  `UserDefaults` token) and reconnects silently on next launch.
- **Home** with Continue Watching / Next Up / Recently Added and your libraries.
- **Browse** libraries as a poster grid; open show → season → episode.
- **Detail** pages with backdrop, overview, Resume / Start-over.
- **On-demand player** with explicit play/pause, ±10 seconds, full-film seek,
  Skip Intro/Credits, a real runtime in iOS Now Playing instead of `LIVE`,
  audio/subtitle/quality menus, and a playback-info panel fed by the server's
  live encoder and delivery telemetry.
- **Subtitles**, including bitmap formats, through reliable server burn-in.
  Changing a subtitle restarts at the same film position.
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

## Test

The iOS and tvOS schemes run the shared XCTest source. Coverage includes origin
and Bonjour URL normalization, authenticated media URLs, bearer headers, HLS
request semantics, playback duration and progress, automatic subtitle
selection, and in-place AVPlayer media selection.

```bash
cd clients/apple
xcodegen generate

xcodebuild -project plurx.xcodeproj -scheme plurx-iOS \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' test

xcodebuild -project plurx.xcodeproj -scheme plurx-tvOS \
  -destination 'platform=tvOS Simulator,name=Apple TV 4K (3rd generation)' test
```

A passing run means both the shared Swift source and the platform-specific
branches compile, the app launches in each simulator, and the client contracts
above still hold. It does not replace real-device playback testing.

## How playback decides

`Caps.swift` builds the device capability set:

- **Video**: `h264` always; `hevc` and `av1` when VideoToolbox reports hardware
  decode for them.
- **Audio**: `aac, ac3, eac3, alac, mp3` (AVPlayer's set — DTS/TrueHD excluded).
- **Container**: `mp4, mov, m4v` (what AVPlayer direct-plays).
- **HDR**: on when the display advertises any HDR mode.

On play the client calls `GET /files/{id}/decision?<caps>` and executes the
server's **delivery plan**:

- `direct` → AVPlayer streams `/files/{id}/direct?token=…`, seeking natively
  over HTTP range.
- `remux` → `POST /files/{id}/hls/sessions` with `copy: true`: the source video
  repackaged into HLS **untouched**, so a 4K HEVC/HDR MKV reaches the screen at
  full quality with no encoder running (audio re-encodes only when the plan
  says so). The playlist is capability-authed by its own session id, so no
  token is needed on it — which is also what lets an Apple TV fetch it itself
  during AirPlay.
- `transcode` → the same `POST` with **no height named**: Auto is the server's
  choice, because the rung depends on which encoder wins and only the create
  response knows that.

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

## Project layout

```
clients/apple/
  project.yml            XcodeGen spec — the iOS + tvOS targets
  Sources/               all shared Swift (compiled into both apps)
    Session, Models, PlurxAPI, Caps, SettingsStore, AppModel   (core)
    ServerDiscovery                                            (Bonjour)
    Theme, Components, AuthImage                               (UI support)
    PlurxApp + RootView, AuthViews, HomeView, LibraryView,
    DetailView, SettingsView                                  (screens)
    PlayerController, PlayerView                              (AVPlayer)
  Tests/                 shared iOS + tvOS unit tests
  Resources/Assets.xcassets   iOS app icon
```

One authenticated path backs everything: API/image requests carry the bearer
header, and AVPlayer/image URLs that can't set headers carry `?token=` inline —
both accepted by the server's `AuthUser` extractor.

## Feature parity

The implementation and prioritized remaining work are tracked in
[Apple client feature parity](../../docs/APPLE-CLIENT-PARITY.md). In short,
browse/play/resume, full-film seek, explicit transport controls, Now Playing,
track and quality selection, playback stats, markers, and next-episode autoplay
are present. These are still outstanding:

- **Automatic** intro/credits skipping. Manual Skip buttons are present.
- Sidecar WebVTT rendering. Today's Apple subtitle path burns every selected
  track for correctness, which forces a re-encode; native text subtitles should
  eventually remain selectable without touching the video.
- Continuous adaptive quality changes and audio-sync controls.
- **Search**, filters/sorting, playlists, downloads/offline, **PiP**, and
  **AirPlay** polish.
- **tvOS launch storyboard** — cosmetic only (a black frame at launch), not a
  blocker for upload. The layered Brand Assets themselves are committed under
  `Resources/tvOS.xcassets` and wired up via
  `ASSETCATALOG_COMPILER_APPICON_NAME: tvOS`; see
  [docs/PUBLISHING.md](../../docs/PUBLISHING.md) §4 for what each store-facing
  key buys and what breaks without it.

## License

Same as the plurx workspace.
