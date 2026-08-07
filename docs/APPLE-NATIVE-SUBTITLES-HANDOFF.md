# Apple native subtitles — what shipped, why, and what remains

**Status:** native text subtitles deployed · copied Dolby Vision resolved on
physical Apple TV 2026-08-03 · **Written:** 2026-08-02 · **Resolved:**
2026-08-03

Companion to [APPLE-CLIENT-PARITY.md](APPLE-CLIENT-PARITY.md), which owns the
viewer parity matrix, and
[CLIENTS-REMEDIATION-PLAN.md](CLIENTS-REMEDIATION-PLAN.md), which owns the
cross-client implementation plan. Read this document first when continuing the
subtitle or Dolby Vision work. It records what changed, why each choice was
made, what was actually tested and deployed, and the original red result. The
2026-08-03 resolution supersedes that historical failure: the OS 26 client had
stopped advertising DV even though `availableHDRModes` still reported it.

The short version: ordinary SRT/SubRip/WebVTT subtitles are native AVPlayer HLS
renditions now. They do not cause subtitle burn-in, a video encoder, a quality
change, or a new session when switched. Scary Movie now retains Dolby Vision
end to end on the physical Apple TV: the client advertises DV only when both
generic HDR eligibility and the current output's DV mode agree, the server
preserves the RPU, and AVPlayer opens the standards-labeled master.

## 1. Outcome — native subtitles and copied Dolby Vision shipped

- [x] SRT, SubRip, and WebVTT tracks are advertised as native HLS WebVTT
  renditions.
- [x] The HLS master carries track language, display name, default, forced,
  and accessibility metadata.
- [x] AVPlayer can fetch the master, child subtitle playlist, and VTT segments
  through capability-authenticated session URLs without adding bearer headers.
- [x] Resume and seek offsets are reflected in VTT segment timing.
- [x] Selecting another native text track or Off uses AVPlayer media selection
  and does not recreate the HLS session.
- [x] Native text selection omits `subtitle_burn`, keeps the selected quality,
  and does not request a video encoder.
- [x] PGS, VobSub, and styled ASS/SSA retain the burn-in fallback.
- [x] English Forced wins over an Italian container default, including when
  `Forced` exists only in the English track title.
- [x] Cold subtitle extraction is deduplicated, atomic, and survives the first
  request being cancelled.
- [x] Apple build number 5 was tested on both simulator platforms and installed
  in place on Bedroom.
- [x] File 5615 reaches the server-side copy/native-subtitle path on Bedroom,
  keeps its Dolby Vision RPU, publishes `SUPPLEMENTAL-CODECS`, and reaches
  AVPlayer `readyToPlay` on the physical Apple TV.
- [x] Android native text subtitles shipped in
  [CLIENTS-REMEDIATION-PLAN.md](CLIENTS-REMEDIATION-PLAN.md) §5.4; physical
  acceptance remains.

`Native subtitles work` and `Dolby Vision works` remain separate claims. Both
are now supported by tests plus live physical-device evidence.

## 2. Why this changed — subtitle choice was accidentally a video decision

Before this work, `PlayerController` sent `subtitle_burn` for every selected
subtitle. That turned a presentation choice into a video processing request:

```text
select any subtitle
        │
        ▼
send subtitle_burn
        │
        ▼
open a new HLS session ──▶ run a video encoder ──▶ replace the player item
        │                          │
        │                          └── Dolby Vision/HDR may be lost
        └── quality and position have to be reconstructed
```

The server already extracted embedded text tracks into cached WebVTT through
[`subtitles.rs`](../crates/plurxd/src/subtitles.rs). The missing layer was an
HLS subtitle rendition that AVPlayer could discover and select itself.

The implemented split is deliberately format-based:

```text
selected subtitle
        │
        ├── SRT / SubRip / WebVTT
        │       └── native HLS WebVTT ──▶ AVPlayer media selection
        │
        ├── PGS / VobSub
        │       └── bitmap burn ──▶ reopen at the same film position
        │
        ├── styled ASS / SSA
        │       └── libass burn ──▶ preserve positioning and styling
        │
        └── Off
                └── deselect AVPlayer legible option in place
```

ASS and SSA remain burns even though they contain text. Converting them to
plain WebVTT can discard positioning, fonts, colors, and karaoke timing. Native
at any cost would be data loss wearing a performance badge.

## 3. Server contract — one HLS session owns video and subtitle timing

### 3.1 Track classification is shared policy

[`tracks.rs`](../crates/plurx-core/src/tracks.rs) is the source of truth for
which formats may become native renditions.

