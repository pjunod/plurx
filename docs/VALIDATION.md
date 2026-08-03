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
```

Use `make validate-staged` during ordinary work. Use `make validate` when the
working tree is complicated or path selection itself is in doubt. Use
`make validate-full` before a risky merge or release; it can take longer and
some checks need browsers, simulators, Docker, or client toolchains.

`make check` is still the mandatory portable baseline: catalog lint, Rust
formatting, clippy, and the Rust test suite. A validation run always includes
that baseline and then adds checks for affected surfaces that Rust cannot see,
such as embedded JavaScript, browser structure, or Android code.

## The contract — promise, impact, evidence

Every point has six pieces:

| Field | Meaning |
|---|---|
| `id` | Stable name used in plans and reports, such as `playback.pipeline` |
| `title` | Short operator-facing name |
| `contract` | The behavior that must remain true |
| `paths` | Repository globs that can affect the behavior |
| `checks` | Commands that produce evidence for the contract |
| `depends_on` | Other contracts that this behavior needs in order to work |

One file may affect several points. A change to the embedded web player, for
example, selects `web.experience` directly and pulls in its server, playback,
library, and core dependencies. Shared checks run once, even when six points
ask for the same Rust gate.

```text
 changed paths
      │
      ▼
 direct point matches ──▶ dependency closure
      │                         │
      └────────────┬────────────┘
                   ▼
       checks allowed by profile
                   │
                   ▼
        deduplicate ──▶ execute ──▶ JSON + JUnit + logs
```

Path selection never suppresses the baseline. `catalog-contract` and
`rust-gate` are `always_checks`, so every commit-profile or CI-profile run
still validates the catalog and executes `make check`. Impact selection only
adds checks; a bad path mapping cannot quietly make ordinary tests disappear.

## Profiles — fast by default, deep when the environment can prove more

| Profile | Intended use | Additional evidence |
|---|---|---|
| `commit` | Pre-commit and ordinary local work | Web syntax/contrast on web changes; structural layout when its golden and Playwright exist |
| `ci` | Linux gate selected from the event diff | Catalog contract · Rust gate · web syntax/contrast · Android unit/lint when affected |
| `full` | Before a risky merge or release | Browser playback · web layout · Apple simulators · Android unit/lint · container build |

The `commit` profile permits explicitly optional checks to skip when a laptop
lacks their tooling. The skip is printed and recorded; it is not reported as a
pass. `--strict` turns missing tools or files into failures for checks selected
on that platform. A platform mismatch remains a skip because Linux cannot run
XCTest, regardless of strictness.

CI fetches full Git history and selects from the pull-request base or previous
push. If that commit is absent or invalid, it runs every point. This makes the
optimization fail open: a bad diff base costs time; it never suppresses tests.

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

The repository does not currently contain `tests/ui-structure.golden`, so the
validator reports `web-layout` as **skipped: missing evidence**, not passed.
The intended one-time activation is below. It runs a real browser sweep and
therefore needs Python Playwright with Chromium, `ffmpeg`, and a buildable
`plurxd`; expect it to take minutes rather than seconds.

```bash
make ui-golden          # capture the current reviewed UI as the answer key
git diff -- tests/ui-structure.golden
make ui-check           # prove a fresh capture matches it
```

Once that file is reviewed and committed, ordinary web validation can enforce
it. Until then, JavaScript syntax and theme contrast still run, but structural
UI drift is an explicit coverage gap.

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
```

**How to read the plan:** `because path:...` is a direct match;
`because dependency:...` was pulled in by another selected point. “No check in
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
profiles = ["commit", "ci", "full"]
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

## Initial inventory — where evidence is strong and where it is not

| Functionality point | Commit / CI evidence | Full-profile addition |
|---|---|---|
| `core.media` · `server.api` · `library.catalog` · `identity.access` · `watch-state.sync` · `plex.compatibility` | Workspace Rust tests, formatting, and clippy | Same gate; add a focused check when a regression escapes it |
| `playback.pipeline` | Rust decision, delivery, and session tests | Risk-weighted real-browser playback matrix |
| `web.experience` | JavaScript syntax and theme contrast; local structural check when available | Structural sweep across every registered layout |
| `apple.client` | XCTest is registered; Linux CI records the platform skip instead of a pass | Shared XCTest source on iOS and tvOS simulators |
| `android.client` | JVM unit tests and Android lint run in CI when affected | Same checks in the pinned build image |
| `operations.packaging` | Rust build/version tests | Release container image build |
| `validation.framework` | Catalog schema, dependency, glob, selection, and audit tests | Same contract |

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

CI uploads that directory even when a check fails. Read `report.json` first to
find the affected promise, then the named log to diagnose the failing layer.

## Non-goals — what the framework does not pretend to prove

- **It does not infer behavior from code.** Humans define the contract and its
  path boundary; the audit only prevents governed files from having no owner.
- **It does not replace focused regression tests.** A point routes tests. The
  test itself must still reproduce the bug and assert the intended result.
- **It does not make hardware interchangeable.** Browser playback, simulators,
  and JVM tests cannot prove Dolby Vision on a physical Apple TV or TV remote
  focus on every Android device. Keep those device observations explicit.
- **It does not use line coverage as a release verdict.** Coverage locates dark
  code; functionality points say which promises matter when that code changes.
