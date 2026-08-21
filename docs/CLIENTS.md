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

### Shared track facts — clients render the server's answer

The shared contract for every first-party detail screen is the per-file
`audio_streams`, `subtitle_streams`, and `playback_defaults` fields from the
native item-detail API. `playback_defaults.audio` and `.subtitle` each carry the
selected stream index, the preferred language code, and one status:

| Status | What the client says |
|---|---|
| `selected` | The preferred language exists and this track is the server default. |
| `available` | The preferred language exists, but policy selected another track or Off. |
| `missing` | Tracks exist, but none matches the preferred language. |
| `unknown` | At least one track has no language tag, so absence of the preferred language cannot be claimed. |
| `no_tracks` | This file has no tracks of that kind; for subtitles, say so plainly. |

Clients mark the selected indices and render the status; they do not fetch
admin settings or reimplement `select_tracks`. That boundary matters for anime:
the server can select Japanese original audio while reporting an available
English dub, and every platform must tell the same story.

A pre-play choice goes back to `/decision` as `audio=<index>` and
`subtitle=<index>` (`-1` is Off). The client executes the returned delivery
plan **as given** — it does not add the selected audio index itself. The plan
already carries it: a remux plan's `url` ends in `?audio=<index>`, and remux
and transcode plans repeat it in `delivery.audio` for the HLS session-create
body. **The plan carries the audio selection only.** A selected subtitle is not
in it and must not be looked for there: a burn-in travels in the HLS
session-create body as `subtitle_burn`, and a text track is fetched as a sidecar
(`/files/{id}/subs/{index}`) and applied by the client itself. A client that
reuses a bare `/stream.mp4` instead gets the server's
language default, not the viewer's choice. Selecting a track other than the
container's own default also means the plan will not be `direct` — the raw
file has no audio selector — so a client must not assume its verdict is stable
across a track change. For bitmap tracks it reads
`selection.subtitle_requires_burn_in` before
presenting the cost; text tracks still use the existing `native` flag because
ASS/SSA and `mov_text` require a burn on native HLS even though the server can
extract them as sidecars. A true
`selection.subtitle_burn_in_blocked_by_hdr` means the existing HDR guard kept
the current delivery instead of replacing it with SDR. The choice belongs to
one playback only; no client writes Playback defaults as a side effect.

`selection.subtitle_requires_burn_in` describes the *server's plan*, and it is
false when the PGS application overlay is enabled — a delivery only the native
clients implement. A client without that route (the web player) must still burn
such a track, so it honours whichever of the two authorities says a burn is
needed. Following the plan there would start a stream that cannot show the
chosen track and replace it a moment later, which is exactly the re-buffer a
pre-play choice exists to avoid.

The server contract is shipped, and the **web**, **Apple**, and **Android**
clients render it: the item detail screen lists both track sets with the
selected one marked, and offers pre-play Audio and Subtitles pickers whose
choice reaches `/decision` and the first session open. Apple states a bitmap
track's burn-in cost on the detail screen, and words PGS as a possibility
rather than a certainty because the `pgs-v1` overlay flag rides on
`/decision`'s tracks, which a detail screen has not fetched. Android prices the
same track from a selection-aware `/decision` preflight, so it reads that flag
and states the cost outright; a preflight that fails claims nothing and leaves
the disclosure to the in-player path.

## 2. Per-platform notes

