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

## Viewer surface

| Web viewer capability | Android implementation |
|---|---|
| Connect, login, remembered session | Native connect/login screens and DataStore session |
| Continue Watching, Next Up, Recently Added | Adaptive home rails |
| Category- or library-grouped home | Home grouping preference |
| Browse, pagination, sort, watch filters | Adaptive library grid |
| Cross-library search | Native search screen |
| Movie/show/season/episode hierarchy | Native detail and episode rows |
| Home-video folders and photos | Folder navigation and photo viewer |
| Resume, restart, watched/unwatched | Detail actions and progress sync |
| Direct, remux, HLS transcode | Media3/ExoPlayer delivery-plan execution |
| Audio/subtitle choice | Embedded tracks are native; server-controlled text still burns and reopens (parity work §5.4) |
| Auto/original/fixed playback quality | Viewer preference and in-player selector |
| Intro/credits markers | Manual skip or automatic skip |
| Autoplay next episode | Ordered season/show traversal |
| A/V sync correction | Persistent per-file correction |
| Playback decision/stats | In-player information panel |
| Classic, Terminal, noirr | Matching palettes, shapes, and typography |
| System, light, dark appearance | Independent appearance preference |

Android also adds platform-native behavior that the browser does not provide:
MediaSession integration, picture-in-picture, immersive playback, hardware
media keys, and D-pad focus states.

## Apple parity handoff

The gap is large enough to keep out of the Apple subtitle release. The detailed
handoff in [CLIENTS-REMEDIATION-PLAN.md](CLIENTS-REMEDIATION-PLAN.md) pins the
wire contracts, exact files, milestones, tests, hardware matrix, rollout
evidence, and non-goals for a separate implementation session.

The subtitle milestone must preserve these outcomes:

- SRT/SubRip/WebVTT sends `native_subtitles = true`, `subtitle = <index>`,
  and no `subtitle_burn`; the selected quality and video recipe do not change.
- Media3 switches native text tracks with `TrackSelectionOverride` and turns
  them off by disabling `C.TRACK_TYPE_TEXT`; neither action creates a new HLS
  session.
- PGS, VobSub, and styled ASS/SSA use `subtitle_burn` at source height.
- Viewer-language automatic selection treats a forced disposition and a
  case-insensitive `Forced` title as equivalent signals. For file 5615 with
  English preferences, subtitle index 2 wins over the Italian container
  default.
- The shared authenticated OkHttp data source fetches capability playlist and
  WebVTT URLs. Session `start_seconds`, VOD state, resume, and seek handling
  retain the source timeline.

Do not side-load `/files/{id}/subs/{index}.vtt` into an offset HLS session: the
whole-file endpoint has no resume offset and would make cue timing incorrect.

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
