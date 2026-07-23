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

> Status: **v0.1.0** — first working cut. Browse, resume, and play on both iOS
> and tvOS. See [Roadmap](#roadmap) for what's deliberately not here yet.
>
> Heads-up: unlike the Android client, this was authored without a compile step
> in the build environment — it's clean, idiomatic Swift targeting the iOS 17 /
> tvOS 17 SDK, but you'll do the first real build in Xcode.

## What works

- **Connect & sign in** to any plurx server by address; the session is remembered
  (Keychain-free `UserDefaults` token) and reconnects silently on next launch.
- **Home** with Continue Watching / Next Up / Recently Added and your libraries.
- **Browse** libraries as a poster grid; open show → season → episode.
- **Detail** pages with backdrop, overview, Resume / Start-over.
- **Player** (`VideoPlayer`) with the native transport, scrubber, and
  audio/subtitle track menus — plus the native info panel on tvOS. Resume seeks to
  your last position; progress posts every 10s and on exit, driving watch state
  and the server-side **Trakt** scrobble.
- **Default audio/subtitle language** (English out of the box) in Settings, mapped
  to AVPlayer's media-selection criteria.
- **tvOS**: D-pad focus via the SwiftUI focus engine (posters lift on focus),
  Menu-button to exit playback.

## Requirements

- **Xcode 15+** (iOS 17 / tvOS 17 SDK).
- [**XcodeGen**](https://github.com/yonaskolb/XcodeGen) to generate the project:
  `brew install xcodegen`.
- An Apple Developer account for signing (free personal team is fine for
  sideloading to your own devices).
- A reachable plurx server (default port `32600`). Because home servers are
  usually plain `http` on the LAN, both targets set
  `NSAppTransportSecurity → NSAllowsArbitraryLoads`.

## Build

```bash
cd clients/apple
xcodegen generate        # writes plurx.xcodeproj from project.yml
open plurx.xcodeproj
```

In Xcode: pick the **plurx-iOS** or **plurx-tvOS** scheme, set your Team under
Signing & Capabilities, choose a device/simulator, and Run.

Both targets share everything under `Sources/`; `project.yml` is the single place
that defines them (bundle id `tv.plurx.app`, deployment target 17.0).

## How playback decides

`Caps.swift` builds the device capability set:

- **Video**: `h264` always; `hevc` and `av1` when VideoToolbox reports hardware
  decode for them.
- **Audio**: `aac, ac3, eac3, alac, mp3` (AVPlayer's set — DTS/TrueHD excluded).
- **Container**: `mp4, mov, m4v` (what AVPlayer direct-plays).
- **HDR**: on when the display advertises any HDR mode.

On play the client calls `GET /files/{id}/decision?<caps>`:

- `direct_play` → AVPlayer streams `/files/{id}/direct?token=…`, seeking natively
  over HTTP range.
- otherwise → `POST`-less `GET /files/{id}/hls/start` opens a native **HLS**
  session (capability-authed by its own session id, so no token needed on the
  playlist), starting at the resume point.

## Project layout

```
clients/apple/
  project.yml            XcodeGen spec — the iOS + tvOS targets
  Sources/               all shared Swift (compiled into both apps)
    Session, Models, PlurxAPI, Caps, SettingsStore, AppModel   (core)
    Theme, Components, AuthImage                               (UI support)
    PlurxApp + RootView, AuthViews, HomeView, LibraryView,
    DetailView, SettingsView                                  (screens)
    PlayerController, PlayerView                              (AVPlayer)
  Resources/Assets.xcassets   iOS app icon
```

One authenticated path backs everything: API/image requests carry the bearer
header, and AVPlayer/image URLs that can't set headers carry `?token=` inline —
both accepted by the server's `AuthUser` extractor.

## Roadmap

Deliberately out of scope for v0.1.0:

- **Transcode scrubber timeline** — the HLS fallback starts the session at the
  resume point, so on a *transcoded* title the native scrubber is relative to that
  point (you can't scrub before it without re-entering). Direct play (the common
  case) has the full, correctly-labelled timeline. A base-offset/seek-restart pass
  will fix transcode seeking.
- **Skip Intro / Skip Credits** buttons (the server sends markers; the Android
  client already surfaces them).
- **Search**, **downloads/offline**, **PiP**, **AirPlay** polish.
- **tvOS app icon** — the target intentionally references no App Icon so it builds
  and runs in the Simulator without Brand Assets. Add an "App Icon & Top Shelf
  Image" asset (and set `ASSETCATALOG_COMPILER_APPICON_NAME`) before installing on
  a physical Apple TV.

## License

Same as the plurx workspace.