| Source codec | Delivery | Reason |
|---|---|---|
| SRT / SubRip | Native WebVTT | Text and timing convert without material presentation loss |
| WebVTT | Native WebVTT | Already matches the HLS text representation |
| PGS | Burn-in | The track contains positioned bitmap planes, not text |
| VobSub / DVD subtitle | Burn-in | The track contains bitmap subtitles |
| ASS / SSA | Burn-in | WebVTT cannot preserve the authored styling contract |

The Apple client mirrors this classification in
[`Models.swift`](../clients/apple/Sources/Models.swift). Server validation still
rejects a native request for a burn-only track, so a stale or incorrect client
cannot silently strip subtitle presentation.

### 3.2 Native requests omit the burn field

For native track 2, the Apple client creates the normal HLS session with the
following relevant fields:

```json
{
  "start": 777,
  "native_subtitles": true,
  "subtitle": 2,
  "copy": true,
  "preserve_dolby_vision": true
}
```

`subtitle_burn` is absent. For Off, `native_subtitles` remains true while
`subtitle` is absent. A bitmap or styled track uses `subtitle_burn` and the
existing reopen-at-position path.

[`hls.rs`](../crates/plurxd/src/http/hls.rs) returns the native master through
the session playlist URL with `native=1`. The master references:

```text
/api/v1/hls/{session}/index.m3u8
/api/v1/hls/{session}/subs/{index}/index.m3u8
/api/v1/hls/{session}/subs/{index}/seg{sequence}.vtt
```

The session identifier is the capability credential for every child resource.
This matters because AVPlayer fetches child playlists and segments itself and
does not inherit the API request's bearer header. Putting a long-lived bearer
token in every URL would make playlist logs and caches credential-bearing;
using the short-lived session capability keeps access scoped to one stream.

### 3.3 The subtitle timeline mirrors the video timeline

The subtitle media playlist copies the video playlist's media sequence,
segment durations, playlist type, and end state. Each VTT response is sliced to
one video segment and shifted using the session's `start_seconds`.

This is why the implementation does not point AVPlayer directly at the older
whole-file `/files/{id}/subs/{index}.vtt` endpoint. That file uses the source
timeline. A session resumed at 777 seconds needs cues mapped onto the
session-relative HLS timeline or captions will appear 777 seconds late.

The timing implementation and tests live in
[`hls.rs`](../crates/plurxd/src/http/hls.rs). The session context that carries
the source start belongs to
[`transcode.rs`](../crates/plurxd/src/transcode.rs).

### 3.4 Master metadata makes selection deterministic

The HLS master advertises each eligible track with:

- `NAME` from the normalized language and title;
- `LANGUAGE` as a BCP 47-compatible language tag;
- `DEFAULT` from the selected/default state without making every forced track
  a default;
- `FORCED` from either the disposition or a case-insensitive `Forced` title;
- `CHARACTERISTICS` for SDH/accessibility tracks;
- `AUTOSELECT` only when the metadata combination is safe for AVPlayer.

File 5615 is the regression case because its English forced track has
`forced=false` and `title="Forced"`. Title-based forced detection is therefore
part of the contract, not a heuristic that may be removed as cleanup.

## 4. Apple behavior — text selection stays inside the current player item

[`PlayerController.swift`](../clients/apple/Sources/PlayerController.swift)
now separates `nativeSubtitle` from `burnSubtitle` before creating a session.
It asks AVPlayer for the `.legible` media-selection group and maps the server's
native-track order onto `AVMediaSelectionOption` entries.

**Selecting SRT/SubRip/WebVTT:** the controller calls
`select(_:in:)` on the current item. It does not replace the item, create a new
session, change `selectedHeight`, or send `subtitle_burn`.

**Selecting Off:** the controller selects `nil` in the same media-selection
group. The current session and video rendition continue.

**Selecting PGS, VobSub, ASS, or SSA:** the helper returns false, and the
controller uses the established burn/reopen path at the current film position.

Automatic selection first filters tracks to the viewer's preferred language.
Within that language, forced disposition or a `Forced` title wins, followed by
the language-matching default and then the first language match. A default in
another language is never used as a fallback.

The Scary Movie preference shape is kept as an XCTest in
[`AppleClientTests.swift`](../clients/apple/Tests/AppleClientTests.swift):

| Index | Language | Title | Container flags | English result |
|---:|---|---|---|---|
| 0 | Italian | Forced | default, forced | Not selected |
| 1 | Italian | Regular | none | Not selected |
| 2 | English | Forced | `forced=false` | Selected natively |
| 3 | English | Regular | none | Available in place |
| 4 | English | SDH | none | Available in place |

## 5. Extraction safety — the producer outlives an impatient client

[`subtitles.rs`](../crates/plurxd/src/subtitles.rs) deduplicates a cold cache
miss by its final sidecar path. Concurrent callers wait on the same extraction
instead of starting parallel FFmpeg processes.

