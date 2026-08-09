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
- FTS5 `contentless_delete=1`: prove a DELETE of an absent rowid is a no-op (one scratch rusqlite test with hiqlite's pragmas). The entire #88-E fix rests on this; the cluster gate never interleaves delete-with-missing-index.
- Vendor RustSec gate can actually fail: seed a known advisory into the lockfile `scripts/vendor-audit-lock` synthesizes and confirm the second `audit-check` job in `rust-audit.yml` goes red (proves `working-directory` + lockfile-only scanning really scans).
- hiqlite `txn()` rollback semantics on a mid-transaction statement error — the no-leftover-`scan_reconcile_guards`-rows assumption in reconcile.
- Reconcile with ~100–150k gone ids on the hiqlite backend: measure the current inlined-`IN`-list ceiling (WO-01 task 5 fixes this proactively; this measures the bound being fixed).

**Playback / stall-cadence adjacents**
- AVPlayer playlist-reload cadence while **paused** >60 s against a live (non-ENDLIST) playlist — settles whether the idle reaper (60 s + 15 s tick, status polls deliberately don't touch `last_access`) contributes to the periodic-stall pattern for paused viewers.
- PGS measurements the M0 ledger still owes: a fade-heavy production title (cue count, PNG bytes vs the 256 MiB per-track cap, whether the 422 refusal is acceptable UX) and a cold demux of a large NAS-resident file (time-to-first-manifest vs the 600 s deadline; staging-dir disk high-water — ffmpeg can write an unbounded `track.sup` before the parser cap applies).

**Offline device-acceptance pass** (one session, both platforms — beyond WO-07's per-task checks)
- iOS 17 `didFinishDownloadingTo` vs iOS 18 `willDownloadTo` against the TS package; `AVAssetCache.isPlayableOffline` accepting TS + one-segment-VTT subtitle rendition; force-quit → relaunch restore ("tap Resume" row); download completion while backgrounded.
- Android: `PlatformScheduler` resuming the dataSync service after reboot on targetSdk 37; `onTimeout` behavior at the real 6 h cap; fully-offline playback in airplane mode including the subtitle track; TV `uiMode` gating on real Android TV hardware.
- Both: post-expiry re-download actually hits the server's `Cached` fast-path (transfer-only, no re-encode).

**CI / ops bookkeeping**
- GitHub branch protection: confirm `pr_gate` is the sole required check (currently assumed from a ci.yml comment).
- Actions history: preflight p95 vs the CI-overhaul 60 s budget; `cluster_auth` real flake rate since M1c (and again after #90's 3-voter contract suite lands) — feeds WO-03 task 8's flake policy.
- `history-check` runtime grows O(all history × subprocesses) inside preflight — watch it against the budget; split or cache when it gets close.
- While doing WO-10 task 3 on the nodes: media mounts `:ro` in each node's compose override, GPU device blocks present, nuc4's port workaround intact, and confirm the private deploy play uses `make docker-up` (else fleet builds report unstamped).

## 3. Explicitly not action items

The master doc's §3 (strengths to keep as patterns) and §1 (gate evidence) carry no tasks. The refuted-claims ledgers live in each WO's "Don't" section — treat them as tripwires, not TODOs.
