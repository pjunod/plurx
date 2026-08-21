# Validation — make cascading change impact explicit

Companion to [FEATURES.md](FEATURES.md) (what plurx does) and
[ARCHITECTURE.md](ARCHITECTURE.md) (how it is built) — this is how a behavior
becomes a named contract with an automatic test obligation.

A functionality point is not a test file or a coverage percentage. It is one
user-visible promise, the paths capable of changing that promise, and the
checks that provide evidence for it. The catalog lives in
[`validation/points.toml`](../validation/points.toml); the runner is
[`scripts/validate`](../scripts/validate).

## Start here — the normal development loop

The validator is a coordinator around the checks the repository already has.
It works out which user-visible promises a change could affect, runs the
relevant checks, and records why each check ran.

```bash
make validate-help     # show the short version of this workflow
make validate-plan     # explain what the staged change selects; run nothing
make validate-staged   # validate the staged change before committing
make validate          # validate every point with the normal local profile
make validate-full     # include browser, client, and packaging checks
make validate-nightly  # exhaustive playback, recovery, bounds, and packaging
```

Use `make validate-staged` during ordinary work. Use `make validate` when the
working tree is complicated or path selection itself is in doubt. Use
`make validate-full` before a risky merge or release; it can take longer and
some checks need browsers, simulators, Docker, or client toolchains.

`make check` is the mandatory portable repository baseline: catalog lint,
historical regression coverage, operations source contracts, Rust formatting,
clippy, and the Rust test suite. Point-aware validation always includes the
catalog, history audit, and Rust gate, then adds checks for affected surfaces
that Rust cannot see, such as embedded JavaScript, browser structure, Android
code, or release packaging.

## The contract — promise, impact, evidence

Every point has six pieces:

| Field | Meaning |
|---|---|
| `id` | Stable name used in plans and reports, such as `playback.pipeline` |
| `title` | Short operator-facing name |
| `contract` | The behavior that must remain true |
| `paths` | Repository globs that can affect the behavior |
| `checks` | Commands that produce evidence for the contract |
| `depends_on` | Provider contracts whose changes can cascade into this consumer |

One file may affect several points. Impact walks **from provider to consumer**:
a change to the core media model selects the server and every client that
consumes it. A change confined to the embedded web player selects
`web.experience`, but does not rerun upstream database and server contracts as
though the browser supplied them. Shared checks run once even when several
selected points ask for the same gate.

```text
 changed paths
      │
      ▼
 direct point matches ──▶ downstream consumer closure
      │                         │
      └────────────┬────────────┘
                   ▼
       checks allowed by profile
                   │
                   ▼
        deduplicate ──▶ execute ──▶ JSON + JUnit + logs
```

Path selection never suppresses the baseline. `catalog-contract`,
`history-regressions`, and the Rust gate are `always_checks`, so every
commit-profile or CI-profile run still validates the catalog and history and
executes the workspace suite. Impact selection only adds checks; a bad path
mapping cannot quietly make ordinary tests disappear.

The Rust gate has two forms that must never drift apart in coverage, only in
packaging. Local profiles (`commit`, `full`, `nightly`) run `rust-gate` —
`make rust-check`, the one-command fmt + clippy + full-workspace suite. The
`ci` profile runs `rust-gate-ci` — `make ci-rust-gate` — which drops exactly
the two pieces that already run as their own CI jobs on the same commit:
clippy is lint.yml's entire job, and the replicated-store member is the
`cluster_auth` job's entire job. Excluding `plurx-cluster-check` also keeps
cargo's feature unification from compiling the hiqlite stack into the PR
gate, which makes the CI suite test `plurx-core` with the features the
shipped `plurxd` actually resolves. The subset re-runs (`api-wire`,
`security-boundaries`, `user-journey`) stay out of the `ci` profile for the
same reason: there they would re-execute binaries the workspace run already
executed with identical feature resolution.

## Profiles — fast by default, deep when the environment can prove more

| Profile | Intended use | Additional evidence |
|---|---|---|
| `commit` | Pre-commit and ordinary local work | Mandatory Rust/catalog baseline; shared API wire check; web syntax, contrast, golden, and accessibility when affected |
| `ci` | Pull requests, the merge queue, and `main` | PR runs scope to the diff — client suites run only when a diff can reach their compiled sources; `merge_group` and push events enable every surface, so the full cross-surface fan-out always sits between a green PR and `main` |
| `full` | Before a risky merge or release | Browser playback; both native-client suites; Android device tests when an explicit disposable device is selected; container startup/restart |
| `nightly` | Scheduled deep regression search | Exhaustive playback and restart matrix; interrupted-production recovery; resource bounds; all runnable full checks; a gating 15-minute PGS parser fuzz campaign; report-only mutation sampling over Rust files changed in the last seven days |

