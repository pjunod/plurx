# Android client parity

The Android client is the native plurx **viewer** for phones, foldables,
tablets, Android TV, and Google TV. This page records what “web parity” means
for that viewer and keeps server administration out of the comparison.

> Status (2026-08-02): native text subtitles landed — Android takes the same
> WebVTT renditions the Apple client does, selects them in place on a
> direct-played file, and opens a video-recipe-preserving session on a remux
> or transcode. Burn is now reserved for bitmap and styled tracks. **Not yet
> verified on hardware:** the §5.4 acceptance cases (no video encoder on a 4K
> HDR remux, PGS burn at 2160, a forced track auto-shown on a remux, no new
> session when toggling two text tracks) still need a device pass. The rest of
> the remediation handoff is
> [CLIENTS-REMEDIATION-PLAN.md](CLIENTS-REMEDIATION-PLAN.md) §5.
> Status: viewer parity exists and both the playback-negotiation arc
> ([CLIENTS-REMEDIATION-PLAN.md](CLIENTS-REMEDIATION-PLAN.md) §4.2–§4.5,
> §5.1–§5.6) and the P2 pass (§7.1–§7.3, §7.5, §7.6) have landed: Dolby Vision
> and route-aware audio caps, `force` and height promises, native text
> subtitles, `vod` sessions, the server's ladder, a TV focus graph whose
> destinations survive scrolling on both axes, rollup-aware watch filters, parallel first paint, the lifecycle-edge
> hygiene pass, and an R8-minified release build. The dynamic-range badge also
> reports what is being *delivered and rendered*, not only what the file
> carries ([MEDIA-BADGES-PLAN.md](MEDIA-BADGES-PLAN.md) §6).
>
> Two acceptance checks remain device-only and unproven in CI: `ShelfFocusTest`
> (needs a TV emulator or device) and the badge's on-screen behaviour on an HDR
> panel.
>
> Performance II N0 source parity is included in Android build 25: the Media3 controller
> posts authenticated `ttff`, passive six-second `stall`, and
> `playback_error` events through the shared client, with attempt and live HLS
> session identity. TTFF starts before the detail/decision requests. A stall is
> emitted with its final duration only after established, requested playback
> recovers; startup, paused buffering, and seek waits are excluded. Unit tests
> and lint prove the wire, controller wiring, reason, and timing contracts; a
> physical Android play producing a joined `ttff` row remains an explicit,
> unclaimed acceptance run.
>
> Build 25 also replaces Android 14+ boot-time `dataSync` service recovery with
> a persisted user-initiated data-transfer job. The local intent and selected
> network constraint are committed synchronously; Media3 stays paused and its
> HTTP source fails closed until the job supplies a network for both socket and
> DNS resolution. Android 6–13 retain the boot/Media3-restart service path.
> Task Manager and timeout stops persist as an explicit Resume state. An active
> build-24 transfer cannot be retroactively given a persisted UIDT registration
> after upgrade without a foreground user action, so it needs one Resume tap.

## Viewer surface

| Web viewer capability | Android implementation |
|---|---|
| Connect, login, remembered session | Native connect/login screens and DataStore session |
| Continue Watching, Next Up, Recently Added | Adaptive home rails |
| Category- or library-grouped home | Home grouping preference |
| Browse, pagination, sort, watch filters | Adaptive library grid; pages paint as they arrive, sort is client-side, and containers classify by `rollup` |
| Cross-library search | Native search screen |
| Movie/show/season/episode hierarchy | Native detail and episode rows |
| Home-video folders and photos | Folder navigation and photo viewer |
| Resume, restart, watched/unwatched | Detail actions and progress sync |
| Direct, remux, HLS transcode | Media3/ExoPlayer delivery-plan execution |
| Audio/subtitle choice | Embedded tracks are native; tracks the server marks `native` arrive as HLS renditions, and everything else burns on a session |
| Detail-screen track facts | Every audio and subtitle track with language, format, and forced/SDH markers, the server's default markers, and its five-state preferred-language verdict |
| Pre-play audio/subtitle choice | Both are chosen on the detail screen and applied on the first session open, with any burn-in cost disclosed before playback starts |
| Auto/original/fixed playback quality | Viewer preference and an in-player selector built from the server's advertised ladder |
| Bound stall downgrade | Server contract available: one `request_id` can name its predecessor and typed stall cause, with the normalized height replayed. Android does not yet reopen from its passive stall detector. Adopting it requires sending `quality_auto` — `sessionHeight` posts the source height for any burn before it consults quality, so an Auto viewer with a burned subtitle is otherwise read as a sticky manual pick — plus a client-side retry budget, which the server deliberately does not bound at the ladder floor. |
| Intro/credits markers | Manual skip or automatic skip; estimated markers show as "Skip Credits (estimated)" to distinguish from chapter-derived markers |
| Autoplay next episode | Ordered season/show traversal |
| A/V sync correction | Persistent per-file correction |
| Playback decision/stats | In-player information panel, including a "Dynamic range" row |
| Durable playback telemetry | TTFF, passive buffering-stall, and playback-error beacons with attempt/session context |
| Source-vs-delivered media badges | Dynamic-range chip dims and names what is on screen (`DV → HDR10`) |
| Classic, Terminal, noirr | Matching palettes, shapes, and typography |
| System, light, dark appearance | Independent appearance preference |

