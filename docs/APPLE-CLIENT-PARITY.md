# Apple client feature parity

The web client remains plurx's reference viewer and administration surface.
The iOS/tvOS app should reach viewer parity without duplicating browser-only
server administration. This document records the boundary so “parity” means a
testable set of behaviors rather than a general impression.

The implementation history, deployment evidence, and unresolved copied-Dolby-
Vision failure are recorded in
[APPLE-NATIVE-SUBTITLES-HANDOFF.md](APPLE-NATIVE-SUBTITLES-HANDOFF.md).

> Status: the native text-subtitle path is implemented, covered on both Apple
> simulator targets, and deployed server-side. The physical Apple TV accepts
> the native WebVTT master and selects the requested rendition; the exact
> copied-Dolby-Vision regression file still falls back after a CoreMedia
> `-12927` rejection. The P2 comfort pass has landed in source — watch filters
> for show libraries, incremental first paint, the ImageIO image pipeline,
> centralized session expiry, and the delivered-dynamic-range badge (§5) — and
> is awaiting its device run. Other P1 items below remain open.

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
| Server setup | Manual address | Bonjour `_plurx._tcp` first; manual fallback; port 32400; resolution runs on the main run loop and always returns or fails within its own deadline | Validate discovery on bare metal, host-network Docker, iPhone, and Apple TV; eventual `NWBrowser`/`NWConnection` migration (`NetService` is legacy) |
| Local-network permission | Browser permission model | Bonjour starts before auth; URL requests wait while the iOS prompt is open | Add UI that distinguishes Denied from multicast unavailable |
| Session | Local login and remembered token | Local login and silent reconnect; bearer in the Keychain; origin and token are written together, so changing servers cannot leave the previous server's credential on disk; a 401/403 on any request after bootstrap clears the token and returns to the login screen instead of printing "Server returned 401" on every screen | Verify Keychain behavior across device restore and app replacement |
| Home and browse | Hubs, libraries, hierarchy | Hubs, libraries, show → season → episode; sort and watch-status filters, with containers classified from the server's `rollup`; hubs/libraries/Coming Soon fetched in parallel, shelves and library pages published as they arrive, spinner only over an empty screen, and a refresh on player dismissal and on `scenePhase == .active` | Denser iPad/tvOS layouts; a device pass on very large libraries |
| Search | Global search | Native API-backed search | Improve keyboard, history, and tvOS focus polish |
| Playback decision | Runtime caps, server delivery plan | Runtime VideoToolbox/display caps; executes `delivery` | Real-device codec/HDR matrix, especially Dolby Vision profiles |
| Transport | Play/pause, ±10, full seek | Explicit play/pause, ±10, full-film slider; HLS seeks reopen at the film position; a seek or track change issued during a stream change is queued and replayed once, last writer wins, instead of being dropped | Device-verify hammered tvOS step-seeks during a quality change, and long transcodes |
| iOS Now Playing | Not applicable | Known duration, elapsed time, pause/play/seek commands, `isLiveStream = false` | Add artwork and series/episode metadata |
| Audio tracks | Select and restart when needed | Select and restart at the same position | Add friendlier channel/codec labels; validate TrueHD/DTS fallback |
| Subtitles | Text WebVTT stays client-side; bitmap tracks burn in | SRT/SubRip/WebVTT use native HLS renditions; PGS, VobSub, and styled ASS/SSA retain burn-in; automatic selection never starts a burn except a forced track (see §2) | Complete the Android parity milestone and broaden physical-device coverage |
| Quality | Auto adaptation, Original, explicit rungs | Server Auto plus explicit ladder rungs; a change that fails to create its session leaves the current stream playing | P1: continuous adaptation and an honest Original option when compatible |
| Playback info | Detailed source, output, network, encoder, stalls | Source/output, dynamic range, access-log bitrate/stalls, server encode speed/ahead/delivery | Add TTFF, frame presentation rate, stall classification, and build stamp |
| Media badges | Source badges on detail pages; source-vs-delivered dynamic range in the player | Same shape: detail pages carry resolution/codec/dynamic range source-only; the player's chip dims and names what is actually being delivered (§5) | Extend the same mechanism to audio (Atmos → AAC) and resolution (4K → rung), each with its own truth table |
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
4. Text subtitle and PGS subtitle: selection restarts at the same position and
   turning subtitles off restores the original delivery plan.
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

#### The accepted tradeoff: text tracks cost a session, not a restart

**Decision, v0.2: keep it.** Any playable file containing an SRT/SubRip/WebVTT
track opens through a copy-HLS session even when its video could have used the
raw direct URL. The guard is one line —
`let direct = normalMode == "direct" && … && !hasNativeSubtitles` in
`PlayerController.open()`.

*What it costs.* A server session plus a segmenter for that title, on every
play, where the web and Android clients serve raw bytes over an HTTP range
request. Nothing else: `copy: true` means the video is repackaged untouched, so
resolution, HDR, Dolby Vision, and bitrate are identical to direct play, and no
encoder is attached.

*What it buys.* Every text track exists as an HLS rendition before the viewer
opens the subtitle menu, so turning subtitles on, switching between two
languages, and turning them off again are all AVPlayer media selections on the
item that is already playing. No reopen, no reseek, no black frame, no lost
buffer. Subtitles are most often reached for *mid-scene*, after a line was
missed — which is exactly when a restart is most expensive to the viewer.

