# WO-04 — Evidence discipline: make unpinned fixes structurally hard

**Repo:** `~/code/plurx` · **Baseline:** `origin/main` @ `e8a910f` · **Priority: P1**

## Context

Four corrective PRs this week (#81, #89, #91, #92) initially shipped headline fixes whose tests stay green when the fix is reverted (e.g. a layout test hosting a standalone lookalike component that never touches `PlayerView`; assertions on bound literals; a module test iterating its own const). All were caught by review, not by any gate — because every gate in the repo audits **existence** (a test file was touched, a suite ran, an assert was added), never **discriminating power** (would anything fail if the fix were reverted?). The strongest counter-example already in-tree is `tests/playback/routing-decisions.toml` + `tests/validation/test_playback_routing_inventory.py`: anchors bind a named test to a named production symbol and the checker fails when either side drifts. Generalize that; add revert-sensitivity as an occasionally-mechanical check.

## Tasks

1. **Adopt a revert-the-fix protocol for corrective PRs, and automate the cheap half.**
   Rule: a PR that claims to fix behavior must contain at least one test that FAILS on the base tree. Mechanically: check out the PR, `git checkout <base> -- <production files>` (keep the PR's tests), run the targeted suite, require failure.
   Implement as `scripts/prove-fix` (args: base sha, test filter, path list) so a reviewer or CI job can run it in one command. Wire it as an opt-in CI job triggered by a `fixes-behavior` label or a `regressions.toml` entry. Rust first; note in the script's help that for Swift the same procedure runs via `make apple-test` on macOS.
   Acceptance: run it retroactively on #92's final state (should pass — b06f78b's rebuilt test) and on #92's original `6ce3ff0` test (should fail = correctly flags the unpinned fix).

2. **Nightly mutation spot-checks scoped to the week's diff.**
   Add a `validation-nightly.yml` step running `cargo-mutants` limited to files changed in the last 7 days (`git log --since=7.days --name-only` → `--file` filters), with a time cap (e.g. `--timeout 300`, whole step ≤ 60 min) and a report artifact; surviving mutants post as a summary, not a failure, for the first month. This is the backstop that also catches vacuous asserts.
   Acceptance: nightly run produces a mutants report; seeded check — temporarily neuter one assertion locally and confirm the mutant survives/reports.

3. **Require anchor rows for client fixes.**
   Extend the routing-decisions pattern: any corrective commit touching `clients/**` must add/update a row in `tests/playback/routing-decisions.toml` (or a new `tests/client-fixes.toml` with the same shape: symbol anchor + test anchor), enforced by extending the existing inventory checker. The marker-row fix (#92) is the immediate candidate — it currently has no row.
   Acceptance: inventory checker red when a listed symbol or test disappears; #92's `PlayerTrailingControlRow`/`PlayerMarkerButtonLabel` get a row.

4. **Make history-audit discriminating.**
   `validation/history.py:165-178` accepts "touched any surviving tests/ path or added an assert-like line" as direct evidence, and its keyword `ISSUE_RE` (`:18-32`) lets naturally-worded fix subjects ("Hide iPad system chrome…", "Address PR review feedback") skip the ledger entirely. Fix: (a) for corrective commits touching `clients/**` or `crates/**`, require an explicit `regressions.toml` row or an anchor row (task 3) instead of the inline heuristic; (b) widen `ISSUE_RE` or key off conventional-commit `fix:`/`Fixed` CHANGELOG sections.
   Acceptance: audit red when a fix commit's only "evidence" is an unrelated test edit (construct the case in a fixture repo or a test of `history.py` itself).

5. **Pin doc version strings.** README/STATUS.html build numbers drifted stale four+ times this week because nothing gates them. One `tests/operations` contract: extract "build N" claims from `clients/apple/README.md`, `clients/android/README.md`, `docs/APPLE-CLIENT-PARITY.md`, `docs/STATUS.html` and compare against `project.yml` / `build.gradle.kts`. (Do WO-09's sweep first or this lands red.)
   Acceptance: editing a build counter without the docs goes red locally via `make operations-check`.

## Don't

- Don't turn any of this into a merge-blocking wall in week one — label-triggered / nightly / report-only first, tighten after a month of signal.
- Don't try to mutation-test Swift; the revert protocol + anchor rows are the Swift-side coverage.
