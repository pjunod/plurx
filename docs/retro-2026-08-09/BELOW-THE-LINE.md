# Below the line — decisions and deferred verification probes

**Assessment baseline:** `origin/main` @ `e8a910f` · Companion to
`docs/RETRO-REVIEW-2026-08-09.md` and WO-01…WO-11. The outcomes below record
the post-assessment execution state through PR #123 and the physical-device
session on 2026-08-09.

Nothing here reopens a work order's “Don't” section. Those entries remain
verified fixes or refuted claims, not a second backlog.

## 1. Approved decisions and resulting state

Paul approved all five recommendations on 2026-08-09.

1. **Yes — cut a real release.** `v0.2.7` is tagged, the changelog carries the
   release heading, and the tag-driven publication contract has an executed
   release to describe. `docs/RELEASING.md` remains the canonical release
   procedure; continuous main did not replace it.
2. **Yes — choose the guard-only PGS policy.** The server independently refuses
   an explicit subtitle burn for probed DV, HDR10, or HLG sources. There is
   deliberately no “burn to SDR anyway” override: an old or context-poor client
   is not permission to discard HDR.
3. **Yes — adopt one stable typeless-sliding envelope, subject to the device
   comparison.** The implementation is complete and available under
   **Settings → Playback → Experimental typeless sliding HLS**. It remains
   default-off because the physical run did not include the required 2–5 minute
   typeless-vs-control comparison. Approval is recorded; changing the default
   remains evidence-gated rather than assumed from a shorter quality-switch
   check.
4. **Yes — retain the review evidence.** The ten review artifacts called out by
   WO-09 (`PR79`, `PR80`, `PR81`, `PR82`, `PR83`, `PR84`, `PR88`, `PR89`,
   `PR90`, and `PR91`) are tracked. Later PR review ledgers are tracked beside
   them.
5. **Yes — make the offline kill switch stop delivery.** Disabling offline now
   cancels preparation and returns `503 offline_disabled` from leased media
   routes for already-ready packages until the setting is re-enabled.

## 2. Deferred verification probes

### Execution proofs for merged assumptions

- **Pass — FTS5 absent delete.** The pinned three-voter hiqlite gate deletes an
  absent rowid from a `contentless_delete=1` table and proves the neighbouring
  row survives. `make hiqlite-spike` is the durable check.
