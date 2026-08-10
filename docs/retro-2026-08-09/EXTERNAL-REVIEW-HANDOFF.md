# External review handoff — what landed, what remains

**Status:** complete through Android PR #135, post-merge main CI, and the playback-fix hosted nightly ·
**Assessment:** [RETRO-REVIEW-2026-08-09.md](../RETRO-REVIEW-2026-08-09.md) ·
**Deferred probes:** [BELOW-THE-LINE.md](BELOW-THE-LINE.md) ·
**Written:** 2026-08-09

Start with §2 for the requested yes/no decisions, then use §3 as the merge and
acceptance ledger. Section 4 records the adversarial follow-up work that found
defects after the original work orders were green. Section 6 is the boundary:
anything listed there still needs named hardware or a deliberately retained
default-off gate. If a review appears to require reopening a work order's
“Don't” section, stop and identify new evidence first; those claims were
already fixed or refuted against the pinned assessment.

## 1. Review boundary — completion is evidence, not chronology

The assessment was pinned at `e8a910fbe8fdec790df8d22cb36413a5e35c78b0`.
Docs-only PR [#95](https://github.com/pjunod/plurx/pull/95), titled
`docs: retrospective review 2026-08-04..08 + work orders`, landed the master
assessment and self-contained work orders at `67d45f50` before implementation
continued.
Every original work order now has merged code and its specified automated
checks. No implementation diff violated a work order's “Don't” section.

One ordering requirement did fail: PR #90 merged to its stacked base at
03:11:39 UTC and reached main through PR #93 at 04:30:48 UTC; the WO-02
correction merged through PR #96 at 05:07:31 UTC. Main therefore carried the
idempotency blocker for 36 minutes 43 seconds, contrary to “WO-02 before
merging #90.” The current code is corrected; the process miss remains part of
the record.

The “one branch + PR per work order” execution shape also varied for WO-10:
its urgently ordered backup task, the remaining fleet/deploy contracts, and
the later release decision landed separately through PRs #99, #107, and #113;
PR #129 repaired the adversarially discovered publication gap. Private Ansible
[#2](https://github.com/pjunod/ansible/pull/2) implemented backup, rotation,
fail-closed restart, and the rollback drill; private Ansible
[#3](https://github.com/pjunod/ansible/pull/3) later corrected the stamped deploy
path used for the live proof. The split followed those dependencies, but it was
still a process variance from the requested one-PR shape.

The later adversarial audit did not treat merged as finished. It found and
closed a failed release publication, absent live backups, unreachable nightly
fuzz/mutation evidence, a history-audit bypass, a live-transcode startup
stall, and an Android unattended-recovery design gap. Section 4 maps those
corrections separately so an external reviewer can distinguish original work
from audit-driven repair.

## 2. The five decisions — all yes, with their current consequence

1. **Yes — cut and publish release 0.2.7.** PR
   [#113](https://github.com/pjunod/plurx/pull/113) prepared the release; the
   separately created annotated tag `v0.2.7` points to its merge. PR
   [#129](https://github.com/pjunod/plurx/pull/129) repaired the timed-out
   publication path. Run
   [31346745768](https://github.com/pjunod/plurx/actions/runs/31346745768)
   published `0.2.7`, `0.2`, and `latest` as the same two-platform image index.
2. **Yes — use the guard-only HDR/PGS policy.** PR
   [#112](https://github.com/pjunod/plurx/pull/112) refuses explicit subtitle
   burns for probed DV, HDR10, and HLG sources. There is no context-poor
   “burn to SDR anyway” escape.
3. **Yes — adopt typeless sliding HLS, but keep it default-off.** PR
   [#101](https://github.com/pjunod/plurx/pull/101) implemented the stable
   envelope behind the experimental setting. The 2–5 minute physical
   typeless-vs-control comparison still gates changing the default.
4. **Yes — retain review evidence.** PR
   [#110](https://github.com/pjunod/plurx/pull/110) preserves the named review
   artifacts and their later ledgers.
5. **Yes — make the offline kill switch stop delivery.** PR
   [#103](https://github.com/pjunod/plurx/pull/103) cancels preparation and
   returns `503 offline_disabled` from leased ready-package media routes while
   offline is disabled.

## 3. Work-order ledger — task, merge, and present evidence

The short SHAs are reachable commits on main. PR links are the review surface;
the work-order files remain the source of the exact task and “Don't” clauses.

| Order | Work order | Merged result | Acceptance evidence | State |
|---:|---|---|---|---|
| 1 | [WO-02](WO-02-pr90-merge-blockers.md) | [#90](https://github.com/pjunod/plurx/pull/90) `1e6380c2` · [#93](https://github.com/pjunod/plurx/pull/93) `74825a1a` · correction [#96](https://github.com/pjunod/plurx/pull/96) `a9f66a00` | Retry-only expiry changes return `Existing`; a failed progress flush does not abandon the remaining entries. | Pass, with the §1 order deviation. |
| 2 | [WO-01](WO-01-scan-and-store-correctness.md) | [#97](https://github.com/pjunod/plurx/pull/97) `5d8b3d05` | Small-library prune floor, disabled-prune refusal, bounded replicated calls, prune assertion, and read-placeholder validation are pinned by server/store tests. | Pass. |
| 3 | [WO-10 task 1](WO-10-release-and-deploy-discipline.md) | [#99](https://github.com/pjunod/plurx/pull/99) `cc0920f1` · private Ansible [#2](https://github.com/pjunod/ansible/pull/2) `be16fb71` · stamped-deploy correction [#3](https://github.com/pjunod/ansible/pull/3) `4c74d276` | PR #2 implemented backup/rotation/fail-closed restart and passed the real rollback drill; the corrected serial path through #3 later produced closed-database snapshots on both live nodes, both with `PRAGMA quick_check = ok`. | Pass after the adversarial live-fleet repair in §5. |
| 4 | [WO-03](WO-03-ci-pipeline-fixes.md) | [#100](https://github.com/pjunod/plurx/pull/100) `d38ec5fd` | Release artifact scope, job timeouts, docs-lane exclusions, glob resolution, parallel mobile-version gate, scoped cluster jobs, iPad leg, and flake policy are enforced by CI/operations tests. | Pass. |
| 5 | [WO-05](WO-05-hls-flow-control-and-event-playlist.md) | [#101](https://github.com/pjunod/plurx/pull/101) `6114a177` | Global-cap release uses reachable retained floors; transitions are visible; typeless shape is stable across pruning; delayed media-origin cannot hold first byte beyond its bound. | Pass; typeless remains default-off pending §6 hardware comparison. |
| 6 | [WO-06](WO-06-apple-player-followups.md) | [#102](https://github.com/pjunod/plurx/pull/102) `ced02d04` | Stall deltas, per-item establishment, finite-VOD end action, focus/layout rows, and current Apple simulator/device suites pin the repaired state machine. | Automated pass; remaining hardware rows are in §6. |
| 7 | [WO-07](WO-07-offline-viewing-followups.md) | [#103](https://github.com/pjunod/plurx/pull/103) `d1e4fe70` | Completion release survives lifecycle changes; playback preempts preparation; disable blocks leased delivery; admin cancel is item-scoped; store tests prove fair claiming. | Pass. |
| 8 | [WO-04](WO-04-evidence-discipline.md) | [#104](https://github.com/pjunod/plurx/pull/104) `f99519cb` | Revert-the-fix, client anchors, discriminating history audit, mobile-doc coupling, and local mutation proof landed. PR #130 supplied the missing hosted isolation proof in §5. | Pass after hosted follow-up. |
| 9 | [WO-08](WO-08-pgs-overlay-followups.md) | [#105](https://github.com/pjunod/plurx/pull/105) `2bb32f88` | PiP gate, durable/self-healing publication, retryable 202/503 client paths, BT.709 fixture, and bounded parser fuzz harness are pinned. PR #130 supplied the hosted fuzz proof in §5. | Pass; production/device gate remains default-off in §6. |
| 10 | [WO-09](WO-09-docs-sweep.md) | [#106](https://github.com/pjunod/plurx/pull/106) `af7bd5f5` | Changelog, status, plans, operations, review artifacts, and doc inventory passed the docs lane and full repository gate. | Pass. |
| 11 | [WO-11](WO-11-media-origin-and-android-ux.md) | [#108](https://github.com/pjunod/plurx/pull/108) `8012b066` | Media-origin call-site mutations are rejected; Android seek coalesces and reports position; tvOS source tests pin the remote HUD path. | Automated pass; TV hardware rows remain in §6. |
| — | [WO-10 remainder](WO-10-release-and-deploy-discipline.md) | [#107](https://github.com/pjunod/plurx/pull/107) `5b293d3a` · [#113](https://github.com/pjunod/plurx/pull/113) `9abe2fdd` | Fleet/deploy contracts, stamped-node ledger, release check, real registry naming, and required-device assertions landed. PR #129 closes publication. | Pass after §5 publication and backup proof. |

Every work-order branch ran its documented local/pre-PR automated checks and
`make check`; hosted and device exceptions are recorded in the ledger and
sections below. Client release-path commits advanced the mobile build counter;
the later Android unattended-recovery correction advances Android to build 25.

## 4. Adversarial follow-up — findings that changed the result

| Finding | Correction | Evidence |
|---|---|---|
| Android build 23 restored network policy only after opening the app; an unopened post-reboot process had no durable owner. | Android build 25 adds persisted recovery state, API 34+ user-initiated transfer ownership, granted-network binding, and the API 23–33 boot fallback. | [#135](https://github.com/pjunod/plurx/pull/135) `604e90e4`; exact-head hostile re-review approved with no findings. Local release gates, PR run [31357486144](https://github.com/pjunod/plurx/actions/runs/31357486144), and post-merge main run [31358047962](https://github.com/pjunod/plurx/actions/runs/31358047962) passed, including Android JVM and instrumented UI. Physical unopened-app reboot remains §6. |
| Explicit history mappings did not discover neutral client-fix subjects, accepted unsafe prefixes, and allowed alias prefixes for one commit. | [#128](https://github.com/pjunod/plurx/pull/128) `46a1d1fc` | Hostile re-review approved the implementation and discriminating tests; the full history audit passes. |
| `v0.2.7` tag CI timed out under arm64 QEMU and GHCR had no package. | [#129](https://github.com/pjunod/plurx/pull/129) `aa5ea003` | Hostile workflow review closed retry, rollback, trust, architecture-binding, registry-error, shell, alias, and TOCTOU failures before merge; run 31346745768 passed. |
| A red deep-validation job skipped the later fuzz and mutation steps. | [#130](https://github.com/pjunod/plurx/pull/130) `d1e5adff` | Deep validation, fuzz, and mutation are independent jobs; the seed is opt-in and its executable tests distinguish unset/empty/false from `1`. |
| Forced transcodes exposed a one-segment EVENT playlist and hls.js reached its live edge before the next rewrite. | [#132](https://github.com/pjunod/plurx/pull/132) `5bcc0a34` | Independent hostile review approved; exact Chrome case passed at 0.994× with 0 hitches and 0 measured stalls; all PR checks and the exact-head hosted deep-validation job passed. |

## 5. Below-the-line evidence — exact probes and operational proof

### Release publication is complete

Run [31346745768](https://github.com/pjunod/plurx/actions/runs/31346745768)
published exactly `linux/amd64` and `linux/arm64`. Tags `0.2.7`, `0.2`, and
`latest` resolve to index digest
`sha256:8e90b4f05d6d750a1546df1a6d44ae786fde9c63caa1aa48a8cb1e5cac73a374`.
The child manifests are
`sha256:54471f75c764d3737fccd03d9916e1f63803c818ba27bbdbb6ec5ccb5d3608e3`
for amd64 and
`sha256:17ab57d150af126e04e147f58c1482d70bc2ff4df87ae73b899d2bcb349224c2`
for arm64. Both report version `0.2.7`, source GitHub, and revision
`9abe2fddd2e02b11646fcf05db88ebbe435de4c9`.

### Both live nodes now have verified pre-deploy snapshots

The private Ansible backup implementation from
[#2](https://github.com/pjunod/ansible/pull/2), run through the stamped deploy
path corrected in [#3](https://github.com/pjunod/ansible/pull/3), stopped one
stack at a time, copied the closed database, restarted the unchanged image,
and passed health checks.

| Node | Snapshot | Bytes | SHA-256 |
|---|---|---:|---|
| nynuc | `plurx.db.predeploy-20260810T004048Z-3603923cce64.bak` | 69,574,656 | `56f80771de22ca17ee8fd7d077ba52b902162cabd496a8cbcbbe755ddf521090` |
| nuc4 | `plurx.db.predeploy-20260810T004143Z-3603923cce64.bak` | 81,661,952 | `9fef22305d92507a8396e3937405f1a62e128348a470e2332bf13456d8da61ee` |

Both snapshots return `PRAGMA quick_check = ok`. The earlier live absence of a
`backups/` directory was a real operational failure, not a documentation gap;
this run is the missing first-deploy proof.

### PGS demux recovered without changing product code

The original cold production attempt on a 25,016,940,779-byte NAS 4K HDR10
episode failed at the 600.08-second preparation deadline before parser
admission. Two later executions of the same bounded command and track
completed unchanged. The timestamped run took 66 seconds from
2026-08-10T01:31:52Z through 01:32:58Z, wrote 13,230,798 SUP bytes, ended at
source timestamp `00:52:11.253`, and produced SHA-256
`d48091d6960e0fd6831479c12c5190d7c78509b28d726549a12ba65ea651d9cc`.

A same-day QNAP NFS not-responding/recovered event makes transient storage or
cache state the leading inference, but the original run's UTC boundary was
not recorded, so causation is unproved. Raising the timeout would mask I/O
stalls; direct MKV parsing would violate the audited raw-SUP boundary; chunked
FFmpeg would risk timestamp and composition state. None is justified by this
recovery. The default-off production/device gate remains in §6.

### Hosted fuzz and mutation jobs now execute independently

Normal nightly run
[31347358863](https://github.com/pjunod/plurx/actions/runs/31347358863)
completed 3,605,409 fuzz executions over 901 seconds without a crash. Its
`nightly-pgs-fuzz-evidence` artifact is 6,825 bytes. Seeded run
[31347364590](https://github.com/pjunod/plurx/actions/runs/31347364590)
reproduced `seeded PGS fuzz-gate crash`, retained its reproducer, uploaded a
2,280-byte evidence artifact, and made the fuzz job red as designed.

The independent mutation job enumerated 554 mutants: 80 caught · 445 missed ·
29 unviable · 0 timeouts. The report-only result and 5,017,002-byte artifact
were uploaded even though the sibling deep-validation job failed. “Missed” is
test-debt evidence, not a green quality score; the acceptance requirement here
was that hosted mutation actually execute and retain its report.

Playback-fix exact-head nightly run
[31354801983](https://github.com/pjunod/plurx/actions/runs/31354801983)
uses merged playback head `5bcc0a349b1d4c5c181538106d8dcd6d68ced1c8`.
Its `playback, recovery, bounds, clients, UI, and packaging` job passed and
uploaded the 200,168,255-byte `nightly-deep-validation-evidence` artifact
(`9050843997`). Its independent parser-fuzz job also passed after 5,893,665
executions in 901 seconds and retained artifact `9050520345`. The duplicate
report-only mutation job also completed successfully and retained the
5,328,647-byte artifact `9051304862`; the earlier mutation evidence remains
the acceptance proof because it demonstrates execution despite a red sibling.

### Other executable probes passed

- FTS5 absent-delete preserves the neighbouring contentless row.
- A duplicate-key failure in hiqlite statement two rolls back statement one.
- The JSON-parameter reconcile path decoded 150,000 gone IDs in 0.007 seconds.
- Disposable PR #114 proved the synthesized-vendor RustSec red path while the
  ordinary workspace remained green.
- Main branch protection requires the strict up-to-date aggregate PR gate.
- The first 100 audited CI runs put fast-preflight p95 at 15 seconds against
  its 60-second budget.
- Direct fleet inspection recorded read-only media binds on both nodes,
  `/dev/dri` exposure, nuc4's TCP 32402/GDM UDP 32415 override, and the private
  deploy play's pinned `make docker-up` call. The later backup repair retained
  those contracts with rebuild disabled.

## 6. Remaining gates — hardware or deliberately default-off only

These are not substitutes for code review. They are claims that only the named
device, elapsed time, or production presentation path can settle.

- **Android build 25:** unopened-app recovery after a real reboot on targetSdk
  37 · real six-hour `onTimeout` · fully offline playback with a selected
  subtitle · Android TV `uiMode` and D-pad behavior. An upgrade from build 24
  requires one foreground Resume tap before the new UIDT can exist; the app
  cannot retroactively schedule that user-initiated job from the background.
- **iPad offline:** completed background transfer · iOS 17
  `didFinishDownloadingTo` versus iOS 18 `willDownloadTo` ·
  `AVAssetCache.isPlayableOffline` for TS plus one-segment VTT · airplane-mode
  start and midpoint seek.
- **Both mobile platforms:** post-expiry re-download proving the server's
  `Cached` transfer-only path without re-encoding.
- **Playback matrix:** 2–5 minute typeless-vs-control EVENT cadence · iPad
  Stage Manager/Split View · iPhone orientation, PiP return, autoplay, and
  background-audio/Now Playing continuity · duration-less MKV natural end ·
  maximum-Dynamic-Type marker readability on a narrow iPhone · physical
  PiP/PGS · tvOS focus and swipe HUD · Android TV D-pad/TV mode and tunneled
  playback · physical High-tier HEVC HDR playback while Main-tier and Dolby
  Vision remain unaffected · ExoPlayer acceptance of the cleared hvcC tier
  bit.
- **Apple stall telemetry:** repeated approximately eight-second throttled
  AVPlayer stalls must reach the server with zero playback reopens.
- **Paused live playback:** leave AVPlayer paused for more than 60 seconds on a
  non-ENDLIST playlist and distinguish playlist reload behavior from the
  server's 60-second idle reaper plus 15-second sweep cadence.
- **Media-origin delivery:** on physical Android Media3, prove
  `X-Plurx-Media-Origin-Ms` reaches `onTransferStart`/`responseHeaders` before
  the first progress post.
- **PGS production presentation:** representative cold extraction through
  parser/manifest publication and a physical overlay presentation; on a
  fade-heavy production title, retain cue count and PNG bytes against the 256
  MiB per-track cap and judge whether a 422 refusal is acceptable UX. The same
  demux command's recovery refutes a deterministic 600-second demux defect;
  it does not prove the complete feature gate.

The physical session already proved the iPhone build-46 source/device suite,
the iPad 128-test suite and full-screen recovery slice, Pixel capability/PiP/
PGS/safe-area/navigation presentation, Pixel package transfer and release,
and Pixel airplane-mode playback with midpoint seek. Those narrower passes do
not silently satisfy the rows above.

## 7. External reviewer procedure — reproduce the claims that matter

Use a clean clone. Do not use the operator's primary checkout as evidence.

```bash
git fetch origin main                         # resolve the final review head
git switch --detach origin/main               # remove local branch ambiguity
make check                                    # baseline policy + Rust gate
make cluster-check                            # three-voter durable-state proof
make hiqlite-spike                            # focused semantic proof
gh run view 31346745768                       # release publication
gh run view 31347358863                       # normal fuzz + mutation isolation
gh run view 31347364590                       # seeded crash must be red
gh run view 31354801983                       # playback-fix exact-head validation
```

For client fixes, inspect the production symbol and its `tests/client-fixes.toml`
anchor together. For corrective Rust commits, inspect
`validation/regressions.toml` and verify the named check can reject the base
implementation. The Android post-merge CI and playback-fix hosted-nightly
result are recorded above; this handoff is ready for an external review
against the final main ancestry.

## 8. Non-goals — do not turn the handoff into a second backlog

- Do not reopen any work-order “Don't” item without new contradictory evidence.
- Do not call default-off typeless HLS or PGS production-ready before §6.
- Do not label the original PGS timeout “fixed”; record fail then unchanged
  recovery, because the cause is an inference.
- Do not accept prose-only physical evidence where a retained result bundle,
  device log, server ledger row, or checksum can be captured safely.
- Do not hide the #90/#96 ordering miss. The defect is gone; the sequencing
  failure still happened.
