# plurx — Client Strategy & Platform Matrix

Hybrid approach: one shared TypeScript core wherever a platform runs web tech; native where the platform demands it. Hardware video decode is available on every path below — the split is about codebase economics, not performance. Platform facts verified 2026-07.

## 1. Codebases (4 total)

| # | Codebase | Covers | Player tech |
|---|---|---|---|
| 1 | **TypeScript web core** | Browser (+ admin UI), Samsung Tizen, LG webOS — later portable to Titan OS (Philips) | MSE/EME + HTML5 `<video>`; AVPlay interop on Tizen where needed |
| 2 | **Swift / tvOS** | Apple TV | AVPlayer for fMP4/HLS; server remux makes MKV a non-issue (see §2) |
| 3 | **Kotlin / Media3** | Android TV & Google TV (= Sony TVs, Nvidia Shield, Fire TV *Android* devices), Android phones/tablets | ExoPlayer/Media3 — true MKV direct play, HEVC/AV1/DV per device |
| 4 | **BrightScript / SceneGraph** | Roku | Roku Video node — strict envelope, leans hardest on server remux/transcode |

Shared across all: the server's OpenAPI-generated types, the device-profile definitions, and a common design language.

**Ship order:** Web first (it's also the admin UI and the Tizen/webOS seed). Then Android TV (cheapest native win, trivial sideload, biggest device coverage), then tvOS, then Tizen/webOS ports, then Roku. Kodi-family Plex clients (§3) cover living rooms in the meantime.

## 2. Per-platform notes

**Web** — MSE playback of direct/remuxed fMP4 + HLS; capability probing via `MediaCapabilities` API feeds the decision engine. HDR in browsers is inconsistent → the profile system, not wishful thinking, decides (tone-mapped stream when the browser can't attest HDR output).

**Samsung Tizen** — Web app (HTML5/JS is still the official app model through Tizen 10 / 2026 sets). MSE caveat: H.264 over MSE caps at 1080p — 4K needs HEVC/AV1 (fine: that's what 4K files are). **No Samsung TV supports Dolby Vision** (HDR10/HDR10+ house); DV files → serve HDR10 base layer (P8) or tone-map (P5). 2024+ sets dropped DTS decode → audio policy handles. Private install is the fussiest: Developer Mode binds to your PC's IP, distributor cert must embed each TV's DUID (Tizen 7+ enforced), certs ~1 yr, and firmware updates have wiped sideloaded apps. Verdict: fine for our own TVs, annoying; the Jellyfin community's installer tooling shows it's automatable.

**LG webOS** — Web app on Chromium (webOS 24=Cr108 → webOS 26=Cr132; older sets run Cr79/87 — the web core must budget for that floor). Solid HEVC/AV1 4K decode from web apps; MKV container OK. Dolby Vision from web apps: P5 mostly works, P8 flaky (often silently falls back) → treat HDR10 as the reliable path, DV as best-effort per profile. Dev Mode session = 1000 hours, renewable indefinitely (automatable) but lapse uninstalls dev apps; rooting via webOS Homebrew Channel is the "install once, forever" option for a personal TV.

**Apple TV (tvOS)** — AVPlayer doesn't do MKV, and we don't fight it: the server's on-the-fly remux to fMP4/HLS (`-c copy`) delivers the same streams AVPlayer loves, with zero video re-encode. (Infuse ships a whole FFmpeg demux + VideoToolbox engine to avoid the server dependency — we *are* the server, so we take the cheap path; a client-side demuxer can be revisited later if offline sync ever matters.) DV: P5 is the reliable profile; audio: Apple TV never bitstreams TrueHD — profile routes TrueHD→(E)AC3/LPCM per policy. Distribution: $99/yr Apple Developer; TestFlight builds last 90 days (re-push cadence) or 1-year dev-profile installs via Xcode.

**Android/Google TV** — The easy one. Media3/ExoPlayer direct-plays MKV natively; HEVC/AV1/DV ride device decoders (Shield, 2019+ Sonys fine; Media3 1.9's dav1d module covers AV1-less SoCs). Sideloading via adb/APK remains trivial; Google's 2026–27 developer-verification rollout explicitly exempts adb and offers a free 20-device tier — private path safe. Sony's entire current lineup is Google TV, so this codebase *is* the Sony story.

**Roku** — Hardest constraints, embraced rather than fought: SceneGraph Video node only (no custom demux/decoders). Envelope: HLS/DASH preferred; HEVC 4K@40Mbps, **AVC capped 1080p/10Mbps**, AV1 only newer devices/DASH-only; DV/HDR10+ device-tier-dependent; AC3/EAC3/DTS passthrough-only with an AAC stereo fallback track required; subs TTML/WebVTT/SRT only → **PGS/VobSub must burn in server-side**. plurx's remux/transcode pipeline makes Roku a well-behaved HLS client. Distribution reality: private channels are dead (since 2022); dev mode sideloads exactly one app; beta channels last 120 days/20 users. Roku ships last, and public store certification is the eventual real path there.

**Explicit non-targets (2026):** Fire TV's new Vega OS devices (no sideloading at all — Android Fire TVs still work via codebase 3), Vidaa/Hisense and Vizio (no private install path), Titan OS (web-app platform — port candidate if it ever opens self-serve).

## 3. Third-party Plex clients — compatibility tiers

The founding assumption "point Infuse at plurx" turned out to be **false** — verified 2026-07: Infuse (like VidHub, Symfonium, and all official Plex apps) requires plex.tv sign-in and has no manual-server option for Plex sources (open feature request since 2020). What actually works with a direct connection, and what it costs:

| Tier | Clients | What they need | Status |
|---|---|---|---|
| **1 — direct connect (v1 target)** | Composite for Kodi (best reference client), PlexKodiConnect, python-plexapi tools, Home Assistant | PMS HTTP API subset + GDM discovery + token or LAN-whitelist auth | Committed — REQ-PLEX-1 |
| **2 — plex.tv-dependent** | Infuse, VidHub, Symfonium, official Plex apps | Emulating plex.tv itself (PIN link flow, `/api/v2/resources`) **plus DNS redirection of plex.tv on the client's network** | **Deferred.** Fragile (Plex controls both ends and ships breaking auth changes), adversarial posture, and Plex's 2025–26 remote-streaming enforcement makes their client behavior a moving target |
| **3 — Jellyfin-compat (idea only)** | Streamyfin, Findroid, Infuse-via-Jellyfin, the whole Jellyfin client ecosystem | A Jellyfin-compatible façade instead of/alongside the Plex one — notably, **Infuse *does* support direct manual connections for Jellyfin servers** | Unscoped; recorded because it may be the cheapest legitimate route to Infuse ever working with plurx |
| Dead | MrMC | — | Project abandoned |

Tier 1 is honest old-Plex compatibility on day one: a Kodi box or the `plexapi` ecosystem sees plurx as a Plex server on the LAN with zero cloud anywhere. The Tier 3 observation is worth a future spike precisely because it turns "emulate a hostile cloud" into "implement a documented open API."

## 4. Living-room coverage timeline

| Stage | Apple TV | Sony/Shield/Android | Samsung | LG | Roku |
|---|---|---|---|---|---|
| Server + web only | Kodi/Composite (Tier 1) | Kodi or browser | web browser app | web browser app | — |
| + Android TV app | Kodi/Composite | **plurx native** | — | — | — |
| + tvOS app | **plurx native** | plurx native | — | — | — |
| + TV web ports | plurx native | plurx native | **plurx web** | **plurx web** | — |
| + Roku app | plurx native | plurx native | plurx web | plurx web | **plurx** |
