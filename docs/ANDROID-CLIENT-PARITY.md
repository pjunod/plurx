# Android client parity

The Android client is the native plurx **viewer** for phones, foldables,
tablets, Android TV, and Google TV. This page records what “web parity” means
for that viewer and keeps server administration out of the comparison.

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
| Audio/subtitle choice | Native plus server-selected track menu |
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
