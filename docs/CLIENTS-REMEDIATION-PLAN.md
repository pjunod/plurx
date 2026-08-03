# Clients remediation plan — restore trust, then raise the quality ceiling

**Status:** **landed** — every milestone implemented, none device-verified ·
**Executes:** [CLIENTS-CODE-REVIEW.md](CLIENTS-CODE-REVIEW.md) §10 as amended
by [CLIENTS-CODE-REVIEW-ASSESSMENT.md](CLIENTS-CODE-REVIEW-ASSESSMENT.md) ·
**Verified against:** Apple subtitle work through `97176881e`, deployed server
hardening through `f93a54ac3`, and the working tree at `2d693e5d4`, 2026-08-02 ·
**Written:** 2026-08-02

**Where it landed** (branch `clients/remediation`):

| Commit | Milestones |
|---|---|
| `0fa0aff` | §8.1–§8.3 server enablers, plus MEDIA-BADGES M1 |
| `3ea2321` | MEDIA-BADGES M2 (web player) |
| `7f4950f` | §4.2, §4.3, §4.5, §5.1–§5.6, §7.1–§7.6, plus MEDIA-BADGES M3 |
| `a6643f7` | §4.1, §4.3, §4.4, §6.1–§6.4, §7.2–§7.4, §7.6, plus MEDIA-BADGES M4 |
| `35ec4c2` | §7.1's focus graph — the half of it that `7f4950f` got wrong (§7.1) |
| `006c489` | the `native` subtitle field this plan's §2.3 misdescribed, and the sidecar re-extraction race |
| `5d5deff` | web MSE transport keyed on bit depth, and the rescue the hls.js transport never had |
| `d3328bc` | both clients route subtitles by `native`; Apple's §6.4 tradeoff becomes a setting |

**Still outstanding: device verification.** Everything above passes the
platform gates in §9.5 and none of it has been run on hardware. The Apple
targets have never been compiled — there is no macOS in the environment they
were written in. `ShelfFocusTest` has never executed, the shrunk release APK
has never been installed, and every "on device" acceptance in §9.4's matrix
is unrun. §3.3's ledger and §9.3 stay open until it is.

**Corrections.** Where a claim below turned out to be wrong, the claim is
kept and corrected in place (marked **Corrected**) rather than rewritten
away — the preamble's "if a claim conflicts with the code, the code wins"
cuts both ways, and a plan that quietly reads as though it had always been
right teaches nobody what the code actually says. The §2.3 subtitle
correction is the important one: that misreading shipped a bug.

This is the build document. The review holds the evidence, the assessment
holds the argument about which conclusions deserve work; every finding both
documents agree on — plus every qualification the assessment raised — was
re-verified against the tree at `97176881e` on 2026-08-02 before it earned a
milestone here. If a claim below conflicts with what you find in the code,
the code wins: re-verify, then flag the drift in your commit message rather
than silently improvising.

**Start the Android/Apple parity session at §5.** Section 5 is one ordered
Android playback-negotiation arc; §5.4 is the direct counterpart to the native
Apple subtitle release. Sections 4, 6, and 7 cover the remaining bidirectional
behavior and lifecycle gaps. Do not reimplement the Apple subtitle server arc:
its wire contract is already deployed and is pinned in §2.3.

**How to work.** Milestones are numbered `§4.1`-style and ordered within
each section; sections 4–6 are independent workstreams and may interleave,
section 8 (server enablers) can land any time but §7's watch-filter item
depends on §8.1. One milestone per commit where practical, docs updated in
the same commit as behavior (README/parity-doc claims this plan touches are
listed in §7.6). Every server-side commit runs `make check` first; client
commits run the platform gates named in §9.5. **Standing instruction:** if a
step seems to require changing the server's decision engine
(`crates/plurx-core/src/playback/`) or the HLS master-playlist/codec arc in
`crates/plurxd/src/http/hls.rs` beyond what §8 explicitly orders, stop and
flag it instead of doing it — §10 explains why.

**Branch.** The whole clients arc lives on `codex/fix-dolby-vision-quality`
(`main` is an ancestor and merges through). A Codex agent is still actively
committing there — last commit `97176881e` at 11:39 on 2026-08-02. Where
this plan's work lands (that branch, or a new branch off its tip) is decided
with Paul at kickoff; do not assume, and never leave the working tree
checked out somewhere he doesn't expect.

---

## 1. Verdicts — what of the response holds, and what changed under it

The assessment's bottom line — accept the review as the remediation
roadmap, with qualifications — is adopted. Each of its claims was checked
against the current tree; four needed amendment, and one had been overtaken
by events before this plan was written.

