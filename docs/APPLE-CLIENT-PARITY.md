# Apple client feature parity

The web client remains plurx's reference viewer and administration surface.
The iOS/tvOS app should reach viewer parity without duplicating browser-only
server administration. This document records the boundary so “parity” means a
testable set of behaviors rather than a general impression.

The implementation history, deployment evidence, and resolved copied-Dolby-
Vision investigation are recorded in
[APPLE-NATIVE-SUBTITLES-HANDOFF.md](APPLE-NATIVE-SUBTITLES-HANDOFF.md).

> Status (2026-08-14): source is v0.2.7, Apple build 58. Native text
> subtitles, the cinematic detail surface, stable seek/recovery, truthful
> delivered-range badges, and app-managed offline viewing on iPhone/iPad have
> landed. Build 58 distinguishes a recognized `pgs-v1` Overlay from the
> Burn-in fallback in the subtitle menu and ships the decidable physical-iPad
> run in
> [APPLE-PGS-OVERLAY-ACCEPTANCE.md](APPLE-PGS-OVERLAY-ACCEPTANCE.md).
> Build 57 requires Select to engage tvOS progress scrubbing, leaving
> left/right free to cross the transport row without a seek. Build 56 restores
> bidirectional tvOS focus between show/season header actions and their non-empty
> child shelves. Build 53 gives season episode artwork a direct Play action
> while the copy remains Details on iPhone/iPad; tvOS keeps one lifted card
> whose Select action plays. Build 52 preserves completed offline asset
> locations across the
> equivalent `/private/var` and `/var` container spellings returned by the
> system. Build 51 divides long final audio tails into bounded HLS segments,
> completes repeated boundaries in the final 5% at their actual media position,
> and gives genuinely early repeats a telemetered **Try Again** / **Close**
> failure instead of another automatic reopen. Build 49 adds first-class
> audiobook details and playback through the
> shared audio player and progress path. Source and simulator coverage verifies
> audio-container direct-play routing, resume/Start over selection, global
> progress, and missing-part advancement; physical-device acceptance remains
> pending. The native scrubber remains local to the current audiobook part and
> audiobook offline packages are not yet supported. Build 46 also emits the
> Performance II N0 TTFF beacon at the first
> advancing online frame, including the live HLS session id when one exists and
> separating cold starts from resumes; passing real-device ingest remains
> unclaimed. The default-off `pgs-v1` overlay client
> is staged but is not a release claim until the server gate and physical
> matrix pass. Copied Dolby
> Vision was resolved on the physical Apple TV 2026-08-03; the historical
> `-12927` investigation is superseded by
> [APPLE-NATIVE-SUBTITLES-HANDOFF.md](APPLE-NATIVE-SUBTITLES-HANDOFF.md)'s
> resolved status. Repository evidence still says build 58 has not reached
> TestFlight and the deployment ledger still ends at server `787eaa6`, so
> publishing plus the broader real-hardware/offline matrix remain release
> gates.

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
| Session | Local login and remembered token | Local login, silent reconnect, bearer token in the Keychain (`TokenVault.swift`) so a new build does not sign the viewer out | — |
| Home and browse | Hubs, libraries, hierarchy | Hubs, libraries, show → season → episode | Sorting, filters, denser iPad/tvOS layouts |
| Session | Local login and remembered token | Local login and silent reconnect; bearer in the Keychain; origin and token are written together, so changing servers cannot leave the previous server's credential on disk; a 401/403 on any request after bootstrap clears the token and returns to the login screen instead of printing "Server returned 401" on every screen | Verify Keychain behavior across device restore and app replacement |
| Home and browse | Hubs, libraries, hierarchy | Hubs, libraries, show → season → episode; in a season listing, artwork plays that episode while the copy opens its detail page on iOS/iPadOS, and tvOS keeps one focus target whose Select action plays; sort and watch-status filters, with containers classified from the server's `rollup`; hubs/libraries/Coming Soon fetched in parallel, shelves and library pages published as they arrive, spinner only over an empty screen, and a refresh on player dismissal and on `scenePhase == .active` | Denser iPad/tvOS layouts; a device pass on very large libraries |
| Search | Global search | Native API-backed search | Improve keyboard, history, and tvOS focus polish |
| Playback decision | Runtime caps, server delivery plan | Runtime VideoToolbox/display caps; executes `delivery` | Real-device codec/HDR matrix, especially Dolby Vision profiles |
| Transport | Play/pause, ±10, full seek | Explicit play/pause, ±10, full-film slider; on tvOS the progress bar is an ordinary focus stop until Select engages scrubbing, so left/right cross between the transport and option groups without seeking, engaged left/right step by ±10 seconds, and Select/Menu leave scrubbing. Up reaches a visible skip marker above the bar and is deliberately inert when none is present. Hidden controls retain the separate four-direction remote seek path. A seek inside the growing playlist's advertised window moves the item's own clock instantly (`seekRoute`, mapped through `media_origin_ms`, held 1.5 s off the live edge), while out-of-window targets reopen the session at the film position after a 350 ms coalescing pause so a press burst costs one create; a seek or track change issued during a stream change is queued and replayed once, last writer wins, instead of being dropped, and the predecessor item's supersession 404s no longer advance the recovery ladder mid-change. In a full-screen iOS window, the status bar and Home indicator stay while any player overlay is visible and retire after the last one leaves; windowed iPad status bars remain system-owned. | Device-verify in-window scrubs land without a spinner, out-of-window scrubs recover cleanly, iPad full-screen/Split View chrome, iPhone portrait/landscape, PiP return, engage-to-scrub feel and hammered tvOS step-seeks during a quality change on a physical Siri Remote, and long transcodes |
| iOS Now Playing | Not applicable | Known duration, elapsed time, pause/play/seek commands, `isLiveStream = false` | Add artwork and series/episode metadata |
| Audio tracks | Select and restart when needed | Select and restart at the same position | Add friendlier channel/codec labels; validate TrueHD/DTS fallback |
| Subtitles | Text WebVTT stays client-side; bitmap tracks burn in | SRT/SubRip/WebVTT use native HLS renditions, chosen by the server's `native` flag rather than a local codec guess; PGS can use the staged default-off `pgs-v1` overlay while VobSub, `mov_text`, styled ASS/SSA, and unrecognized overlay versions retain burn-in/refusal; the menu labels Overlay and Burn-in separately; automatic selection never starts a burn except a forced track (see §2) | Run [the physical acceptance procedure](APPLE-PGS-OVERLAY-ACCEPTANCE.md) on iPad Pro, then complete the separate iPhone and Apple TV rows |
| Quality | Auto adaptation, Original, explicit rungs | Server Auto plus explicit ladder rungs; a change that fails to create its session leaves the current stream playing | P1: continuous adaptation and an honest Original option when compatible |
| Playback info | Detailed source, output, network, encoder, stalls | Source/output, dynamic range, access-log bitrate/stalls, server encode speed/ahead/delivery, and selected-subtitle route/state (`PGS overlay · preparing`, ready overlay, unavailable, native WebVTT, or burned in); recovery actions, shorter self-recovered AVPlayer stalls, and a one-shot TTFF measurement reach the bounded server client log | Add frame presentation rate and build stamp; validate TTFF ingest and sub-threshold stall cadence on physical iPad |
| Media badges | Source badges on detail pages; source-vs-delivered dynamic range in the player | Same shape: detail pages carry resolution/codec/dynamic range source-only; the player's chip dims and names what is actually being delivered (§5) | Extend the same mechanism to audio (Atmos → AAC) and resolution (4K → rung), each with its own truth table |
| Intro/credits | Manual and automatic skip | Manual marker button | P1: persisted auto-skip and next-episode handling for end credits |
| Autoplay | Next episode, then next season; default on | Same traversal and default | Add a cancelable countdown and “Up Next” metadata |
| Audio sync | Persisted per-file ±ms correction | Missing | P1: expose the existing server offset endpoint and restart at position |
| Progress/Trakt | Periodic and final progress | Every 10 seconds, exit, and natural end | Verify app interruption/background transitions |
| PiP/AirPlay | Browser/platform dependent | Explicit iOS PiP controls on the AVPlayer surface; while a PGS application overlay is active, PiP and external playback are refused with a visible explanation because those outputs do not carry the sibling overlay layer, and the app never substitutes an SDR burn | Physically confirm the documented refusal on iPad Pro; add a dedicated AirPlay affordance and remote-device session tests for non-overlay playback |
| Offline/downloads | Missing | App-managed HLS packages on iPhone/iPad; durable background transfer, local-only playback, progress merge, quota/activity UI; `/private/var` and `/var` spellings of the same app container persist as one relaunch-safe asset path, and rejected locations leave a local diagnostic without exposing the path; hidden on tvOS | Physical background/process-death, airplane-mode, selected-subtitle, midpoint-seek, and removal-reconciliation matrix |
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
   session once (§2); every selection after it is in place. A PGS track carrying
   recognized `overlay: "pgs-v1"` stays on the existing video item and draws
   positioned images without an encoder; without that capability it retains the
   legacy SDR burn or HDR refusal. Use
   [the iPad Pro procedure](APPLE-PGS-OVERLAY-ACCEPTANCE.md) to prove which path
   ran and whether HDR/Dolby Vision stayed unchanged.
