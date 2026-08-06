# CI test overhaul — fast failures, selective evidence, safe reuse

**Status:** Milestone 1 implemented in this working tree · review corrections
applied · Milestones 2–5 ready to build · **Written:** 2026-08-05 · **Reviewed:**
2026-08-06

Companion to [VALIDATION.md](VALIDATION.md) (how changed paths select behavior
contracts) and [ARCHITECTURE.md](ARCHITECTURE.md) (how the repository fits
together) — this plan turns the existing functionality-point catalog into a
faster CI scheduler. Read §2 before changing the order, then execute one
milestone at a time and keep the acceptance checks green. If a step seems to
require weakening the single required `PR validation gate`, stop and flag it;
the goal is less irrelevant work, not less evidence for affected behavior.

## 1. Objective — answer the cheapest decisive question first

CI should provide the earliest trustworthy answer to three questions:

1. **Is the change policy-complete?** Version counters, catalog ownership,
   corrective-history evidence, and workflow contracts should fail before a
   runner installs a browser, simulator, emulator, cross compiler, or image
   toolchain.
2. **Which behavior can the change affect?** A docs-only PR should prove its
   documentation contracts and stop. A client change should pay for that
   client, not every other package and delivery surface.
3. **Has this exact evidence already passed?** A rebase that changes no inputs
   to a deterministic suite should reuse a verified result. A relevant base
   change, toolchain change, or test change must invalidate that result.

The target is a layered gate, not a smaller universal test command. Full
coverage still belongs on `main`, in the merge queue, and nightly; PR feedback
should be proportional to the behavior under review.

## 2. Evidence — the failures cluster before the slow work

### 2.1 The 2026-08-05 sample

The audit inspected the latest 100 pull-request runs of `ci.yml`, from
2026-08-03 through 2026-08-05, directly through the GitHub Actions API. The
sample contained 74 successes, nine cancellations, and 17 failed workflow
runs. The separate `lint.yml` workflow had 100 successes in its latest 100 PR
runs. Three failed CI runs contained two causal failed jobs, producing 20
actionable failure signals. The rows below count job-level signals, not unique
workflow runs:

| Failure cause | Failure signals | Earliest decisive detector | Waste observed before the redesign |
|---|---:|---|---|
| Mobile build counter was not advanced | 4 | `python3 -m validation.mobile_versions` | The combined validator continued into about 90–100 seconds of Rust work after the policy failure |
| Corrective-history evidence was stale or missing | 4 | `scripts/history-audit` | The combined validator continued into the complete Rust gate and, for broad changes, other selected checks |
| Web structural golden was stale | 3 | Browser structure check | Ran 54 captures for about 306–308 seconds while unrelated jobs ran |
| Apple suite failed | 4 | Focused compile/model tests before dual-platform execution | Two expectation revisions paid for simulator setup; one run used an unavailable simulator; one exposed a pinned-Xcode compile error |
| Container smoke contract failed | 3 | Source-level container contract, then focused smoke | Two runs waited for a container that could not open its database; one passed behavior and failed privileged cleanup |
| Validation catalog contract failed | 1 | Validation unit tests | The general job still launched its broader selected check set |
| Genre-backfill cursor race | 1 | Focused Rust unit test | Failed inside the complete workspace suite |

The four version and four history signals occurred in eight distinct runs, so
eight of 17 failed runs were policy-hygiene failures detectable without a
compiler, browser, simulator, emulator, or image build. The catalog-contract
signal is classified separately as a validation failure. The version failures
were deterministic policy errors: Apple `CURRENT_PROJECT_VERSION` or Android
`versionCode` remained equal to the base. The history failures were stale or
missing regression-evidence mappings, including the documentation-only PR in
the sample. The web failures were deterministic structural-golden drift. Two
Apple failures were revisions of the same expected middot-versus-space
mismatch; the other two were environment/compatibility failures. The Rust race
was fixed by commit `6fb8e3ad`; it is evidence for a flake policy, not a reason
to retry every failure until green.

The failed `functionality points (CI profile)` job appeared in 11 of the 17
failed runs, but that label hid materially different causes. The runner
continued after a cheap check failed so it could write complete evidence. That
is useful inside a tier, but wasteful across tiers. Milestone 1 moves policy
checks into a fail-fast predecessor job; Milestone 2 keeps independent suite
results without making a known policy failure pay for compilation.