| Assessment claim | Verdict | Basis |
|---|---|---|
| The ten "confirmed findings" | **Adopted** — nine still reproduce in the tree | Clients unchanged since the review snapshot: zero commits under `clients/` in `f8655c166..97176881e`, and a checksum sweep of the working tree against HEAD shows no dirty client sources. Review line numbers remain valid. |
| Apple validation harness breaks the iOS build | **Overtaken** — the two dirty files (`PlurxApp.swift`, `PlayerController.swift`) were reverted to HEAD; the `--live-scary-movie` harness is gone from the tree | Verified 2026-08-02: `PlurxApp.swift` matches HEAD and contains no `LiveScaryMovieValidationView`. What remains of the P0 is a build-proof and a guardrail (§4.1, §10). |
| DV fallback is not always strip/remux — a DV source without a compatible base must be re-encoded | **Adopted, verified** | `DvHandling` has exactly the three arms the assessment describes: `None` / `Strip` (server ffmpeg has `dovi_rpu` *and* the file has a compatible base) / `Reencode` (`crates/plurx-core/src/playback/mod.rs:306-325`). The Android omission is therefore worse than the review's headline: a DV Profile 5 title doesn't lose DV — it loses the original stream entirely. |
| Audio passthrough must be route- and format-aware, recomputed per decision | **Adopted, API pinned** | Media3 1.10.1 is already the client's version; `AudioCapabilities.getCapabilities(Context, AudioAttributes, routedDevice)` consults `ACTION_HDMI_AUDIO_PLUG` extras and (API 33+) `AudioManager.getDirectProfilesForAttributes` — exactly the mechanism asked for. §5.2 carries the details. |
| Bonjour / TV-focus outcomes need runtime proof before being called deterministic | **Adopted for the claims, amended for the schedule** | The *language* amendment is accepted: this plan words both as "reproduce, then trust." The *scheduling* is not: the minimal Bonjour fix is a three-line run-loop scheduling change that is correct under every runtime outcome, so it ships in P0 (§4.4) with the device repro as its acceptance evidence — a known probable hang in the connect path should not wait in P2 for a proof it can carry with it. The focus graph keeps the assessment's order: repro test first, then the structural fix (§7.1). |
| Automatic subtitles are policy; automatic transcoding should not be | **Adopted, with one carve-out (Paul, 2026-08-02)** | The standing rule in §3.1: automatic selection must never start a burn — *except a forced track*, which may, always at source height. Forced tracks exist because the film is unintelligible without them; refusing them by default trades a comprehension failure for an encoder slot. |
| §11 is already a snapshot; re-pass before treating it as the server verdict | **Adopted and executed** | The re-pass found the assessment undercounted: not five but **nineteen** commits landed after the review snapshot (fourteen after the assessment's own `98165c615`). §2.2 records the new server verdict; the practical effect on this plan is small and stated there. |
| Recommended P0→P3 order | **Adopted with the two amendments above** | Harness item becomes verify+guard; Bonjour moves to P0. Everything else keeps the assessment's order, including treating Android capability/quality/subtitles/VOD as one initiative (§5). |
| Acceptance checks and non-goals | **Adopted** | Folded into §9 and §10, extended with the forced-subtitle case the carve-out creates and with the platform build gates. |

---

## 2. The tree and the contract at `97176881e`

### 2.1 Where things stand

- **Branch:** `codex/fix-dolby-vision-quality`, HEAD `97176881e`;
  `main` = `a1d1dd259`, an ancestor. Zero client commits since the review
  snapshot `f8655c166`; all nineteen intervening commits are server-side
  (`crates/plurxd`, `crates/plurx-core`), all in the native-HLS/DV arc.
- **Working tree:** clean except `docs/CLIENTS-CODE-REVIEW.md` (uncommitted
  cross-link to the assessment) and the untracked assessment itself — both
  commit together with this plan. `clients/apple/Support/*-Info.plist` and
  `clients/android/.idea/` in `git status` are gitignored generator litter;
  ignore them.
- **The review's Apple-harness P0 no longer exists in the tree** — the two
  dirty files were reverted, not committed. §4.1 proves it and guards the
  door it came through.

### 2.2 What the nineteen server commits did — the post-§11 verdict

Review §11 assessed `ab5438ca2` + `a2f5239f9` and flagged three CODECS
edges. The assessment asked for a re-pass; here it is. The arc that
followed was a device-driven experiment in HLS master metadata, and it
**ended in a deliberate revert**:

```
 ab5438ca2  AUTOSELECT uniqueness (RFC 8216)      ── survives
 a2f5239f9  CODECS attr on the native master      ─┐
 1841f6a98  exact DV profile strings              │
 2605962a9  full RFC 6381 avc/hevc strings        │  the CODECS
 0bd7f4246  SUPPLEMENTAL-CODECS for DV 8.1        │  experiment —
 3ea6cbd5d  RESOLUTION / VIDEO-RANGE              │  all REVERTED by
 be783/b4a9/fa368  temp ?diagnostic= modes        ─┘  97176881e
 0be6d9870  drop #EXT-X-INDEPENDENT-SEGMENTS      ── survives
 6e0d1379e  subtitle playlists mirror video segs  ── survives
 98165c615  per-segment MPEGTS timestamp maps     ── survives
 312a9ccdd  forced renditions DEFAULT=NO          ── survives
 200bf7793  -strict unofficial → dvcC box in DV   ── survives (plurx-core)
 97176881e  master carries NO codec claims at all ── HEAD
```

The standing server verdict this plan codes against:

- **The native master is minimal.** `#EXT-X-VERSION:7`, no
  `#EXT-X-INDEPENDENT-SEGMENTS`, `#EXT-X-STREAM-INF` carries only
  `BANDWIDTH` (+ `SUBTITLES="subs"` when text renditions exist) — no
  `CODECS`, no `RESOLUTION`, no `VIDEO-RANGE`. AVPlayer inspects the
  published init segment instead; a test pins
  `!master.contains("CODECS=")`. **Do not re-add codec metadata to the
  master** — every richer shape was tried against a real device this
  weekend and rejected (§10).
- **Review §11.2's three CODECS edges are moot at the wire level** (the
  attribute they lived in no longer exists). Residue: `copied_hls_codecs` /
  `hls_codecs` / `hls_supplemental_codecs` are still computed and carried
  in `crates/plurxd/src/transcode.rs` with no playlist consumer — see
  §8.3. The §11.1 follow-up (probe `disposition.hearing_impaired`) is
  still live — §8.2.
- **Subtitle renditions are segment-mirrored.** The subtitle media playlist
  copies the video playlist's segmentation
  (`subtitle_media_playlist(&video)`), each `seg#####.vtt` is sliced from
  the cached sidecar to that segment's window, and every slice's header
  stamps `X-TIMESTAMP-MAP=MPEGTS:<90kHz offset>,LOCAL:00:00:00.000`. Cue
  alignment at resume/seek offsets is the server's job; clients just play.
- **Forced renditions are always `DEFAULT=NO`** (`312a9ccdd`) and duplicate
  same-language tracks are `AUTOSELECT=NO` (`ab5438ca2`), so **the client
  must explicitly enable its chosen rendition** through its player's
  selection API — the Apple client does
  (`PlayerController.applyNativeSubtitleSelection`), and the Android native
  path in §5.4 must do the ExoPlayer equivalent.
- **Preserved-DV copy sessions mux a real `dvcC` box** (`200bf7793`,
  `-strict unofficial`, `crates/plurx-core/src/transcode/mod.rs:1055-1066`) —
  the fix behind the CoreMedia `-12927` class. Server-side; nothing for
  clients to do, but it is why DV-over-native-HLS is now expected to work
  on Apple hardware.

### 2.3 The wire contract, exactly (re-verify each against the file at build time)

**`GET /api/v1/files/{id}/decision`** — caps as query params
(`crates/plurxd/src/http/stream.rs:113-137`):

| Param | Meaning |
|---|---|
| `vcodec`, `acodec`, `container` | CSV lowercase short names. Presence of any = "real caps reported". |
| `hdr` | `1` iff the client decodes HDR *and* the display shows it. |
| `dv` | `1` = decodes Dolby Vision. **Absent means no** — the server says so in as many words. |
| `dvprofile` | CSV probed DV profiles, e.g. `5,8`. **When present, authoritative over `dv`** (`stream.rs:193`; pinned by `explicit_dolby_vision_profiles_override_the_legacy_all_profiles_bit`). |
| `force` | `auto` \| `original` \| `transcode` — **anything else parses as Auto** (`Force::parse`, `plurx-core/src/playback/mod.rs:84-91`). This is the §5.3 bug. |
| `maxheight` | Exists; deliberately unsent by both clients (founding decision: 4K-on-small-screen direct-plays). Keep it unsent. |

Response (`DecisionResponse`, `stream.rs:321+`): flattened verdict +
`delivery` (execute this, not `method`), `source`
(`SourceSummary`: `width`, `height`, `hdr`, `bit_depth`, `bitrate` — **the
source-height promise reads `source.height` from here**), `audio[]`,
`subtitles[]` (`SubTrackDto` — two flags, below), `markers[]`, `ladder[]`
(`transcode::Rung { height, total_kbps, peak_kbps }`, top rung first).

**Corrected `006c489` / `d3328bc`. This paragraph used to say
`SubTrackDto.text` was the server-computed `is_native_text_subtitle`
(`subrip|srt|webvtt|vtt`). It never was, and the misreading shipped a
bug.** `text` is `!is_bitmap_subtitle(&s.codec)`
(`crates/plurxd/src/http/stream.rs:435`). The two predicates disagree on
`mov_text` and on styled ASS/SSA: those arrive `text: true`, are filtered
out of the master, and 400 when asked for by index. A 2160p WEB-DL in the
library with 23 `mov_text` tracks therefore offered 23 subtitles and could
publish none of them. Every subtitle now carries **both** flags, and they
answer two different questions:

