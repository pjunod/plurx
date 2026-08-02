# Apple client feature parity

The web client remains plurx's reference viewer and administration surface.
The iOS/tvOS app should reach viewer parity without duplicating browser-only
server administration. This document records the boundary so “parity” means a
testable set of behaviors rather than a general impression.

The implementation history, deployment evidence, and unresolved copied-Dolby-
Vision failure are recorded in
[APPLE-NATIVE-SUBTITLES-HANDOFF.md](APPLE-NATIVE-SUBTITLES-HANDOFF.md).

> Status (2026-08-02): the native text-subtitle path is implemented and
> covered on both Apple simulator targets. The physical Apple TV accepts the
> native WebVTT master and selects the requested rendition; the exact
> copied-Dolby-Vision regression file still falls back after a CoreMedia
> `-12927` rejection, which is the one open failure
> ([APPLE-NATIVE-SUBTITLES-PLAN.md](APPLE-NATIVE-SUBTITLES-PLAN.md) §5.4).
> The version deployed on the fleet is the earlier server at `787eaa6`: the
> 2026-08-02 selection, timing, and extraction fixes are committed locally
> only and reach a node when Paul pushes and runs the ansible playbooks,
> which pin every node to `origin/main`. The Apple fixes need a build ≥ 6.
> The real-hardware matrix below has not been re-run since those fixes.

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
| Session | Local login and remembered token | Local login, silent reconnect, bearer token in the Keychain (`TokenVault.swift`) so a new build does not sign the viewer out | — |
| Home and browse | Hubs, libraries, hierarchy | Hubs, libraries, show → season → episode | Sorting, filters, denser iPad/tvOS layouts |
| Search | Global search | Native API-backed search | Improve keyboard, history, and tvOS focus polish |
| Playback decision | Runtime caps, server delivery plan | Runtime VideoToolbox/display caps; executes `delivery` | Real-device codec/HDR matrix, especially Dolby Vision profiles |
| Transport | Play/pause, ±10, full seek | Explicit play/pause, ±10, full-film slider; HLS seeks reopen at the film position | Validate rapid/overlapping seek cancellation and long transcodes |
| iOS Now Playing | Not applicable | Known duration, elapsed time, pause/play/seek commands, `isLiveStream = false` | Add artwork and series/episode metadata |
| Audio tracks | Select and restart when needed | Select and restart at the same position | Add friendlier channel/codec labels; validate TrueHD/DTS fallback |
| Subtitles | Text WebVTT stays client-side; bitmap tracks burn in | SRT/SubRip/WebVTT use native HLS renditions; PGS, VobSub, and styled ASS/SSA retain burn-in | Complete the Android parity milestone and broaden physical-device coverage |
| Quality | Auto adaptation, Original, explicit rungs | Server Auto plus explicit ladder rungs | P1: continuous adaptation and an honest Original option when compatible |
| Playback info | Detailed source, output, network, encoder, stalls | Source/output, access-log bitrate/stalls, server encode speed/ahead/delivery | Add TTFF, frame presentation rate, stall classification, and build stamp |
| Intro/credits | Manual and automatic skip | Manual marker button | P1: persisted auto-skip and next-episode handling for end credits |
| Autoplay | Next episode, then next season; default on | Same traversal and default | Add a cancelable countdown and “Up Next” metadata |
| Audio sync | Persisted per-file ±ms correction | Missing | P1: expose the existing server offset endpoint and restart at position |
| Progress/Trakt | Periodic and final progress | Every 10 seconds, exit, and natural end | Verify app interruption/background transitions |
| PiP/AirPlay | Browser/platform dependent | Explicit iOS PiP controls on the AVPlayer surface | Add AirPlay affordances and remote-device session tests |
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
4. Text subtitle and PGS subtitle, which behave differently on purpose. A text
   track (SRT/SubRip/WebVTT) and Off apply **inside the running player** — no
   restart, no new session, no quality change, no video encoder — and the cues
   stay with the picture across a seek and a resume. The one restart is the
   first native selection on a file that was direct-playing, which enters a
   session once (§2); every selection after it is in place. A PGS track still
   reopens at the same film position and burns at source height, and turning
   subtitles off restores the original delivery plan.
5. Episode ending: next episode begins; season rollover begins the first episode
   of the next season; a finale stays stopped.

This is the release gate because simulator builds cannot validate hardware
decode, HDR, background Now Playing, multicast discovery, or a real Siri Remote.

### 2. Native subtitles and the accepted session tradeoff

Implemented for SRT/SubRip/WebVTT. The server advertises those tracks as HLS
WebVTT renditions, including language, title, forced, default, and accessibility
metadata. AVPlayer selects and disables the renditions in place, so switching a
text track does not replace the player item, change quality, add an FFmpeg
subtitle filter, or encode video. Resume and seek offsets are reflected in the
rendition segment timeline.

PGS, VobSub, and styled ASS/SSA still reopen at the same film position and burn
at source height. This is intentional: those formats cannot be represented as
ordinary WebVTT without losing pixels or important styling.

Merely *containing* native text tracks no longer costs a file its direct play.
Until 2026-08-02 it did: any playable file with a text track opened through a
copy-HLS session even when its video could have used the raw direct URL, so
every such title paid for a segmenter session whether or not the viewer ever
opened the subtitle menu — and on the Bedroom Apple TV that path degrades
further, to a compatibility transcode, through the unresolved `-12927` failure.
Paul chose the alternative on 2026-08-02: stay on true direct play, and enter a
session at the moment a native subtitle is actually selected. The cost is one
reopen at that boundary, paid only by viewers who turn subtitles on, and every
selection after it is in place. Automatic selection cannot trigger it either —
it is constrained to native formats, so it can never start a burn on its own,
the single exception being a forced track, which burns at source height.

The cross-client implementation handoff is
[CLIENTS-REMEDIATION-PLAN.md](CLIENTS-REMEDIATION-PLAN.md), especially §5.4.

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
