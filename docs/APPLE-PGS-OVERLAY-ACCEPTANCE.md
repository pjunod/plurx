# Apple PGS overlay acceptance — one iPad Pro run, one decidable record

**Status:** ready for operator execution after Apple build 58 is merged and
uploaded · **Covers:** [PGS overlay plan](PGS_OVERLAY_PLAN.md) Milestone 2 ·
**Written:** 2026-08-14

Companion to [APPLE-CLIENT-PARITY.md](APPLE-CLIENT-PARITY.md) (what the Apple
client ships) and [PLAYBACK.md](PLAYBACK.md) (the server/client delivery
contract) — this is the physical iPad Pro procedure for the staged `pgs-v1`
path. Follow it in order and record the table in §7. If a step seems to require
an agent to deploy a server, install on the iPad, use signing credentials, or
inspect private media, stop: those are operator actions and are not authorized
for an agent by this procedure.

The pass is intentionally narrow. It proves a recognized PGS application
overlay stays synchronized over Dolby Vision or HDR10 on one physical iPad Pro.
It does not enable the gate by default or satisfy the separate iPhone and Apple
TV rows in the milestone.

## 1. Install Apple build 58 from TestFlight

Build 58 is the first build whose subtitle menu says whether a PGS choice is an
`Overlay` or a `Burn-in`. Build 57 has not been uploaded and is not an acceptable
substitute: its menu calls both paths `Burn-in`, so its result is ambiguous even
when the playback-info panel is correct.