Android also adds platform-native behavior that the browser does not provide:
MediaSession integration, picture-in-picture, immersive playback, hardware
media keys, and D-pad focus states.

## Apple parity handoff

The handoff in [CLIENTS-REMEDIATION-PLAN.md](CLIENTS-REMEDIATION-PLAN.md) pins
the wire contracts, exact files, milestones, tests, hardware matrix, rollout
evidence, and non-goals. These are the subtitle outcomes it required, now
implemented and pinned by `PlaybackPolicyTest`:

- SRT/SubRip/WebVTT sends `native_subtitles = true`, `subtitle = <index>`,
  and no `subtitle_burn`; the selected quality and video recipe do not change.
- Media3 switches native text tracks with `TrackSelectionOverride` and turns
  them off by disabling `C.TRACK_TYPE_TEXT`; neither action creates a new HLS
  session.
- PGS, VobSub, `mov_text`, and styled ASS/SSA use `subtitle_burn` at source
  height. `/decision` reports `text: true` for `mov_text` and ASS/SSA — neither
  is a bitmap — but the session endpoint rejects both as native renditions and
  the master never advertises them. The client therefore routes on
  `SubTrackDto.native` (the server's own `is_native_text_subtitle`), falling
  back to a codec table only for a server that predates the field, and never on
  `text`. A 2160p WEB-DL MP4 with 23 `mov_text` tracks is what the distinction
  costs when it is missing: every track offered, every explicit pick a 400.
- On direct play those same tracks are free — the player reads the container's
  own track, and `/files/{id}/subs/{index}.vtt` would extract either format if
  asked, since that endpoint turns away only bitmaps
  (`plurxd/src/http/stream.rs:770`). Only a *session* has to burn them.
- Viewer-language automatic selection treats a forced disposition and a
  case-insensitive `Forced` title as equivalent signals. For file 5615 with
  English preferences, subtitle index 2 wins over the Italian container
  default.
- The shared authenticated OkHttp data source fetches capability playlist and
  WebVTT URLs. HLS sessions anchor to `media_origin_ms`; progressive remuxes
  read `X-Plurx-Media-Origin-Ms`, with `start_seconds` retained only as the
  old-server fallback. VOD state, resume, and seek handling retain that source
  timeline.

Do not side-load `/files/{id}/subs/{index}.vtt` into an offset HLS session: the
whole-file endpoint has no resume offset and would make cue timing incorrect.

## Detail-screen track facts and pre-play selection

The detail screen answers "does this have my audio and subtitles?" before
anything is decoded, and lets the viewer choose both. The whole policy boundary
is [CLIENTS.md](CLIENTS.md) §"Shared track facts — clients render the server's
answer"; nothing below re-derives it.

- Each media card lists every `audio_streams` and `subtitle_streams` entry.
  Audio rows carry language, title, channels, and codec; subtitle rows carry
  language, title, format (`SRT`, `PGS`, `VobSub`, `ASS`, `MOV Text`), and the
  forced and SDH markers. An untagged track is named `Unknown language` rather
  than left blank — that track is the reason a status can be `unknown`. A file
  with no subtitle tracks says "No subtitles in this file."
- The server's own picks — `playback_defaults.audio.selected_index` and
  `.subtitle.selected_index` — carry a **Default** chip, and each list is
  followed by one sentence for its `preferred_language_status`. All five states
  are distinct: `selected` ("English audio."), `available` ("English audio
  available — Japanese plays by default.", the dual-audio anime case),
  `missing` ("No English subtitles."), `unknown` ("Can't tell whether this has
  English subtitles — a track has no language tag.") and `no_tracks`. `unknown`
  is never folded into `missing`; a status this build does not recognize, and a
  server that omits `playback_defaults` entirely, print no sentence at all
  rather than an invented one.
- A choice is per playback. It is held per file, keyed on the item, and travels
  as optional `?audio=`/`?subtitle=` arguments on the player route — `-1` is
  Off, and an omitted argument means "no choice, keep the server's policy". It
  is never written back as a Playback setting, and the next item, the next
  episode, and Play-next all start from their own defaults.
- The choice reaches the **first** `/decision`, so the plan that comes back
  already carries it and no restart or re-buffer is needed to apply it. That
  plan is executed as given: the remux URL's `?audio=` is the server's, and the
  client normalizes rather than appends that one parameter so a later in-player
  switch cannot leave two of them on the wire. The subtitle travels in the HLS
  session-create body, and `delivery.audio` is repeated there. A choice other
  than the container's own default is never answered `direct`, so the verdict
  is re-read from every selection-aware decision instead of assumed stable.
- Direct play is the one transport that hands Media3 the whole container, so
  the selected audio track is pinned there with a `TrackSelectionOverride`
  matched by language and then by order within it. Without that pin ExoPlayer's
  own `setPreferredAudioLanguage` could put a different track on the speakers
  than the one the detail screen marks — the same reason text selection has
  always been carried by the controller rather than by the selector.
- Burn-in cost is disclosed before playback starts, from a selection-aware
  `/decision` preflight rather than a codec table: bitmap tracks are priced by
  `selection.subtitle_requires_burn_in`, and text tracks keep the existing
  `native` flag because ASS/SSA and `mov_text` carry text and still burn. A true
  `selection.subtitle_burn_in_blocked_by_hdr` is reported as "HDR playback is
  kept unchanged, so they will not be shown" — not as subtitles-on. A preflight
  that fails claims nothing; the in-player path still discloses the burn.

`TrackFactsTest` and `TrackSelectionTest` (JVM) pin the vocabulary, the route
encoding, the plan execution, and the pricing; `DetailTrackFactsTest`
(androidTest) pins the rendering and the click behavior.

## Administrative boundary

The embedded web app remains plurx's administrative control plane: first-server
setup, libraries, users, metadata keys, integrations, scan control, and system
logs. Keeping those mutations in one surface avoids reproducing high-impact
server administration on a TV remote. The Android Settings screen is therefore
for device-local viewer and playback preferences plus sign-out/change-server.

## Layout verification

The shared Compose UI has compact, expanded, and television form factors. The
v0.1 pass was built and exercised on API 36 AOSP profiles matching:

- Pixel 10 Pro XL — `1344×2992`, 480 dpi.
- Pixel 10 Pro Fold, open — `2076×2152`, 390 dpi.
- Android TV 1080p — `1920×1080`, 320 dpi.

The test gate is:

```bash
cd clients/android
./gradlew testDebugUnitTest :app:assembleDebug :app:lintDebug
./gradlew :app:connectedDebugAndroidTest  # once per running profile
```

The connected Compose test renders a core card/control surface under all three
themes. The manual pass uses a disposable plurx library to verify real home,
settings, detail, photo, and playback content rather than empty previews.

## D-pad focus

`ShelfFocusTest` (androidTest) is the reproduction
[CLIENTS-REMEDIATION-PLAN.md](CLIENTS-REMEDIATION-PLAN.md) §7.1 asked for
before the fix. Every movement in it is a real D-pad key event; a bare
`FocusRequester.requestFocus()` would prove nothing, because it consults
neither the declared order nor whether the destination is still attached.

**The requester lives on the container; the `up`/`down` order lives on the
card.** A shelf's `FocusRequester` used to ride on the card at index 0, which a
`LazyRow` disposes as soon as the viewer scrolls past it — every neighbouring
shelf then aimed `up`/`down` at a requester attached to nothing. It now rides on
the row's `LazyRow` behind a `focusGroup()`, which a horizontal scroll cannot
dispose. The `up`/`down` overrides cannot follow it there: a card collects focus
properties with `visitSelfAndAncestors(FocusProperties, untilType = FocusTarget)`,
and `LazyRow` carries a focus target of its own inside it (`ScrollableNode`
delegates a `FocusTargetModifierNode(Focusability.Never)`), so a block declared
on the row modifier is invisible to every card in the row. They are declared per
card, pointing at the *neighbour's* container.

**The vertical list is scrolled, not lazy.** Home's shelf list is a
`Column(verticalScroll)`: in a `LazyColumn` a shelf scrolled out of the window
detached its requester and the next press towards it threw
`IllegalStateException: FocusRequester is not initialized`. Spatial focus search
composes beyond-bounds lazy items to avoid exactly that, but a custom `up`/`down`
destination bypasses spatial search, so the rescue never runs. Three hub shelves,
the picker and one shelf per library (or per kind) is a small enough list to
compose eagerly; each shelf is still a `LazyRow`.

Poster cards and pickers carry one focus target apiece (`clickable` is already
focusable — hygiene, not a behaviour fix), and the "Group by" picker is a stop on
the vertical chain rather than a touch-only control: right-aligned, it is outside
the beam of a card on the left of the shelf above, so spatial search alone steps
straight past it.

**Run it on a TV profile.** It cannot run in CI or in a cloud sandbox — there
is no device — so it is written, compiled, and unproven until someone runs it
against `plurx_android_tv_1080p_api36`.