This is a small, recent sample. It is sufficient to choose the first canaries,
but not to freeze the order forever. §9 makes the ranking data-driven.

### 2.2 Documentation was scoped but still paid for Rust twice

PR #67 changed documentation only. Expensive surface jobs were correctly
skipped, but the portable baseline still took about 2 minutes 25 seconds and
the separate lint workflow took another 35 seconds. The PR ran more than 600
Rust tests plus Clippy even though no executable input changed.

The existing impact graph already knows how to skip browser, Apple, Android,
build, and container jobs. The missing distinction was between:

- documentation that is executable evidence and needs fast contract tests; and
- source or build inputs that can change a shipped artifact and need a runtime
  or compiler suite.

## 3. Contract — optimize without creating blind spots

### 3.1 Required behavior

- `PR validation gate` remains the one required branch-protection result. Every
  PR launches it, even when all executable jobs are inapplicable.
- The fast preflight always runs. It fails closed on policy or catalog errors.
- Scope resolution fails open. A missing base, invalid catalog, or unknown path
  enables more tests rather than suppressing them, emits a typed reason, and is
  counted in CI telemetry. Green-and-slow is not an acceptable silent mode.
- A mixed docs-and-code PR is a code PR. Documentation never hides an
  executable path in the same diff, and its documentation paths still select
  `docs-contract` like any other affected contract.
- `main`, tags, scheduled validation, and future merge-queue runs retain the
  exhaustive posture appropriate to those events.
- Reused evidence must identify the exact inputs and trusted successful run
  that produced it. A dependency cache hit is not proof that tests passed.
- A retry may diagnose a flake, but the original failure remains visible and
  actionable. CI must not turn nondeterminism into a green merge.

GitHub leaves a path-filtered required workflow in a pending state. That is why
the required workflow stays alive and skips jobs internally; workflow-level
`paths-ignore` is not the docs-only mechanism. See GitHub's
[workflow syntax](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax).

### 3.2 Non-goals

- **Do not remove full validation.** Move it to the event and surface that
  justify it.
- **Do not make line coverage the scheduler.** Coverage measures executed
  lines, not whether a client, protocol, migration, or release rule is at risk.
- **Do not shard before measuring.** Splitting a 20-second crate into four jobs
  adds queue and setup cost without improving feedback.