| Field | Predicate | What it unlocks |
|---|---|---|
| `text` | `!is_bitmap_subtitle` (`plurx-core/src/tracks.rs:134-139`) | **There is extractable text here.** The WebVTT sidecar `GET /files/{id}/subs/{index}.vtt` (which turns away bitmaps and nothing else, `stream.rs:787`), a `<track>` on a direct/remux `<video>`, and a pre-extracted burn. |
| `native` | `is_native_text_subtitle`: `subrip\|srt\|webvtt\|vtt` (`plurx-core/src/tracks.rs:144-149`) | **This can be published as an HLS rendition.** The same predicate that puts the `EXT-X-MEDIA` line in the master and that `POST /files/{id}/hls/sessions` enforces on an explicit `subtitle` pick (`hls.rs:209`). |

> **The distinction, in one sentence, so it cannot be misread again:**
> `text` says the server can *hand you the words*, `native` says the server
> can *publish them as a rendition* — so anything session-shaped gates on
> `native`, and only a sidecar or a burn may gate on `text`.

`text && !native` is a real and common shape (`mov_text`, ASS/SSA), not an
edge case, and it is the only shape the two flags exist to separate.
Bitmap tracks are `text: false, native: false`; `native: true` implies
`text: true` and nothing implies the reverse. A client older than `006c489`
sees no `native` at all and falls back to restating the codec list
(Android `PlaybackPolicy.isNativeTextSubtitle`, Apple
`SubtitleTrack.isNativeHLS`) — the only reason either client still repeats
a server rule.

**`POST /api/v1/files/{id}/hls/sessions`** — body `CreateSession`
(`crates/plurxd/src/http/hls.rs:70-105`): `playback_id` (stable per player
instance; supersession key), `request_id` (fresh per attempt), `height`,
`subtitle_burn` (bitmap only — burn is for tracks with no text to send),
`native_subtitles: bool`, `subtitle` (initially selected native rendition —
the **absolute subtitle-stream index**, not a filtered ordinal), `start`,
`audio`, `copy`, `aac`, `preserve_dolby_vision`, `audio_offset_ms`.

Height semantics (`hls.rs:150-164`, the comment is the contract):
`null` → server Auto (`auto_height`, min(source, 1080) on hardware);
`height == source.height` → passes through unsnapped — **"the source's own
height is the Original/forced-burn promise"**; any other explicit value →
snapped onto the ladder.

Response `StartResponse` (`hls.rs:46-61`): `session_id`, `playlist_url`
(**carries `?native=1&subtitle=N` when `native_subtitles` was set**,
`hls.rs:198-204`), `duration_ms`, `start_seconds`, `encoder`, **`vod`**
(whole stream already on disk: treat like direct play — `start_seconds` is
`0.0`, the client seeks; `transcode.rs:2129-2134`), **`ladder`**.

**`GET /api/v1/files/{id}/subs/{index}.vtt`** — the whole track as one
WebVTT, bearer/token-authed, server-cached. **No start-offset parameter**,
so a side-loaded copy is timeline-correct only for direct play (player
time 0 = source time 0). This is why §5.4 uses native renditions, not
side-loading, for session-based modes.

**`GET /api/v1/items` (`list_items`)** attaches
`watch + resolution + child_count` and **never `rollup`**
(`crates/plurxd/src/http/browse.rs:106-118`); rollups exist only on
`item_detail` via the single-item recursive CTE
(`store/sqlite/watch.rs:275`). Both clients' show-library watch filters
stay broken until §8.1 lands.

**Landed `0fa0aff`:** `list_items` now attaches the same rollup for
container kinds, from one batched recursive CTE per page
(`browse.rs:120`, `Store::watch_rollups`, `store/sqlite/watch.rs:299`).
The paragraph above describes the tree before that commit; §7.2's client
half went in with `7f4950f` (Android) and `a6643f7` (Apple).

---

## 3. Standing rules — read before any milestone

### 3.1 The subtitle rule

> **Automatic subtitle selection must never start a burn — except a forced
> track, which may, always at source height.**

Decided with Paul 2026-08-02, amending the assessment's stricter rule.
Concretely, for *automatic* (cold-start) selection on both platforms:

| Track shape | Automatic behavior |
|---|---|
| Forced (disposition flag *or* "forced" in title), any codec | Auto-apply. Text → native/free; bitmap → **the one permitted auto-burn**, at source height. |
| Default-flagged, native text (`native == true`) | Auto-apply via the free path (rendition / embedded track). Never a burn. |
| Default-flagged, bitmap or styled | **Not** auto-applied. Explicit selection only (which may burn, at source height). |
| Merely same-language (`?? matching.first`) | Never auto-applied. This tail is deleted (§6.1). |

**Corrected:** the "native text" row read `text == true` when this plan was
written, which is the wrong predicate — §2.3. A default-flagged `mov_text`
track is `text: true, native: false`, so that row as written would have
auto-applied it and taken a 400 on every session-mode play. The shipped
policy functions ask `native` — both are spelled `automaticSubtitleIndex`,
in Android's `PlaybackPolicy.kt` and Apple's `PlayerController.swift` — and
the row above now says what they do. The forced row is unchanged: forced
applies whatever the codec, which is the carve-out.

Implement the policy as one pure, unit-tested function per platform so the
whole table is one screen of code and Paul can flip the carve-out with one
line. Manual selections are untouched by this rule — a viewer who picks a
PGS track gets a burn, at source height, as today.

### 3.2 Burns and "Original" carry source height, everywhere

Any session whose height is a *promise* rather than a transcode rung —
subtitle burn, or Quality = Original — sends `height = source.height` from
`DecisionResponse.source`. Auto with a genuine transcode verdict keeps
`height = null` (the server's Auto is smarter than the client's guess);
an explicit rung sends the rung. The web player and Apple client already
obey this; §5.3 brings Android in line.

### 3.3 Claims need reproductions

Where the review asserts a runtime outcome from code reading alone
(Bonjour "never completes", focus "crashes"), the fix ships with the test
or device check that turns the claim into evidence — §9 names each. Write
commit messages accordingly: "prevents X from being possible", not "fixes
crash", until the repro exists.

---

## 4. P0 — trust: lifecycle, errors, credentials, discovery

Small, isolated, in this order. Everything here is a "the app lied to you
or broke under you" class fix.

### 4.1 Prove the Apple build, and fence the door the harness came through

**State:** the `--live-scary-movie` harness was reverted out of the working
tree (verified 2026-08-02); HEAD was never broken.
**Do:** run all four builds (iOS + tvOS, Debug + Release) via
`xcodegen generate` + `xcodebuild` for both schemes to prove the tree, and
add the guardrail so the *next* validation harness can't recreate the bug:
a note in `clients/apple/README.md`'s development section — any live
validation entry point must be fenced `#if os(tvOS) && DEBUG` (or the
platform it targets), must never read credentials from launch
arguments/environment in Release, and must not touch persisted settings
(the reverted harness overrode saved `subLang`).
**Acceptance:** four green builds logged in the commit message; README
carries the fencing rule.

### 4.2 Android: `onPlayerError` + the one-shot compatibility rescue