*Why it is not an accident.* The alternative was considered and declined for
v0.2, not overlooked. A direct-play session that has no subtitle renditions
cannot grow them: the first selection has to tear the item down and rebuild it
as a copy session, which is the restart this design exists to avoid.

*The alternative, if Paul wants it.* **Direct-until-first-toggle**: direct-play
the file, and reopen as a copy-HLS session the first time a subtitle is
selected. That reclaims the session for the majority of plays where subtitles
are never touched, at the price of one clean restart at first selection — the
code already restarts cleanly for burns, so this is a contained change to the
same `guard`, not a redesign. It is *not* implemented; it is an open option,
and switching to it is a behavior change that belongs in its own commit with its
own line in this document.

#### What automatic subtitle selection is allowed to do

Automatic (cold-start) selection never starts a burn — **except a forced track,
which may, always at source height**. The whole policy is one pure function,
`PlayerController.automaticSubtitleIndex`, and the carve-out is one line of it:

| Track shape (within the viewer's preferred language) | Cold start |
|---|---|
| Forced — disposition flag *or* "forced" in the title, any codec | apply; bitmap forced tracks are the one permitted automatic burn, at source height |
| Default-flagged and native text (SRT/SubRip/WebVTT) | apply through the free rendition path |
| Default-flagged but bitmap (PGS/VobSub) or styled (ASS/SSA) | never automatic — explicit selection only |
| Merely the same language, unflagged | never automatic |

The rows that decline are the substance. Before this, a 4K HDR remux whose only
English subtitle track was a default-flagged PGS cold-started as a burn
transcode on every play: an encoder slot, H.264, and no HDR, for a track nobody
had asked for. Manual selection is untouched — a viewer who picks a PGS track
still gets a burn, at source height.

The cross-client implementation handoff is
[CLIENTS-REMEDIATION-PLAN.md](CLIENTS-REMEDIATION-PLAN.md), especially §5.4;
the session tradeoff above is its §6.4 and the selection table is its §3.1.

### 3. Add the remaining web playback controls

In order:

1. Persisted auto-skip for intro and credits markers.
2. Per-file audio-sync adjustment using the existing server endpoint.
3. Continuous Auto quality changes using the advertised ladder and measured
   delivery reserve.
4. Cancelable Up Next countdown with episode artwork and metadata.
5. Explicit PiP and AirPlay controls with session cleanup tests.

### 4. Fill discovery and browsing comfort gaps

Search, library sorting/filtering, and Keychain token storage are done. What
remains here is permission-denied guidance — a screen that distinguishes a
denied Local Network prompt from multicast simply being unavailable. Multicast
discovery must remain best-effort: Bonjour normally does not cross VLANs, guest
Wi-Fi, VPNs, or a Docker bridge, so manual setup remains a supported path rather
than an error screen.

Two browsing gaps closed with it, both about not lying while loading. Watch
filters now classify shows and seasons from the `rollup` the server attaches to
containers, because a container has no watch row of its own — before this,
"Watched" and "In progress" filtered a TV library to nothing and "Unwatched"
listed finished series. And first paint no longer waits on the last response:
hubs, libraries, and Coming Soon go out together, shelves and library pages
publish as they arrive, and the spinner is reserved for a screen that has never
held anything — so pull-to-refresh, leaving the player, and returning from the
background all refresh in place instead of blanking.

### 5. What the dynamic-range badge is allowed to claim

The player's HDR/DV chip used to be built from the source probe alone, which
made it a statement about the file rather than about the picture: a Dolby Vision
disc remux read `DV` while a forced 1080p rung was delivering tone-mapped SDR.
It now answers both questions at once, from three inputs and no more:

| Layer | Where it comes from |
|---|---|
| Source | `SourceSummary.hdr` / `hdr_format` — what the file carries |
| Delivered | `delivered_dynamic_range` on the decision, overridden by the same field on the HLS session response the moment a session attaches |
| Rendered | `AVPlayer.eligibleForHDRPlayback` — delivered HDR on an SDR display is rendered SDR |

The chip text always starts from the source grade, because that claim stays true
either way. When the rendered grade differs it dims to ~0.45 and appends an
arrow (`DV → HDR10`, `HDR → SDR`), with a spelled-out accessibility label
("Dolby Vision, playing as HDR10") and a matching "Dynamic range" row in the
playback-info panel carrying the server's own reason string.

**Eligibility is the whole of the local signal, deliberately.** AVFoundation
exposes nothing public about which HLS variant is active, so there is no honest
per-stream confirmation to add; `UIScreen.currentEDRHeadroom` and
`AVDisplayManager` are explicit non-goals in
[MEDIA-BADGES-PLAN.md](MEDIA-BADGES-PLAN.md) §9. Eligibility is read at render
time rather than cached, so turning an Apple TV's Dolby Vision output off in
Settings changes the badge without relaunching the app.

The badge is a reporter. Nothing it computes reaches a decision, a capability
query, or a session request; the audio and resolution badges stay source-only in
this pass because their truth tables are not written yet. Detail pages stay
source-only by design — there is no session there to report a downgrade against.

## Release gate

An Apple build is ready for broader TestFlight use when:

- both platform schemes build and their shared tests pass;
- the five real-hardware playback cases above pass on iPhone and Apple TV;
- a denied local-network permission produces an actionable screen and never a
  misleading “wrong password” message;
- lock-screen playback shows a duration and Pause, not `LIVE` and Stop;
- closing, seeking, changing tracks, and autoplay leave no orphan HLS session;
- all still-open P0 rows have an owner or are explicitly moved out of P0.
