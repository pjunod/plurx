# Apple native subtitles — continuation plan: the review verdict and the road to a green DV master

**Status:** ready to execute · **Continues:**
[APPLE-NATIVE-SUBTITLES-HANDOFF.md](APPLE-NATIVE-SUBTITLES-HANDOFF.md) §8 ·
**Reviewed:** branch `agent/native-apple-subtitles` tip `f35ada1` against
production `787eaa6` · **Written:** 2026-08-02

Read the [handoff](APPLE-NATIVE-SUBTITLES-HANDOFF.md) first — it records what
shipped, why each choice was made, and what the live server proved. This
document is the continuation: an independent review of that implementation
(every finding re-verified against the code, three of them empirically against
the shipped FFmpeg flags), the defects to fix in priority order, and the
milestones that finish the arc. Work milestone by milestone (§5); every
milestone ends with an acceptance check that is a command or an observable
fact. Standing instruction: if a step seems to require changing the wire
contract (§3), editing the HLS master outside the §5.4 ladder, or crossing a
guardrail (§6), stop and flag it instead of improvising. The landing route
(§4) was deliberately left to you — decide it at kickoff, say which route you
took, and record why.

## 1. Where the work stands — reviewed, green, one red light

### 1.1 The refs, so nobody guesses

| Ref | Commit | What it is |
|---|---|---|
| `origin/agent/native-apple-subtitles` | `f35ada1` | The feature branch: all client work + docs. This plan lives here. |
| `origin/main` = `deploy/native-apple-subtitles` | `787eaa6` | Production server, deployed on `nynuc` · `m6` · `nuc3` (`nuc4` blocked by Plex on the port, accepted) |
| merge-base of the two | `a1d1dd2` | Where the lines diverged |
| local `main` | `4bb7dd6` | `origin/main` + the status-page commit, **unpushed** |
| local `agent/native-apple-subtitles` | was `f8655c1` | Advanced to carry this plan's commit |

Two facts about this topology, both verified by full-diff audit, both load
bearing for §4:

1. **The branch is a strict superset of `origin/main` in code.** Main's 37
   post-divergence commits touched nine server files; eight are byte-identical
   at both tips (the branch consolidated them via `513d4a8` + merge
   `ba93846`), and the ninth (`http/mod.rs`) differs only by the branch's
   added `/connect.svg` route. Every "−" hunk in `git diff origin/main
   origin/agent/native-apple-subtitles` is merge-base-era content the branch
   itself replaced, in files main never touched after divergence. Nothing on
   main is lost by taking the branch.
