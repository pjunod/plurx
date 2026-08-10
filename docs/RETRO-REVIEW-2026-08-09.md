# plurx retrospective review — the week of 2026-08-04 → 2026-08-08

**Verified against:** pinned `origin/main` @ `e8a910fbe8fdec790df8d22cb36413a5e35c78b0` (merge of PR #92, 2026-08-08 20:11 PT).
**Open PR #90** reviewed at head `2109946`.
**Scope:** all 47 first-parent merges on main since 2026-08-04 (PR #38 → PR #92; pre-window base `128158e8`), plus the open M1d PR.
**Method:** fresh cloud clone; full gate run at the pinned sha; ten parallel area reviews (design, architecture, correctness, tests, CI/CD, deploy, docs); every blocker raised in this week's pre-merge reviews re-verified against what actually merged; headline new findings re-verified by direct code probes before publication.

Companion work orders (one per subject, sized for a GPT agent) live in `docs/retro-2026-08-09/WO-01 … WO-11`. Each is self-contained: context, file:line evidence at the pinned sha, exact tasks, acceptance checks.

Execution results, the five approved decisions, adversarial corrections, and
the remaining hardware-only gates are consolidated in
[`docs/retro-2026-08-09/EXTERNAL-REVIEW-HANDOFF.md`](retro-2026-08-09/EXTERNAL-REVIEW-HANDOFF.md).

---

## 1. Baseline health at the pinned sha (executed here, Linux, rustc 1.97.1)

| Gate | Result |
|---|---|
| `make check` (validation-lint · history-check · operations-check · fmt · clippy `-D warnings` · workspace tests) | **GREEN** (exit 0; 348 core + 332 plurxd + 11 contract + 27 misc tests) |
| `cargo test --workspace --no-fail-fast` | **GREEN** (0 failures, 1 ignored) |
| `make cluster-check` (three-voter M1b/M1c contract) | **GREEN** |
| `make hiqlite-spike` | **GREEN** |
| `make release-check` | **RED** — `CHANGELOG.md has no '## [0.2.7]' section`. Pre-existing, now load-bearing: zero git tags exist, so the tag-triggered publish pipeline has never run (→ WO-10) |

`make apple-test` is macOS-only and was not runnable here; CI runs it (iPhone 16 Pro + Apple TV simulators — still **no iPad destination** despite two iPad-specific fixes this week).

## 2. The headline: the review→fix→merge loop worked

This was the strongest week of process the project has had. Four PRs (#84, #88, #91, #92) were reviewed pre-merge with CHANGES REQUESTED, and **every one merged with a real fix commit rather than merging over the objections**. Re-verified in the merged tree, blocker by blocker:

- **#84 (clustering M1b):** hiqlite is now optional/feature-gated (`plurx-core/Cargo.toml:15,19`; the Docker `plurxd` build excludes it); the incompatible-voter proof is real (preflight subprocess never calls `start_node`, exit-42 contract, non-vacuous leader identification); vendored crates got a synthesized-lockfile RustSec scan (`scripts/vendor-audit-lock` + second `audit-check` in `rust-audit.yml`). One residual: M1b's 3 s timeout wrapper did not get extended to M1c's ~30 call sites (WO-01).
- **#88 (clustering M1c):** all five blockers fixed — prune bound wired to a real config key (`storage.scan_prune_percent`, default 10); `library_roots` gets both a trigger-based reset on path edits and an admin `POST /libraries/:id/root-identity/reset`; the O(items×gone) reconcile became a prepared loop; the parity check reads voter-locally; FTS moved to `contentless_delete=1` plus an admin `search-index/rebuild` on both backends; TOCTOU and gate-coverage holes closed. One assertion the review flagged (`store_contract.rs:905` `let _ = prune_empty_items`) was never restored.
- **#91 (HLS ahead-window + Apple natural end):** the fix commit **reverted the identity release threshold** — hysteresis restored at 180 s hold / 150 s release — and landed everything asked: `hold_reason` (time/bytes/global) in SessionInfo and the web panel, a genuinely biting test on the `want_suspend == suspended` guard, an end-to-end test that a client fetch resumes a held session, the honest "this is not a structural deadlock escape" caveat restored in four places, CHANGELOG heading corrected. The Apple eject-on-unknown-duration blocker got a corroborated-duration gate (`endAction`, `PlayerController.swift:430-444`) plus `finished` resets on every open/stop.
- **#92 (Skip Credits layout):** build bumped 37→38 (the version-gate preflight failure that made the Swift suite unreachable is cleared), the test was rebuilt around the production `PlayerTrailingControlRow` + shared `PlayerMarkerButtonLabel` with `.lineLimit(1)`, and STATUS.html build numbers were touched for the first time in four builds.

Merged-state spot checks of earlier fix-before-merge claims (#79 eviction accounting, #82 `validate_uuid`, #83 17-name ban list with superset test) all still hold on main.

**Bottom line: nothing previously flagged as a blocker is live on main today**, with two small exceptions carried into WO-01 (M1c timeout coverage; the weakened prune assertion) and one process gap (below).

## 3. What's genuinely good this week (worth keeping as patterns)

- **Offline viewing (#67–#69, #73)** — the best-designed large feature: packages are content-addressed cache entries with a durable intent row on top, so preparation reuses the producer's claim/resume/publish machinery; quota checks share the insert transaction; typed error codes (429/507); hashed client-generated lease tokens; bounded-cardinality metrics with a test that label sets can't grow. The plan-review's twelve blocking revisions all landed (TS format coherent end-to-end, no FK cascade on packages, iOS 17/18 delegate split, Android dataSync timeout handling, explicit stream keys, cache-only playback via placeholder upstream).
- **Owner attribution** — offline preparation is visible in-product (activity page: what, whose profile, phase, %, bytes; `prepare_work_seconds_total`) and stoppable. This honors the "nothing uses my hardware anonymously" rule. Gap: stop is owner-or-global only, no per-item admin cancel (WO-07).
- **The cluster gate direction** — `plurx-cluster-check` at main is a real multi-process proof (independent digests, known-value validation, exit-status checks), and the #90 head turns it into the best gate in the repo (identity-varying negatives, refused-transition post-conditions, the store contract suite running against a live 3-voter cluster in CI).
- **Media origin (#56–#58)** — a real timeline bug (copy sessions lie about t=0 by up to a GOP) fixed with an additive wire contract, honest fallbacks, and a server integration test whose fixture is deliberately mid-GOP so the two answers differ.
- **Docs candor** — CLUSTERING-PLAN/PHASE3-SPIKE write deferrals down as M2 activation gates instead of glossing; OPERATIONS/PERF-PLAN were corrected to match code in the #91 fix commit.

## 4. The systemic weakness: existence-checking gates, and fixes that tests don't pin

Four times this week (#81, #89, #91, #92) a headline fix merged (initially) with a test that stays green if the fix is reverted. That is not four accidents — every layer of the validation system audits **existence, not discriminating power**: points.toml maps paths to suites (any Swift edit is "covered" by the simulator suite regardless of what the test renders); history-audit accepts "a tests/ path was touched, an assert was added" as evidence; regressions.toml is prose claims of coverage, never a demonstrated red→green. The one mechanism that actually binds tests to production symbols — `routing-decisions.toml` anchors with the inventory checker — is the best artifact in the repo and is exactly the shape to generalize. WO-04 turns this into: a revert-the-fix protocol for corrective PRs, nightly `cargo-mutants` spot-checks scoped to the week's diff, required anchor rows for client fixes, and a discriminating history-audit.

Second process gap: **preflight serializes policy ahead of signal** — a stale build number skips the entire Swift suite, so authors repush blind (this exact sequence happened on #92). WO-03 makes mobile-version a parallel gate instead of a test-hiding prerequisite.

## 5. Confirmed new findings (probe-verified here; ranked)

1. **P0 — PR #90 must not merge as-is:** the fix commit added server-derived `expires_at` (`now_unix() + TTL`, `http/offline.rs:384` at pr/90) to `same_request`, so a legitimate same-`request_id` retry of a download create returns 409 instead of idempotent `Existing`. Two-line fix. Everything else in #90's fix commits verified genuinely fixed — after this, it's merge-ready. (WO-02)
2. **P0 — small libraries can wedge on deletion forever:** `prune_limit = known × percent / 100` with no floor (`scan/mod.rs:318-322`): a 9-file library at the default 10 % gets limit 0, one deleted file → `RefusedPrune` every scan, forever, and the TOFU establish path refuses too. Ships to every install today. Fix is `.max(1)` when pruning is enabled. (WO-01)
3. **P0 — no rollback path for fleet deploys:** two schema migrations shipped this week (v14 offline, v15 scan); migrations auto-apply on start, are forward-only, an older binary refuses a newer DB, nodes run `reset --hard @{u}` + rebuild, and there is **no backup step and no documented recovery**. A bad migration = both plurx nodes crash-looping with no way back. (WO-10)
4. **P1 — the global byte cap can hold sessions with an unreachable release:** the cap compares **total** on-disk bytes but releases at half, while GC can never prune inside 180 s retention behind each session's frontier — so with 3-4 concurrent high-bitrate remuxes the "release at half" line sits below the un-prunable retention floor and no client behavior can ever release the hold. Confirmed in code (`transcode.rs:646-651` + `:1286`); arithmetic says defaults can hit it. This is the one true deadlock shape left in flow control. (WO-05)
5. **P1 — CI release artifacts secretly re-grow the hiqlite closure:** `ci.yml:333` builds `--workspace`, and feature unification (probe-confirmed with `cargo tree`) compiles plurx-core **with** `hiqlite-store` into the uploaded plurxd artifact — the exact 191→331-crate growth #84's fix exists to prevent, and a binary that diverges from the Docker one. One-line fix (`-p plurxd`). (WO-03)
6. **P1 — Apple: stalls below ~12-14 s are still invisible**, and the motivating iPad trace (12.29 s) sits exactly at the detection boundary. #86's stall beacon fires only when the recovery ladder does. Piggyback `accessLog().numberOfStalls` deltas on the existing progress cadence and the periodic-stall mystery finally produces server-side evidence. (WO-06)
7. **P1 — the EVENT→sliding-live playlist mutation (#61) is an RFC 8216 violation and the best-fitting explanation of the 2-5 min iPad stall cadence** (first prune lands 2-4 min in; each reopen restarts the clock). The server-side fix is cheap — serve the typeless sliding shape from the first response — and the review left a concrete one-device experiment to settle it. (WO-05)
8. **P1 — Apple regressions-in-waiting in the recovery ladder:** `establishedPlayback` is never reset in `open()` (probe-confirmed — the buffering detector is armed during successor-session gate fills; one slow fill after a mid-film quality change spends the one-shot, a second gives a healthy server a terminal error screen), and `endAction` treats **every** session as a growing playlist (`sessionId != nil`, `:1796`), so a NULL-duration VOD title ends in an endless reopen/create loop instead of finishing. (WO-06)
9. **P1 — offline lifecycle leaks:** Android's package-release call depends on an in-memory map that pauses/restarts lose (server keeps `ready` rows + pinned bytes + quota for 7 days; a 15 GiB profile can wedge on 507s); the "live playback outranks preparation" arbitration — the feature's headline promise — has **zero** test coverage on the `ensure_offline` path; the offline kill switch stops preparation but not delivery. (WO-07)
10. **P2 — PGS overlay debt:** iOS auto-PiP still engages with the overlay active (subtitles silently vanish — the manual path is blocked, the automatic one isn't); the fuzzer exists but runs in no workflow; publication isn't crash-durable (no fsync) and a torn generation is served as Ready forever; BT.601 is hard-coded where HD PGS is typically BT.709; a capacity 503 permanently fails the client's subtitle selection. The two accepted-plan riders (server-side HDR-burn guard; one-tap SDR burn escape) shipped in neither form — flagged as decisions, not defects. (WO-08)

Full per-area detail, including ~25 medium/low findings and the refuted-claims ledger (things checked and cleared — do not re-raise), lives in the work orders.

## 6. Per-area verdicts

| Area | Verdict | Work order |
|---|---|---|
| Clustering M0–M1c (merged) | **Solid.** Store seam + replicated.rs policy-as-code are exemplary; all pre-merge blockers really fixed; residuals are M1c timeout coverage, read-path `validate_sql`, small-library prune floor | WO-01 |
| Clustering M1d (open #90) | **Near-ready.** Best gate in the repo now; one new idempotency regression blocks merge | WO-02 |
| Offline viewing | **Strong server, fragile client lifecycle.** Ship-quality core; completion-release and arbitration coverage need work | WO-07 |
| PGS overlay | **Good architecture, unfinished safety story.** Server contract excellent; fuzz-in-CI, durability, PiP, color matrix outstanding | WO-08 |
| HLS flow control | **Recovered.** #91's fix commit undid the ceiling-chase and added real observability; global-cap release basis and the EVENT mutation are the remaining structural items | WO-05 |
| Apple client | **Coherent state machine now**, verified fixes throughout; telemetry threshold + two flag-lifecycle bugs remain | WO-06 |
| Media origin + Android | **Feature correct end-to-end;** client-side consumption untested, seek UX regressions | WO-11 |
| CI/CD + validation | **M1 of the overhaul landed faithfully;** artifact feature-unification, missing timeouts, docs-lane holes, and the evidence-discipline gap are the real items | WO-03, WO-04 |
| Deployment | **Interface clean (ship + private ansible), compose/Docker strong;** no rollback/backup story, fictional registry template, mobile-deploy behavior now ungated | WO-10 |
| Docs | **Mostly honest and current** — better than any prior audit; one stale-build sweep + missing clustering/PGS CHANGELOG entries + STATUS Phase-4 markers | WO-09 |

## 7. Operational notes for Paul

- The fleet ledger still says every node runs `787eaa6` (2026-08-01) while main gained ~47 merges. If that's real, the week's work isn't deployed anywhere; if it's stale, the ledger is lying. Worth one `curl /api/v1/server` per node (GPT prompt in WO-10).
- `release-check` is red and has never gated anything (no tags exist). Decide once: either cut 0.2.7 properly (CHANGELOG heading + tag → the publish pipeline finally runs) or subordinate RELEASING.md to the continuous-main reality. WO-10 has both paths.
- Ten `docs/PR*-REVIEW.md` files from this week's pre-merge reviews are sitting **untracked** in your working tree; WO-09 includes committing them if you want them kept.

## 8. Suggested order of execution

WO-02 (before #90 merges) → WO-01 → WO-10 (backup step) → WO-03 → WO-05 → WO-06 → WO-07 → WO-04 → WO-08 → WO-09 → WO-11. Each work order states its own priority and acceptance checks; they are independent unless noted.
