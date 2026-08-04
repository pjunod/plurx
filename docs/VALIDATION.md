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

Path selection never suppresses the baseline. `catalog-contract` and
`history-regressions`, and `rust-gate` are `always_checks`, so every
commit-profile or CI-profile run still validates the catalog and history and
executes `make rust-check`. Impact selection only adds checks; a bad path
mapping cannot quietly make ordinary tests disappear.

## Profiles — fast by default, deep when the environment can prove more

| Profile | Intended use | Additional evidence |
|---|---|---|
| `commit` | Pre-commit and ordinary local work | Mandatory Rust/catalog baseline; shared API wire check; web syntax, contrast, golden, and accessibility when affected |
| `ci` | Pull requests and `main` | Impact-selected Linux contracts plus parallel browser, Apple, Android, build, and container jobs when their surfaces can change |
| `full` | Before a risky merge or release | Browser playback; both native-client suites; Android device tests when an explicit disposable device is selected; container startup/restart |
| `nightly` | Scheduled deep regression search | Exhaustive playback and restart matrix; interrupted-production recovery; resource bounds; all runnable full checks |

The `commit` profile permits explicitly optional checks to skip when a laptop
lacks their tooling. The skip is printed and recorded; it is not reported as a
pass. `--strict` turns missing tools or files into failures for checks selected
on that platform. A platform mismatch remains a skip because Linux cannot run
XCTest, regardless of strictness.

CI fetches full Git history and selects from the pull-request base. The
portable catalog, history, Rust, and focused Linux contracts remain one
baseline job. Browser layout, Android JVM, Apple simulator, Android device,
release-build, and container checks run as parallel jobs only when the diff can
affect their contracts. Coverage runs after merge on `main`, where its badge is
published; a pull request does not rerun the Rust suite merely to discard the
number.

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
seconds.

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

## Historical fixes — every scar names the check that guards it

Path ownership answers which promise a change can affect; it does not prove
that the checks reproduce bugs already encountered. `make history-check`
closes that gap by walking every non-merge corrective commit reachable from
`HEAD`—including conventional `fix` commits, plain-English correction verbs,
compatibility corrections, and `perf` fixes—and requiring one of two forms of
evidence:

- the fixing commit added or changed a surviving test or assertion; or
- [`validation/regressions.toml`](../validation/regressions.toml) explicitly
  maps the commit to a current functionality point and runnable check.

The explicit mapping is for checks whose evidence lives outside the fixing
patch: both Apple platforms compiling, a real container completing its
non-root lifecycle, the browser playback matrix, or the UI structural sweep.
It is not an exemption. Unknown commits, checks, and points fail; duplicate
claims fail; and a new corrective commit with neither a test nor a mapping
fails the baseline.

```bash
make history-check
# history ok: 234 corrective commits · 159 direct test changes ·
#             66 explicit current-check mappings · 9 non-runtime corrections
```

The exact counts grow with the repository. Read the result as a partition:
every corrective commit must have direct test evidence, an explicit executable
check mapping, or a reason proving that it changed only documentation,
comments, or ignored generated state. The three counts must sum to the number
audited. The machine-readable inventory is written to
`target/validation/history.json`, including the subject, functionality points,
and coverage route for every commit.

Operations get a second, focused layer because several real failures were
valid shell/YAML that pointed at the wrong target. `make operations-check`
pins the Compose project and state mount, configurable HTTP/discovery ports,
discovery networking, build stamp, real Apple/Android ship targets, concrete
CI simulators, container port/state/cleanup behavior, and copy-video reporting.

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