5. Episode ending: next episode begins; season rollover begins the first episode
   of the next season; a finale stays stopped.

This is the release gate because simulator builds cannot validate hardware
decode, HDR, background Now Playing, multicast discovery, or a real Siri Remote.

### 2. Native subtitles, the session tradeoff, and who decides it

Implemented for SRT/SubRip/WebVTT. The server advertises those tracks as HLS
WebVTT renditions, including language, title, forced, default, and accessibility
metadata. AVPlayer selects and disables the renditions in place, so switching a
text track does not replace the player item, change quality, add an FFmpeg
subtitle filter, or encode video. Resume and seek offsets are reflected in the
rendition segment timeline.

PGS with recognized `overlay: "pgs-v1"` instead uses the staged application
overlay and never reopens or re-encodes video. PGS without that advertised
capability, VobSub, MP4 `mov_text`, and styled ASS/SSA retain the legacy reopen
and source-height burn on SDR; the client refuses that burn while HDR/Dolby
Vision is already being delivered. Those formats cannot be represented as
ordinary WebVTT without losing pixels, timing, or important styling, and the
server correctly refuses to publish them as text renditions.

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
#### The session tradeoff is a setting, defaulting to today's behavior

**Decision, v0.2: make it changeable.** The tradeoff below is real in both
directions, so Settings → Subtitles → **Subtitle switching** decides it, and
its default is exactly what the app has always done.