**Files:** `clients/android/app/src/main/java/tv/plurx/app/player/Controller.kt`,
`player/PlayerScreen.kt` (listener at `:413-446`).
**Now:** no `onPlayerError` anywhere; a rejected stream is a silent black
surface (`playFailed` only covers the create call, `Controller.kt:226-228`).
**Do:** implement PLAYBACK.md's error-fallback contract, which the Apple
client already embodies (`PlayerController.swift:477-497`): on
`onPlayerError`, if `deliveryMode` was direct/remux and no rescue has been
attempted for this item, reopen as a forced compatibility transcode at
`realPosition()` (an HLS session create — guaranteed H.264/AAC); on a
second failure, or a failure in an already-transcoded mode, surface a real
failure state in `PlayerScreen` (message + Retry + Back), not a frozen
surface. Track the once-per-item flag in the `Controller`. While in
`buildPlayer` (`Controller.kt:280-293`), also set
`DefaultRenderersFactory.setEnableDecoderFallback(true)` — a flaky hardware
decoder then degrades to software instead of erroring into the new path —
and wire `setAudioAttributes(AudioAttributes[USAGE_MEDIA/CONTENT_TYPE_MOVIE],
/* handleAudioFocus = */ true)` + `setHandleAudioBecomingNoisy(true)`
(review §4.7; two builder lines, and this milestone already owns the file).
**Acceptance:** with a direct-play stream killed server-side mid-play, the
player recovers exactly once into a transcode at the current position; a
second kill shows the failure screen. Unit-test the rescue policy
(mode × already-retried → action) as a pure function like
`PlayerPolicyTest` does for existing policies.

### 4.3 Both platforms: changing servers clears the persisted token

**Files:** Android `ui/AppViewModel.kt:258-261` + `data/SettingsStore.kt`;
Apple `Sources/AppModel.swift:243-247` + `SettingsStore`.
**Now:** both clear only the in-memory token; kill-and-relaunch between
connect(B) and login sends server A's bearer to server B over plain HTTP —
and A's still-valid session gets destroyed when B answers 401.
**Do:** clearing or changing the origin clears the persisted token in the
same write — one `SettingsStore` entry point on each platform so the
invariant ("origin and token are written together or not at all") has one
owner. Audit the other origin-write path (`connectToOrigin` /
`connect()`) for the same pairing.
**Acceptance:** state-machine unit test on each platform (both have the
seams: `AppViewModelTest`, hosted `AppModel` tests): connect to B, drop the
process before login, re-init → no `Authorization` header may be sent to B
and A's token is gone from storage.

### 4.4 Apple: Bonjour resolution gets a run loop

**File:** `Sources/ServerDiscovery.swift` (`BonjourResolver.resolve()`,
`:151-157`).
**Now:** `NetService.resolve(withTimeout:)` is called from a cooperative-
pool thread whose run loop never spins; delegate callbacks (including the
timeout) can never fire; the continuation leaks and `await` never returns.
Reachable from the connect screen (row stays `resolving`, all rows
disabled) and from saved-session recovery (bootstrap spinner).
**Do:** the minimal correct fix — schedule the service on the main run loop
before resolving (`service.schedule(in: .main, forMode: .common)` with the
resolver hopping to `@MainActor`, or make the whole resolver `@MainActor`).
Keep a belt-and-braces `Task`-side timeout that resumes the continuation
with failure if nothing fired in 6 s, so no future scheduling regression
can leak it again. `NWBrowser`/`NWConnection` replacement is the eventual
architecture (NetService is deprecated) — **out of scope here** (§10);
this milestone is scheduling, not redesign.
**Acceptance:** on-device (§3.3): with the saved server down and another
`_plurx._tcp` service visible, tapping a discovered row resolves or fails
within the 5 s NetService timeout — no permanent spinner. Add a unit test
that resolve() against a nonexistent service *returns* (failure) rather
than hanging the test runner.

### 4.5 Android: the autoplay toggle must not release the live player

**File:** `player/PlayerScreen.kt:413` (`DisposableEffect(controller,
preferences.autoplayNext)`).
**Now:** flipping "Autoplay next episode" mid-play disposes the effect →
`controller.release()` → the effect body re-registers on the **released**
player and restarts at the original `startMs`.
**Do:** key the effect on `controller` alone; read the preference inside
the listener through `rememberUpdatedState(preferences.autoplayNext)`.
**Acceptance:** compose UI test (or instrumented policy test): toggling the
preference during playback changes neither the player instance nor the
position; `release()` is called exactly once, at screen exit.

---

## 5. P1 — Android playback negotiation, one initiative

The assessment is right that these are one system: capability reporting,
quality selection, subtitles, and VOD share `Caps.kt`, the
`decision`/`create` request models, `Controller`, and the hardware matrix.
Build them as one arc (§5.1→§5.6), testing the *decision the server makes*
at each step, not just the fields sent. §5.1–§5.2 change what the server
is told; §5.3–§5.6 change what the client does with the answer.

### 5.1 Tell the server about Dolby Vision (`dv` / `dvprofile`)

**File:** `data/Caps.kt`.
**Now:** `Caps.query` sends `vcodec/acodec/container/hdr` only. Absent `dv`
means no (server's words), so every Android device gets `Strip` (remux +
ffmpeg per play, HDR10 base) — or, per the verified `DvHandling::Reencode`
arm, a **full re-encode** for DV without a compatible base (Profile 5) or
when the server ffmpeg lacks `dovi_rpu`.
**Do:** probe two facts and claim their intersection:

1. Decoder: `MediaCodecList` for `video/dolby-vision`; collect supported
   profiles from `CodecCapabilities.profileLevels` and map the constants to
   Dolby profile numbers — `DolbyVisionProfileDvheDtr` → 4,
   `DolbyVisionProfileDvheStn` → 5, `DolbyVisionProfileDvheSt` → 8 (ignore
   the AVC/AV1-based profiles until the library contains such files).
2. Display: `Display.HdrCapabilities` contains `HDR_TYPE_DOLBY_VISION`
   (the existing `displayIsHdr` helper already reads the caps object).

Send `dv=1&dvprofile=<csv>` when both hold — `dvprofile` is authoritative
(§2.3). **Never claim profile 7** even if the decoder lists `DvheDtb`: the
review's reasoning stands — dual-layer P7 should keep taking the server's
strip path, exactly as the Apple client's `5,8` claim does.
Also: `Caps.query` is binder IPC and runs on the main thread per decision
(`AppViewModel.kt:106,306`) — move it onto a background dispatcher as part
of this touch; §5.2 makes it strictly heavier.
**Acceptance:** unit-test the profile mapping. On a DV-capable device
(Shield/DV TV): a Profile 8 title's `/decision` returns direct play with
`preserve_dolby_vision`; a Profile 7 title still returns the strip remux;
on a non-DV panel nothing is claimed. Server-side nothing changes — that
is the point.

### 5.2 Tell the server about the HDMI sink (route-aware audio)

**File:** `data/Caps.kt`.
**Now:** `ac3/eac3/dts/truehd` are claimed only when `MediaCodecList` has a
*decoder*; passthrough sinks (Shield → AVR) have none for TrueHD, so
lossless Atmos comes back `transcode_audio` → AAC 256k.
**Do:** merge sink capabilities exactly as the assessment qualifies —
route- and format-aware, not a blanket claim. Media3 1.10.1 (already the
dependency) provides it:

```kotlin
val caps = AudioCapabilities.getCapabilities(
    context,
    AudioAttributes.Builder()          // the same attributes the player uses
        .setUsage(C.USAGE_MEDIA)
        .setContentType(C.AUDIO_CONTENT_TYPE_MOVIE)
        .build(),
    /* routedDevice = */ null,         // let Media3 resolve the active route
)
```