2. **Main has none of the client work.** No Apple subtitle code, no Android
   version/badge/focus work, no QR connect, none of the five docs
   (handoff, both reviews, remediation plan, badges plan). Until the branch
   lands, production deploys carry a server whose clients and documentation
   exist only on this branch — and any *server* fix committed only here
   (§2's P0-2, P0-3) cannot reach the fleet, because Ansible pins every node
   to `origin/main`.

The branch history contains duplicated commit pairs (same subject and
timestamp, different hashes — e.g. `9d012db`/`6e0d137`, `ab8988d`/`3ce4153`).
That is an artifact of merge `ba93846` pulling in a parallel replay of the
server line. Harmless; do not "clean it up" with a rewrite.

### 1.2 The review verdict

The implementation is sound and worth keeping — the review confirmed, in
code, every load-bearing handoff claim: the minimal master shape and its test
pins, DEFAULT/FORCED/AUTOSELECT emission rules, title-based Forced detection
(file 5615's contract), the shared native-vs-burn classification on both
sides of the wire, subtitle playlists mirroring all three video-playlist
producers, per-segment VTT slicing with the 2^33 `X-TIMESTAMP-MAP` wrap,
capability-scoped child URLs with index validation, cancellation-surviving
atomic extraction, in-place native select/Off on the Apple client with no
session churn, and the burn carve-out sending source height. The guardrail
against the whole-file `/files/{id}/subs/{index}.vtt` endpoint is respected
(no client reference exists).

What the review *added* is §2: two server timing defects that ship wrong cues
today on the flagship paths, one policy violation in the client's automatic
selection, and a punch list of robustness gaps — each with the failure
scenario and the fix.

### 1.3 The gate, re-run for this review

The branch tree passed the full gate on 2026-08-02, independently of the
handoff's run: `cargo fmt --all -- --check` clean, `cargo clippy --workspace
--all-targets -- -D warnings` clean on 1.97.1, `cargo test --workspace
--no-fail-fast` **528 passed / 0 failed / 1 ignored** (the ignored test is
`storeprobe`'s deliberate 600 MB probe). Environment: rustc 1.97.1, 2 cores,
ffmpeg 6.1.1, macOS-style symlinked `TMPDIR`. The twelve subtitle/HLS tests
(`native_hls_master_advertises_selection_language_names_and_forced_metadata`,
`subtitle_playlist_and_vtt_mirror_video_segments_at_resume_timeline`,
`cold_extraction_is_deduplicated_and_survives_waiter_cancellation`, …) all
pass. A green gate is necessary, not sufficient: both P0 timing defects below
live in the gap between what the tests assert and what the muxers actually
emit — the suite pins the wrong expectation (§2.2).

## 2. Defects to fix — three P0s, four P1s, a punch list

Line numbers cite the reviewed tree at `f35ada1`; re-verify against the file
at build time in case code moved. Fix P0s first and together (§5.1) — they
are small, and two of them are wrong on the live fleet right now.

### 2.1 P0-1 · Automatic selection can start a burn for a non-forced track

`clients/apple/Sources/PlayerController.swift:624-628`:

```swift
return matching.first(where: {
    $0.forced || $0.title?.localizedCaseInsensitiveContains("forced") == true
})?.index
    ?? matching.first(where: { $0.default })?.index
    ?? matching.first?.index
```

The `default` and `first` fallbacks ignore format. Owner policy (decided
2026-08-02, recorded in
[CLIENTS-REMEDIATION-PLAN.md](CLIENTS-REMEDIATION-PLAN.md) §3.1): **automatic
selection must never start a burn — except a forced track, which may, always
at source height.** Failure scenario: viewer language `eng`, a Blu-ray remux
whose only English subtitle is a non-forced PGS (or an anime MKV whose only
English track is ASS) → `load()` assigns it (`PlayerController.swift:255-258`)
→ `open()` maps it to `subtitleBurn` → every playback start silently spawns a
burn-in transcode: video encoder, DV/HDR loss, the exact bug this project
existed to kill. This is *broader* than the pre-branch behavior, which only
auto-picked `default`-flagged tracks.

**Fix:** constrain the second and third fallbacks to `$0.isNativeHLS`
(return nil when the only language matches are burn formats). Keep the forced
arm format-agnostic — a forced bitmap track is the sanctioned exception, and
it already burns at source height via `burnHeight`
(`PlayerController.swift:324-325`); that carve-out must survive the fix.
Add the XCTest §2.6 demands: today's `testAutomaticSubtitles…` fixtures are
all-subrip, so this hole is invisible to the suite.

### 2.2 P0-2 · Copy-session cues lead the picture by up to a GOP

`crates/plurxd/src/http/hls.rs:445-451` shifts cues by the *requested*
`context.start_seconds`. But copy sessions seek with `-noaccurate_seek -ss`
(`crates/plurx-core/src/transcode/mod.rs:986`, comment at `:978` explains
why), so the session's media timeline origin is the **preceding keyframe**,
not the requested start. Cues therefore lead the picture by
`(start − keyframe)` ∈ [0, GOP) on every resumed or seeked copy session —
the flagship Apple path. On 4K film GOPs that is 1–6 s of visible lead. The
existing test enshrines the wrong base:
`crates/plurxd/src/http/mod.rs:4011-4028` asserts `00:00:00.000` for a
9–11 s cue at start 10 over a 0.8 s-GOP fixture, which is only correct when
start lands exactly on a keyframe.

**Fix:** resolve the *actual* session origin into the session context and
shift cues by that. Two implementation options, in preference order: (a)
derive it from the first published init/media segment's PTS (truthful for
both the ffmpeg-muxed and `copyseg` producers, one probe, cacheable on the
session); (b) probe the last keyframe ≤ start at create time. Update the
mod.rs test to a fixture where start ≠ keyframe and assert the cue lands in
sync — that test is the tomb for this bug.

### 2.3 P0-3 · Transcode sessions render native cues ~1.4 s early

Transcode sessions emit MPEG-TS segments whose first video PTS is ≈1.423 s
(verified empirically with the exact `hls_args` flags — no `-muxdelay` /
`-muxpreload` are set, so ffmpeg's defaults apply), but the subtitle slicer
anchors segment 0 at `X-TIMESTAMP-MAP=MPEGTS:0`
(`crates/plurxd/src/http/hls.rs:767-775` computes
`segment_start × 90000`). Native cues therefore render ~1.42 s early on
every transcode+native session. The client sends `nativeSubtitles: true`
unconditionally (`PlayerController.swift:332`), so this hits any explicit
quality rung — and the compatibility-transcode fallback that file 5615
currently lands on after `-12927`.

**Fix:** add `-muxdelay 0 -muxpreload 0` to `hls_args`
(`crates/plurx-core/src/transcode/mod.rs:941-961`); verified to move the
first PTS to 0.000000. Then add the test class §2.6 names: compare the
emitted `X-TIMESTAMP-MAP` against the *measured* first PTS of a produced
session, for both the mpegts and copy paths — the test that would have
caught both P0-2 and P0-3.

### 2.4 P1s — selection robustness and extraction hardening

**P1-1 · Rendition NAME is not deduplicated, and the client maps options
positionally.** Server: `hls.rs:499-512` gives two same-language untitled
tracks the identical `NAME` ("English" twice), violating RFC 8216
§4.3.4.1's MUST-unique rule. Client: `PlayerController.swift:566-570`
selects `group.options[ordinal]` — a positional bet with no contractual
basis. AVFoundation may merge same-NAME entries and may synthesize a
phantom closed-caption option into the `.legible` group (the master's
`EXT-X-STREAM-INF` carries no `CLOSED-CAPTIONS=NONE`), and either event
shifts every ordinal: "English Forced" selects Italian, or nil. It worked
on device for 5615's clean five-name master; it is not robust. **Fix,
client side (safe now):** filter `group.options` to subtitle-type options
and match by `extendedLanguageTag` + `displayName` against a client-side
replica of the server's `subtitle_name`, falling back to ordinal. Server
side: de-dupe NAME ("English (2)"). `CLOSED-CAPTIONS=NONE` is the *correct*
HLS authoring fix but it is a master change — it goes through the §5.4
ladder, never directly to production (§6.2).

**P1-2 · Selection changes racing an in-flight open() are dropped, then
locked out.** `reopen()` silently returns while `isChangingStream`
(`PlayerController.swift:271`), `open()` re-applies the nativeSubtitle it
captured at entry (`:295-296`, applied at `:378`), and
`guard index != selectedSubtitle` (`:179`) makes re-tapping the shown
checkmark a no-op. With cold extraction taking up to 180 s, a viewer who
picks a track mid-open sees a checkmark on N while the stream renders the
old selection, and tapping N again does nothing. **Fix:** reconcile after
`open()` completes — if `selectedSubtitle` changed during the awaits, apply
the current value (native in place; one follow-up reopen for burn).

**P1-3 · Legible-group failure is a silent no-op that leaves the UI
lying.** `guard let group = try? await …loadMediaSelectionGroup(for:
.legible) else { return }` (`PlayerController.swift:565-566`). Against an
older server that ignores `native_subtitles` (no `?native=1`, no subtitle
group in the master), every native selection does nothing while the
checkmark stays and the stats panel reports "native WebVTT". **Fix:** on
group/ordinal-resolution failure, fall back to the legacy burn/reopen path
for text tracks (older servers accept `subtitle_burn` for them), or surface
an error and clear `selectedSubtitle`. Never leave state claiming a
selection the player isn't rendering.

**P1-4 · Extraction has no timeout and no failure memo.**
`crates/plurxd/src/subtitles.rs:117-143`: a wedged ffmpeg (NAS stall) parks
every waiter forever on `notified()` with the flight stuck in the map; a
*failed* extraction removes the map entry and caches nothing, so each
subsequent VTT segment request from AVPlayer relaunches a full-source read
of a 19 GB MKV that fails again — repeated multi-GB scans plus 500s.
**Fix:** bound the producer (timeout + kill), and memoize the `Err` per
key for a short TTL so a failing track costs one scan per window, not one
per segment request.

### 2.5 P2 punch list

Real, smaller, none block §5.4. Do them opportunistically inside the
milestone whose files they touch.

| # | Where | Defect → fix |
|---|---|---|
| P2-1 | `hls.rs:514-520` | `contains("forced")` classifies "Non-Forced"/"Unforced" titles as FORCED=YES, hiding the track from Apple's subtitle UI → word-boundary match with a negation guard; keep plain "Forced" (5615's contract) |
| P2-2 | `hls.rs:467-481` | `language_tag` maps 10 languages, passes "dut"/"cze"/"gre"… through as non-BCP-47, defeating viewer-language matching; duplicates a smaller table than `tracks.rs`'s `LANG_ALIASES` → derive from the one existing table |
| P2-3 | `hls.rs:747-760` | A cue spanning an internal segment boundary is split and hard-clipped into both segments (flicker at 6 s boundaries; authored end time destroyed) → emit the overlapping cue un-clipped in every segment it intersects; AVPlayer dedupes by cue identity |
| P2-4 | `hls.rs:427-429` + `subtitles.rs` | Whole cached VTT re-read + re-parsed per segment request, no size cap on the sidecar → cap accepted sidecar size at publish; consider a parsed cue index per (file, index) |
| P2-5 | `PlayerController.swift:373-379` | Every reopen force-resumes (`player.play()`), un-pausing a paused viewer on burn switch / audio change / compat fallback → capture and restore `isPlaying` + rate |
| P2-6 | `PlayerController.swift:480,508-533` | Compat fallback reads a dead item's `currentTime()` (0/invalid) → VOD/direct items that fail pre-seek restart at 0:00; the superseded `open()` can also overwrite `playbackError` late → retry from last observed `currentMs`; fence the superseded open |
| P2-7 | `PlayerController.swift:302` | `direct = … && !hasNativeSubtitles` abolishes true direct play whenever any native track exists, even with subs Off forever — every such play becomes a copy session, and on Bedroom today that path degrades to compat transcode (`-12927`) → decision required: keep (in-place selection needs the session master) and document, or stay direct until first native selection and accept one reopen at that boundary. Flag to Paul with the tradeoff; don't decide silently |
| P2-8 | `PlayerController.swift:133,581-587` | `appliesMediaSelectionCriteriaAutomatically = true` + legible criteria lets AVPlayer auto-enable a rendition before the explicit selection lands: transient double subtitles on burn sessions whose file also has same-language text → apply selection before `play()`, or own selection fully with criteria off |
| P2-9 | `PlayerController.swift:621-647` | Untagged-language tracks are never auto-selectable (`languageCode(nil)` ≠ pref), diverging from the server's shared policy ("untagged tracks remain eligible", `tracks.rs:178`) → decide and align both sides; failure direction today is safe (no subtitle) |
| P2-10 | `crates/plurxd/src/web/index.html:1256` | "Scan this from the **Plurx** iPhone…" hardcodes the brand in user-facing text; this tree brands as `${APP_NAME}` ("cinemarr") → interpolate APP_NAME. (`.connectqr img` background `#fff` is a deliberate QR quiet zone — keep, comment it) |
| P2-11 | `hls.rs:563-571` | Two forced tracks in one language both get FORCED=YES + AUTOSELECT=NO; Apple's authoring rules require AUTOSELECT=YES on forced renditions → candidate rung for the §5.4 ladder, not a direct change |

### 2.6 Tests the suite is missing

The gate is green because these don't exist. Each lands with its milestone:

- Auto-selection excludes burn formats in the `default`/`first` fallbacks;
  forced-bitmap auto-burn sends `height == decision.source.height` (M1).
- `X-TIMESTAMP-MAP`/cue shift vs the **measured** first PTS of a real
  produced session, mpegts and copy paths (M1) — the tomb for P0-2/P0-3.
- create() 400s: unknown native index, burn-only native request; child-route
  400/404s (burn-only index, out-of-range) (M2). The handoff's "server
  validation still rejects" claim is currently entirely untested.
- Route-level Off: `?native=1` without `subtitle` → zero `DEFAULT=YES` while
  a container-default non-forced track exists (M2).
- NAME uniqueness with duplicate same-language untitled tracks; `quoted()`
  escaping of quotes/CRLF/commas in titles (M2).
- `selectSubtitle` routing itself: burn→native reopens once, native→native
  stays in place, `activeBurnedSubtitle` state machine — the handoff's
  in-place claim is proved only for the pure helper today (M2).
- Growing EVENT playlist: successive subtitle-mirror fetches stay stable
  (M2). Cue spanning an internal boundary appears in both segments (P2-3's
  tomb, M3).
- Compat fallback re-applies the native selection and preserves
  position/pause (M2/M4).

## 3. Contract — pinned facts; re-verify at build time

### 3.1 The wire contract

Copied from the reviewed code; re-verify against `hls.rs` / `stream.rs` /
`PlurxAPI.swift` at build time in case fields moved.

```json
{
  "start": 777,
  "native_subtitles": true,
  "subtitle": 2,
  "subtitle_burn": null,
  "copy": true,
  "preserve_dolby_vision": true,
  "height": null
}
```

- `subtitle` is the **decision index**: the track's position within the
  decision's `subtitle_streams` list. The master enumerates the same list in
  the same order and names child URIs `subs/{index}` by that position — one
  index space end to end, no re-derivation.
- Native select: `native_subtitles:true` + `subtitle:N`, **no**
  `subtitle_burn`. Off: `native_subtitles:true`, `subtitle` absent. Burn:
  `subtitle_burn:N` (the server accepts `native_subtitles:true` alongside —
  the client always sends it).
- `height`: null lets server Auto pick; a burn on a file the device could
  otherwise take sends `decision.source.height` (the source-height
  carve-out); `selectedHeight` wins when the viewer chose a rung.
- Playlist URL comes back with `?native=1&subtitle=N`; children are
  `/api/v1/hls/{session}/subs/{index}/index.m3u8` and `…/seg{seq}.vtt`.
  The session id is the capability — no bearer headers on child fetches,
  and no bearer tokens in child URLs, ever (handoff §3.2's reasoning).
- The whole-file `/files/{id}/subs/{index}.vtt` endpoint has no offset
  parameter: source-timeline only, valid only for direct play. Never inside
  an offset session.

### 3.2 The master shape, and what pins it

Production master is deliberately minimal: `#EXT-X-VERSION:7`, one variant
with `BANDWIDTH`, plus `SUBTITLES` group when native tracks exist. **No
CODECS** (test-pinned: `!contains("CODECS=")`), no `SUPPLEMENTAL-CODECS`, no
`VIDEO-RANGE`, no `EXT-X-INDEPENDENT-SEGMENTS`. This shape is the survivor
of real-device iteration: every richer master either changed nothing or
reintroduced `-1002` (`ed38ea9`, reverted by `787eaa6` — never reapply).
Rendition lines carry `NAME` / `LANGUAGE` / `DEFAULT` / `FORCED` /
`CHARACTERISTICS` / `AUTOSELECT` per handoff §3.4; forced renditions are
always `DEFAULT=NO`, so clients select explicitly. Title-based Forced
detection (disposition `forced=false`, title "Forced") is contract, not
cleanup fodder — file 5615 is the regression case.

### 3.3 Owner policy (verbatim, decided 2026-08-02)

Automatic subtitle selection must never start a burn — **except a forced
track, which may, at source height.** A container default in another
language is never a fallback. Explicit user selections may burn (that is
the user asking); the policy governs what the player does on its own.

### 3.4 Versions, changelog, release path

Workspace `0.2.0` (single source: root `Cargo.toml` `[workspace.package]`);
Apple `0.2.0` build `5` (`clients/apple/project.yml`); Android `0.2.0`
versionCode `3` (`build.gradle.kts`). **`CHANGELOG.md` `[Unreleased]` has no
entry for any of this arc** — native renditions, in-place Apple selection,
QR connect, version stamps, MediaFacts — on either branch or main; that debt
lands with M5. Per [RELEASING.md](RELEASING.md): entries describe what an
operator sees, not the diff; at release this arc is a backwards-compatible
feature set → minor bump `0.2.0 → 0.3.0`, tag `v0.3.0` (CI refuses a tag
that disagrees with Cargo.toml). Next store uploads: Apple build ≥ 6,
Android versionCode ≥ 4.

## 4. Landing route — decide at kickoff

Paul delegated this decision to the executing agent (2026-08-02). Decide it
first, announce it, record why. The facts in §1.1 make either route cheap;
they do not make them equal.

```
            origin/main 787eaa6 ──── deployed (ansible pins nodes here)
           /                    \
  a1d1dd2 ─                      ?── the reconciliation you are deciding
           \                    /
            branch f35ada1 ────── all clients + docs + QR (+ this plan)
```

### 4.1 Route A — merge the branch into main, then branch per task

`git merge agent/native-apple-subtitles` into main should be conflict-free
or nearly so (§1.1: strict code superset; the residual diff is additions).
Then every §5 milestone works as a short topic branch off main, merged when
its acceptance passes.

- For: server fixes (P0-2, P0-3) become deployable immediately — the fleet
  only follows `origin/main`. Docs, clients, and QR stop being stranded.
  One line of history going forward.
- Against: lands ~4,500 line-adds in one merge; if anything in the client
  tree was not ready to be "on main", it is now. (The review found nothing
  in that category — the client work is the *newest* validated state, per
  handoff guardrail §9.8.)

### 4.2 Route B — continue on the branch, fold back at the end

Merge `origin/main` into the branch (trivial by the same superset logic),
do all §5 work there, merge to main when the DV master plays on Bedroom.

- For: main stays exactly what is deployed until the whole arc is done.
- Against: P0-2/P0-3 are live-fleet defects — cues are wrong on resumed
  copy sessions *today*. Holding those fixes hostage to the DV
  investigation inverts the priority. If you pick B anyway, carve the two
  server fixes out to main first (cherry-pick), then continue on the
  branch.

Recommendation embedded in the structure: A, or B-with-the-carve-out.
Either way §5.1 reaches `origin/main` before §5.4 starts.

### 4.3 Externalities the routes share

- **Ansible repo:** the inventory changes (nodes pinned to `origin/main`,
  `nuc3` added) are local commits `26d8b2b` + `1d6ac4c` in `~/code/ansible`
  — a repo with **no git remote**. They exist on Paul's machine only. Deploys
  work; just know the pin lives outside this repo, and say so when you
  deploy. `nuc4`'s port conflict with Plex is accepted — do not report it
  as a running plurx.
- **Local `main` is ahead of `origin/main`** by the status-page commit(s);
  pushing main publishes those too. Fine — just expected.
- Pushes are native on Paul's machine; agent sessions commit locally and
  say what needs pushing.

## 5. Milestones

Run the full gate (`make check`, i.e. fmt + clippy `-D warnings` + `cargo
test --workspace --no-fail-fast`, on the pinned 1.97.1) before every
handover, and the Apple simulator suites for client changes (commands in
[clients/apple/README.md](../clients/apple/README.md)). Commit in sensible
feature/fix units as you go; update
[CHANGELOG.md](../CHANGELOG.md) `[Unreleased]` and the status page
(`docs/STATUS.html`) in the same commits as the behavior they describe.

### 5.1 M1 — policy and timing correctness (P0-1..3)

Fix the three P0s together with their tombs from §2.6: the auto-selection
format constraint (keeping the forced-at-source-height carve-out), the
copy-origin cue shift, `-muxdelay 0 -muxpreload 0`, and the
measured-first-PTS test class. Server fixes reach `origin/main` per §4 and
deploy to the fleet via the ansible playbooks; client fix ships in the next
TestFlight build (≥ 6).

**Acceptance:** new tests green in the gate; on a resumed 4K copy session
with native subs, a cue's text and its dialogue are in sync at the seek
point (observable on Bedroom or simulator against a real file); an
auto-selection XCTest proves a non-forced PGS-only language match selects
nothing.

### 5.2 M2 — selection robustness (P1-1..3)

Client-side option matching by language+name replica (positional only as
fallback), post-open selection reconciliation, legible-group failure
fallback; server-side NAME de-duplication. Add §2.6's validation, Off,
routing, and escaping tests. `CLOSED-CAPTIONS=NONE` goes on the §5.4 ladder
list, not here.

**Acceptance:** gate + simulator suites green including the new tests; a
two-untitled-English-tracks fixture selects the right track through the
client mapping; killing the server mid-open leaves no lying checkmark.

### 5.3 M3 — extraction hardening (P1-4, P2-4)

Producer timeout + failure memo with TTL; sidecar size cap at publish;
internal-boundary cue duplication (P2-3) while you are in the slicer.

**Acceptance:** a test where the producer wedges (fake ffmpeg sleeping)
times out, waiters get a bounded error, and a repeat request within the TTL
does not relaunch extraction; boundary-spanning cue appears whole in both
segments.

### 5.4 M4 — make the copied DV master physically playable (`-12927`)

The handoff's §8.1 ladder stands. Two amendments from this review:

**Step 0 — capture the evidence before writing any code.** The one artifact
nobody has examined is the init segment itself plus AVPlayer's actual error
chain. From a live copy session of file 5615: fetch `init.mp4`, dump its
boxes (`hvcC` contents, presence/shape of `dvcC`/`dvvC` next to `hvc1` —
`-strict unofficial` muxes real dvcC since `200bf77`), and capture the
device's CoreMedia error log for the master-wrapped vs direct-playlist
plays of the *same* URL. The direct child playlist plays and the
master-wrapped one does not; whatever differs in AVPlayer's handling is in
that gap, and the boxes + logs narrow it before experiment one.

**Ladder additions:** after the handoff's steps 1–5, two candidate rungs
this review surfaced — `CLOSED-CAPTIONS=NONE` on the variant (Apple
authoring rules; also de-risks P1-1's phantom-option hazard) and
`AUTOSELECT=YES` on forced renditions (P2-11, an authoring-rules
compliance item). One change per deploy, device-observed, exactly like the
rest of the ladder. Unit tests prove syntax; only Bedroom proves
acceptance.

**Acceptance:** the handoff's own bar, verbatim — Bedroom plays file 5615
from the native master ≥ 60 s, seeks, resumes, no `-12927`, no
compatibility fallback; then §8.2's DV-proof list (display path reports
Dolby Vision; track 2 → 3 → Off in place with no new session and no new
FFmpeg process). Revisit P2-7 (direct-play abolition) after this lands —
the degradation it causes on Bedroom disappears when the master plays.

### 5.5 M5 — hardware matrix, docs, release

Run the release gate from
[APPLE-CLIENT-PARITY.md](APPLE-CLIENT-PARITY.md) on iPhone and Apple TV
(direct MP4 · copied MKV · transcode · native text · PGS burn · resume ·
seek · audio switch · autoplay). Then pay the documentation debt in the
same series of commits:

- `APPLE-CLIENT-PARITY.md` §1 item 4 still says text-subtitle selection
  "restarts at the same position" — contradicted by its own §2; fix it and
  the release-gate case it feeds.
- Root `README.md`'s reading path links none of the five client docs; add
  the handoff (and this plan) with a clause each.
- `CHANGELOG.md` `[Unreleased]` entries per §3.4; release `0.3.0` when
  Paul calls it.
- `docs/STATUS.html`: Apple tile/section and operator checklist still said
  "P0 in source, not yet shipped" — refreshed 2026-08-02 alongside this
  plan; keep it current as milestones land.
- Apple build ≥ 6 to TestFlight; Android versionCode ≥ 4 when its parity
  work ships (out of scope here, §7).

**Acceptance:** matrix results recorded in the parity doc; docs above
updated in the same PR/commits as their behavior; CHANGELOG entry exists
for every user-visible change this plan shipped.

## 6. Guardrails — what this pass must not do

Handoff §9's eight guardrails stand unchanged; read them. Additions, each
with its reason:

1. **No master changes outside the §5.4 ladder** — including
   "obviously correct" ones like `CLOSED-CAPTIONS=NONE`. Every master
   regression so far passed unit tests and failed on the physical device;
   the ladder exists because the device is the only oracle.
2. **Keep the `!contains("CODECS=")` pin and the title-based Forced tests.**
   They encode device-verified survivals; a refactor that "simplifies" them
   away reopens closed wounds.
3. **Do not fix P0-2 by pointing sessions at the whole-file VTT endpoint.**
   Its timeline is wrong by construction inside an offset session (handoff
   §9.3); the fix is a truthful origin, not a different wrong one.
4. **Do not let the P1-3 fallback send `subtitle_burn` for a track the
   server classifies native** on current servers — that recreates the
   original bug through the back door. The legacy-burn fallback is for
   servers that predate `native_subtitles` only.
5. **Preserve the forced-burn source-height carve-out** when touching
   auto-selection or `burnHeight` — it is owner policy (§3.3), and it is
   easy to delete by accident while adding the format constraint.
6. **Verify like the deployment target.** Rust gate on pinned 1.97.1 with
   `--no-fail-fast`; simulator suites for client work; physical Bedroom for
   anything the handoff's §6.3 class touches. A green sandbox is not a
   green fleet.
7. **Commit docs and status with behavior, push through Paul.** Sessions
   commit locally; nothing here can push. Say plainly what needs
   `git push` and what needs an ansible deploy after each milestone.

## 7. Non-goals — deliberately out of scope

- **Android native-subtitle parity.** Planned, owned by
  [CLIENTS-REMEDIATION-PLAN.md](CLIENTS-REMEDIATION-PLAN.md) §5.4 — Media3
  text-track overrides against the same wire contract and classification;
  its acceptance is written there ("selecting an SRT on a 4K HDR remux
  starts no video encoder and keeps HDR … toggling between two text tracks
  does not create a new server session"). Implement it in its own session;
  do not copy AVPlayer idioms sideways.
- **Media badges.** Separate arc, separate plan
  ([MEDIA-BADGES-PLAN.md](MEDIA-BADGES-PLAN.md)); the only contact point is
  that both read the same track metadata.
- **Web-player subtitle UX.** The web client's burn-based flow is unchanged
  by this arc; changing it is its own decision.
- **The Chrome DV remux refusal.** Same family of symptom (a browser MSE
  parser rejecting a DV copy), different decoder, different evidence trail.
  §5.4's step-0 box dump will produce artifacts that investigation can
  reuse, but do not merge the two hunts — they have different oracles.

---

This plan is kept honest the same way the handoff is: statuses and dates are
absolute, every fix cites the line it changes, and the acceptance for each
milestone is observable. When reality disagrees with this document, update
the document in the commit that proves it.