| Setting | What a play does | What it costs |
|---|---|---|
| **Instant** (default) | Any file carrying an SRT/SubRip/WebVTT track opens through a copy-HLS session even when its video could have used the raw direct URL | A server session plus a segmenter for that title, on every play, where the web and Android clients serve raw bytes over an HTTP range request |
| **After a short pause** | The same file direct-plays, and rebuilds as a copy-HLS session at the same film position the first time a subtitle is chosen | One clean restart, and only for viewers who actually use subtitles |

The whole of the setting is one pure function,
`PlayerController.needsNativeSubtitleSession(hasNativeTextTrack:readiness:subtitlesInUse:)`;
`open()` reads nothing else about it. A file with *no* native text track
direct-plays under both settings — there is nothing a session could publish, so
a PGS or `mov_text` track can never cost a direct play.

*What Instant costs.* A session and a segmenter per play, and nothing else:
`copy: true` means the video is repackaged untouched, so resolution, HDR, Dolby
Vision, and bitrate are identical to direct play, and no encoder is attached.

*What Instant buys.* Every text track exists as an HLS rendition before the
viewer opens the subtitle menu, so turning subtitles on, switching between two
languages, and turning them off again are all AVPlayer media selections on the
item that is already playing. No reopen, no reseek, no black frame, no lost
buffer. Subtitles are most often reached for *mid-scene*, after a line was
missed — which is exactly when a restart is most expensive to the viewer. That
is why it remains the default.