- **Pass — RustSec red path.** Disposable, never-merged
  [PR #114](https://github.com/pjunod/plurx/pull/114) left the ordinary
  workspace scan green with zero vulnerabilities, then made only the
  synthesized-vendor scan fail on three critical `smallvec` 0.6.9 advisories.
  The draft and remote branch were deleted after the result was captured.
- **Pass — hiqlite rollback.** A duplicate-key failure in statement two rolled
  back statement one's guard-shaped insert. The same semantic gate proves no
  row remains after the failed `txn()`.
- **Pass — 150,000 gone ids.** The current JSON-parameter path decoded 150,000
  ids through `json_each` in 0.007 seconds. WO-01 removed the inlined `IN` list,
  so SQLite's host-parameter ceiling no longer applies.

### Physical playback and client evidence

- **Pass — iPad source/device suite.** An iPad Pro 13-inch (M4) on iOS 26.6 ran
  the complete 128-test Apple suite with zero failures after
  [PR #121](https://github.com/pjunod/plurx/pull/121) bundled the shared native
  API fixture into the device test target. Both 124-test simulator suites and
  `make check` also passed before merge.
- **Pass — iPad full-screen and recovery slice.** On real playback, system
  status-bar count was zero after controls auto-hid. A 1080p → 720p quality
  change recovered playback in 6.932 seconds, below the approximately
  14-second bound. Server telemetry recorded the one observed subthreshold
  stall as `outcome=self_recovered` with `stall_delta=1`.
- **Not settled — EVENT cadence.** The recovery slice did not run a long
  typeless session against a control session, so it does not establish whether
  the EVENT-to-sliding mutation causes the reported 2–5 minute cadence. The
  experimental setting remains default-off.
- **Fail — cold production PGS demux.** Track 0 from a 25,016,940,779-byte
  NAS-resident 4K HDR10 episode hit the production 600-second deadline at
  600.08 seconds before producing a SUP file. The 256 MiB `-fs` bound held by
  construction, but cue count and PNG bytes could not be measured because
  parser admission never began. The production M0 ledger records the same
  result.
- **Pass with bounded scope — Pixel Fold phone checks.** A Pixel 10 Pro Fold on
  Android 17 advertised H.264, HEVC, AV1, VP9, AAC, MP3, Opus, FLAC, AC-3,
  E-AC-3, MKV, MP4, WebM, MOV, TS, and HDR. It did not advertise Dolby Vision.
  Fourteen phone-applicable capability, PiP, PGS-overlay, safe-area,
  navigation, poster, and theme checks passed. The device exposed an Android
  17 incompatibility in Espresso 3.5.0's reflective
  `InputManager.getInstance` path; PR #123 pins Espresso 3.7.0, which removes
  that framework failure. Thirteen remaining full-suite cases depend on TV
  focus mode or exact emulator dimensions and are not recorded as Fold app
  regressions.
- **Pass — iPhone source/device suite.** An iPhone 17 Pro Max on iOS 26.6 ran
  the current Apple build-46 suite: 132 tests with zero failures. The tests
  themselves completed in 3.225 seconds after the unlocked phone accepted the
  run. This supersedes the earlier 128-test build-45 pass.

### Offline device acceptance

- **Partial — iPad lifecycle.** A request for *Cunk on Britain* S1E2 survived
  45 seconds in the background, force-quit, and relaunch; Downloads restored
  the Preparing row. The server then published a ready 769,817,520-byte package
  for the 1,723,040 ms episode. The client transfer and offline playback were
  not completed after the disposable test install returned the app to sign-in
  and the iPad disconnected.
- **Pass — Pixel production transfer, restart recovery, and offline playback.**
  The signed-in Pixel 10 Pro Fold requested the existing 769,817,520-byte
  *Cunk on Britain* S1E2 package and began a real Media3 transfer. The physical
  run exposed a build-22 lifecycle defect: after force-stop, the saved
  Any-network choice was not reapplied and the row returned queued. Build 23
  reapplies that policy before restoring rows. Installed over the same app
  data, it resumed the partial transfer after both upgrade/process death and a
  second explicit force-stop; on the second run the media cache grew from
  207,995 KiB to 230,899 KiB. The transfer also recovered after a device
  reboot and completed at 769,817,381 downloaded bytes (100%). The Pixel
  package ID was then absent from the server ledger, confirming completion
  released its server row and quota; the remaining ready row belongs to the
  separate iPad request. With airplane mode enabled and the temporary server
  route removed, the downloaded item
  started locally and continued playing after a midpoint seek to 14:10 of
  28:43. This package had no selected subtitle, so the subtitle row and real
  six-hour timeout remain separate physical checks. The reboot proof covers
  post-unlock app restoration; an unattended `PlatformScheduler` launch before
  opening the app remains unclaimed.

The following physical rows still require the named hardware; source and
simulator tests are not substitutes:

- iOS 17 `didFinishDownloadingTo` vs. iOS 18 `willDownloadTo` against the TS
  package; `AVAssetCache.isPlayableOffline` accepting TS plus a one-segment-VTT
  subtitle rendition; completed background transfer; airplane-mode start and
  midpoint seek.
- Android unattended `PlatformScheduler` launch after reboot on targetSdk 37;
  real six-hour `onTimeout`; fully offline playback with a selected subtitle
  track; Android TV `uiMode` and D-pad behavior on actual TV hardware.
- Post-expiry re-download on both platforms proving the server's `Cached`
  transfer-only fast path without a re-encode.
- iPad Stage Manager/Split View, tvOS focus and swipe HUD, a physical PGS
  overlay/PiP path, and real HDR/Dolby Vision output.

### CI and operations bookkeeping

- **Pass — branch protection.** Main uses strict up-to-date-branch enforcement
  and requires only the aggregate `PR validation gate`. PR #121 passed that
  gate after refreshing from main; PR #123 follows the same lane.
- **Pass — Actions history.** The 100 completed CI runs from 2026-08-08
  00:21 UTC through 2026-08-09 14:23 UTC put fast-preflight p95 at 15 seconds
  against the 60-second budget. The three-voter contract job recorded 27
  successes, zero failures, and 14 intentional path-based skips.
- **Pass — history audit headroom.** The final local `make check` covered 293
  corrective commits; the combined CI preflight p95 remains 15 seconds.
- **Pass — fleet contract.** nynuc and nuc4 report stamped
  `v0.2.7-15-g83403ef`, schema 15. Every media bind is read-only, both
  containers have `/dev/dri`, and nuc4 retains TCP 32402 plus GDM UDP 32415.
  The private deploy play invokes `make docker-up`, and its contract test pins
  that call. No `backups/` directory exists yet because neither node has
  performed a post-automation redeploy; the earlier scratch rollback drill
  remains the executed recovery proof.

## 3. Explicitly not action items

The master doc's §3 (strengths to keep as patterns) and §1 (gate evidence)
carry no tasks. The refuted-claims ledgers live in each WO's “Don't” section —
treat them as tripwires, not TODOs.
