# Below the line — decisions and deferred verification probes

**Assessment baseline:** `origin/main` @ `e8a910f` · Companion to
`docs/RETRO-REVIEW-2026-08-09.md` and WO-01…WO-11. The outcomes below record
the post-assessment execution state through PR #135, plus the adversarial audit
and physical-device session on 2026-08-09. The consolidated review packet is
[EXTERNAL-REVIEW-HANDOFF.md](EXTERNAL-REVIEW-HANDOFF.md).

Nothing here reopens a work order's “Don't” section. Those entries remain
verified fixes or refuted claims, not a second backlog.

## 1. Approved decisions and resulting state

Paul approved all five recommendations on 2026-08-09.

1. **Yes — cut and publish release 0.2.7.** The first GHCR publication timed
   out after 60 minutes while Rust was compiling under arm64 emulation. PR
   [#129](https://github.com/pjunod/plurx/pull/129) repaired the publication
   path, and run
   [31346745768](https://github.com/pjunod/plurx/actions/runs/31346745768)
   published `0.2.7`, `0.2`, and `latest` as the same two-platform index,
   `sha256:8e90b4f05d6d750a1546df1a6d44ae786fde9c63caa1aa48a8cb1e5cac73a374`.
   `docs/RELEASING.md` remains the canonical procedure; continuous main did
   not replace it.
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
- **Fail, then bounded unchanged recovery — cold production PGS demux.** Track
  0 from a 25,016,940,779-byte NAS-resident 4K HDR10 episode hit the production
  600-second deadline at 600.08 seconds before producing a SUP file. The 256
  MiB `-fs` bound held by construction, but cue count and PNG bytes could not
  be measured because parser admission never began. Two later executions of
  the same bounded command and track completed without a product-code change.
  The timestamped recovery ran from 2026-08-10T01:31:52Z through 01:32:58Z,
  wrote 13,230,798 SUP bytes through source timestamp `00:52:11.253`, and
  produced SHA-256
  `d48091d6960e0fd6831479c12c5190d7c78509b28d726549a12ba65ea651d9cc`.
  A same-day QNAP NFS not-responding/recovered event makes transient storage or
  cache state the leading inference, not a proved cause. The unchanged command
  refutes a deterministic 600-second demux defect; it does not complete the
  default-off production/device gate.
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
  the then-current Apple build-46 suite: 132 tests with zero failures. The tests
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
- Android build 25 now has the targetSdk-37 source path for unattended reboot:
  a persisted UIDT job, synchronous intent/network state, granted-network
  socket and DNS binding, and an explicit system-stop Resume state. A build-24
  in-flight transfer needs one foreground Resume tap after upgrade because no
  earlier UIDT registration exists. The actual unattended reboot remains a
  physical-device check;
  real six-hour `onTimeout`; fully offline playback with a selected subtitle
  track; Android TV `uiMode` and D-pad behavior on actual TV hardware.
- Post-expiry re-download on both platforms proving the server's `Cached`
  transfer-only fast path without a re-encode.
- iPad Stage Manager/Split View; iPhone orientation, PiP return, autoplay, and
  background-audio/Now Playing continuity; duration-less MKV natural end;
  maximum-Dynamic-Type marker readability on a narrow iPhone; tvOS focus and
  swipe HUD; and a physical PGS overlay/PiP path.
- Repeated approximately eight-second throttled AVPlayer stalls with server
  stall evidence and zero playback reopens.
- AVPlayer paused for more than 60 seconds against a live non-ENDLIST playlist,
  distinguishing its playlist-reload behavior from the server's 60-second idle
  reaper plus 15-second sweep cadence.
- Physical High-tier HEVC HDR playback with Main-tier and Dolby Vision titles
  unaffected; Android TV tunneled playback/D-pad behavior; and ExoPlayer
  acceptance of the cleared hvcC tier bit.
- Android Media3 receiving `X-Plurx-Media-Origin-Ms` through
  `onTransferStart`/`responseHeaders` before the first progress post.
- A fade-heavy production PGS title: retain cue count and PNG bytes against the
  256 MiB per-track cap, and judge whether a 422 refusal is acceptable UX.

### CI and operations bookkeeping

- **Pass — branch protection.** Main uses strict up-to-date-branch enforcement
  and requires only the aggregate `PR validation gate`. PR #121 passed that
  gate after refreshing from main; PR #123 follows the same lane.
- **Pass — Actions history.** The 100 completed CI runs from 2026-08-08
  00:21 UTC through 2026-08-09 14:23 UTC put fast-preflight p95 at 15 seconds
  against the 60-second budget. The three-voter contract job recorded 27
  successes, zero failures, and 14 intentional path-based skips.
- **Pass — history audit headroom.** The latest audit covers 316 corrective
  commits, 239 direct test changes, 80 explicit current-check mappings, 14
  client-fix anchors, and 11 non-runtime corrections. The combined CI
  preflight p95 remains 15 seconds.
- **Pass — hosted nightly isolation and seeded red path.** PR
  [#130](https://github.com/pjunod/plurx/pull/130) split deep validation,
  parser fuzz, and mutation into independent jobs. Normal run
  [31347358863](https://github.com/pjunod/plurx/actions/runs/31347358863)
  completed 3,605,409 fuzz executions without a crash; seeded run
  [31347364590](https://github.com/pjunod/plurx/actions/runs/31347364590)
  reproduced the deliberate crash and retained its evidence while making the
  fuzz job red. The report-only mutation job executed 554 mutants and retained
  its artifact. Playback-fix exact-head run
  [31354801983](https://github.com/pjunod/plurx/actions/runs/31354801983)
  passed deep validation and completed 5,893,665 independent fuzz executions.
- **Pass — live-transcode startup publication.** PR
  [#132](https://github.com/pjunod/plurx/pull/132) prevents the first
  live-transcode playlist response from exposing only one unfinished EVENT
  segment. Independent hostile review approved the bounded gate; the exact
  Chrome case passed at 0.994× with zero hitches and zero measured stalls, and
  the playback-fix exact-head hosted deep-validation job passed.
- **Pass after repair — live backup invariant.** An adversarial check found
  nynuc and nuc4 on `v0.2.7-66-g3603923` without `/srv/plurx/backups`, proving
  the earlier deployment bypassed or predated current automation. The clean
  private-Ansible backup implementation from
  [#2](https://github.com/pjunod/ansible/pull/2) (`be16fb71`) then ran through
  the stamped deploy path corrected by
  [#3](https://github.com/pjunod/ansible/pull/3) (`4c74d27`), serially with
  rebuild disabled: it stopped each stack, copied the closed database,
  restarted the unchanged image, and passed both health probes. nynuc's
  `plurx.db.predeploy-20260810T004048Z-3603923cce64.bak` is 69,574,656 bytes
  with SHA-256 `56f80771de22ca17ee8fd7d077ba52b902162cabd496a8cbcbbe755ddf521090`;
  nuc4's `plurx.db.predeploy-20260810T004143Z-3603923cce64.bak` is 81,661,952
  bytes with SHA-256
  `9fef22305d92507a8396e3937405f1a62e128348a470e2332bf13456d8da61ee`.
  Both read-only snapshots return `PRAGMA quick_check = ok`; both services
  still report the unchanged running build and healthy endpoints. The scratch
  rollback drill remains the restore proof, while this run closes the missing
  live-snapshot gap.
- **Pass — live fleet contract.** The original direct inspection found both
  nodes stamped at `e8a910f` on schema 15, every media bind read-only, both
  containers exposing `/dev/dri`, and nuc4 retaining TCP 32402 plus GDM UDP
  32415. The private deploy play invokes `make docker-up`, and its contract
  test pins that call. The later backup repair ran with rebuild disabled and
  left the then-running `v0.2.7-66-g3603923` image unchanged; these are retained
  inspection results, not a claim that either historical SHA is today's fleet
  tip.

## 3. Explicitly not action items

The master doc's §3 (strengths to keep as patterns) and §1 (gate evidence)
carry no tasks. The refuted-claims ledgers live in each WO's “Don't” section —
treat them as tripwires, not TODOs.