Claim a codec when a decoder exists **or** `caps.supportsEncoding(...)`
accepts the bitstream: `ENCODING_AC3` → `ac3`, `ENCODING_E_AC3` /
`ENCODING_E_AC3_JOC` → `eac3`, `ENCODING_DTS` / `ENCODING_DTS_HD` → `dts`,
`ENCODING_DOLBY_TRUEHD` → `truehd`. Under the hood this reads
`ACTION_HDMI_AUDIO_PLUG` extras and, on API 33+,
`AudioManager.getDirectProfilesForAttributes` — the route-aware decision
the assessment names. **Recompute per `/decision` call** (already the call
pattern; now it matters): re-plugging HDMI, an AVR power-cycle, or
switching to TV speakers changes the truthful answer, and a stale claim
over-promises. Leave a comment saying exactly that, per the review's ask.
**Acceptance:** unit-test the merge logic with faked decoder/sink sets
(decoder-only, sink-only, neither, both). On device: TrueHD MKV over an
AVR → direct play, passthrough audio; same box on TV speakers → the server
transcodes audio. §9.4's matrix carries both.

### 5.3 Make `force` mean what the menu says, and heights keep promises

**Files:** `ui/AppViewModel.kt:306-309` (decision), `player/Controller.kt:220`
(create), `data/ViewerPreferences.kt` (the enum stays for storage).
**Now:** `force=<rung>` parses as Auto server-side, so an explicit "720p"
silently does nothing for direct/remux verdicts; and
`"original".toIntOrNull()` sends `height = null`, so an Original-quality
burn restarts 4K as 1080p (the server Auto rung).
**Do:** one mapping in each direction, per §2.3 and §3.2:

```
/decision force  = auto → "auto" · original → "original" · rung → "transcode"
create   height  = rung selected            → the rung
                   burn OR quality=Original → decision.source.height
                   otherwise (true Auto)    → null
```

The server snaps stray rungs onto the ladder and passes
`height == source.height` through unsnapped (`hls.rs:150-164`) — send the
promise, let the server keep it. Plumb `source.height` from
`DecisionResponse.source` into `PlanLike` so `Controller` has it.
**Acceptance:** unit-test the two mappings as pure functions (every enum
value × burn/no-burn). On device: `force=720` over a remote link yields a
720p transcode where today it direct-plays; a PGS burn on a 4K remux
stays 2160-tall (§9.4).

### 5.4 Rebuild subtitles around native text — burn only bitmap