The operator performs the upload from a merged, clean `main` checkout with the
App Store Connect credentials already configured by
[PUBLISHING.md](PUBLISHING.md#ansible-owns-the-repeatable-mobile-deploy):

```bash
git switch main              # upload the reviewed source, not an agent branch
git pull --ff-only origin main
scripts/ship --apple         # test, archive, and upload iOS + tvOS build 58
```

Then, in App Store Connect, wait until iOS build 58 is processed and assign it
to the operator's internal TestFlight group. On the iPad Pro, open TestFlight,
install or update **Noirr Cinema**, and require the build detail to show
`0.2.7 (58)` before continuing. Uploading, assigning, and installing are all
operator actions; no agent may perform them under this issue.

**Stop result:** TestFlight does not show `0.2.7 (58)` on the iPad. Do not run
the matrix against another build and do not report device evidence.

## 2. Enable the staged server capability

On the operator's own server, set this exact environment value in the server's
normal deployment configuration and restart the server:

```bash
PLURX_PGS_OVERLAY=1          # advertise and serve the staged pgs-v1 protocol
```

The value is read at server startup. Setting it without a restart leaves the
capability off. Changing the environment and restarting the operator's server
are operator actions; this document does not authorize an agent to do either.

## 3. Use a real HDR/Dolby Vision title with a real PGS track

Use operator-owned media with both of these properties:

- the video is Dolby Vision or HDR10 and already plays in that grade on this
  iPad Pro with subtitles Off;
- the chosen subtitle stream reports `hdmv_pgs_subtitle` or `pgssub`.

Do not infer PGS from the subtitle language, title, file extension, or the fact
that a subtitle burns. ASS/SSA and VobSub burn by design and would make this run
prove nothing. The operator can confirm the codec read-only on their server:

```bash
ffprobe -v error -select_streams s \
  -show_entries stream=index,codec_name:stream_tags=language,title \
  -of json '<path-to-operator-owned-media>'
```

Require the selected stream's `codec_name` to be `hdmv_pgs_subtitle` or
`pgssub`. Keep the title and filesystem path out of issue comments, pull
requests, screenshots, and logs. Record only the source grade, resolution,
subtitle codec, and anonymous track index.

Choose two unmistakable subtitle moments at least five minutes apart:

- **Cue A:** a cue with a clear beginning and end, used for ordinary playback
  and pause/resume.
- **Cue B:** a cue later in the title, used as the long-seek target.

Prefer a title with a second audio track for §5 because switching audio is a
deterministic player-item replacement. Without one, the replacement row is
runnable only when §4 reports `Remux · HLS` and a long seek visibly enters the
stream-change spinner. A `Direct play` title with one audio track has no
replacement trigger; record `Not run — Direct play with one audio track` rather
than treating a successful seek as replacement evidence.

## 4. Establish the Off baseline, then select the overlay

Start the title with subtitles Off. Open **Playback info** and capture these
rows before selecting PGS:

| Row | Required baseline |
|---|---|
| `Method` | `Direct play` or `Remux · HLS`; not a transcode caused by subtitles |
| `Dynamic range` | `Dolby Vision (rendering)` or `HDR10 (rendering)` |
| `Source` | The expected HEVC/HDR source shape |

Now open **Subtitles** and inspect the codec-confirmed PGS row before selecting
it:

- A row ending in `HDMV_PGS_SUBTITLE  Overlay` or `PGSSUB  Overlay` proves the
  current server advertised `pgs-v1` and build 58 recognized it.
- A row ending in `Burn-in` means the overlay precondition failed: the gate is
  off, the server or client is stale, or the server did not recognize the
  confirmed PGS track. Stop. A burn-in run cannot prove HDR/Dolby Vision
  preservation.

This is the end-to-end gate check. It comes after `ffprobe` has proved the track
is PGS, so `Burn-in` cannot be an ASS/SSA or VobSub choice masquerading as a gate
failure. The check reads the server's advertised capability through the
production client instead of assuming that an edited environment file reached
the running process.

Capture the row ending in `Overlay`, then select it. Video must continue;
selecting the row must not replace the player or show a playback spinner. Open
**Playback info** again:

- while preparation is running, `Subtitles` may end in
  `PGS overlay · preparing`;
- after preparation, `Method` ends in `PGS overlay` and `Subtitles` ends in
  `PGS overlay`;
- `PGS overlay · unavailable`, the red preparation notice, or a menu row ending
  in `Burn-in` is a failure, not a partial pass;
- `Transcode · subtitle burn-in` proves the wrong path ran and is a failure.

At Cue A, require the complete subtitle to be visible, inside the video picture,
and legible. The `Dynamic range` row must still exactly match the Off baseline.
Any `SDR` result, downgrade arrow, loss of the device's HDR presentation, or
change to a subtitle-burn transcode is **No** for preservation.

## 5. Exercise synchronization and player-item replacement

Use the iPad's built-in screen recording so onset, clear, and post-seek frames
can be reviewed rather than recalled. A 60 fps recording gives approximately
16.7 ms per frame: six frames are 100 ms and nine frames are 150 ms. Preserve
the recording privately; do not attach frames that expose a title, account, or
server address to GitHub.

Run these cases with the PGS overlay selected:

| Case | Action | An unambiguous **Yes** |
|---|---|---|
| Steady cue | Play through Cue A without touching controls | Onset and clear are within six recorded frames (100 ms) of the authored picture/dialogue event; no stale object remains after clear |
| Seek away/back | Seek away from Cue A, then back into its middle | The correct complete cue is present within nine frames (150 ms) or one source-video frame after the first stable target frame; the cue from the old position is absent |
| Rate 1×→0×→1× | Pause in the middle of Cue A for five wall-clock seconds, then resume | Video and overlay freeze together; the cue neither clears nor advances while paused, and resumes against source time rather than the five seconds of wall time |
| Item replacement | If another audio track exists, switch to it and back. Otherwise, only from a fresh `Remux · HLS` session, seek at least five minutes forward to Cue B within 30 seconds and require the stream-change spinner. If the baseline is `Direct play` or no spinner appears, record `Not run` with that reason | After the observed replacement, Cue B appears within nine frames/one source frame, no Cue A object survives, `Method` returns to the same copy/direct video class, and `Dynamic range` still matches the Off baseline |

The pause/resume row is the rate transition the shipping player exposes: 1× to
0× and back to 1×. Do not claim an untested 1.5× path; build 58 has no playback-
speed selector. A direct-play seek uses the current item's file timeline and
does not replace the `AVPlayerItem`. A long forward seek can replace the item
only when a growing remux session has not published that target; the visible
stream-change spinner is the operator's confirmation that this route ran. An
audio-track switch also replaces the item and is preferred because its trigger
is deterministic even if the server already holds a completed cache entry. No
spinner means no replacement evidence, so do not infer a pass from correct Cue
B playback alone.

## 6. Confirm the documented PiP and AirPlay limitations

The PGS bitmap is an application layer above the video, not part of the video
frames. Build 58 therefore blocks output modes that would carry only the video
plane. It never rescues either output by burning the PGS and turning HDR into
SDR.

**Picture in Picture:** with the overlay active, tap the PiP control, then send
the app to the background once. The pass is all of the following: no PiP window
starts · the app shows “PGS overlays stay in the app and are not available in
Picture in Picture, AirPlay, or external playback. HDR playback was kept
unchanged.” · returning to the player retains the in-app overlay and the same
dynamic-range row. A PiP window containing video without the subtitle is a
failure because the limitation was not enforced.

**AirPlay/external playback:** first turn PGS Off, start AirPlay from iPadOS
Control Center, return to the app, and try to select the PGS row. The pass is:
selection is refused with the same notice · the remote video is not replaced by
an SDR burn · the subtitle remains Off. Stop AirPlay, select PGS again, and
confirm the in-app overlay returns. While that overlay is active, the player
sets external playback unavailable; the app currently has no separate AirPlay
button.

These limitations are shipped behavior, not provisional advice. If the device
does something different, record **No** and update the parity documentation
before any release claim; do not rewrite the observation as a pass.

## 7. Record one result without private library data

Copy this table into the acceptance issue. Use `Yes`, `No`, or `Not run`; do not
use “looks good.” Every row must be `Yes` to complete this iPad slice. A `No` or
`Not run` keeps the physical Apple milestone pending; `Not run` records that an
observation was unavailable rather than disguising it as a pass.

| Evidence | Result |
|---|---|
| Device model and iPadOS version recorded | |
| TestFlight shows Noirr Cinema `0.2.7 (58)` | |
| Server restarted with `PLURX_PGS_OVERLAY=1` by the operator | |
| Source recorded as Dolby Vision or HDR10, without title/path | |
| Subtitle codec is `hdmv_pgs_subtitle` or `pgssub` | |
| Subtitle menu says `Overlay`, never `Burn-in`, for the selected track | |
| Playback info reaches ready `PGS overlay`, not preparing/unavailable | |
| Subtitle visible, complete, positioned, and legible | |
| Dynamic-range row and device presentation unchanged from Off baseline | |
| Steady onset/clear within 100 ms | |
| Seek reconciliation within 150 ms or one source frame | |
| Pause/resume keeps overlay on source time | |
| Replacement item has only the correct target cue, or `Not run` with the missing-trigger reason | |
| PiP is blocked with the documented notice and no burn fallback | |
| AirPlay selection is refused with the documented notice and no burn fallback | |

Also record the pre/post `Method`, `Dynamic range`, and `Subtitles` strings,
whether the replacement was triggered by audio switch or long seek, and whether
the stream-change spinner appeared. The recording may remain private; the issue
needs the measured frame counts, not a copy of operator-owned media.

## 8. What this run cannot claim

- It does not validate iPhone or Apple TV timing, HDMI output, or tvOS focus.
- It does not prove every Dolby Vision profile; record the one source profile
  actually used.
- It does not enable automatic/default PGS selection or change
  `PLURX_PGS_OVERLAY`'s default-off state.
- It does not prove PiP or AirPlay carry overlays. The accepted behavior for
  build 58 is a visible refusal that preserves the existing video.
- Simulator or XCTest results cannot fill any row in §7 that names the physical
  iPad, TestFlight, HDR/Dolby Vision presentation, PiP, or AirPlay.