The `commit` profile permits explicitly optional checks to skip when a laptop
lacks their tooling. The skip is printed and recorded; it is not reported as a
pass. `--strict` turns missing tools or files into failures for checks selected
on that platform. A platform mismatch remains a skip because Linux cannot run
XCTest, regardless of strictness.

CI fetches full Git history and selects from the pull-request base. The
fast policy preflight runs mobile release hygiene first when applicable, then
the history audit, catalog and validation unit tests, and operations contracts.

Mobile release hygiene reads two different refs, and the distinction is
load-bearing. `PLURX_VALIDATION_BASE` is the recorded pull-request base sha and
scopes *which* release inputs the branch touched; it is the branch point.
`PLURX_VALIDATION_MERGE_TARGET` is `origin/<base ref>` re-fetched when the job
runs, and supplies the counters the branch has to clear, because that is what
the branch actually merges into. They name the same commit only until the base
moves. Scope is the whole of scope: both the changed paths and the
workspace-version comparison that marks a release are read against the recorded
base, and only the two counters are read against the target. That split is
load-bearing in both directions. Reading the workspace comparison off the target
would conflate "this branch shipped a release" with "a release landed on the
target", so every branch open across a release — including an ordinary
dependency-only `Cargo.toml` edit, which is in this point's paths — would go red
and be told to bump two store counters it never touched. Reading the counters
off the branch point is the original defect. Two branches that bump a build
counter to the same value auto-merge with
no conflict marker and produce no `BEHIND` signal in this job, so a branch
measured only against its branch point stays green forever once an unrelated
release bump lands that same counter on the base. Re-baselining means a pull
request can go red without its own head moving; that is correct, and the
failure names the base ref and both counters so the fix is unambiguous without
reading the workflow. An unreadable merge target fails the check rather than
falling back to the branch point: scope selection fails open because a bad diff
base only costs time, but a missing counter baseline would report green on a
tree that cannot ship. A local `--changed-from` run passes one ref and uses it
for both roles, which is right for a branch measured against a fixed point.
Every expensive fan-out job in the main CI workflow waits for that preflight.
A documentation-only pull request stops after those executable documentation
contracts; it does not compile the Rust workspace or provision browsers and
native-client environments. A documentation change may include
`validation/regressions.d/**` and keep this lane because those entries are
consumed only by the history audit that preflight already runs; selector code and
`validation/points.toml` still take the executable-change lane.

For executable changes, the portable Rust and focused Linux contracts remain
one baseline job. Browser layout, Android JVM, Apple simulator, Android device,
release-build, and container checks run as parallel jobs only when the diff can
affect their contracts. Coverage runs after merge on `main`, where its badge is
published; a pull request does not rerun the Rust suite merely to discard the
number. [CI_TEST_OVERHAUL_PLAN.md](CI_TEST_OVERHAUL_PLAN.md) records the
measured failure history and the remaining suite-splitting, invalidation,
rebase-evidence, and telemetry milestones.

The impact graph selects browser and native unit suites. Explicit path owners
select the narrower environment checks: Android application and build files
justify an emulator; Rust and toolchain files justify cross-target release
builds; server, image, Compose, and lifecycle files justify the container
smoke test. A server contract change can still fan out to every consuming
client without pretending that server-only code changed Android focus
behavior.

`PR validation gate` waits for every selected job and accepts an unselected
job only when GitHub records it as skipped. Configure that aggregate as the
required branch-protection check; individual jobs remain visible evidence but
do not make an unrelated surface part of every merge. Pushes and tags enable
all surfaces, and an absent or invalid pull-request base also enables all
jobs. Impact optimization therefore fails open: a bad diff base costs time;
it never suppresses tests. The scheduled workflow still runs the `nightly`
profile.

### Which ffmpeg the profiles assume

Every CI profile runs **ffmpeg 6**, from the pinned `ubuntu-24.04` runner image.
`.github/actions/ffmpeg` is the single place that installs it; it prints the
build that actually resolved into the job log and step summary, and fails the
job when the major is not the one that lane named. So `runs-on` and the expected
major move together in a reviewable diff, and neither can move on its own.

