# Publishing the apps — TestFlight, the App Store, and Google Play

Companion to [RELEASING.md](RELEASING.md) (the server's version numbers and the
GHCR image) — this is *how the two client apps get onto a device that isn't
yours*. Building them for yourself is already covered where they live:
[clients/apple/README.md](../clients/apple/README.md) and
[clients/android/README.md](../clients/android/README.md). This doc starts where
those stop, at the archive, and ends at a review queue.

> **The one fact that decides everything below: App Review cannot reach your
> server.** A reviewer sits on Apple's or Google's network and opens plurx to a
> connect screen asking for a host they can't route to. Almost every rejection
> risk in this document is a restatement of that sentence. Solve it first
> (§1); the build mechanics are the easy part.

The order here is the order to do it in — §1 and §2 get you to TestFlight, §3
turns that into a public listing, §5 does the same for Play. §4 explains the
store-facing keys that land with this doc, so nobody deletes one later without
knowing what it bought.

## 1. The demo server — solve this before you write any store copy

An app whose entire function is "connect to a server you host" is testable only
if the tester has a server. Apple's reviewers will not stand one up, and
[Guideline 2.1](https://developer.apple.com/app-store/review/guidelines/#performance)
lets them reject anything they can't exercise. This is the single most common
rejection for self-hosted clients, and it is not a code problem.

Three ways out, in increasing order of effort:

**Internal TestFlight only.** Testers you add by Apple ID under your own
account (up to 100) get builds with **no review at all**. They have their own
plurx server or access to yours, so the problem evaporates. If the audience is
you, family, and a handful of friends, stop reading at §2 — this is the whole
answer, and it costs one $99/yr membership.

**External TestFlight (up to 10,000 testers).** The first build for a given
version goes through a lighter Beta App Review, and the reviewer *does* try to
use it. You need a reachable demo server.

**Public App Store listing.** Same requirement, stricter reviewer, and now the
listing copy and privacy disclosures matter too (§3).

### What a demo server has to be

| Requirement | Why |
|---|---|
| Reachable from the public internet, over **HTTPS with a real certificate** | A reviewer on a corporate network can't reach `192.168.1.10`, and a self-signed cert reads as "broken app" |
| A dedicated **demo account**, non-admin, password in the App Review notes | Never hand out an admin login; a reviewer poking Settings → Libraries on your real server is a bad day |
| Seeded with content you have the **right to distribute** | This is the part people get wrong |
| Up for the whole review window, and again for every resubmission | Reviews get reassigned; a server that was up on Tuesday and down on Thursday reads as a broken app |

**On the content:** point the demo server at films that are legally yours to
show a stranger — the Blender open movies (*Big Buck Bunny*, *Sintel*, *Tears
of Steel*, *Cosmos Laundromat*) are the standard choice, CC-BY, and they're
real H.264/HEVC files with proper metadata so the library actually looks like a
library. Add a couple of large MKVs if you want the reviewer to see the
transcode path work. Do **not** point a reviewer at your own library: plurx is
a player for media you already have, which is fine, but a reviewer browsing a
wall of scene releases is an invitation to a
[5.2](https://developer.apple.com/app-store/review/guidelines/#intellectual-property)
intellectual-property rejection that no appeal will fix.

**How to read a rejection here:** "we were unable to sign in" or "the app
displayed no content" is a 2.1, not a judgement on the app. The usual fix is
the same binary resubmitted with working credentials and a sentence in the
review notes explaining that plurx is a client for a server the user hosts.
A 4.2 (minimum functionality) or 5.2 rejection is a different animal and means
the listing or the demo content needs rethinking — Infuse, VLC, and Swiftfin
all ship on the App Store, so the *category* is not the problem.

## 2. Apple — source tree to TestFlight

### 2.1 One-time setup

```bash
brew install xcodegen          # the .xcodeproj is generated, never committed
xcode-select -p                # must point at an Xcode 26 install (see below)
```

**Xcode 26 or later is mandatory.** Since **28 April 2026** App Store Connect
rejects any upload not built with Xcode 26 against the iOS/tvOS 26 SDK
([Apple](https://developer.apple.com/news/upcoming-requirements/?id=02032026a)).
That is a *build* requirement, not a deployment-target one — the targets stay
at iOS/tvOS 17.0, so the app still runs on a five-year-old Apple TV. Xcode
26.0–26.3 need macOS Sequoia 15.6; 26.4 and later need macOS Tahoe 26.2, so
check which Xcode your Mac can actually run before planning around a date.

In the [Apple Developer portal](https://developer.apple.com/account):

1. Join the Apple Developer Program — **$99/yr**, and the account must be
   active before you can upload anything.
2. Register the bundle ID **`tv.plurx.app`** (Identifiers → App IDs). Both
   targets deliberately share it; that's what makes iOS and tvOS one App Store
   record with one purchase, not two apps.
3. In App Store Connect, create the app record and add **both platforms**
   (iOS and tvOS) to it.

Then set your team once, in `clients/apple/project.yml`:

```yaml
settings:
  base:
    DEVELOPMENT_TEAM: "YHK542LK23"   # Paul Junod's paid developer team
```

### 2.2 Version and build numbers

Two numbers, and they are not the same number:

| Key | Where | Rule |
|---|---|---|
| `MARKETING_VERSION` | `project.yml` | The version humans see — `0.1.0`. Tracks the server's number ([RELEASING.md](RELEASING.md)) so a bug report naming one identifies the other |
| `CURRENT_PROJECT_VERSION` | `project.yml` | The build number. **Must strictly increase on every upload**, including uploads that get rejected, expire, or that you delete |

App Store Connect rejects a duplicate build number at upload time, after the
whole archive has finished — so bump it before you archive, not after you wait
twenty minutes to be told.

### 2.3 Archive and upload

From Xcode: generate, open, pick the scheme, Product → Archive, then
Distribute App → App Store Connect. Or headless, which is what you want once
you've done it twice:

```bash
cd clients/apple
xcodegen generate                                    # writes plurx.xcodeproj

# iOS. For the Apple TV build, all four platform words change together:
#   plurx-iOS → plurx-tvOS · platform=iOS → platform=tvOS · -t ios → -t appletvos
SCHEME=plurx-iOS; PLATFORM=iOS; ALTOOL_TYPE=ios

xcodebuild -project plurx.xcodeproj \
  -scheme "$SCHEME" \
  -destination "generic/platform=$PLATFORM" \
  -archivePath "build/$SCHEME.xcarchive" \
  archive                                            # signs with your team

xcodebuild -exportArchive \
  -archivePath "build/$SCHEME.xcarchive" \
  -exportOptionsPlist ExportOptions.plist \
  -exportPath "build/$SCHEME"                        # produces plurx.ipa

xcrun altool --upload-app -f "build/$SCHEME/plurx.ipa" -t "$ALTOOL_TYPE" \
  --apiKey "$ASC_KEY_ID" --apiIssuer "$ASC_ISSUER_ID"  # App Store Connect API key
```

`ExportOptions.plist` is three keys — `method: app-store-connect`, `teamID`,
and `uploadSymbols: true`. The committed Team ID identifies the paid developer
team; it is not a credential. Certificates and App Store Connect API keys remain
in the login Keychain and environment, never in this repository.

Note `altool`'s platform word for Apple TV is **`appletvos`**, not `tvos` —
`tvos` is rejected outright, and it's the kind of typo you discover after the
archive step, not before. Both platforms upload to the same app record.

The API key (App Store Connect → Users and Access → Integrations) beats an
Apple ID password here because it survives 2FA prompts, which is the difference
between a scriptable upload and one that hangs waiting for a phone.

**Acceptance check:** the build appears under TestFlight → iOS Builds within
~15 minutes with state "Ready to Submit", and no email arrives from Apple about
missing Info.plist keys or an invalid binary. If email arrives, §4 is the list
of keys it's probably about.

### 2.4 TestFlight

**Internal testing** — add testers by Apple ID under Users and Access, assign
the build to an internal group. Available in minutes, no review, builds expire
after 90 days.

**External testing** — create a group, add testers by email or a public link,
submit the build for Beta App Review. First build of each version gets
reviewed; subsequent builds of the same version usually don't. This is where
the demo server (§1) starts mattering.

## 3. Apple — turning that into a public listing

Everything in §2 stays true; a public submission adds paperwork. Each of these
blocks the "Submit for Review" button:

**Privacy nutrition label** (App Store Connect → App Privacy). plurx collects
nothing — no analytics, no crash reporting, no third-party SDKs — so this is
"Data Not Collected" all the way down. Answer it honestly and it takes two
minutes; the label is separate from, and must agree with, the privacy manifest
in the binary (§4).

**Privacy policy URL** — required even when you collect nothing. A static page
saying "plurx sends data to the server you configure and nowhere else" is a
valid privacy policy. A **support URL** is required too; a GitHub issues page
counts.

**Age rating** — answer the questionnaire for the app's *own* content, not for
what a user might load into their server. plurx ships no content.

**Screenshots.** Apple's required set changes; check what App Store Connect
asks for at upload time. As of now that's one 6.9" iPhone set (1290×2796 or
1320×2868), one 13" iPad set (2064×2752 or 2048×2732), and — because the tvOS
target is part of the same record — an Apple TV set at 1920×1080 or 3840×2160.
The screenshots in [docs/img/](img/) are of the web UI and are the wrong
aspect for all three; these have to be captured from the Simulator.

**Review notes** — the highest-leverage text box in the whole submission:

> plurx is a client for a media server the user runs on their own hardware; it
> ships no content of its own. A demo server has been prepared for review at
> https://demo.example.com — username `review`, password `<…>`. The library
> contains Creative Commons films (Blender Foundation open movies). The app
> requires an arbitrary-loads ATS exception because home servers are commonly
> reached over plain HTTP on a LAN or a private overlay network; see §4.

**The name.** The repo currently answers to three names — `plurx` (bundle ID,
`CFBundleDisplayName`, this repo), `cinemarr` (the web UI's `APP_NAME`), and
`noirr` ([brand/BRAND.md](../brand/BRAND.md)). App Store Connect wants one, it
must be globally unique, and it's awkward to change after launch. Settle this
before reserving the name, not after.

## 4. The store-facing keys in the repo, and what each one buys

These land in the same change as this doc. Each exists because something fails
without it — deleting one to "clean up" costs a review cycle. Before this
change only the ATS row existed, which is why the first archive of the tvOS
target had nowhere to get an icon from.

| Key / file | Where | What it buys | What breaks without it |
|---|---|---|---|
| `NSLocalNetworkUsageDescription` | both targets, `project.yml` | Explains the iOS local-network permission prompt | LAN requests are denied when the person declines access |
| `NSBonjourServices` (`_plurx._tcp`) | both targets, `project.yml` | Lets the app browse for plurx and trigger the permission prompt before sign-in | Automatic discovery fails and the first login request can race the prompt |
| `NSAppTransportSecurity` → `NSAllowsArbitraryLoads` | both targets | Plain-HTTP connections to any host | Every `http://` server fails |
| `ITSAppUsesNonExemptEncryption: false` | both targets | Skips the export-compliance questionnaire on every upload | A manual questionnaire per build, and a build stuck in "Missing Compliance" until you answer it |
| `Resources/PrivacyInfo.xcprivacy` | both targets | Declares no tracking, no collection, and `UserDefaults` under reason `CA92.1` | App Store Connect **rejects the upload by email** for a missing required-reason declaration |
| `Resources/tvOS.xcassets/tvOS.brandassets` | tvOS target | Layered app icon (400×240, 1280×768) + Top Shelf (1920×720, 2320×720) | The tvOS archive cannot be uploaded at all |

**On the ATS exception.** `NSAllowsLocalNetworking` is the narrow, no-questions
alternative — it covers unqualified hostnames, `.local` names, RFC1918
addresses, link-local, and IPv6 ULA. What it does not cover is Tailscale's
`100.64/10` CGNAT range or a plain-HTTP server behind a custom domain, both
normal plurx deployments. And the two keys are mutually exclusive in practice:
on iOS 10+ the presence of `NSAllowsLocalNetworking` makes
`NSAllowsArbitraryLoads` be ignored entirely. So the broad exception stays, and
the justification goes in the review notes (§3). Reviewers accept this for
server-client apps; VLC and Infuse ship the same exception.

**What the exception does not buy: self-signed certificates.** Turning off ATS
turns off *ATS's* rules — cleartext, TLS version floor, cipher requirements. It
does not touch URLSession's server-trust evaluation, so an `https://` server
whose certificate doesn't chain to a trusted anchor still fails with
`NSURLErrorServerCertificateUntrusted`. Supporting that needs a
`URLSessionDelegate` trust override plus an `AVAssetResourceLoaderDelegate` for
the AVPlayer path, and [Session.swift](../clients/apple/Sources/Session.swift)
has no delegate at all today. Until then the supported shapes are plain HTTP,
or HTTPS with a certificate the device already trusts (Let's Encrypt, or a
private CA installed as a trusted profile).

**On the tvOS icon.** tvOS icons are *layered* — three parallax planes that
separate as the icon takes focus. The committed set is generated from the same
1024px mark the iOS icon uses, split into midnight background · ink `p` ·
accent cursor, and committed under
[`clients/apple/Resources/tvOS.xcassets`](../clients/apple/Resources/tvOS.xcassets).
There is still no launch storyboard for tvOS, which is cosmetic (a black frame
at launch), not a blocker.

## 5. Google Play

The Play path is cheaper and slower: **$25 once** instead of $99/yr, but a new
**personal** developer account cannot publish to production until it applies for
production access, and that application requires **12 testers opted in
continuously for the 14 days preceding it** — then Google reviews the
application, which takes its own days to weeks
([Play Console Help](https://support.google.com/googleplay/android-developer/answer/14151465?hl=en)).
Organisation accounts (D-U-N-S verified) skip this. Fourteen days is the floor,
not the total; no amount of engineering shortens it, so start the closed test
before you need it.

### 5.1 What has to change in the repo first

Both of these are real blockers, not polish:

**Target API 36.** `clients/android/app/build.gradle.kts` pins
`compileSdk`/`targetSdk` **35**. From **31 August 2026**, new apps and updates
must target **Android 16 (API 36)**; Android TV apps must target at least API
34. Since one APK serves phones and TV here, API 36 is the number. An extension
to 1 November 2026 can be requested from the Play Console.

This is not a one-line change. `compileSdk` must be ≥ `targetSdk`, and
`compileSdk 36` is beyond what AGP 8.7.2 supports — so the whole pinned
toolchain moves with it: AGP, and the `platforms;android-36` /
`build-tools;36.0.0` packages named in
[clients/android/README.md](../clients/android/README.md) and baked into
`clients/android/Dockerfile` (rebuild the image with `make android-image`
afterwards, or the Docker build keeps using the old SDK). Budget an afternoon
and a full re-test on a TV device, not a one-line diff.

**A release signing config.** `build.gradle.kts` defines no `signingConfigs`,
so `assembleRelease` produces an unsigned or debug-signed artifact that Play
refuses. Generate an upload key, keep it out of git, and enrol in **Play App
Signing** so Google holds the distribution key — losing an upload key is
recoverable, losing a distribution key without Play App Signing means the app
can never be updated again.

```bash
keytool -genkey -v -keystore plurx-upload.jks \
  -keyalg RSA -keysize 2048 -validity 10000 -alias upload   # store OUTSIDE the repo
```

### 5.2 Build and upload

Play takes an **App Bundle**, not the APK the README builds for sideloading:

```bash
cd clients/android
# → app/build/outputs/bundle/release/app-release.aab
./gradlew :app:bundleRelease
```

The sideload APK (`make android`) stays exactly as documented — Play
distribution and the server's own `/download/plurx-android.apk` are independent
channels, and keeping both is deliberate.

Upload the `.aab` in Play Console → Testing → Closed testing, run the 14-day
gate, then promote to production. `versionCode` must increase on every upload,
same rule as Apple's build number.

### 5.3 The Play paperwork

**Data safety form** — Play's equivalent of the nutrition label, and it is
audited harder. Same honest answer: no collection, no sharing. **Content
rating** questionnaire, **privacy policy URL** (required), and a **target
audience** declaration.

**Android TV** is a separate form factor in the Play Console with its own
checklist: the app already ships the `LEANBACK_LAUNCHER` intent filter and a TV
banner, but the listing needs TV screenshots (1920×1080) and the TV form factor
explicitly enabled, or the app simply won't appear on TV devices.

## 6. Known gaps — the checklist

Ordered by what blocks a submission soonest.

- [ ] **Demo server** stood up: HTTPS, CC-licensed library, `review` account
      (§1)
- [ ] `DEVELOPMENT_TEAM` filled in, bundle ID registered, app record created
      (§2.1)
- [ ] Simulator screenshots at the three required sizes (§3)
- [ ] Privacy policy + support URLs published (§3, §5.3)
- [ ] **One name chosen** across bundle, web UI `APP_NAME`, and brand (§3)
- [ ] Android `targetSdk` 35 → **36** before 2026-08-31 (§5.1)
- [ ] Android release signing config + upload keystore (§5.1)
- [ ] Auth token moves from `UserDefaults` (Apple) to the **Keychain**, and from
      plaintext DataStore (Android) to a **Keystore-encrypted** value — note
      `EncryptedSharedPreferences` is deprecated and is not the answer. Not a
      store requirement, but it's a bearer token to your whole library sitting
      in a plaintext plist
- [ ] tvOS launch storyboard (cosmetic)
- [ ] Complete the P1 viewer items in the
      [Apple parity matrix](APPLE-CLIENT-PARITY.md), especially free text
      subtitles and audio-sync controls

## 7. Non-goals

- **No CI-driven store uploads.** The server's release pipeline is automated
  ([RELEASING.md](RELEASING.md), "Cutting a release" step 6) because a container
  image is reproducible
  and reversible. Store submissions are neither: a bad build sits in review for
  days and a released version can't be unreleased. These stay manual and
  deliberate.
- **No Mac Catalyst / visionOS target.** More SDKs to keep current for an
  audience that has a browser.
- **No F-Droid or alternative Android stores.** The server already hands out
  the APK at `/download/plurx-android.apk`, which covers everyone who wants a
  sideload without a second submission process to maintain.
- **Sideloading is not deprecated by any of this.** Both README build paths
  keep working, and they remain the fastest way to run your own build on your
  own hardware.