**Web** — MSE playback of direct/remuxed fMP4 + HLS; capability probing via `MediaCapabilities` API feeds the decision engine. HDR in browsers is inconsistent → the profile system, not wishful thinking, decides (tone-mapped stream when the browser can't attest HDR output).

**Samsung Tizen** — Web app (HTML5/JS is still the official app model through Tizen 10 / 2026 sets). MSE caveat: H.264 over MSE caps at 1080p — 4K needs HEVC/AV1 (fine: that's what 4K files are). **No Samsung TV supports Dolby Vision** (HDR10/HDR10+ house); DV files → serve HDR10 base layer (P8) or tone-map (P5). 2024+ sets dropped DTS decode → audio policy handles. Private install is the fussiest: Developer Mode binds to your PC's IP, distributor cert must embed each TV's DUID (Tizen 7+ enforced), certs ~1 yr, and firmware updates have wiped sideloaded apps. Verdict: fine for our own TVs, annoying; the Jellyfin community's installer tooling shows it's automatable.

**LG webOS** — Web app on Chromium (webOS 24=Cr108 → webOS 26=Cr132; older sets run Cr79/87 — the web core must budget for that floor). Solid HEVC/AV1 4K decode from web apps; MKV container OK. Dolby Vision from web apps: P5 mostly works, P8 flaky (often silently falls back) → treat HDR10 as the reliable path, DV as best-effort per profile. Dev Mode session = 1000 hours, renewable indefinitely (automatable) but lapse uninstalls dev apps; rooting via webOS Homebrew Channel is the "install once, forever" option for a personal TV.

**Apple (iOS + tvOS)** — AVPlayer doesn't do MKV, and we don't fight it: the
server's on-the-fly remux to fMP4/HLS (`-c copy`) delivers the same streams
AVPlayer loves, with zero video re-encode. DV P5 is the reliable profile;
Apple TV never bitstreams TrueHD, so the profile routes TrueHD to (E)AC3 or
LPCM. Distribution requires the Apple Developer program; TestFlight builds
last 90 days and development profiles last one year. **The iOS + tvOS
implementation lives in [`clients/apple/`](../clients/apple/README.md)** — one
SwiftUI/AVPlayer source tree, two targets, wired to the runtime-caps →
`/decision` flow. iPhone and iPad also use `AVAssetDownloadURLSession` for
app-managed background offline downloads of server-prepared HLS; the action
stays hidden on tvOS. `xcodegen generate` builds both targets.

**Android/Google TV** — The easy one. Media3/ExoPlayer direct-plays MKV
natively; HEVC/AV1/DV ride device decoders (Shield, 2019+ Sonys fine;
Media3's dav1d module covers AV1-less SoCs). Sideloading via adb/APK remains
trivial; Google's 2026–27 developer-verification rollout explicitly exempts
adb and offers a free 20-device tier — private path safe. Sony's entire current
lineup is Google TV, so this codebase *is* the Sony story. **The native viewer
lives in [`clients/android/`](../clients/android/README.md)** — one adaptive
Compose APK for phones, foldables/tablets, Android TV, and Google TV, with
search, browse, photos, watched state, three web-matched themes, and the full
runtime-caps → `/decision` playback flow. Run
`./gradlew :app:assembleDebug` to build it.

Phones and tablets also use Media3's `DownloadService`, a non-evicting
app-private cache, and a cache-only player for offline viewing. Android TV and
Google TV deliberately hide offline controls; a television is expected to
remain attached to the server and keeps the existing streaming path.

**Native EPUB reading** — iPhone/iPad and Android phone/tablet use their native
detail/navigation shells around the same bounded web publication core as the
browser. SwiftUI presents an ephemeral `WKWebView`; Compose presents a locked
down `WebView`. Each loads only the saved Cinema origin, hands the login token
directly into JavaScript memory, and permits subsequent navigation only to
that origin's root, bundled assets, and one publication capability. Neither
bridge offers JavaScript a token getter. Closing waits for the locator save,
destroys the publication session, clears JavaScript authority, and returns to
detail; profile loss dismisses the reader. tvOS and Google TV compile the
shared models but never expose **Read**. iPhone and iPad also keep a
profile/revision-scoped original EPUB in app-private storage. A bounded
declared-resource cache is published beside it with one atomic rename, then
served to the same reader core through a private same-origin WebKit scheme.
The offline WebView contains no bearer or network origin and blocks HTTP(S)
subresources at WebKit's loader boundary. Reading positions remain local
while disconnected and only the newest dated locator is replayed after
reconnect. Android offline EPUB startup is the remaining M4 client;
television clients remain intentionally excluded.

**Native stall recovery contract** — The server accepts a bound
`previous_session_id` plus `reopen_reason: "stall"` and returns its normalized
`height` under the attempt's `request_id`. That wire is additive and available
to both native codebases. **Apple and Android have adopted it.** Android sends
`quality_auto: true` when an Auto viewer's subtitle burn-in posts a promise
height; an Auto session that posts no height omits the field. Android stops
after three consecutive bound reopens
that do not resolve to a strictly lower rung. A user-initiated seek, quality
change, or track change resets that budget and invalidates the stall request.

Two obligations come with adopting it. A client that posts a *promise* height —
the source height a subtitle burn or Quality = Original sends — while the
viewer is on Auto must also send `quality_auto: true`, or the server reads that
height as a sticky manual pick and never steps the session down; Android's
`sessionHeight` answers the burn case before it consults quality, so this is
its live case, not a hypothetical one. And the retry budget at the ladder floor
belongs to the client: the server repeats the floor rung indefinitely and
raises no terminal error of its own, by design.

Apple's shape of both, in `PlayerController`, is the reference contract.
`quality_auto` goes on *every* create as the viewer's own answer
(`selectedHeight == nil`), never inferred from whether a height happened to be
posted. `StallReopenBudget` counts consecutive bound reopens that failed to
resolve a strictly lower rung and stops after two — deliberately independent of
the five-seconds-of-progress rearm, because a link starved at the floor stalls,
reopens, briefly plays, and stalls again forever otherwise. It bounds one
starvation episode rather than the whole title: a minute of recovered film
clears it, so a blip ten minutes in does not leave the remaining hour with no
automatic recovery. A height the server
could not state — `0` for a remux of an unprobed source, or absent from an
older server — is read as *no step down* and spends budget rather than
resetting it. One stall mints one `request_id` and carries it through every
replay of that recovery, and a bound create refused with `400` is re-posted
once unbound under a fresh identity, because a claim that was normalized before
a retryable failure replays as `invalid stall reopen` for the original id.

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
