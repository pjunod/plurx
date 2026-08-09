# Below the line — decisions for Paul + deferred verification probes

**Baseline:** `origin/main` @ `e8a910f` · Companion to `docs/RETRO-REVIEW-2026-08-09.md` and WO-01…WO-11.
Nothing here blocks a work order. Section 1 needs Paul, not GPT. Section 2 is P2/P3 probes the retro deliberately left out of the WOs — cheap to run opportunistically, worth recording results for.

## 1. Decisions that are Paul's (GPT can prepare both options, not choose)

1. **Release model** (WO-10 task 2): cut 0.2.7 for real (CHANGELOG heading + first-ever tag → the publish pipeline finally runs) vs. subordinate RELEASING.md to continuous-main and retire the tag machinery. Everything downstream (registry template, `release-check` cadence) follows this choice.
2. **PGS riders** (WO-08 decisions box): (a) add the server-side HDR-burn refusal now, or accept codec-blind `subtitle_burn` until the overlay exits staging; (b) build the one-tap "burn to SDR anyway" escape from the guard notice, or drop the idea.
3. **EVENT-playlist fix adoption** (WO-05 task 3): after the one-iPad experiment, adopt typeless-sliding-from-first-response (it is spec-correct regardless of whether it explains the stalls) or keep EVENT and accept the mid-stream mutation.
4. **The ten untracked `docs/PR*-REVIEW.md` files** in your working tree (WO-09): commit as the evidence trail or delete; just don't leave them untracked.
5. *(minor)* **Offline kill-switch semantics** (WO-07 task 4): should disabling offline also cut delivery of already-ready packages, or only stop preparation? Gate or document accordingly.

## 2. Deferred verification probes

**Execution proofs for assumptions the merged code rests on**

- **Pass — FTS5 absent delete.** The pinned three-voter hiqlite gate now
  deletes an absent rowid from a `contentless_delete=1` table and proves the
  neighbouring row survives. `make hiqlite-spike` is the durable check.
- **Pass — RustSec red path.** Disposable, never-merged
  [PR #114](https://github.com/pjunod/plurx/pull/114) left the ordinary
  workspace scan green with zero vulnerabilities, then made only the
  synthesized-vendor scan fail on three critical `smallvec` 0.6.9 advisories.
  The draft and remote branch were deleted after the result was captured.
- **Pass — hiqlite rollback.** A duplicate-key failure in statement two
  rolled back statement one's guard-shaped insert. The same semantic gate
  proves no row remains after the failed `txn()`.
- **Pass — 150,000 gone ids.** The current JSON-parameter path decoded
  150,000 ids through `json_each` in 0.007 seconds. WO-01 removed the inlined
  `IN` list, so SQLite's host-parameter ceiling no longer applies.

**Playback / stall-cadence adjacents**

- **Hardware blocked — paused AVPlayer cadence.** Every registered iPad,
  iPhone, and Apple TV was unavailable to CoreDevice on 2026-08-09. A
  simulator cannot settle background playlist reload or the 60-second reaper.
- **Fail — cold production PGS demux.** Track 0 from a
  25,016,940,779-byte NAS-resident 4K HDR10 episode hit the production
  600-second deadline at 600.08 seconds before producing a SUP file. The
  256 MiB `-fs` bound held by construction, but cue count and PNG bytes could
  not be measured because parser admission never began. The production M0
  ledger records the same result.

**Offline device-acceptance pass** (one session, both platforms — beyond WO-07's per-task checks)

- **Hardware blocked on 2026-08-09.** CoreDevice reported every registered
  iOS/tvOS device unavailable, and ADB reported no attached Android device.
  The acceptance rows below remain required; source and simulator tests are
  not substitutes.

- iOS 17 `didFinishDownloadingTo` vs iOS 18 `willDownloadTo` against the TS package; `AVAssetCache.isPlayableOffline` accepting TS + one-segment-VTT subtitle rendition; force-quit → relaunch restore ("tap Resume" row); download completion while backgrounded.
- Android: `PlatformScheduler` resuming the dataSync service after reboot on targetSdk 37; `onTimeout` behavior at the real 6 h cap; fully-offline playback in airplane mode including the subtitle track; TV `uiMode` gating on real Android TV hardware.
- Both: post-expiry re-download actually hits the server's `Cached` fast-path (transfer-only, no re-encode).

**CI / ops bookkeeping**

- **Mismatch — branch protection.** Main requires `rustfmt + clippy` and
  `functionality points (CI profile)` directly; `PR validation gate` is not a
  required context. Correcting this repository security boundary needs an
  explicit owner-approved protection change.
- **Pass — Actions history.** The 100 completed CI runs from 2026-08-08
  00:21 UTC through 2026-08-09 14:23 UTC put fast-preflight p95 at 15 seconds
  against the 60-second budget. The three-voter contract job recorded 27
  successes, zero failures, and 14 intentional path-based skips.
- **Pass — history audit headroom.** A local `make history-check` covered 291
  corrective commits in 8.99 seconds; the combined CI preflight p95 remains
  15 seconds.
- **Pass — fleet contract.** nynuc and nuc4 report stamped v0.2.7 build
  `e8a910f`, schema 15. Every media bind is read-only, both containers have
  `/dev/dri`, and nuc4 retains TCP 32402 plus GDM UDP 32415. The private
  deploy play invokes `make docker-up`, and its contract test pins that call.
  No `backups/` directory exists yet because neither node has performed a
  post-automation redeploy; the earlier scratch rollback drill remains the
  executed recovery proof.

## 3. Explicitly not action items

The master doc's §3 (strengths to keep as patterns) and §1 (gate evidence) carry no tasks. The refuted-claims ledgers live in each WO's "Don't" section — treat them as tripwires, not TODOs.