- **Do not trust PR-authored cache contents as a pass ledger.** GitHub warns
  that cache contents are untrusted and can be read across allowed scopes. Use
  caches for dependencies and intermediate compilation only. See the
  [dependency-caching security guidance](https://docs.github.com/en/actions/reference/workflows-and-actions/dependency-caching).
- **Do not skip release, security, or migration evidence on a probabilistic
  guess.** Those checks either have a complete invalidation contract or run.

## 4. Target flow — policy gates work before environments

```text
 PR event
    │
    ▼
 resolve changed paths + affected contracts
    │
    ▼
 T0 preflight (<60s p95)
 versioning · history · catalog · validation tests · workflow contracts
    │ fail ───────────────────────────────────────────────▶ stop expensive work
    │ pass
    ├── docs only ─────────▶ docs contracts ──────────────▶ PR gate
    │
    ▼
 compute selected suites + content fingerprints
    │
    ├── verified prior pass ─▶ report reused evidence ────┐
    │                                                     │
    └── no valid pass ───────▶ run affected suites ───────┤
                                                          ▼
                                             aggregate PR validation gate
```

The first minute decides whether later runner-minutes are justified. Parallel
work begins only after that decision. This deliberately adds one preflight
checkout plus about six seconds of measured compute to the 74% of sampled runs
that eventually pass. The trade buys fewer wasted runner-minutes and earlier
policy failures. T0 must therefore remain compiler-free; if it grows a Rust,
browser, simulator, emulator, or image toolchain, move that work to a later
tier instead of parallelizing the gate away.

## 5. Milestone 1 — fast preflight and documentation lane

**Status:** implemented in this change.

### 5.1 Scope has an explicit `docs_only` result

[`validation/ci_scope.py`](../validation/ci_scope.py) treats a non-empty diff as
documentation-only when every path is Markdown, documentation content,
license/notice material, or `validation/regressions.toml`. The last carve-out
is safe because that file is read only by the history audit that always runs in
T0; it cannot select or suppress a runtime suite. Selector code and
`validation/points.toml` remain executable changes. The result disables Rust,
browser, Apple, Android, cross-target, and container surfaces. An empty diff,
unknown diff, push, or tag does not earn the shortcut.

The scope also reports `mobile_version` independently. That lets CI run the
highest-frequency policy failure before the general functionality-point job.

### 5.2 Preflight gates every expensive job

The new `fast policy and contract preflight` runs, in order:

1. mobile release versioning when the diff touches release inputs;
2. corrective-history evidence;
3. catalog lint and the validation-framework unit tests;
4. CI, deploy, container, and shipping source contracts.

Every expensive fan-out job in `ci.yml` has `needs: [scope, preflight]`. A
version error therefore stops browser, simulator, emulator, cross-build,
Docker, and complete Rust work before those environments are provisioned. The
dependency adds a small latency tax to green code PRs; keeping preflight below
60 seconds p95 and free of compilers is the explicit acceptance condition.

### 5.3 Lint reports success without compiling docs-only changes

The badge-oriented lint workflow still launches, because an omitted required
workflow can remain pending. It resolves the same scope and turns a docs-only
run into a small successful job without installing Rust or running Clippy.

### 5.4 Acceptance checks

```bash
python3 -m unittest tests.validation.test_runner \
  tests.operations.test_contracts                 # scope and workflow contracts
scripts/validate lint                              # every governed path is owned
make operations-check                             # source-level CI contract
```

Observable CI acceptance:

- a docs-only PR runs `scope`, `preflight`, the lightweight lint job, and
  `PR validation gate`; the portable Rust job is skipped;
- an Apple or Android source change with a stale build counter fails in
  preflight before any native job starts;
- a mixed `docs/**` plus `crates/**` change does not use the docs-only lane;
- the literal PR #67 shape—its three documentation files plus
  `validation/regressions.toml`—does use the lane, while replacing the mapping
  file with `validation/points.toml` does not;
- a mixed docs-and-code PR still selects `docs-contract` for its docs paths;
- every inapplicable job is `skipped`, while `PR validation gate` is `success`.

## 6. Milestone 2 — split the universal Rust gate into owned suites

The current `rust-gate` combines formatting, Clippy, every workspace unit test,
and documentation tests. It is an effective local release baseline but too
coarse for change selection and duplicated by `lint.yml`.

### 6.1 Give every test tier one job and one timeout

Every PR target below is a p95 wall-time service level. Set
`timeout-minutes` to roughly twice that budget so an infrastructure hang ends
as a named failure instead of inheriting GitHub's six-hour default.

| Tier | Purpose | PR p95 target | Job timeout | Full-run event |
|---|---|---:|---:|---|
| T0 policy | Version, catalog, history, workflow contracts | <60s | 2m | Every PR and merge group |
| T1 static | Rustfmt, Clippy, JS syntax, deterministic doc contracts | <3m | 6m | Affected language/build inputs |
| T2 unit | Pure Rust, Swift, Kotlin, and JavaScript behavior | <5m per selected suite | 10m | `main` and nightly all suites |
| T3 integration | SQLite upgrades, HTTP wire, ffmpeg, client model integration | <10m per selected surface | 20m | `main` and nightly |
| T4 environment | Browser, simulators, emulator, image lifecycle, cross-build | <15m per selected surface | 30m | Merge queue, `main`, release |
| T5 exhaustive | Recovery, playback matrices, bounds, fuzz/soak | No PR SLO | Explicit per suite | Nightly and manual |

Each command belongs to exactly one tier. A higher tier may consume artifacts
from a lower tier, but it must not silently rerun the same tests.

### 6.2 Replace `rust-gate` with named components

Add these checks to [`validation/points.toml`](../validation/points.toml):

| Check | Proposed command | Primary owners |
|---|---|---|
| `rust-format` | `cargo fmt --all --check` | Rust source and manifests |
| `rust-clippy` | `cargo clippy --workspace --all-targets -- -D warnings` | Rust source, manifests, toolchain |
| `core-unit` | `cargo test -p plurx-core --lib` | Core model, scan, store, metadata |
| `server-unit` | `cargo test -p plurxd --bin plurxd` | HTTP, delivery, playback runtime |
| `plex-unit` | `cargo test -p plurx-compat-plex --lib` | Plex compatibility crate |
| `pgs-unit` | `cargo test -p plurx-pgs --lib` | PGS parser and bounded overlay model |
| `rust-doc-*` | `cargo test -p <crate> --doc` | Public examples owned by each selected crate |

Keep `make rust-check` as the convenient exhaustive local composition. CI
selects the named checks directly and runs each test binary once. Remove the
duplicate Clippy execution from either `ci.yml` or `lint.yml` only after branch
protection and badges point at the retained result.

### 6.3 Separate tests that currently hide inside the workspace run

The following contracts deserve their existing focused names rather than a
second execution after `cargo test --workspace`:

- `database-upgrades` owns migration and future-schema refusal;
- `api-wire` owns the shared Rust/Swift/Kotlin fixture;
- `security-boundaries` owns authorization matrices;
- `offline-contract` owns package, lease, quota, and cache boundaries;
- `user-journey` owns seeded browse, playback, progress, and settings flows.

Use dedicated Cargo integration-test targets for the focused Rust contracts,
for example `cargo test -p plurx-core --test database_upgrades`. Broad crate
checks use `--lib` or `--bins`, so Cargo's target boundary mechanically keeps
focused integration targets out; do not maintain substring `--skip` filters
that silently stop matching after a module rename. A catalog meta-test must
enumerate every test target and assert that each focused target contains at
least one test, appears in exactly one named check, and is absent from the
broad command's `--list` output. One assertion should produce one set of
runner-minutes.

For Apple changes, add a T1 environment assertion before T4 starts: verify
`/Applications/Xcode_16.4.app` exists and that `xcrun simctl list runtimes`
contains the pinned iOS and tvOS runtimes. The check should fail in seconds
with an environment-drift label, before any simulator boots.

### 6.4 Coverage targets emphasize contracts

- 100% of governed changed paths map to at least one functionality point.
- 100% of release-input changes select the corresponding version gate.
- 100% of authentication, migration, playback-routing, offline-ownership, and
  client wire boundaries have named focused checks.
- Nightly and `main` coverage publish per-crate trends as diagnostics; PRs do
  not rerun the Rust suite for a line-coverage ratchet.
- No raw line-coverage target can substitute for a missing critical contract.

Example cases that must remain explicit:

- Apple source changes with no build increment fail T0;
- deleting a migration input selects `database-upgrades`;
- changing shared API JSON selects Rust plus both decoder suites;
- changing only a Plex mapper selects `plex-unit`, not native simulators;
- changing the ffmpeg command builder selects server unit and playback
  integration, but not Android focus instrumentation.

### 6.5 Operators can force evidence without editing CI

Add three fail-safe controls with the tier split:

- repository variable `CI_FORCE_FULL=1` selects every suite and disables
  evidence reuse;
- PR label `ci:full` does the same for one PR;
- PR label `ci:no-reuse` keeps impact selection but runs every selected suite.

Include `labeled` and `unlabeled` pull-request activity types so changing an
override starts a new run. Record the repository variable and labels in
[OPERATIONS.md](OPERATIONS.md), print the active override in the job summary,
and test that an unknown diff plus every override takes the conservative path.
Recovery from a selector incident must not require editing the selector.

## 7. Milestone 3 — make path ownership describe invalidation

### 7.1 Add test-input closures, not just source triggers

Each check needs a complete invalidation set. Extend the catalog schema with an
optional `inputs` list or derive one from point paths plus shared build inputs.
The closure must include:

- production source read by the check;
- the test source and fixtures;
- manifests and lockfiles;
- generated-code inputs;
- the command wrapper and workflow job;
- compiler, SDK, container, browser, and runner pins;
- provider points that feed the selected consumer.

If the closure is incomplete, the check may still be path-selected, but it is
not eligible for result reuse in §8.

### 7.2 Add a first-class documentation contract

Create a `docs-contract` T0/T1 check that validates, without network access:

- repository-relative links and named local files exist;
- exhaustive inventories still match their source/test anchors;
- copied commands name existing Make targets or scripts;
- plan status links resolve;
- generated screenshots or mockups referenced by docs exist.

Documentation that is already executable evidence—such as playback-routing
inventory text—continues to run its focused validation unit test. The docs-only
lane is cheap because its proofs are cheap, not because prose is ignored.
External URLs are deliberately excluded from this network-free PR check.
`validation-nightly.yml` performs the networked external-link sweep, records
redirects and failures, and does not make an upstream outage block a PR. On a
mixed docs-and-code diff, `docs-contract` is path-selected alongside the code
suites even though the PR no longer qualifies for the docs-only lane.

### 7.3 Keep docs-only classification narrow

The shortcut is valid for `docs/**`, Markdown, license/notice material, and
`validation/regressions.toml`. The mapping carve-out is narrow: only the
always-run history audit consumes it, so it cannot suppress runtime evidence.
These are deliberately not docs-only:

- `.github/workflows/**` because it changes the gate itself;
- `tests/**` because it changes evidence and invalidation;
- `validation/*.py` and `validation/points.toml` because they change selection;
- embedded web assets under `crates/plurxd/src/web/**` because they ship;
- build files, manifests, resources, and generated release inputs.

Acceptance: table-driven tests cover every allowed and rejected path family,
including deletions, renames, and mixed diffs. The literal PR #67 set—
`docs/OFFLINE-VIEWING-PLAN.md`, `docs/OFFLINE-VIEWING-REVIEW.md`,
`docs/STATUS.html`, and `validation/regressions.toml`—must be docs-only;
replacing the mapping file with `validation/points.toml` must not be.

## 8. Milestone 4 — measure reuse before enforcing it

GitHub's current concurrency group cancels obsolete PR runs. That saves work
during a force-push, but it also creates the best reuse opportunity: jobs that
finished before the cancellation retain their own conclusions. Reuse therefore
keys on a successful **job**, never the conclusion of its containing run.

### 8.1 Shadow mode proves the value and the safety first

Build the fingerprint and lookup modules first, but keep every selected suite
running. Each job records whether an earlier successful job would have matched,
how many runner-minutes the candidate reuse would have saved, and whether the
fresh result agrees. Run shadow mode for at least two weeks and 100 PR runs.

Enforcement proceeds only when candidate reuse would avoid at least 10% of
reuse-eligible runner-minutes and the investigation ledger contains zero stale
matches. A fingerprint match followed by a fresh failure is a stop condition,
not a statistic to average away. If the hit rate misses the threshold, keep the
fingerprint telemetry and do not build a resolver that skips tests.

### 8.2 Fingerprints bind paths, content, and the effective merge tree

For each deterministic check, calculate:

```text
fingerprint = SHA-256(
  fingerprint schema version
  + check id and exact command
  + workflow and wrapper (path, blob) pairs
  + runner/toolchain/SDK/container identity
  + sorted (path, blob-or-ABSENT) pairs for the complete input closure
)
```

The path is part of every pair: a multiset of blob IDs would collide when two
files exchange contents and would not describe a rename. An explicit `ABSENT`
marker makes deletion part of the identity.

Compute the PR's effective merge tree locally with
`git merge-tree --write-tree <base> <head>` after fetching both commits; do not
depend on GitHub's asynchronously generated merge ref. A conflict, missing
object, unsupported Git version, or any other failure means the fingerprint is
unavailable and the suite runs. If `main` changes a path inside the closure,
the fingerprint changes. If it changes an unrelated path, the fingerprint can
remain reusable.

### 8.3 Evidence has an explicit trust and retention model

A successful job publishes a small JSON manifest as a workflow artifact:

```json
{
  "schema": 1,
  "check_id": "apple-simulators",
  "fingerprint": "sha256:...",
  "run_id": 123,
  "job_id": 456,
  "job_conclusion": "success",
  "workflow_sha": "...",
  "head_sha": "...",
  "base_sha": "...",
  "runner": "macos-15/xcode-16.4",
  "image_version": "...",
  "tests": 196,
  "result": "passed"
}
```

The resolver may reuse the manifest only when GitHub reports that the
originating **job** concluded `success` and every fingerprint field matches.
The containing run may later fail or be cancelled; that does not erase a job's
completed evidence. Lookup, download, parse, merge-tree, or verification
failure means **run the test**.

PR-authored manifests are no more trusted than the PR-authored workflow that
ran them. Required review through `CODEOWNERS` or an equivalent branch rule
must protect `.github/workflows/**`, `validation/**`, and the resolver before
enforcement begins. Every decision logs the candidate run/job, trust source,
fingerprint fields, and rejection reason. Fork PRs may reuse only matching
evidence published by trusted `main`; evidence authored by the fork is never a
reuse source.

Record the repository's resolved Actions artifact-retention period in
[OPERATIONS.md](OPERATIONS.md). That period bounds the reuse window; expiry is
safe because a missing artifact reruns the suite.

Use Actions caches for Cargo, Gradle, browser downloads, simulator build data,
and other rebuildable intermediates. Cache contents are not signed evidence and
must never be the sole pass record.

### 8.4 Every selected job still reports on every PR SHA

Every selected job launches and either:

- runs the suite and publishes fresh evidence; or
- verifies prior evidence and writes the source run, source job, fingerprint,
  and exact reuse reason to the job summary.

The job concludes successfully in both cases, so branch protection receives a
result for the new SHA. Never omit a required workflow through path filters;
GitHub documents that such checks can remain pending.

### 8.5 T4 reuse requires the environment, not just its label

Evidence is never reused for:

- T0 preflight and scope resolution;
- dependency and security audits;
- tests marked nondeterministic or under flake investigation;
- a check whose workflow, command, runner, toolchain, test, or fixture changed;
- tag/release publication evidence;
- scheduled exhaustive checks;
- merge-group checks unless the exact merge-group tree has prior evidence.

T4 environment evidence is reusable only when the fingerprint contains the
hosted runner's `ImageVersion` plus the exact relevant environment inventory:
Xcode path, SDK and simulator runtimes for Apple · browser version for layout ·
image digest and runtime version for containers. If any value is unavailable,
T4 runs. The T1 Apple assertion in §6.3 checks the pinned Xcode and runtimes
before a simulator boots. Unreused `main` and nightly runs bound the exposure
and refresh evidence whenever hosted images change.

If a merge queue is enabled, add the `merge_group` trigger. GitHub requires it
for required Actions checks in a queue; otherwise the queue waits for a status
that never arrives. See [Managing a merge
queue](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/configuring-pull-request-merges/managing-a-merge-queue).

### 8.6 Rebase acceptance matrix

| Rebase or evidence state | Expected action |
|---|---|
| Base changed docs unrelated to selected suites | T0 reruns; selected deterministic suites may reuse evidence |
| Base changed `plurxd` while PR changes Apple only | Provider closure invalidates affected consumers; unrelated suites may reuse |
| Toolchain, workflow, command, test, or fixture changed | Every dependent fingerprint changes |
| PR changed only commit messages | T0 reruns; deterministic suite fingerprints remain stable |
| Prior suite job failed or was cancelled | That suite runs |
| Prior run was cancelled after suite X concluded `success` | X may reuse; unfinished and unsuccessful jobs run |
| Evidence lookup or local merge-tree construction fails | The affected suite runs |
| Fork PR has only fork-authored evidence | No reuse; trusted matching `main` evidence remains eligible |
| T4 hosted image or runtime inventory changed | T4 fingerprint changes and the suite runs |

Implementation tests must build temporary Git graphs for each row and prove the
fingerprints, not mock the desired booleans. Shadow mode must then compare every
candidate reuse with the fresh result before enforcement can land.

## 9. Milestone 5 — order from telemetry and make flakes expensive to own

### 9.1 Publish one timing and outcome record per check

Extend `target/validation/report.json` and JUnit output with:

- queue, setup, compile, and test durations;
- selected point and changed-path reason;
- executed, reused, skipped, failed, cancelled, timed-out, or shadow-match
  outcome;
- typed scope outcome: normal or `fail-open:no-base`, `fail-open:diff-error`,
  `fail-open:unknown-path`, or `fail-open:catalog-error`;
- test count and failed test names;
- retry count, if a diagnostic retry was requested;
- content fingerprint and originating run for reused evidence.

Retain rolling 30-day aggregates for median and p95 duration, failure rate,
cancelled runner-minutes, and failure time. Raw logs may expire; the summary
must remain small enough to compare month over month. Count fail-open outcomes
weekly and alert on any sustained nonzero value: fail-open protects evidence,
but it is still a scheduler defect.

### 9.2 Rank canaries by expected wasted time

Use this score within each affected surface, with a Beta(1,1) prior so a check
with no failures in a small window does not receive a permanent zero:

```text
smoothed failure probability = (failures + 1) / (eligible runs + 2)

canary score = smoothed failure probability × downstream runner-minutes avoided
               ÷ canary duration
```

High score runs first. Low score runs in parallel after T0. Recompute monthly;
the order should change when the codebase changes. Keep the current hand-ranked
order as the tie-breaker until each check has at least 30 eligible executions.

The current first canaries are mobile versioning, validation/workflow
contracts, web structural drift for web changes, and focused shared Apple model
expectations for Apple changes. The full five-minute browser sweep should be
split so static/golden eligibility checks precede rendering, while capture
work remains parallel with other same-surface integration tests after T0.

### 9.3 Flake policy

1. A failing deterministic test fails the job immediately.
2. CI may rerun the exact test once for diagnosis, but the job remains failed
   and records `flaky-confirmed` when the retry passes.
3. A flaky test gets an owner, issue, first-seen date, and 14-day repair target.
4. Quarantine is allowed only in nightly with an explicit expiry. It never
   removes the test from the affected PR surface without replacement evidence.
5. Three flaky observations in 30 days block new tests in that suite until the
   owner restores determinism or splits the environment dependency out.

Silence is not stability. A retry that erases the first failure only makes the
next engineer pay for it again.

## 10. Delivery milestones — each change has a measurable exit

### 10.1 Milestone 1: policy first and docs-only

**Files:** `.github/workflows/{ci,lint}.yml` · `validation/ci_scope.py` ·
`tests/{validation,operations}/**` · this plan.

**Exit:** §5.4 passes, the literal PR #67 file set takes the docs lane, and a
docs-only PR completes without Rust compilation.

### 10.2 Baseline: record cost before changing suite boundaries

**Files:** new `scripts/ci-usage-report` · committed
`validation/ci-baseline.json` · this plan.

The script uses the authenticated GitHub Actions API to sweep run and job
durations, conclusions, attempts, and changed base/head pairs:

```bash
scripts/ci-usage-report \
  --workflow ci.yml \
  --runs 100 \
  --output validation/ci-baseline.json
```

**Exit:** the committed record names the query window and source repository and
contains wall time, runner-minutes, cancellations, failure time, and candidate
rebase reruns. Every percentage target in §11 can be recomputed from it.

### 10.3 Milestone 2: named Rust and static suites

**Files:** `Makefile` · `validation/points.toml` · `validation/runner.py` · CI
workflows · validation and operations docs.

**Exit:** every selected Rust test executes once; Clippy has one authoritative
PR job; focused Cargo targets pass the inventory meta-test; every job has an
explicit timeout; `CI_FORCE_FULL`, `ci:full`, and `ci:no-reuse` pass their
conservative-path tests; `make rust-check` remains the exhaustive local command.

### 10.4 Milestone 3: complete invalidation and docs contract

**Files:** catalog schema/parser · catalog data · docs checker · table-driven
scope tests · networked nightly external-link sweep.

**Exit:** 100% audited paths have a point and every reuse-eligible check has a
verified complete input closure; mixed diffs retain docs evidence; external
link failures are reported nightly without making upstream outages block PRs.

### 10.5 Milestone 4: shadow fingerprints, then evidence reuse

**Files:** fingerprint/evidence modules · workflow artifact steps · temporary
Git-graph integration tests · operations contracts.

**Exit A — shadow:** the §8.6 matrix passes and at least two weeks/100 PR runs
show the candidate hit rate, avoided runner-minutes, and zero stale matches;
every suite still runs.

**Exit B — enforcement:** only if §8.1's threshold is met, every reuse names a
successful originating job and trusted source; any uncertainty reruns the
suite. If the threshold is not met, Milestone 4 ends after Exit A.

### 10.6 Milestone 5: telemetry, canaries, and flake ownership

**Files:** validation report schema · JUnit writer · CI summaries · operations
guide.

**Exit:** 30 days of outcomes can rank checks by failure time and wasted
runner-minutes; no flaky retry can turn a failed first attempt green.

## 11. Service levels — know when the overhaul worked

| Measure | Target after Milestone 3 | Target after Milestone 5 |
|---|---:|---:|
| Docs-only PR wall time | <60 seconds p50 · <2 minutes p95 | same |
| Deterministic policy failure | <60 seconds p95 | <45 seconds p95 |
| First affected unit result | <3 minutes p50 | <2 minutes p50 |
| Rebase with unchanged suite inputs | normal rerun | <2 minutes p95 with verified reuse |
| PR runner-minutes | 30% below 2026-08-05 baseline | 50% below baseline |
| Required checks left pending by scoping | 0 | 0 |
| Governed changed paths with no point | 0 | 0 |
| Fail-open scope resolutions | ~0 per week · alert on every occurrence | same |
| Shadow/reuse stale matches | 0 | 0 |
| Failed-first-attempt jobs hidden by retry | 0 | 0 |

Run `scripts/ci-usage-report` and commit `validation/ci-baseline.json` before
Milestone 2 lands. Optimization without a before number is a story;
optimization with failure time and runner-minutes is an engineering result.