**Files:** `player/Controller.kt` (`switchSubtitle` `:164-173`,
`openSession` `:207-244`), `data/Models.kt` (`CreateSessionReq:228-241`,
`HlsStart:213-219`), `player/PlayerScreen.kt` (`TrackMenu`).
**Now:** every server-listed selection sets `activeMode = "transcode"` +
`subtitle_burn` — an SRT on a 4K HDR remux costs the stream its
resolution (§5.3's height bug) *and* its HDR, for a track the server hands
over free. The decision's forced/default track is never auto-applied on
server-controlled streams (review §3.2).
**Do — the native path, mirroring Apple, on the machinery §2.2 shows was
hardened all weekend:**

1. **Models:** add `native_subtitles: Boolean?` + `subtitle: Int?` to
   `CreateSessionReq`; add `vod: Boolean = false` and
   `ladder: List<Rung> = emptyList()` to `HlsStart` (§5.5/§5.6 consume
   them; one wire change, decode once).
2. **Selection routing** in `switchSubtitle(index)`:
   - `null` → today's behavior (return to plan mode, renditions off).
   - `track.text && mode == direct` → in-place ExoPlayer text override
     (embedded tracks already surface in `TrackMenu`; unify so the server
     row and the embedded row are one row, selected by language+index
     match rather than shown twice).
   - `track.native && mode ∈ {remux, transcode}` → an HLS session with
     `native_subtitles = true`, `subtitle = index` (absolute stream
     index, §2.3): for a remux-class plan send `copy = true` (+
     `aac` per `delivery`'s audio verdict, `preserve_dolby_vision` if the
     plan preserved it) so video stays untouched; for a transcode-class
     plan it's the same session as today plus the rendition flags. Video
     recipe unchanged in both — the server comment guarantees `subtitle`
     "changes only HLS metadata, never the video recipe."
   - `!track.native` (bitmap, *and* `text && !native`: `mov_text`,
     ASS/SSA) → burn as today, `height` per §5.3.

   **Corrected:** the two session-mode bullets read `track.text` /
   `!track.text` when this plan was written, inheriting §2.3's misreading.
   `text` is not the rendition predicate, so as written they would have
   routed every `mov_text` and ASS/SSA track into `native_subtitles` and
   collected a 400 per pick. The invariant the shipped
   `PlaybackPolicy.subtitleRoute` table exists to hold: **a track that is
   `text` but not `native` is never a `NativeRendition`.** Direct play is
   the exception and stays on `text` — the container's own track renders
   with its styling intact, and the sidecar can extract it.
3. **Rendition control:** the returned `playlist_url` carries
   `?native=1&subtitle=N`, which sets `DEFAULT=YES` on the chosen
   non-forced rendition — the *initial* selection needs nothing more. For
   switches within a live session, and for **forced renditions (always
   `DEFAULT=NO`, §2.2)**, select explicitly via
   `TrackSelectionOverride` on the `TRACK_TYPE_TEXT` group whose ordinal =
   position among the file's native-text tracks in source order — the
   exact mapping Apple's `nativeSubtitleOrdinal` implements
   (`PlayerController.swift:538-540`). Turn renditions off with
   `setTrackTypeDisabled(TRACK_TYPE_TEXT, true)` (the menu's Off row
   already does this).
4. **Auto-apply at start:** run the §3.1 policy function over
   `decision.subtitles` when the controller first opens — the same pure
   function §6.1 builds for Apple, ported; unit-test the table's four
   rows. This closes review §3.2 (forced subs silently absent on
   remux/transcode).
5. **Do not side-load `/files/{id}/subs/{index}.vtt`** for session modes:
   the endpoint has no offset parameter (§2.3), so a side-loaded track is
   timeline-wrong the moment `start > 0`. Direct play doesn't need it
   (embedded tracks). If ExoPlayer fights rendition selection in practice,
   stop and flag before reaching for side-loading — the fallback has a
   correctness cost the native path doesn't.

**Acceptance:** selecting an SRT on a 4K HDR remux starts no video
encoder and keeps HDR (verify via the session's `encoder` field and the
stream itself); PGS explicit selection burns at 2160; a forced-sub film
auto-shows the forced track on a remux; toggling between two text tracks
does not create a new server session. §9.4's matrix adds the
text-subtitle and forced-subtitle cases on Android hardware.

### 5.5 Honor `vod` — cached sessions resume and seek natively

**Files:** `player/Controller.kt` (`openSession:237-242`, `seekTo:131-144`),
`data/Models.kt` (`HlsStart` — decoded in §5.4.1).
**Now:** `vod` is never decoded; a cache-hit resume starts at 0:00
(`baseMs = start_seconds = 0` and nobody seeks), and every in-film seek
churns a session that native seeking would handle.
**Do:** when `hls.vod`: set `baseMs = 0`, and after `prepare()` seek the
player to the requested position; in `seekTo`, when the active session is
VOD and the target is within `duration`, `player.seekTo(target)` — no new
session. Non-VOD transcode seeks keep session-per-seek (the live playlist
can't be range-sought). Apple's `isVOD` handling
(`PlayerController.swift:347-355`) is the reference behavior.
**Acceptance:** resume into a fully-cached transcode starts at the saved
position with `encoder == "cached"`; ten scrubs produce zero new sessions
(watch `/api/v1/hls` session ids); a non-cached transcode still reopens
per seek.

### 5.6 Render the server's ladder; decode `added_at`

**Files:** `player/PlayerScreen.kt` (quality rows), `data/Models.kt`
(`Item`), `ui/LibraryScreen.kt:153`.
**Now:** the quality menu is a hardcoded enum offering rungs above the
source (`ViewerPreferences.kt:51-62`); `Item` never decodes the `added_at`
the server sends, so merged-collection "Added" sort is a no-op
concatenation.
**Do:** build the in-player quality menu from `decision.ladder` (heights +
`total_kbps` for labels) + Auto + Original, falling back to the enum only
when `ladder` is empty (old server); the *stored preference* keeps its
enum values — §5.3's mapping already translates. Add
`added_at: Long? = null` to `Item` and make the merged "added" sort
interleave by it, as Apple already does.
**Acceptance:** the menu offers only rungs the source can feed; "Added"
sort on a merged collection interleaves by recency (unit-testable — the
sort is a pure function over decoded items).

**Corrected `7f4950f`, two things.** (1) `added_at` is an **`i64`** —
epoch seconds (`plurx-core/src/domain.rs:143`) — not a `String`; the
Android field is `Long?`. (2) The ladder is
`[360, 480, 720, 1080]` (`crates/plurxd/src/transcode.rs:4222`) and no
ladder anywhere ever contains 2160 or 1440: 4K rungs are deliberately
absent, and above-ladder heights exist only as explicit
Original/forced-burn requests. So the acceptance's original wording ("a
1080p source shows no 2160/1440 rows") named the wrong hazard — those rows
could only ever have come from the client's own enum, which is exactly
what this milestone deletes as the menu's source. The real drift ran the
other way: the enum had no 360 value, and one had to be added for the
ladder's bottom rung to be storable.

---

## 6. P1 — Apple: playback quality and intent

### 6.1 Automatic subtitles obey the §3.1 rule

**File:** `Sources/PlayerController.swift`
(`automaticSubtitleIndex:615-629`); tests
`Tests/AppleClientTests.swift:164-203`.
**Now:** the chain is forced → default → **`?? matching.first`** with
`subLang` defaulting to `eng` — so an English-audio film with an unflagged
English track auto-enables full subs on every play, and when the only
English track is PGS (the standard 4K disc remux), **cold start is a burn
transcode**: encoder slot per play, H.264 SDR, HDR gone.
**Do:** replace with the §3.1 policy function: forced (flag or
title-sniff, as now) → apply whatever the codec (the carve-out; burn
already carries source height here, `PlayerController.swift:324-328`);
default-flagged **and native text** (`isNativeHLS`) → apply; otherwise
`nil`. The `matching.first` tail is deleted. Keep the existing "never
fall back to a flagged default in another language" guard.
**Acceptance:** extend the unit tests to pin all four table rows of §3.1 —
in particular: unflagged-English-PGS-only → `nil` (no auto-burn), and
forced-PGS → its index (auto-burn permitted). On device: a 4K HDR remux
with a default-flagged PGS English track cold-starts as a copy with no
encoder attached.

### 6.2 A seek during a stream change wins, not vanishes

**File:** `Sources/PlayerController.swift` (`reopen:265-273`; tvOS
30 s-step seeks `PlayerView.swift:916-924`).
**Now:** `guard !isChangingStream` silently drops any seek/track intent
that lands while a replacement is in flight — the tvOS progress bar snaps
back.
**Do:** keep the no-overlap guard (it exists for a good reason — shared
`playback_id` supersession) but record the newest requested position
(`pendingReopenMs`) instead of discarding it; when the in-flight change
lands, issue exactly one trailing `reopen` at the recorded position if one
accumulated. Track-change requests queue the same way (last-writer-wins is
correct — supersession's whole design).
**Acceptance:** unit-test the queueing policy (in-flight × new-request →
resulting sequence). On tvOS: hammer the step-seek during a quality
change — the bar lands where the last press pointed.

### 6.3 Release the old session only after the new one exists

**File:** `Sources/PlayerController.swift` (`open():284-338`).
**Now:** `releaseCurrentSession()` runs before `createHlsSession`; if the
create fails, the viewer's current item points at a playlist the client
just deleted — buffered runway, then a stall, and `fail()` deliberately
stays quiet while an item exists (`:406-410`).
**Do:** create first; on success, release the predecessor (or let
server-side supersession retire it — it already does when the new create
lands with the same `playback_id`; an explicit DELETE after success is
belt-and-braces, fine to keep). On create failure: the old session and
item stay live, and the change surfaces as a transient error toast rather
than a coming stall.
**Acceptance:** with the server refusing creates (kill plurxd briefly), a
quality change during playback leaves the current stream playing;
restoring the server and retrying succeeds. Assert session-count telemetry
never dips to zero during a successful change.

### 6.4 Decide the native-subtitle session tradeoff, on the record

**File:** `Sources/PlayerController.swift` (`open():298-302`);
doc `docs/APPLE-CLIENT-PARITY.md`.
**Now (review §3.4):** any file containing an SRT/VTT track never
direct-plays — it opens a copy-HLS session so renditions exist if subs get
toggled. Identical stream quality, but a server session + segmenter per
play where web/Android serve raw bytes.
**Do:** keep the behavior for v0.2 — restart-free subtitle toggling is the
better viewer experience and the session cost was accepted knowingly — but
write it into APPLE-CLIENT-PARITY.md as a deliberate tradeoff with this
reasoning, so it reads as chosen, not drifted. If Paul would rather have
direct-until-first-toggle (one clean restart at first selection, the code
already restarts cleanly for burns), that is a contained change to the
same `guard` — flag, don't assume.
**Acceptance:** the parity doc names the tradeoff and its why; no code
change unless Paul opts for the alternative.

**Landed differently (`a6643f7` wrote the doc, `d3328bc` added the code).**
Both branches are defensible and neither is defensible *for everyone*, so
the choice moved into Settings rather than staying in the source:
`SubtitleReadiness.instant` is the default and is exactly the v0.2
behaviour, and `.onDemand` direct-plays and rebuilds at the same film
position on the first subtitle pick — the clean reopen a bitmap burn
already takes. One pure function, read when a title starts, both branches
pinned by tests. APPLE-CLIENT-PARITY.md §2 still carries the reasoning.

---

## 7. P2 — comfort, performance, hygiene

Ordered by user-visible pain; each is small enough to be its own commit.
File references are the review's (still valid, §1).

### 7.1 Android TV focus graph — repro first, then restructure

Per §3.3 and the assessment's explicit order for this one: write the
instrumented repro before the fix — an androidTest that scrolls a shelf's
`LazyRow` until the first card disposes, then walks focus down and back
up (the review predicts `FocusRequester is not initialized` or a dead
press, `ui/components/Common.kt:233-243`). Then restructure: attach the
**`FocusRequester`** to each row container (`focusGroup` per shelf)
instead of to a specific card, and leave the **`focusProperties` block on
the cards**; make the "Group by" picker reachable (`HomeScreen.kt:139-155`);
collapse the `.clickable(...).focusable()` doubled targets
(`Common.kt:102-103,279-281`). **Acceptance:** the repro test passes
against the fix and fails against the old graph (run it once on the
pre-fix commit to prove it bites); a physical D-pad walk of a 30-item
shelf reaches every neighbor and the picker.

**Corrected `35ec4c2`. The instruction above used to say only "attach
requesters to row containers (`focusGroup` per shelf) instead of specific
cards", and `7f4950f` read that as covering the `focusProperties` block
too — which produced a fix that was silently dead.** The two halves of a
shelf's focus wiring live in different places, and they have to:

- **The requester belongs on the container.** On the card at index 0 it is
  disposed the moment the viewer scrolls past it, and every neighbour
  aiming at the shelf then calls `requestFocus()` on a requester bound to
  nothing (`IllegalStateException: FocusRequester is not initialized`).
  The row survives scrolling because the row is what exists.
- **The `up`/`down` overrides cannot follow it out there.** A card's
  `FocusTargetNode.fetchFocusProperties` compiles to
  `visitSelfAndAncestors(FocusProperties, untilType = FocusTarget)` — the
  walk stops at the **first ancestor focus target**. `LazyRow` carries
  `Modifier.scrollable`, and `ScrollableNode` delegates a
  `FocusTargetModifierNode(Focusability.Never)`, which sits between the
  cards and anything the caller hangs on the row. A `focusProperties`
  block declared on the `LazyRow` is therefore **invisible to every card
  inside it**. It costs nothing on the card, because what it points at is
  the neighbour's *container*, and containers do not dispose.

The vertical axis had the same defect and a worse failure mode — a custom
up/down destination bypasses spatial focus search, and with it the
mechanism that composes out-of-window lazy items during a search, so a
scrolled-away shelf threw rather than no-opping. The shelf list is a
`Column` with `verticalScroll` for that reason, with the revisit threshold
named in a comment. `ShelfFocusTest` presses actual D-pad keys instead of
calling `requestFocus()`, which bypassed the resolution under test and
passed either way; it has still never executed, per the header.

### 7.2 Watch filters for show libraries (client half — needs §8.1)

Both clients filter on `item.watch` alone (Android
`LibraryScreen.kt:137-145`; Apple `AppModel.swift:625-634`); containers
have no watch rows, so "Watched"/"In progress" are empty and "Unwatched"
lies. Once §8.1 attaches rollups in `list_items`: containers classify as
`rollup.watched == leaves` → watched, `0 < watched < leaves` → in
progress, else unwatched — the reasoning `orderedSeasonCandidates`
already applies elsewhere. Android's `Item` already decodes `rollup`;
Apple's model needs the field. **Acceptance:** a TV library with one
finished show, one half-watched, one untouched filters into the right
three buckets on both platforms.

**Corrected `a6643f7`:** Apple's `Item` already carried `rollup` too —
only the classification was missing on either platform.

### 7.3 First-paint parallelism and the library spinner

Android (`AppViewModel.kt:229-300`, `LibraryScreen.kt:59,153`,
`DetailScreen.kt:93-103`): hubs → libraries → per-library items run
strictly sequentially (2+N round trips before first paint); libraries
page the whole collection behind a spinner and re-fetch on sort even
though sorting is client-side; detail blocks on the serial
season/episode walk. Parallelize with `coroutineScope`/`async`, render
what has arrived, resolve the play target after paint, and stop
re-fetching on sort. Apple (`AppModel.swift:409-429`,
`LibraryView.swift:123-132`): publish pages incrementally; spinner only
when `!hasHomeContent` (`AppModel.swift:181`, `HomeView.swift:105-116` —
the file's own cached-content policy). **Acceptance:** cold-start home
paints after the first responses arrive, not the last; a 1 000-item
library shows its first page in one round trip; pull-to-refresh never
blanks a populated dashboard.

### 7.4 Apple image pipeline and session-expiry handling

`AuthImage.swift:24-33`: decode-downsample via ImageIO to the cell size,
add an `NSCache`, retry through the `waitsForConnectivity` session — a
failed first poster is currently a placeholder forever, and tvOS
`.extraLarge` grids decode full-res per cell. Centralize
`APIError.http(401|403)` after bootstrap → `clearToken(); phase =
.needLogin` (`AppModel.swift:95-98,263-266` handle bootstrap only), so a
rotated token stops degrading every screen to error strings. Refresh home
on player dismissal and `scenePhase == .active` (review §6.4 — tvOS
currently never refreshes). **Acceptance:** scrolling a large tvOS grid
stays smooth (no per-cell full decode in Instruments); revoking a token
mid-session lands on login within one request; finishing an episode
updates Continue Watching without relaunch.

### 7.5 Android lifecycle edges, retained selections, TV pipeline flags

As one hygiene pass, each with the review's reference: stop NSD discovery
after saved-server recovery (`try/finally`, `AppViewModel.kt:443`);
`withTimeout(5s)` around `serverBrowser.resolve` (`ServerDiscovery.kt:
147-174`) so `busy` can't wedge; cap launch validation's 80 s splash
(`AppViewModel.kt:118-131`); union `displayCutout` into safe insets
(`SafeAreas.kt:22,37`); scope `TightTvButtonFocusBounds` to TV
(`TvFocus.kt:158-163`); hoist `selectedAudio`/`selectedSubtitle` next to
the audio-offset saved state so a quality change stops resetting them
(review §4.6, `PlayerScreen.kt:273-277`); set tunneling on TV devices
(`setTunnelingEnabled(true)`); `rememberSaveable` for search/sort/filter;
fix `runCatching` swallowing `CancellationException` (`DetailScreen.kt:
248-256` et al. — `validateSavedSession:484` shows the pattern).
**Acceptance:** review §5.5/§5.6's specific repros stop reproducing;
existing test suites stay green.

### 7.6 Packaging and the documentation debt

Enable R8 + `shrinkResources` for Android release (or swap
`material-icons-extended` for core + inlined icons — either ends the
thousands-of-unused-icons APK); `proguard-rules.pro` stops being dead
config. Then the doc pass the review itemized (§7 table): both READMEs'
version/status lines, Apple README's Keychain/test-count/feature claims,
Android README's quality-menu claims, APPLE-CLIENT-PARITY.md's stale
"remaining" items — plus §6.4's new tradeoff paragraph. Same-commit rule:
each doc claim updates in the commit that makes it true.
**Acceptance:** release APK size drops measurably (record before/after in
the commit); a `grep` of the review's drift table finds no stale claim.

---

## 8. Server enablers (plurxd / plurx-core) — small, scoped, gated

Run `make check` before each commit (`cargo test --workspace
--no-fail-fast` — plurx-core failures hide behind plurxd's otherwise).

### 8.1 Batched watch rollups in `list_items`

**Files:** `crates/plurxd/src/http/browse.rs:88-118`,
`crates/plurx-core/src/store/mod.rs:471`, `store/sqlite/watch.rs:275`.
**Now:** `list_items` batches `child_counts` but attaches no rollup;
`watch_rollup` exists only single-item (recursive CTE per call).
**Do:** add a batched `watch_rollups(user_id, &[item_ids]) ->
HashMap<i64, WatchRollup>` — same recursive-CTE shape seeded from the id
set (or the single CTE run per container id in one connection pass;
libraries hold tens of containers, not thousands). Attach
`.with_rollup(...)` for container kinds in `list_items` — mirror how
`item_detail:214-223` gates it by kind. Leaves keep `watch` only.
**Acceptance:** a store test: seed a show with watched/half/unwatched
seasons; `list_items` returns rollups matching `item_detail`'s for the
same containers; the endpoint's query count stays O(1) in item count
(one rollup query per page, like `child_counts`).

### 8.2 Probe `disposition.hearing_impaired`; demote the SDH title sniff

**Files:** `crates/plurx-core/src/scan/probe.rs:160` (dispositions),
`domain.rs:301-308` (`SubtitleStream`), `crates/plurxd/src/http/hls.rs`
(`subtitle_characteristics`).
**Now:** `SubtitleStream` carries `default`/`forced` only; SDH detection
for the `CHARACTERISTICS` attribute sniffs titles ("sdh", "closed
caption", …) — review §11.1's follow-up, still live after the re-pass.
**Do:** parse `disposition.hearing_impaired` in the probe alongside
`forced`, add the field to `SubtitleStream` (serde default `false` so
stored probe JSON predating the field deserializes), and make
`subtitle_characteristics` prefer the disposition with the title sniff as
fallback — files probed before this change keep working, newly-probed
files stop depending on naming conventions. Check whether the probe cache
keys on a version; if re-probing is manual, say so in the commit message
rather than forcing a rescan.
**Acceptance:** probe-fixture test: a stream with
`disposition.hearing_impaired=1` and a bland title gets the
accessibility `CHARACTERISTICS`; the existing title-sniff tests still
pass as the fallback.

### 8.3 Housekeeping notes — record, don't refactor

Two residues of the CODECS experiment, **flag-only** because that arc is
live and owned elsewhere (§10): (1) `hls_codecs` /
`hls_supplemental_codecs` are computed per session
(`crates/plurxd/src/transcode.rs`, `copied_hls_codecs` and friends) with
no playlist consumer since `97176881e` — leave the machinery, add one
comment at `HlsContext` noting the master no longer reads it, so the next
reader doesn't hunt for a consumer that isn't there. (2)
`copied_audio_codec`'s `_ → "mp4a.40.2"` arm mislabels genuinely-copied
FLAC/Opus/DTS — unreachable from current clients; one comment saying so
(review §11.2 asked for exactly this). **Acceptance:** comments exist;
no behavior change; `make check` green.

**Corrected `0fa0aff`: that arm is *not* unreachable.** The web player
claims `flac` and `opus` whenever the browser does, and Safari takes the
copy-HLS path, so a FLAC-in-MKV remux reaches it today and is labelled
AAC. It is harmless for a different reason than the review gave — the
native master carries no `CODECS` attribute since `97176881e`, so the
wrong label is written to nothing at all. That is also why it stays
recorded rather than fixed: a fix landed now is untestable through any
wire output, and it wants doing in the same change as whatever starts
reading the result.

---

## 9. Acceptance — proving the changed behavior

### 9.1 Per-milestone checks

Each milestone above ends with its own acceptance; those are the merge
bar. This section adds the cross-cutting evidence.

### 9.2 The observable outcomes (assessment's list, amended)

- iOS and tvOS, Debug and Release, compile with no live-validation entry
  point in the tree (§4.1).
- Toggling autoplay during Android playback does not restart, release, or
  move the current stream (§4.5).
- An Android direct/remux failure performs exactly one
  compatibility-transcode retry at the current position; a second failure
  is a visible terminal error (§4.2).
- Changing servers clears the persisted token before the new origin can be
  used, on both platforms (§4.3).
- A supported DV Profile 8 title preserves DV on Android; Profile 7
  follows the server's strip path; Profile 5 on non-claiming hardware
  follows the re-encode path — all three, not two (§5.1).
- A TrueHD Atmos title uses passthrough only when the active Android route
  truthfully supports that exact format; on TV speakers it does not
  (§5.2).
- Selecting SRT/WebVTT on Android starts no video encoder and discards no
  HDR; selecting PGS explicitly may burn, at source height (§5.4, §5.3).
- Apple cold start never auto-selects a burn-only subtitle — **except a
  forced track, which burns at source height** (§6.1, §3.1).
- A cached Android HLS session resumes at the requested position and
  seeks without session churn (§5.5).
- A seek issued during an Apple stream change lands at the seek target
  once the change completes (§6.2); a failed change leaves the previous
  stream playing (§6.3).

### 9.3 Claims that required reproduction (§3.3 ledger)

- Bonjour hang: device check in §4.4 + the non-hanging unit test.
- Focus-graph failure: the §7.1 instrumented repro, proven to bite on the
  pre-fix commit.

### 9.4 The hardware matrix

Extend APPLE-CLIENT-PARITY.md §1's five cases and run on both platforms
after P1: DV Profile 7 · DV Profile 8 · DV Profile 5 (if the library has
one) · TrueHD Atmos over an AVR · text-subtitled 4K HDR remux ·
PGS-subbed 4K remux · forced-subtitle foreign-dialogue film ·
cached-transcode resume. These are exactly the cases §5–§6 change; record
the before/after verdict per case in PLAYBACK-TESTING.md's format.

### 9.5 The gates

```bash
# Android — from clients/android
./gradlew test lintDebug assembleDebug        # unit + lint + build
./gradlew connectedAndroidTest                # on a TV device/emulator for §7.1

# Apple — from clients/apple
xcodegen generate
xcodebuild -scheme plurx-iOS  -configuration Debug   build
xcodebuild -scheme plurx-iOS  -configuration Release build
xcodebuild -scheme plurx-tvOS -configuration Debug   build test
xcodebuild -scheme plurx-tvOS -configuration Release build

# Server (§8 only) — repo root
make check                                    # fmt + clippy -D warnings + tests
cargo test --workspace --no-fail-fast         # both crates' failures, not the first
```

Scheme/target names above are from `project.yml` — re-verify before
scripting against them.

---

## 10. Non-goals and guardrails

- **Do not redesign the server decision engine.** The reviewed engine
  chooses correctly from the facts it receives; every quality problem in
  scope is an input problem. (Assessment's guardrail, adopted verbatim.)
- **Do not re-add codec metadata to the native HLS master.** The
  CODECS/SUPPLEMENTAL-CODECS/RESOLUTION/VIDEO-RANGE shapes were each tried
  against real AVPlayer hardware on 2026-08-01/02 and deliberately
  reverted in `97176881e`; the minimal master is a conclusion, not an
  omission. The dormant `hls_codecs` plumbing gets a comment (§8.3), not
  a consumer.
- **Do not rebuild the Bonjour stack.** §4.4 is a scheduling fix; the
  `NWBrowser` migration is future work with its own device-test budget.
- **Do not send `maxheight`.** Deliberate policy (§2.3): a decodable 4K
  stream direct-plays and the device downscales.
- **Do not resurrect the live-validation harness as-is.** Any future
  harness obeys §4.1's fencing rule: platform+DEBUG-fenced, no credentials
  from launch args in Release, no persisted-settings overrides.
- **Do not treat LOW items as release-blocking** (assessment). They ride
  along in §7 commits that touch their files, or wait.
- **Do not switch the working tree's branch or rebase anything** without
  Paul — a Codex agent works this checkout natively; branch choice is a
  kickoff decision (header note).
- **Docs move with behavior.** A commit that changes what a client does
  updates the claim about it (§7.6 lists the known debt); this plan's own
  claims are dated 2026-08-02 and go stale like any others — re-verify
  cited lines before editing around them.