The producer is not owned by the first HTTP request. If that request times out
or AVPlayer cancels it, extraction continues, validates the output, and
atomically renames the completed temporary file into the cache. A later caller
therefore sees the published `.vtt` instead of a completed but stranded `.tmp`
file.

The regression test
`cold_extraction_is_deduplicated_and_survives_waiter_cancellation` cancels the
first waiter, permits the producer to finish, and verifies both publication and
deduplication.

## 6. Verification — what the gates and live server actually proved

### 6.1 Automated gates passed

The implementation passed the requested Rust gate:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The workspace test run passed with one intentional large-storage probe ignored.
The server coverage includes native master and subtitle playlists,
capability authorization, metadata selection, resume timing, cancellation-safe
extraction, and bitmap/styled fallback.

Both Apple simulator suites passed:

| Suite | Result |
|---|---:|
| iOS Simulator | 35 / 35 |
| tvOS Simulator | 44 / 44 |

The relevant XCTest asserts automatic selection of file 5615's track 2 and
in-place native selection/Off without invoking the burn/reopen callback. The
repeatable simulator commands are documented in
[`clients/apple/README.md`](../clients/apple/README.md).

### 6.2 Live file 5615 proved the native server path

On the live server, English preferences selected subtitle index 2 at resume
offset 777. The server emitted a copy-video command containing:

```text
-ss 777 ... -c:v copy -tag:v hvc1 ...
```

There was no `subtitles=` FFmpeg filter and no video encoder selected because
of the subtitle. The subtitle child request logged:

```text
serving native HLS WebVTT subtitle
file_id=5615 index=2 codec=subrip language="eng" title="Forced"
start_seconds=777.0
```

That proves the server selected the requested English track, served it as
native WebVTT, aligned it to the resume point, and preserved the copy recipe.
It does not by itself prove that AVPlayer accepted the video rendition.

### 6.3 Physical Apple TV exposed the remaining video failure

The regression source is a 19 GB MKV with HEVC Main 10, Dolby Vision profile
8.1/HDR10 compatibility, and E-AC-3 audio.

Three observations narrow the remaining fault:

1. The copied video media playlist played directly on the physical Apple TV
   and AVPlayer fetched its init and media segments.
2. The codec-neutral multivariant master avoided the user-visible
   `NSURLErrorDomain -1002` (`unsupported URL`) failure.
3. AVPlayer still rejected the copied rendition reached through that master
   with CoreMedia `-12927`, after which the client opened its compatibility
   transcode fallback.

An experiment that derived and advertised exact codec data from the init
segment (`ed38ea9`) reintroduced `-1002`. It was reverted by production commit
`787eaa6`. Do not redeploy or reapply `ed38ea9`: its unit tests passed, but the
physical client rejected it.

The honest verdict is:

| Claim | Result |
|---|---|
| English Forced was selected | Passed |
| Subtitle was native WebVTT | Passed |
| Subtitle caused no burn filter | Passed |
| Server prepared copied Dolby Vision video | Passed |
| Physical AVPlayer played that copied DV master | **Failed** (`-12927`) |
| Playback retained Dolby Vision after fallback | **Failed**; fallback transcodes |

## 7. Deployment — code, nodes, and installed app

### 7.1 Plurx commits and refs

| Purpose | Ref / commit |
|---|---|
| Required starting point | `afa91d4` (`agent/fix-apple-detail-edge-clipping`) |
| Final feature content | `513d4a8` |
| Pushed feature tip | `origin/agent/native-apple-subtitles` at `ba93846` |
| Pushed production server | `origin/main` at `787eaa6` |
| Reverted unsupported codec experiment | `ed38ea9`, reverted by `787eaa6` |

The final production tree at `787eaa6` is byte-for-byte equal to the previously
validated codec-neutral tree at `a888073`. The shared checkout was left on the
user's unrelated documentation branch; implementation, validation, and deploy
work used isolated worktrees.

### 7.2 Ansible nodes

The media Ansible inventory now includes Plurx on `nuc3` and pins every Plurx
entry to `origin/main`, preventing a node from following whichever feature
branch happens to be checked out locally.

| Node | Result on 2026-08-02 |
|---|---|
| `nynuc` | Healthy, server build `787eaa6` |
| `m6` | Healthy, server build `787eaa6` |
| `nuc3` | Added to inventory; healthy, server build `787eaa6` |
| `nuc4` | Image built from `787eaa6`; service cannot bind port 32400 because Plex owns it |

The Ansible changes are local commits `26d8b2b` and `1d6ac4c`. That repository
has no configured Git remote, so those two commits could not be pushed. The
known `nuc4` conflict was explicitly accepted and ignored; do not misreport it
as a successful running Plurx service.