This is a deliberate choice rather than an inherited default, because the two do
not agree. **ffmpeg 8 declares `-readrate_initial_burst` and then ignores it**,
which costs the copy path its startup burst — the burst-then-hold behaviour
[PLAYBACK.md](PLAYBACK.md) promises — with no warning, because the capability
probe sees the option advertised. That is [#380](https://github.com/pjunod/plurx/issues/380),
and the plurxd side of it is #386. Before this pin, no CI job had ever run
ffmpeg 8; the gate was green by accident of whichever image `ubuntu-latest`
resolved to that week, and a promotion past 24.04 would have turned `main` red
in one silent step with no diff to blame.

So "green in CI" and "green on a worker" currently mean different things, and
this is the difference: a worker host on Ubuntu 26.04 runs ffmpeg 8.0.1, where
`make validate` fails on the pacing assumptions in
`crates/plurxd/src/transcode.rs` until #386 lands. The nightly `ffmpeg8-pacing`
job is where that gap is watched — it runs the same capability contract as the
`playback-recovery` point against a real ffmpeg 8, pinned by the `ubuntu:26.04`
container tag. It is nightly rather than required on purpose: making the gate a
matrix over both majors before #386 exists would leave a required check red by
design. `tests/operations/test_contracts.py` enforces the whole arrangement —
no job may install ffmpeg outside the action, name a major without pinning the
image or container that supplies it, or drop either major's coverage.

### Base syncs — the gate revalidates, the reviewer does not

`main` is protected with `required_status_checks.strict: true`, so a pull
request has to be up to date before it merges. Clear that by merging `main`
**into** the branch. Never rebase a branch that has a recorded approval: a merge
keeps the approved commit an ancestor, while a rebase rewrites every sha and
sends the whole reviewed range back through review.

That base sync is revalidated but not re-reviewed. `PR validation gate` reruns
at the new head — which is the point of `strict`, and is load-bearing here
rather than ceremonial, because the required profile is itself versioned in
`main`: this catalog and `validation/points.toml` can add a check to the `ci`
profile after a branch was cut, so a byte-identical patch can legitimately face
a check set that did not exist when it was approved. The independent approval,
by contrast, is carried forward across a base sync Git proves empty — two
parents, the second already on `main`, and an automatic merge whose tree is
exactly what `git merge-tree` computes from the two parents. Anything Git cannot
prove empty is reviewable content and takes a delta pass. The rule and its
refusals live in the merge gate itself; see SwarmDeck `docs/OPERATIONS.md`.

## Load-sensitive cluster checks — a timeout is not a verdict

One check drives real replicated infrastructure rather than a library, so the
host it runs on is part of the experiment:

| Check | Point | What the host can change |
|---|---|---|
| `cluster-auth` (`make cluster-check`) | `cluster.auth` · `persistence.upgrades` | Three voters run as separate processes and every call carries a three-second per-operation deadline (`STORE_TIMEOUT` in `crates/plurx-core/src/store/hiqlite.rs`). Under a full `make validate` those voters compete with every other check for the same cores, and that deadline is reachable by scheduling pressure alone |

**What a timeout there means.** `Database("replicated store operation timed out")`
is the host reporting that it could not finish an operation in three seconds.
It is not durable-state evidence in either direction: nothing was proved and
nothing was found broken. The production deadline stays at three seconds
because it is a server safety bound, so the suite absorbs load by re-attempting
a deadlined step from a reset target instead of by relaxing it.

**Why this check reaches that deadline before the others do.** The import
contract and the production bound push against each other by design. The
byte-budget transaction builder (#282) sizes every transaction as close to the
WAL payload capacity as it can, because a transaction that stays comfortably
small would not prove the bound it exists to prove. Maximising the payload also
maximises how long one replicated operation takes, so this check spends most of
its time on operations deliberately sized to sit near the three-second ceiling.
That is not only a test-harness concern: a real library import on a busy server
runs the same builder against the same fixed bound, so an operator seeing this
timeout in production is seeing the same interaction, not a different bug.

**How to tell it from a real regression.** The failures look different at the
client, and the check now says which of these three it saw:

- A **replicated deadline** names itself, states that the bound was neither
  proved nor violated, and points back at this section. Rerun `make
  cluster-check` alone on an idle machine; it takes about ten seconds.
- A **durable-state or size violation** carries the contract's own verdict.
  An oversized Raft transaction, for example, is refused by `hiqlite-wal` in
  the leader (`` `data` length must not exceed `wal_size` ``) and reaches the
  client as `ClientWriteError: panicked` — a byte comparison that reports
  identically on an idle and a saturated host.
- A deadline whose voter then **fails a consistent readiness read** is treated
  as the violation, not as load: a busy voter still answers that probe, while
  a leader killed by an oversized transaction does not.

Never re-diagnose a red `cluster-auth` from elapsed time. Read which of those
three the failure text claims, and reproduce it in isolation before treating it
as a durable-state regression.

### A busy port is not an un-migrated store

A voter's raft and API ports are chosen by binding port zero, reading the
port, and releasing the listener. Nothing holds them: the process that binds
them for real is a child started afterwards, and hiqlite binds its own sockets
from an address string, so there is no listener to hand over. Under
gate-parallel load another process can claim one of those ports in between,
and that is an environment fact rather than durable-state evidence.

It used not to read as one. hiqlite serves both listeners from detached
`tokio::spawn` tasks that `.unwrap()` the serve future
(`hiqlite-0.14.0/src/start.rs:148` and `:231`), so a losing bind panicked a
background task and nothing else: `start_node` had already returned `Ok`,
`wait_until_healthy_db` probes only the *local* database, and the voter
announced readiness with a dead listener. The collision then surfaced as
whatever the crippled voter failed at next:

| Port lost | What the gate used to print |
|---|---|
| raft | `no such table: cluster_meta` — a linearizable read reaching a state machine that never applied the schema batch, which is exactly what a genuinely un-migrated store looks like |
| API | `replicated store operation timed out`, then `auth store has not been opened` |

Both of those are contract verdicts, and the first is a serious one. Neither
was true. A voter now proves both of its listeners accept before it announces
readiness, and a bind failure is reported as `port collision: …` and stops the
voter, so a busy port cannot go on to be reported as durable-state damage.
Cluster starts reallocate their ports up to five times on that classification
and on nothing else — see `is_port_collision` and `with_port_retry` in
`crates/plurx-cluster-check/src/lib.rs`.

**How to tell them apart.** Read the verdict, never the elapsed time. A `port
collision:` failure names the loopback address that collided and means nothing
was proved; rerun the check alone. Anything else is the contract's own answer
and is reproducible on an idle host. Note that the API-port case above shares
its `replicated store operation timed out` text with an ordinary replicated
deadline — the difference is that a collision now says so first, so a timeout
arriving on its own is still the deadline it has always been.

### PortReservation — the listener is held until the voter starts

The residual race between `allocate_nodes` (which binds, reads the port, and
releases the listener) and the child process binding the same port from its
address string cannot be eliminated on a general OS: the child is a separate
process, and there is no mechanism to hand a listening socket across
`Command::spawn`. The race is made as short as possible in two ways:

1. **`PortReservation`** (`crates/plurx-cluster-check/src/lib.rs`, added for
   issue #381). `allocate_nodes` now returns a `PortReservation` whose
   listeners are held alive until the caller consumes it. The site that starts
   the child process — `ClusterProcesses::start` — holds the reservation
   through the `NodeSpec` construction and drops it immediately before
   spawning, so the window between "port released" and "child binds" is the
   narrowest possible sequence of `drop` + `Command::spawn`.
2. **`start_cluster_with_port_retry`** wraps every start in `with_port_retry`,
   which retries the entire allocation up to five times on `is_port_collision`
   and on nothing else. A collision on a transiently occupied port is
   self-healing; a collision on a permanently held port fails with a message
   naming the collision, never a durable-state verdict.

The two mechanisms are complementary: `PortReservation` closes the window as
far as the OS allows, and the retry loop answers any collision that still
manages to slip through before the child binds.

### Previous instances of the same defect shape

This is the fourth issue whose root cause is a check that cannot tell its
environment from the contract it asserts:

- #315: benchmark wall clock reported as a performance regression.
- #368: replicated deadline reported as a WAL-size violation.
- #374: data-directory lock reported as a second daemon.
- #381: port collision reported as an un-migrated store (this issue).

## The UI golden — a saved answer key, not a magic test

“Golden” is testing jargon for a reviewed, known-good output saved in the
repository. The test makes the same output from the current application and
compares the two:

```text
known-good UI structure ──▶ tests/ui-structure.golden
current UI structure    ──▶ target/ui-baseline/structure.txt
                                      │
                    same ── pass ◀────┴────▶ different ── fail + readable diff
```

In plurx, the committed golden is **not a screenshot**. For every supported
layout, route, and viewport, it records portable structural facts:

- which DOM shapes and accessibility states exist, and how many;
- the keyboard tab order;
- the API method and path calls made while rendering; and
- the registered layouts and the surfaces on which they are allowed.

The same browser pass also fails on deterministic accessibility defects:
duplicate IDs, broken ARIA references, unnamed interactive controls, and
images with no `alt` contract. Those rules are direct assertions, not saved in
the golden; the golden preserves the larger structure and keyboard/request
behavior after those assertions pass.

It deliberately excludes pixels, timestamps, local paths, and arbitrary page
text. Pixel rendering changes with the browser, fonts, and machine, so
screenshots and their hashes stay under `target/ui-baseline/` for optional
same-machine before/after comparison. They are never the committed golden.

The golden is needed because the web app is generated in the browser from one
large embedded HTML/JavaScript file. Rust can compile and pass its tests while
a class, `aria-disabled`, tab stop, route request, or entire JavaScript-rendered
screen has changed. The saved answer key gives those browser-visible contracts
something automatic to compare against. It is not required by the validation
framework in general; it is evidence for the `web.experience` functionality
point specifically.

There are two legitimate outcomes when `make ui-check` reports drift:

| What happened | What to do |
|---|---|
| The UI change was accidental | Fix the UI and rerun `make ui-check` |
| The UI change was intentional | Run `make ui-golden`, inspect the golden diff, then commit it with the UI change |

Never regenerate the golden merely to make a failure disappear. Rewriting the
expected answer without reviewing the diff turns the check into an automatic
approval of whatever the application did.

The repository contains the activated `tests/ui-structure.golden`: 54 captures
across every registered layout, route, and desktop/mobile viewport. A check
rebuilds `plurxd` first so it cannot accidentally serve an old embedded web
app, then runs a real browser sweep. It needs Python Playwright with Chromium,
`ffmpeg`, and a buildable `plurxd`; expect it to take minutes rather than
seconds. The generated player media includes a synthetic audio track so the
browser exercises a real media timeline, but validation browsers always launch
with host output muted and operating-system media-key integration disabled.
Audio is still decoded and selectable; the gate neither sounds the workstation
speakers nor claims macOS Now Playing.

```bash
make ui-golden          # capture the current reviewed UI as the answer key
git diff -- tests/ui-structure.golden
make ui-check           # prove a fresh capture matches it
```

`make ui-golden` is only for an intentional reviewed UI change. Ordinary local
and CI runs use `make ui-check`; they never rewrite their own expected answer.

## Run it — plan first when the impact is surprising

```bash
scripts/validate list                                      # inventory every point
scripts/validate lint                                      # schema + path coverage
scripts/validate plan --profile commit --staged            # explain a staged change
scripts/validate run --profile commit --staged             # what the hook runs
scripts/validate run --profile full --point playback.pipeline
make validate-help                                         # Makefile-oriented guide
make validate-plan                                         # staged plan, no checks
make validate-staged                                       # staged commit profile
make validate                                              # commit profile, every point
make validate-full                                         # every runnable deep check
make validate-nightly                                      # exhaustive scheduled tier
make history-check                                         # every past fix has evidence
make operations-check                                      # deploy/CI/ship contracts
```

**How to read the plan:** `because path:...` is a direct match;
`because consumer:...` was pulled downstream from a changed provider. “No check in
this profile” means the contract is known but its evidence belongs to a deeper
or platform-specific profile. It does not mean the point passed.

**How to read the result:** `passed` means the command returned zero · `failed`
means the command or a required prerequisite failed · `skipped` names an
unavailable tool, file, platform, or explicit skip variable. Point status in
`report.json` is `passed` · `failed` · `partial` · `not-covered` · `not-run`.
Only executed evidence earns `passed`.

Install the pre-commit hook once:

```bash
make hooks        # copies scripts/pre-commit into .git/hooks/pre-commit
```

The hook uses the staged diff, not unstaged experiments beside it. It still
runs the mandatory baseline on every commit, then adds any point-specific
commit checks.

## Behavior fixes — prove the test distinguishes the correction

A corrective pull request should leave behind at least one test that passes on
the pull request and fails when only the production correction is restored from
the base tree. `scripts/prove-fix` performs both runs inside a disposable local
clone; it never rewrites the checkout in which it was invoked.

```bash
# Rust: filter one retained test and restore one or more production files.
scripts/prove-fix origin/main repaired_case crates/plurxd/src/example.rs

# Swift: use the same protocol on macOS with the shared simulator suite.
scripts/prove-fix --command 'make apple-test' origin/main - \
  clients/apple/Sources/PlayerView.swift
```

Both proof runs compile into an isolated cargo target directory, never the one
an ordinary `cargo` or `make validate` invocation uses. This is mandatory, not a
convenience: the second run compiles reverted source, and cargo's freshness
check is mtime-based, so a shared target directory leaves the pre-fix artifact
newer than the corrected checkout and the next gate silently reuses it. The
isolated directory is a sibling of the inherited `CARGO_TARGET_DIR`
(`<dir>-prove-fix`), so proofs still reuse each other's dependency builds and
pay one cold workspace build rather than one per invocation; with no inherited
value it is a directory inside the disposable clone, matching what cargo would
have done anyway. `tests/validation/test_prove_fix.py` asserts the isolation so
a refactor cannot quietly reintroduce the sharing.

The first run must be green. The second run must be red after the named
production paths are taken from the base revision. A test that stays green is
evidence that the patch touched a test, not that the test protects the fix.
Pull requests may opt into the first-month report-only CI job with the
`fixes-behavior` label. It applies the protocol to changed Rust production
files and writes the result to the job summary; Swift uses the documented
manual command because Linux cannot run XCTest.

Corrective client commits also add or update a row in
[`tests/client-fixes.toml`](../tests/client-fixes.toml). Each row binds the
commit to a current production symbol and a current test symbol. Removing or
renaming either anchor fails the validation inventory. This is deliberately
stronger than accepting any test edit in the same commit: an unrelated test
cannot satisfy the post-review history policy.

The scheduled workflow adds a second, exploratory layer. `cargo-mutants` is
limited to Rust files changed in the previous seven days, gives each mutation
300 seconds, and has a 60-minute job cap. Its report is uploaded from
`target/mutants`; surviving mutants are summarized but do not fail the nightly
workflow during the first signal-gathering month.

## Historical fixes — every scar names the check that guards it

Path ownership answers which promise a change can affect; it does not prove
that the checks reproduce bugs already encountered. `make history-check`
closes that gap by walking every non-merge corrective commit reachable from
`HEAD`—including conventional `fix` commits, plain-English correction verbs,
compatibility corrections, and `perf` fixes—and requiring one of two forms of
evidence:

- for older commits, the fixing commit added or changed a surviving test or assertion;
- for client corrections after the review baseline, a client-fix anchor names
  the production-to-test relationship;
- for other runtime corrections after that baseline, an explicit regression
  mapping or anchor names the current evidence; or
- a fragment in [`validation/regressions.d/`](../validation/regressions.d/)
  explicitly maps the commit to a current functionality point and runnable
  check.

The explicit mapping is for checks whose evidence lives outside the fixing
patch: both Apple platforms compiling, a real container completing its
non-root lifecycle, the browser playback matrix, or the UI structural sweep.
It is not an exemption. Unknown commits, checks, and points fail; duplicate
claims fail; and a new corrective commit with neither a test nor a mapping
fails the baseline.

### One mapping, one file

The mapping ledger is a directory, not a file. Each entry lives in its own
`validation/regressions.d/<first-commit-prefix>-<slug>.toml`, holds exactly one
`[[coverage]]` table, and repeats `version = 1`. The audit loads every `*.toml`
there in file-name order and treats them as one ledger; entry order carries no
meaning.

```toml
# validation/regressions.d/a1b2c3d4-playback-pipeline.toml
version = 1

[[coverage]]
commits = ["a1b2c3d4"]
points = ["playback.pipeline"]
checks = ["rust-gate"]
reason = "One sentence naming the current check that exercises the defect."
```

The file name is not cosmetic. The audit rejects a fragment whose name does not
begin with its own first mapped commit, so two corrective changes can never
choose the same path. That is the whole point of the directory: a single
append-only `validation/regressions.toml` put every new entry at the same
offset, so each merge to `main` conflicted every other open pull request
carrying a mapping — and clearing that conflict with a rebase rewrote the SHAs a
reviewer had pinned an approval to, spending a review cycle on nothing. Adding a
file collides with nothing. The loader refuses to run while a shared
`validation/regressions.toml` exists, so the hotspot cannot come back.

[`validation/regressions.d/README.md`](../validation/regressions.d/README.md)
carries the field-by-field format next to the entries themselves.

```bash
make history-check
# history ok: 288 corrective commits · 213 direct test changes ·
#             75 explicit current-check mappings · 4 client-fix anchors ·
#             9 non-runtime corrections
```

The exact counts grow with the repository. Read the result as a coverage audit:
every corrective commit must have direct test evidence, an explicit executable
check mapping, a client-fix anchor, or a reason proving that it changed only
documentation, comments, or ignored generated state. The evidence counts cover the number
audited. The machine-readable inventory is written to
`target/validation/history.json`, including the subject, functionality points,
and coverage route for every commit.

Operations get a second, focused layer because several real failures were
valid shell/YAML that pointed at the wrong target. `make operations-check`
pins the Compose project and state mount, configurable HTTP/discovery ports,
discovery networking, build stamp, real Apple/Android ship targets, concrete
CI simulators, container port/state/cleanup behavior, and copy-video reporting.

The same operations gate extracts current mobile build claims from the Apple
and Android READMEs, the Apple parity document, and the status page. Those
claims must match `CURRENT_PROJECT_VERSION` and `versionCode`; advancing a
client build without its user-facing status documents is red before a PR opens.

The READMEs and the parity document each carry exactly one anchored `> Status:`
line, so the gate reads a single declared current claim from them. `STATUS.html`
has no such anchor — it is prose, and every Apple build number in it is treated
as a current claim. That page also has to narrate history, and a sentence like
"build 52 corrected the reported iPad mini failure" stays true after the next
bump while a regex cannot see its tense. Mark such a mention explicitly:

```html
<span data-build-history>Apple build 52</span> corrects the reported iPad mini failure
```

Marking is the exception, never the default. An unmarked mention is still swept,
so a status page that falls behind a build bump is red; the only way to exempt
one is to write the marker around it, which is a reviewable edit. Use it only
for mentions that are genuinely about a past build — rewording a true sentence
to dodge the sweep degrades the page for its readers.

The exemption is granted only by the marker itself: a bare `data-build-history`
attribute — or one with an empty value, which is how a formatter serializes a
boolean attribute — on a real `<span>` element that closes around its own
sentence. The gate parses the page rather than pattern-matching its text, so the
marker is recognized only at attribute-name position on a tag the HTML tokenizer
actually produces. A name match alone also fires on markup that is not the
marker, and these all read as ordinary mentions and stay swept:

```html
<span title="data-build-history">Apple build 52</span>   <!-- a value, not the marker -->
<span data-build-history-note>Apple build 52</span>      <!-- a different attribute -->
<span data-build-history="false">Apple build 52</span>   <!-- the marker takes no value -->
<span data-build-history/>Apple build 52</span>          <!-- malformed opening tag -->
```

Marker-shaped characters that are not an element grant nothing either. A comment,
another element's quoted attribute value, and `<script>` or `<style>` text can
all spell an opening or closing tag without being one; the build number between
them is still on the rendered page, so it is still a claim:

```html
<!-- <span data-build-history> --><p>Apple build 52</p><!-- </span> -->
<div title="<span data-build-history>">Apple build 52</div>
```

Everything else fails closed the same way and returns the mention to the sweep:
an unclosed marker span, the marker on any element other than `<span>`, and a
marker that drifts across a nested `<span>` and so no longer wraps its own
sentence. In the other direction, HTML's own insignificant variation is honored,
so a formatter cannot turn a correctly marked sentence red: attribute order,
attribute-name case, surrounding whitespace and line breaks, and a quoted
attribute value containing `>` all still match. `tests/operations/test_mobile_build_claims.py`
pins both lists.

Android needs no equivalent. Its build claim is read only from the anchored
`> Status:` line in `clients/android/README.md`, and no whole-file sweep runs
against Android numbers anywhere, so a historical Android mention cannot break
the gate. `STATUS.html`'s Android build numbers are consequently unvalidated —
the opposite gap, and out of scope here; adding that coverage would mean
adopting the same marker for the historical Android mentions the page already
carries.

## Add a functionality point — define the behavior before its command

Start with the promise and the code capable of violating it. Then attach the
smallest existing check that proves the promise. Add a new command only when
the current test layers cannot observe the behavior.

```toml
[[points]]
id = "search.results"
title = "Cross-library search"
contract = "A viewer sees authorized movies, series, and episodes ranked by a stable query."
paths = [
  "crates/plurxd/src/http/search.rs",
  "crates/plurx-core/src/store/sqlite/media.rs",
  "clients/**/Search*",
]
checks = ["rust-gate", "search-contract"]
depends_on = ["identity.access", "library.catalog", "server.api"]
```

Checks are declared once and reused:

```toml
[[checks]]
id = "search-contract"
title = "Search API contract cases"
command = "cargo test -p plurxd http::search::tests -- --nocapture"
profiles = ["commit", "ci", "full", "nightly"]
requires = ["cargo"]
missing = "fail"
timeout_seconds = 120
```

The supported path syntax is repository-relative `*` · `**` · `?` · brace
choices such as `{watch,trakt}.rs`. Keep globs narrow enough that the plan
explains a real cascade, but broad enough that a newly added file cannot evade
the contract.

Run these acceptance checks after editing the catalog:

```bash
scripts/validate lint                                      # no orphan source files
python3 -m unittest discover -s tests/validation -p 'test_*.py'
scripts/validate plan --profile ci --paths path/you/changed.rs
```

`lint` audits every governed tracked or untracked source file. Its coverage
target is exact: 100% of `settings.audit_paths` must match at least one point,
and 100% of points must name at least one valid check. This is catalog
coverage, not a claim that every behavior is exhaustively tested.

## Current inventory — what is actually validated

| Functionality point | Commit / CI evidence | Full-profile addition |
|---|---|---|
| `core.media` | Workspace Rust tests, formatting, and clippy | Scheduled resource-bound checks reach downstream consumers |
| `persistence.upgrades` | Focused historical migrations, reopen persistence, and refusal of future schemas | Same focused contract |
| `server.api` · `api.contract` | Seeded read/write journeys plus one canonical JSON fixture checked against live Rust responses and decoded by Swift and Kotlin | Native simulator suites exercise their decoders too |
| `library.catalog` · `metadata.enrichment` | Scan, browse, matching, retry, and user-owned metadata tests | Downstream browser/client surfaces run when a provider changes |
| `identity.access` | Focused anonymous/viewer/admin/scoped-key boundaries | Same contract across native client suites |
| `playback.pipeline` | Rust decision, delivery, stream, HLS, and session tests | Risk-weighted browser matrix; nightly exhaustive quality/restart and interruption recovery |
| `watch-state.sync` | Seeded progress/watched/settings journey plus store and Trakt tests | Same journey through deeper consumers |
| `offline.viewing` | Store/API contracts cover ownership, quotas, stable hashed leases, expiry, package media, and recovery; Swift and Kotlin suites cover client contracts | Native simulator/device transfer restoration, cache-only playback, and disconnected physical-device evidence |
| `plex.compatibility` | Plex discovery, metadata, media, playback, and timeline tests | Browser playback when shared delivery changes |
| `web.experience` | JavaScript syntax, theme contrast, 36-capture structural golden, keyboard/request invariants, and accessibility smoke | Risk-weighted shipped-player matrix; nightly exhaustive quality and restart cases |
| `apple.client` | Dedicated iOS and tvOS simulator CI, sharing one XCTest source | Local/full simulator run on macOS |
| `android.client` | JVM models/policies and lint plus dedicated instrumented Compose/TV-focus CI | Explicit disposable-device run for `full` |
| `operations.packaging` | Compose, CI, ship-target, port, state, build-stamp, and report contracts plus release builds | Image must start non-root, become healthy/ready, expose metrics, restart, keep its instance identity, and clean up |
| `validation.framework` | Catalog schema, historical-fix coverage, cycles, duplicate IDs, glob/audit coverage, staged renames, cascade selection, timeout/fail-fast, and report tests | Same contract |

The cross-client fixture is `tests/contracts/native-api.json`. It is not a
second server implementation: Rust compares representative keys and JSON types
with live handler responses, while Swift and Kotlin decode the same bytes with
their production models. Adding a field remains compatible; renaming a field
or changing its type fails at the consumer boundary where the drift matters.

Line coverage remains a diagnostic badge, not the acceptance target. A high
line percentage can execute code without asserting its behavior; the catalog
instead makes each important promise name the evidence meant to protect it.

## Evidence — enough detail to reproduce the failing layer

Every run writes under `target/validation/`:

| Artifact | What it answers |
|---|---|
| `report.json` | Which points were selected, why, their profile coverage, and each check result |
| `junit.xml` | CI-ingestible pass, failure, and skip cases |
| `logs/<check>.log` | Complete combined output for the check command |
| `history.json` | Every corrective commit and whether direct tests or an explicit current check cover it |

CI uploads that directory even when a check fails. Read `report.json` first to
find the affected promise, then the named log to diagnose the failing layer.

## Non-goals — what the framework does not pretend to prove

- **It does not infer behavior from code.** Humans define the contract and its
  path boundary; the audit only prevents governed files from having no owner.
- **It does not replace focused regression tests.** A point routes tests. The
  test itself should reproduce the bug and assert the intended result. The
  historical ledger is reserved for evidence that necessarily lives in a
  platform, browser, or runtime check outside the fixing patch.
- **It does not make hardware interchangeable.** Browser playback, simulators,
  and JVM tests cannot prove Dolby Vision on a physical Apple TV or TV remote
  focus on every Android device. Keep those device observations explicit.
- **It does not use line coverage as a release verdict.** Coverage locates dark
  code; functionality points say which promises matter when that code changes.
