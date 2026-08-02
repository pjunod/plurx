# Client code-review assessment — what to trust and what to fix first

**Assessed:** 2026-08-01 · **Code:** `98165c615` plus the working tree ·
**Outcome:** assessment only; no client behavior changed

Companion to [CLIENTS-CODE-REVIEW.md](CLIENTS-CODE-REVIEW.md), which contains
the complete findings and evidence. This document answers the narrower
question: *which conclusions should drive the next client work, where does the
review overstate its evidence, and what order produces the most user-visible
improvement?* Read [PLAYBACK.md](PLAYBACK.md) for the delivery contract and
[PLAYBACK-TESTING.md](PLAYBACK-TESTING.md) for the end-to-end validation model.

## Verdict — use the review as the remediation roadmap

The review is specific, internally consistent, and grounded in the actual
client/server contract. Its central conclusion is right: both clients usually
execute the server's delivery plan correctly, but Android supplies incomplete
inputs to that plan and both clients have gaps in stateful playback behavior.

The committed client code has not materially changed since the review's
`f8655c166` snapshot. The findings therefore remain applicable to the assessed
tree. The two uncommitted Apple validation files also remain present.

The review is most valuable where it follows a choice all the way through:

```text
 incomplete capability or policy input
                 │
                 ▼
       server makes a valid decision
                 │
                 ▼
 client executes a needlessly worse stream
                 │
                 ▼
 lower quality · encoder cost · failed playback
```

That causal chain is stronger evidence than a generic list of code smells. The
quality findings should be treated as product defects, not cleanup.

## Confirmed findings — the current code supports the report

| Area | Assessment | Consequence |
|---|---|---|
| Apple validation harness | Confirmed build break | The iOS sources reference a tvOS-only type. A direct iOS type-check fails in [PlurxApp.swift](../clients/apple/Sources/PlurxApp.swift). |
| Android quality override | Confirmed broken feature | Numeric rungs are sent as `force=720`, `force=1080`, and so on; the server parses them as Auto. |
| Android subtitles | Confirmed quality loss | Every server-controlled selection becomes `subtitle_burn`; text tracks do not use the server's native path and Auto burns omit source height. |
| Android player errors | Confirmed missing path | No player-error listener provides a failure screen or the one-shot compatibility transcode used by Apple. |
| Android autoplay toggle | Confirmed lifecycle defect | The preference is an effect key, so changing it releases the live player and then restarts through the same controller. |
| Android Dolby Vision | Confirmed under-reporting | Runtime caps send HDR but no `dv` or `dvprofile`, preventing supported DV delivery. |
| Android HDMI audio | Confirmed under-reporting | Caps inspect decoders but not the active output route's passthrough support. |
| Server switching | Confirmed credential-lifecycle defect | Both clients clear the in-memory token but retain the persisted token while saving a new origin. |
| Apple automatic subtitles | Confirmed expensive fallback | The selection chain ends at any same-language track, so cold start may automatically burn a bitmap subtitle. |
| Apple stream changes | Confirmed intent loss | A seek during an in-flight replacement is dropped, and the prior server session is deleted before its replacement succeeds. |

The rest of the review is directionally credible, but these items have the
clearest line from current code to broken behavior.

## Qualifications — correct the edges without weakening the findings

### Dolby Vision does not always take the same fallback

The review says every unsupported DV title takes the strip/remux path. The
server is more specific:

- DV with a compatible HDR base can be stripped and remuxed.
- DV without a compatible base must be re-encoded when the client does not
  claim support.

The omission remains a P1 quality problem. The second branch makes the cost
worse than the review's headline, not better.

### Audio passthrough must describe the active route and the actual format

Adding HDMI sink capabilities is the right fix, but a blanket
"encoding supported" check can over-claim. Capability reporting should use
Media3's route- and format-aware passthrough decision so codec profile, channel
layout, audio attributes, and the active output device agree. Recompute it for
each decision because reconnecting an AVR, soundbar, television, or headphones
changes the truthful answer.

### Some HIGH outcomes still need runtime proof

The Bonjour resolver and TV focus graph contain real design hazards. The
review's absolute outcomes — resolution *never* completes and focus navigation
*crashes* — are stronger than code reading alone proves. Keep them in the fix
queue, but require a focused device reproduction before calling either
deterministic.

The distinction matters: suspicious asynchronous or focus code justifies a
test; it does not manufacture a runtime result.

### Automatic subtitles are policy; automatic transcoding should not be

Whether a preferred-language setting means "always show full subtitles" is a
product choice. Starting a video transcode without an explicit viewer action
should not be. The durable rule is:

> Automatic subtitle selection must never turn copy playback into a burn.

