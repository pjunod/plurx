# Apple client feature parity

The web client remains plurx's reference viewer and administration surface.
The iOS/tvOS app should reach viewer parity without duplicating browser-only
server administration. This document records the boundary so “parity” means a
testable set of behaviors rather than a general impression.

> Status: the P0 playback blockers are implemented in source and compile for
> both iOS and tvOS. Real-device playback validation is still required before
> calling them shipped. P1 and P2 items below remain open.

## What parity means

- **P0 — correct playback.** A person can find a server, sign in once, browse,
  start a title, pause, seek, choose tracks, understand playback health, and
  finish an episode without switching to the web client.
- **P1 — daily-driver comfort.** Search, efficient subtitles, sync correction,
  adaptive quality, polished next/skip behavior, and Apple platform features.
- **P2 — breadth.** Offline media, playlists, advanced library controls, and
  accessibility/localization hardening.
- **Web-only administration.** Libraries, users, jobs, integrations, system
  diagnostics, and destructive server operations stay in the web app. Porting
  those would add two admin surfaces without improving living-room playback.

## Current matrix

| Area | Web client | iOS / tvOS now | Remaining work |
|---|---|---|---|
| Server setup | Manual address | Bonjour `_plurx._tcp` first; manual fallback; port 32400 | Validate discovery on bare metal, host-network Docker, iPhone, and Apple TV |
| Local-network permission | Browser permission model | Bonjour starts before auth; URL requests wait while the iOS prompt is open | Add UI that distinguishes Denied from multicast unavailable |
| Session | Local login and remembered token | Local login and silent reconnect | Move bearer token from `UserDefaults` to Keychain |
| Home and browse | Hubs, libraries, hierarchy | Hubs, libraries, show → season → episode | Sorting, filters, denser iPad/tvOS layouts |
| Search | Global search | Missing | P1: add API-backed search to the root navigation |
| Playback decision | Runtime caps, server delivery plan | Runtime VideoToolbox/display caps; executes `delivery` | Real-device codec/HDR matrix, especially Dolby Vision profiles |
| Transport | Play/pause, ±10, full seek | Explicit play/pause, ±10, full-film slider; HLS seeks reopen at the film position | Validate rapid/overlapping seek cancellation and long transcodes |
| iOS Now Playing | Not applicable | Known duration, elapsed time, pause/play/seek commands, `isLiveStream = false` | Add artwork and series/episode metadata |
| Audio tracks | Select and restart when needed | Select and restart at the same position | Add friendlier channel/codec labels; validate TrueHD/DTS fallback |
| Subtitles | Text WebVTT stays client-side; bitmap tracks burn in | Every selected track burns in for correctness | P1: make text WebVTT selectable without video re-encode; retain burn-in for PGS/VobSub |
| Quality | Auto adaptation, Original, explicit rungs | Server Auto plus explicit ladder rungs | P1: continuous adaptation and an honest Original option when compatible |
| Playback info | Detailed source, output, network, encoder, stalls | Source/output, access-log bitrate/stalls, server encode speed/ahead/delivery | Add TTFF, frame presentation rate, stall classification, and build stamp |
| Intro/credits | Manual and automatic skip | Manual marker button | P1: persisted auto-skip and next-episode handling for end credits |
| Autoplay | Next episode, then next season; default on | Same traversal and default | Add a cancelable countdown and “Up Next” metadata |
| Audio sync | Persisted per-file ±ms correction | Missing | P1: expose the existing server offset endpoint and restart at position |
| Progress/Trakt | Periodic and final progress | Every 10 seconds, exit, and natural end | Verify app interruption/background transitions |
| PiP/AirPlay | Browser/platform dependent | AVPlayer foundation only; no product-level controls or QA | P1: explicit PiP/AirPlay affordances and remote-device session tests |
| Offline/downloads | Missing | Missing | P2, only if product scope expands beyond server-connected playback |
| Accessibility | Browser semantics and keyboard controls | SwiftUI labels and tvOS focus basics | P2: VoiceOver, Dynamic Type, Reduce Motion, contrast, and focus audit |

## Recommended sequence

### 1. Prove the P0 path on real hardware

Use the same movie that exposed the `LIVE` problem and verify all delivery
modes, not merely one compatible MP4:

1. Direct-play MP4: pause, lock-screen pause, full-film seek, progress.
2. Copy/remux MKV: same controls, HDR preserved, audio switch.
3. Transcode: full-film seek restarts correctly; stats show encoder speed and
   server-ahead reserve.
4. Text subtitle and PGS subtitle: selection restarts at the same position and
   turning subtitles off restores the original delivery plan.
5. Episode ending: next episode begins; season rollover begins the first episode
   of the next season; a finale stays stopped.

This is the release gate because simulator builds cannot validate hardware
decode, HDR, background Now Playing, multicast discovery, or a real Siri Remote.

### 2. Remove unnecessary subtitle transcodes

The correctness-first Apple implementation burns all selected subtitles. It
works for every format, but a text track should not consume an encoder or alter
the video. The durable design is an HLS master playlist with WebVTT subtitle
renditions for text tracks and server burn-in only for bitmap tracks. Direct
play needs either the same HLS wrapper or an AVFoundation composition that can
present the external track. Keep one user-facing menu across both paths.

Acceptance: English SRT/ASS/VTT can be toggled instantly, does not create a
transcode session, keeps style/position where supported, and still aligns after
a resume or server-side seek.

### 3. Add the remaining web playback controls

In order:

1. Persisted auto-skip for intro and credits markers.
2. Per-file audio-sync adjustment using the existing server endpoint.
3. Continuous Auto quality changes using the advertised ladder and measured
   delivery reserve.
4. Cancelable Up Next countdown with episode artwork and metadata.
5. Explicit PiP and AirPlay controls with session cleanup tests.

### 4. Fill discovery and browsing comfort gaps

Add permission-denied guidance, search, library sorting/filtering, and Keychain
storage. Multicast discovery must remain best-effort: Bonjour normally does not
cross VLANs, guest Wi-Fi, VPNs, or a Docker bridge, so manual setup remains a
supported path rather than an error screen.

## Release gate

An Apple build is ready for broader TestFlight use when:

- both platform schemes build and their shared tests pass;
- the five real-hardware playback cases above pass on iPhone and Apple TV;
- a denied local-network permission produces an actionable screen and never a
  misleading “wrong password” message;
- lock-screen playback shows a duration and Pause, not `LIVE` and Stop;
- closing, seeking, changing tracks, and autoplay leave no orphan HLS session;
- all still-open P0 rows have an owner or are explicitly moved out of P0.