*What the alternative gives back.* Most plays never open the subtitle menu at
all, and on those the server does no work: the Apple client behaves like the web
and Android clients and pulls raw bytes. The first selection takes the same
clean reopen a bitmap burn already takes, at `realPositionMs()`, so the film
resumes where it was.

Three rules keep the alternative from degrading into repeated restarts:

- The choice is read **once, when playback starts**. Changing it mid-film never
  rebuilds the stream under the person watching.
- A subtitle chosen automatically at cold start (the policy below) counts as
  in use, so an auto-applied track is visible from the first frame rather than
  after a restart.
- Once a text track has been asked for, the stream **keeps** its renditions for
  the rest of the title, including after subtitles are turned off again.
  Dropping back to direct play would be a second restart nobody asked for.

Both branches are pinned by
`testSubtitleReadinessDecidesWhetherAPlayInvolvesTheServerAtAll` and
`testFirstSubtitleChoiceDuringDirectPlayRebuildsTheStreamOnceAndNoMore`.

#### Which tracks can be a rendition: the server's `native`, not `text`

`SubTrackDto` carries two booleans that are easy to confuse. `text` is
`!is_bitmap_subtitle` — "there are characters in here". `native` is
`is_native_text_subtitle` (`subrip|srt|webvtt|vtt`) — "this can be an HLS WebVTT
rendition". They disagree on MP4 `mov_text` and on styled ASS/SSA, and the
server's segmenter enforces `native`: a non-native track is absent from the HLS
master, and an explicit pick of one is answered with 400.

The Apple client used to derive that answer locally from the codec string. It
now decodes `native` and prefers it, falling back to the local codec list only
against a server that does not send the field — one property,
`SubtitleTrack.isNativeHLS`, read by every site that asks "can this be a
rendition?": the master ordinal (`nativeSubtitleOrdinal`), the burn test
(`subtitleRequiresBurn`), the automatic-selection policy, the session guard
above, and the player-menu route label (`Overlay` for recognized `pgs-v1`,
`Burn-in` for the fallback).

**That native-classification change did not alter Apple playback**, and that is
worth stating plainly rather than claiming a fix that did not happen. The
client's hardcoded list and `is_native_text_subtitle` name the same four codecs,
so the shape that
motivated this — a 2160p WEB-DL MP4 with 23 `mov_text` streams, every one of
them `text` and none of them in the HLS master — was already routed to burn-in
by iOS/tvOS and already labelled "Burn-in" in the menu. The client that listed
those tracks as ordinary selectable text is the one reading `text`, not this
one.

What the change buys is that the two can never diverge. The client no longer
holds an opinion about a policy the server owns: if the segmenter ever learns to
convert `mov_text`, or the list is corrected, Apple follows without a client
release, and a track the server declines to publish can never shift the ordinal
a viewer's pick resolves to. That is the same reason this client executes
`delivery` rather than re-deriving policy from `method`.

#### The staged PGS overlay reports its actual path

Build 58 corrects the one remaining contradictory diagnostic. The playback-
info panel already separated `PGS overlay · preparing`, ready overlay,
unavailable overlay, native WebVTT, and burned-in subtitles, while its Method
and Dynamic range rows independently showed whether video stayed direct/remuxed
and HDR/Dolby Vision. The subtitle menu still appended `Burn-in` to every
non-WebVTT track, including a recognized `pgs-v1` overlay. It now appends
`Overlay` for that one protocol and keeps `Burn-in` for missing or unknown
overlay capabilities.

The output-mode limitation is deliberate. The custom `AVSynchronizedLayer` is
part of the in-app player and is not carried by Apple's system PiP or external-
playback video surfaces. With PGS active, the app blocks PiP and disables
external playback. If AirPlay is already active, selecting PGS is refused. Each
case shows the same explanation and leaves the existing HDR/Dolby Vision video
unchanged; neither silently starts a burn.

[APPLE-PGS-OVERLAY-ACCEPTANCE.md](APPLE-PGS-OVERLAY-ACCEPTANCE.md) names build
58, the operator-owned server gate/restart and TestFlight upload, the exact PGS
codec check, the diagnostic strings, 100/150 ms timing bounds, item-replacement
trigger or explicit `Not run`, and Yes/No PiP/AirPlay observations. A direct-
play title with one audio track cannot satisfy the replacement row merely by
seeking: without the visible stream-change spinner, no item replacement was
observed. Until that physical matrix is recorded, the client remains staged
behind the server's default-off gate.