### 7.3 Bedroom Apple TV

The in-place install preserved the existing application data and reported:

| Field | Value |
|---|---|
| Device | Bedroom, Apple TV 4K (3rd generation), tvOS 26.6 |
| Bundle | `tv.plurx.app` |
| App version | `0.2.0` |
| Build | `5` |

CoreDevice accepted a non-foreground launch while the display was asleep.
Foreground activation was denied by tvOS with `System is asleep - foreground
app launch forbidden`; the app was installed correctly, but this should not be
described as a visible foreground launch.

## 8. Historical remaining-work plan — copied DV was resolved 2026-08-03

This section records the path that was open on 2026-08-02. It is not the
current task list. The outcome in §1 supersedes it: the client capability
advertisement, not the copied media, caused the fallback, and physical Apple TV
playback reached `readyToPlay` with Dolby Vision preserved on 2026-08-03.

### 8.1 Reduce the master failure to one attribute or relationship

Continue from the codec-neutral production tree. Compare the known-playing
direct video media playlist with the smallest possible master that references
the exact same URL. Add one feature at a time:

1. One video variant, no `CODECS`, no audio group, no subtitles.
2. Add the existing audio relationship without changing the video playlist.
3. Add one WebVTT subtitle group with no default selection.
4. Add forced/default/accessibility metadata.
5. Add standards-backed Dolby Vision signaling only after the codec-neutral
   wrapper plays.

Capture AVPlayer's error log and the server's child-resource requests for every
step. A unit test proving that a master is syntactically plausible is not a
physical-device acceptance test.

**Acceptance:** Bedroom plays file 5615 from the native master for at least 60
seconds, seeks, and resumes without CoreMedia `-12927` or compatibility
fallback.

### 8.2 Prove Dolby Vision rather than inferring it from `-c:v copy`

Once the master plays, verify all of these together:

- the server logs `copy-video` and `-c:v copy`;
- no `subtitles=` filter or video encoder appears;
- AVPlayer stays on the original HLS session;
- the output/display path reports Dolby Vision rather than SDR or a fallback
  transcode;
- switching track 2 to track 3 and then Off creates no new session and no new
  FFmpeg video process.

**Acceptance:** the physical output retains Dolby Vision while English Forced,
English Regular, and Off are selected in place.

### 8.3 Complete Android parity in a separate implementation session

Android still sends every server-controlled subtitle as `subtitle_burn`, which
reopens playback and may force a video encode. Implement §5.4 of
[CLIENTS-REMEDIATION-PLAN.md](CLIENTS-REMEDIATION-PLAN.md) rather than copying
Apple-specific AVPlayer code. Media3 must use its own text-track override while
preserving the same server contract and burn-only classification.

**Acceptance:** Android selects file 5615 track 2 natively, switches to track 3
and Off without creating another HLS session, and retains the selected quality.

### 8.4 Finish the real-hardware matrix

After the Dolby Vision fault is fixed, repeat the release gate from
[APPLE-CLIENT-PARITY.md](APPLE-CLIENT-PARITY.md) on iPhone and Apple TV:
direct MP4 · copied/remuxed MKV · ordinary transcode · native text subtitle ·
PGS burn · resume · seek · audio switch · episode autoplay.

Simulator coverage proves source and API behavior. It cannot prove hardware
decode, HDR/Dolby Vision output, HDMI behavior, or Siri Remote interaction.

## 9. Guardrails — what the next pass must not regress

1. **Do not send `subtitle_burn` for native text.** It recreates the original
   performance and HDR-loss bug.
2. **Do not convert styled ASS/SSA by default.** Losing authored presentation
   is not parity.
3. **Do not use the whole-file VTT endpoint inside an offset HLS session.** Its
   source timeline is wrong after resume or seek.
4. **Do not put bearer tokens in HLS child URLs.** Session capability URLs are
   sufficient and have a smaller exposure surface.
5. **Do not let a container default override the viewer's language.** File
   5615 exists because mux metadata is not a user preference.
6. **Do not infer Dolby Vision success from FFmpeg copy logs.** The physical
   decoder and display path are the acceptance boundary.
7. **Do not reapply `ed38ea9`.** It passed server tests and failed the physical
   Apple client with `unsupported URL`.
8. **Do not replace the Apple client with an older snapshot.** The work began
   at `afa91d4` specifically to preserve the newer Apple behavior on that
   branch.

The native subtitle implementation is complete enough to keep. The next job is
not another subtitle rewrite; it is a focused AVPlayer/Dolby Vision master
compatibility investigation with physical-device evidence as its gate.