Forced native text can remain automatic. Bitmap and styled tracks should
require an explicit selection, even when they are marked default.

### The server addendum is already a snapshot

Section 11 assesses `ab5438ca2` and `a2f5239f9`. Five subsequent HLS subtitle
commits landed before this assessment: `6e0d1379e`, `ab8988d76`, `8c071c674`,
`068cfc8c5`, and `98165c615`. Those commits do not invalidate the client
priorities, but §11 should not be treated as the current server verdict without
a new pass over the resulting playlist and cue-timeline behavior.

## Recommended order — restore trust, then raise the quality ceiling

| Order | Work | Why it comes here |
|---|---|---|
| P0 | Remove or fully fence the Apple validation harness | Restore a buildable iOS target and prevent test credentials or launch hooks from reaching Release. |
| P0 | Fix the Android autoplay effect | A normal in-player preference must not release the player. |
| P0 | Add Android player-error handling and one-shot rescue | A rejected or interrupted stream needs a deterministic recovery or a visible terminal error. |
| P0 | Clear persisted credentials when changing origins | A credential for server A must never be sent to server B. |
| P1 | Fix Android quality-force mapping | Explicit rungs should force a transcode decision before their height is applied. |
| P1 | Add Android DV and route-aware audio capabilities | These determine whether high-end hardware receives the source quality it was bought to play. |
| P1 | Rebuild Android subtitle handling around native text | Text subtitles should not re-encode video; bitmap burns must retain source height. |
| P1 | Decode and honor HLS `vod` | Cached sessions should resume and seek on their native timeline. |
| P1 | Prevent Apple automatic subtitle burns | Cold start should not consume an encoder or discard HDR because of a preference default. |
| P2 | Reproduce Bonjour and TV focus failures | Turn plausible runtime claims into pinned tests before changing their architecture. |
| P2 | Queue Apple seek/track intent and replace sessions safely | The newest viewer action should win without deleting the last playable stream first. |
| P3 | Address loading, refresh, filters, packaging, and doc drift | These matter, but they should not displace broken playback and capability negotiation. |

Android capability reporting, quality selection, subtitles, and VOD handling
should be one playback-negotiation initiative rather than unrelated patches.
They share the same request models, player controller, server contract, and
hardware acceptance matrix. Fixing them together makes it possible to test the
whole decision rather than a collection of fields.

## Acceptance checks — prove the changed behavior, not just the new code

The P0/P1 work is complete only when these outcomes are observable:

- iOS and tvOS Debug and Release builds compile without the live-validation
  entry point available in production.
- Toggling autoplay during Android playback does not restart, release, or move
  the current stream.
- An Android direct/remux failure performs exactly one compatibility-transcode
  retry at the current position; a second failure becomes a visible error.
- Changing servers clears the persisted token before the new origin can be
  used, on both platforms.
- A supported DV Profile 8 title preserves DV on Android; unsupported Profile
  7 behavior follows the server's compatible-base policy.
- A TrueHD Atmos title uses passthrough only when the active Android route
  truthfully supports that exact format.
- Selecting SRT/WebVTT on Android does not start a video encoder or discard
  HDR. Selecting PGS explicitly may burn, but retains source height.
- Apple cold start never selects a burn-only subtitle automatically.
- A cached Android HLS session resumes at the requested position and seeks
  without unnecessary session churn.

Run these alongside the existing source × quality × operation matrix in
[PLAYBACK-TESTING.md](PLAYBACK-TESTING.md). DV Profile 7 · DV Profile 8 ·
TrueHD over an AVR · text subtitles · PGS on a 4K HDR remux are the new
hardware cases the review predicts will change.

## Non-goals — keep the response proportional

- Do not treat every LOW item as release-blocking. Most are valid maintenance
  notes, but they do not compete with lost HDR, broken quality controls, a
  released player, or leaked credentials.
- Do not redesign the server decision engine while fixing client inputs. The
  reviewed engine is generally choosing correctly from the facts it receives.
- Do not claim Bonjour or D-pad outcomes are resolved through static tests
  alone. Their acceptance evidence belongs on the affected devices.
- Do not preserve temporary live-validation hooks in Release for convenience.
  A repeatable test harness should be explicit, isolated, and credential-safe.

## Bottom line — fix the contract inputs before polishing the shells

The review should be accepted with the qualifications above. Its most important
insight is that plurx already has a sound server-owned delivery contract; the
native clients sometimes give that contract incomplete facts or lose viewer
intent while executing it. Restore build, lifecycle, error, and credential
safety first. Then make Android tell the full truth about the device and stop
subtitles from silently buying a worse video stream.