#### What automatic subtitle selection is allowed to do

Automatic (cold-start) selection never starts a burn — **except a forced track,
which may, always at source height**. The whole policy is one pure function,
`PlayerController.automaticSubtitleIndex`, and the carve-out is one line of it:

| Track shape (within the viewer's preferred language) | Cold start |
|---|---|
| Forced — disposition flag *or* "forced" in the title, any codec | apply; bitmap forced tracks are the one permitted automatic burn, at source height |
| Default-flagged and native text (SRT/SubRip/WebVTT) | apply through the free rendition path |
| Default-flagged but bitmap (PGS/VobSub), `mov_text`, or styled (ASS/SSA) | never automatic — explicit selection only |
| Merely the same language, unflagged | never automatic |

The rows that decline are the substance. Before this, a 4K HDR remux whose only
English subtitle track was a default-flagged PGS cold-started as a burn
transcode on every play: an encoder slot, H.264, and no HDR, for a track nobody
had asked for. Manual selection is untouched — a viewer who picks a PGS track
still gets a burn, at source height.

"Native text" in that table means `SubtitleTrack.isNativeHLS`, which prefers the
server's `native` flag — not the broader `text`. A default-flagged `mov_text`
track is text and would otherwise have been auto-applied to a rendition that does
not exist.

The cross-client implementation handoff is
[CLIENTS-REMEDIATION-PLAN.md](CLIENTS-REMEDIATION-PLAN.md), especially §5.4;
the session tradeoff above is its §6.4 — resolved by making it a setting rather
than by choosing for the viewer — and the selection table is its §3.1.

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

### Episode cards split on touch; tvOS keeps one focus target

On iPhone and iPad, the landscape artwork in a season listing is a Play action;
the title, metadata, and media badges below it are one separate Details action
that opens `Route.item`. VoiceOver receives those two labels separately, because
combining them would make the interaction depend on sighted hit testing again.
This changes those season shelves from the previous portrait `PosterCard` tiles
to 16:9 `EpisodeCard` tiles; the landscape layout is what gives artwork and copy
independent touch targets without changing every other poster shelf.

tvOS deliberately does not copy that split. The focus engine selects a card,
not a geometric half of one, so an episode card keeps its existing single lift
and Select starts playback. This trades the card's previous detail navigation
for the primary couch action instead of exposing two actions the Siri Remote
cannot aim at. The season header's Play action and each card both reach the
same `PlayContext` player cover; this is browse routing, not a second playback
policy.

### Show and season shelves remain reachable from the header

tvOS treats both halves of the show hierarchy boundary as directional focus
sections: the header actions and the non-empty child shelf below them. Pressing
Down from Play or the watch action therefore enters a show's seasons shelf, and
does the same from a season header into its episodes shelf. Pressing Up returns
to the header even when the viewer moved horizontally before leaving the shelf.
The same section wrapper covers every populated horizontal browse rail,
including Home's Coming Soon row, and the playable-detail hero above child
folders; neighboring vertical bands therefore do not reintroduce the asymmetric
focus boundary elsewhere. An empty `detail.children` still renders no shelf
because `MediaRow`'s pre-existing non-empty guard omits it; focus routing must
not hide an API or scan problem.

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
- the staged PGS path has the physical evidence required by
  [APPLE-PGS-OVERLAY-ACCEPTANCE.md](APPLE-PGS-OVERLAY-ACCEPTANCE.md), or remains
  explicitly default-off and unclaimed;
- the five real-hardware playback cases above pass on iPhone and Apple TV;
- a denied local-network permission produces an actionable screen and never a
  misleading “wrong password” message;
- lock-screen playback shows a duration and Pause, not `LIVE` and Stop;
- closing, seeking, changing tracks, and autoplay leave no orphan HLS session;
- all still-open P0 rows have an owner or are explicitly moved out of P0.
