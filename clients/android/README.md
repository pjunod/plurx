# plurx for Android

A native **Kotlin / Jetpack Compose** client for [plurx](../../README.md) that runs on the
same APK across **phones, tablets, and Android TV / Google TV** (Sony, Nvidia Shield,
Android-based Fire TV, Chromecast with Google TV, etc.).

Playback is **Media3 / ExoPlayer**, so this is the client that direct-plays what the
browser can't: Matroska (`.mkv`) containers, TS, and HEVC / AV1 / DTS / TrueHD wherever the
device's decoders support them. The app probes those decoders at runtime and hands the
result to the server's `/decision` endpoint, so the server only transcodes what *this*
hardware genuinely can't play — everything else streams the original file untouched.

> Status: **v0.1.0** — first working cut. Browse, resume, and play across phone/tablet and
> TV. See [Roadmap](#roadmap) for what's intentionally not here yet.

## What works

- **Connect & sign in** to any plurx server by address (`http://192.168.1.10:32600`); the
  session is remembered so the app reconnects silently on next launch.
- **Home** with Continue Watching, Next Up, and Recently Added hubs, plus your libraries.
- **Browse** libraries as a poster grid; open shows → seasons → episodes.
- **Detail** pages with backdrop, overview, and Resume / Start-over.
- **Player** built on ExoPlayer:
  - **Direct play** of the original file with native seeking (HTTP range).
  - **Transcode / remux fallback** via `stream.mp4`, with seek handled by re-requesting the
    stream at the new offset.
  - Custom Compose controls, a scrubber, ±10s, **Skip Intro / Skip Credits** from server
    markers, and an **audio / subtitle** track menu.
  - Progress is reported back to the server every 10s and on pause/exit, which drives your
    watch state and the server-side **Trakt** scrobble.
- **Default audio & subtitle language** (English out of the box) in Settings, wired into
  ExoPlayer's track selector.
- **Android TV**: ships a `LEANBACK_LAUNCHER` entry and a TV banner, so it appears on the TV
  home row; the UI is D-pad focusable (posters grow + outline on focus).

## Requirements

- **Android 6.0 (API 23)** or newer — covers phones and the vast majority of Android
  TV / Google TV boxes.
- A reachable plurx server (default port `32600`). Because home servers are usually plain
  `http` on the LAN, the app sets `usesCleartextTraffic="true"`.

## Build

Three ways, in order of least host setup — all produce the same
`app/build/outputs/apk/debug/app-debug.apk`.

### In Docker — recommended for a server (no host JDK or SDK)

A pinned image ([`Dockerfile`](Dockerfile): JDK 17 + the exact Android SDK) is
the whole toolchain, so a headless box needs only Docker. From the **repo
root**:

```bash
make android          # builds the image once, then the debug APK in it
```

The APK lands in `clients/android/app/build/outputs/apk/debug/`, owned by you —
the container runs as your UID (`-u $(id -u)`), so nothing is left root-owned.
First run pulls the base image + SDK and downloads the Gradle deps; later runs
reuse the cached image and a `clients/android/.gradle-docker` cache.
`make android-image` rebuilds just the image (e.g. after bumping the SDK). Needs
the Docker daemon running and outbound internet on the first build.

### Manually — host JDK 17 + Android SDK

On a headless Ubuntu/Debian server:

```bash
# 1. JDK 17 (AGP 8.7 needs it; 21 also works, avoid 23/24)
sudo apt update && sudo apt install -y openjdk-17-jdk unzip
export JAVA_HOME=/usr/lib/jvm/java-17-openjdk-amd64

# 2. Android command-line tools — must end up at cmdline-tools/latest/
mkdir -p ~/android-sdk/cmdline-tools && cd ~/android-sdk/cmdline-tools
curl -fsSL https://dl.google.com/android/repository/commandlinetools-linux-11076708_latest.zip -o clt.zip
unzip -q clt.zip && mv cmdline-tools latest && rm clt.zip
export ANDROID_HOME=$HOME/android-sdk
export PATH=$ANDROID_HOME/cmdline-tools/latest/bin:$PATH

# 3. the exact packages this app pins (compileSdk 35 / build-tools 35.0.0)
yes | sdkmanager --licenses
sdkmanager "platform-tools" "platforms;android-35" "build-tools;35.0.0"

# 4. build (from clients/android/)
cd ~/plurx/clients/android && ./gradlew :app:assembleDebug
# → app/build/outputs/apk/debug/app-debug.apk
```

Persist `JAVA_HOME`, `ANDROID_HOME`, and the `PATH` line in `~/.bashrc` so new
shells find them. If the `commandlinetools` URL 404s, grab the current link from
the "Command line tools only" box at <https://developer.android.com/studio>.

### In Android Studio

Open `clients/android/` in **Android Studio** (Ladybug / 2024.2+) and Run — it
provisions the SDK for you.

**Toolchain** (pinned): AGP 8.7.2, Gradle 8.14.3, Kotlin 2.0.21, JDK 17, Media3
1.5.1, `compileSdk`/`targetSdk` 35, `minSdk` 23. Outside Docker the SDK location
comes from `local.properties` (`sdk.dir=…`) or `ANDROID_HOME`.

## Install

**Phone / tablet** — enable "Install unknown apps" for your file manager, copy the APK over,
and tap it. Or over USB with developer mode on:

```bash
adb install -r app-debug.apk
```

**Android TV / Google TV** — enable Developer options → USB/Network debugging, then push it
over the network:

```bash
adb connect 192.168.1.55        # your TV's IP
adb install -r app-debug.apk
```

The app then shows up in the TV launcher's Apps row.

## First run

1. Launch **plurx**. On the connect screen enter your server address — host and port are
   enough (`192.168.1.10:32600`); `http://` is assumed if you leave the scheme off.
2. Sign in with your plurx username and password.
3. That's it — the token is stored (DataStore) and reused until it stops working or you sign
   out from Settings.

## How playback decides

The interesting part lives in `data/Caps.kt` and `player/`. On play, the client:

1. Enumerates the device's decoders (`MediaCodecList`) and HDR display support, and builds a
   caps map: `vcodec`, `acodec`, `container`, `hdr`.
2. Calls `GET /api/v1/files/{id}/decision?<caps>`. The server replies with a **method**
   (`direct_play` / `remux` / `transcode`) and a `play_url`.
3. `direct_play` → ExoPlayer streams `/files/{id}/direct` (seekable via range). Anything else
   → `/files/{id}/stream.mp4?start=…`, a live fast-seek remux, and seeks re-request at the
   new position. Either way the true timeline position is what gets scrobbled.

This is why the Android app transcodes so rarely compared to a browser: MKV/HEVC/etc. that a
`<video>` tag refuses, ExoPlayer just plays.

## Project layout

```
app/src/main/java/tv/plurx/app/
  data/        Session, wire Models, Retrofit PlurxApi, shared OkHttp (Net),
               runtime Caps prober, DataStore SettingsStore
  ui/          AppViewModel (session + loaders), Compose screens
               (Auth, Home, Library, Detail, Settings), theme/, components/
  player/      PlayerScreen (Compose controls) + Controller (direct vs
               transcode playback, track menu, ExoPlayer wiring)
  PlurxApp     Application — points Coil's image loader at the authed OkHttp client
  MainActivity NavHost tying the screens together
```

One authenticated `OkHttpClient` (`data/Net.kt`) backs **API calls, Coil image loading, and
Media3 playback**, so posters and video streams carry the same bearer token as the API.

## Roadmap

Intentionally out of scope for v0.1.0, in rough priority order:

- Search across libraries.
- Client-side A/V **audio-offset** application on direct play (today the server applies sync
  offsets on transcode paths; the field is read but not yet acted on for direct play).
- HLS delivery for the transcode path (proper in-stream seeking instead of seek-by-restart).
- Proper TV **focus engine** polish (initial focus, row memory) and a dedicated leanback
  browse layout.
- Downloads / offline, Cast, PiP.

## License

Same as the plurx workspace.
