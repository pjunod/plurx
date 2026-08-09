# WO-03 — CI pipeline fixes

**Repo:** `~/code/plurx` · **Baseline:** `origin/main` @ `e8a910f` · **Priority: P1 (task 1 is P0-adjacent)**
Note: `.github/workflows/` edits can't be pushed by every agent (protected path for some tooling) — land them in a normal PR.

## Context

The CI overhaul's M1 (#74: policy preflight + docs-only lane) matches its plan faithfully. These are the concrete holes found in a full pipeline review, ordered by risk.

## Tasks

1. **Release artifacts re-grow the hiqlite closure — build plurxd alone.**
   `.github/workflows/ci.yml:333` runs `cargo build --release --workspace`. Because `plurx-cluster-check` enables `plurx-core/hiqlite-store`, resolver-2 feature unification compiles plurx-core WITH hiqlite into that invocation — probe-confirmed (`cargo tree -p plurxd` has zero hiqlite/openraft; `cargo tree --workspace` links `hiqlite v0.14.0` + `openraft v0.9.25`). The uploaded `plurxd-{target}` artifact therefore carries the ~331-crate graph that #84's fix exists to exclude, and diverges from the Docker-built binary.
   Fix: `cargo build --release -p plurxd` for the artifact step (add a separate `--workspace` compile check if you want the members built, but don't ship its plurxd).
   Acceptance: artifact build log shows the ~191-crate count; optionally compare binary size to the Docker build.

2. **`timeout-minutes` on every job.** Zero exist in `ci.yml`/`lint.yml`/`rust-audit.yml` (only validation-nightly has one). A hung `xcodebuild` runs to GitHub's 360-minute default at 10× macOS billing. Add sensible per-job values now (don't wait for overhaul M2), plus a `tests/operations` contract asserting every ci.yml job declares one. Acceptance: `grep -c timeout-minutes .github/workflows/ci.yml` ≥ job count; contract test red when one is removed.

3. **Docs-only lane must exclude release paths.** `validation/ci_scope.py:37-46` classifies `**/*.md` anywhere as docs — verified: `clients/apple/Sources/Notes.md` → docs-only, which skips the mobile-version preflight on the PR and then fails it post-merge on main (release-path tables in `mobile_versions.py:20-23` include that tree). Fix: exclude `clients/**` and `crates/**` from `DOCS_ONLY_PATHS`, or derive docs-ness from the release-path tables. Acceptance: unit test pinning `is_docs_only(("clients/apple/Sources/x.md",)) is False`.

4. **Glob registrations must resolve.** `validation/runner.py:238-239,331`: literal evidence paths must exist (good — added this week), but any pattern containing `*`/`?`/`{` is exempt, so a renamed directory silently un-selects its checks forever (the hiqlite-spike entry is itself a brace pattern). Fix: lint requires each pattern to match ≥1 tracked file, with an explicit allow-list for intentionally forward-looking globs. Acceptance: lint red on a pattern matching nothing.

5. **Run the mobile-version gate in parallel, not as a test-hiding prerequisite.** `ci.yml:53-79` puts it in `preflight`, which every expensive job `needs` — so a stale build number skips the entire Swift suite and the author repushes blind (this happened on #92: the Swift tests never ran). Fix: make mobile-version its own job feeding `pr_gate` only; keep history/catalog/operations contracts as true prerequisites. Acceptance: a PR with only a stale versionCode still reports Swift suite results, and `pr_gate` still fails.

6. **Cluster gate dedup + scoping.** The `cluster-auth` check carries the `ci` profile (`validation/points.toml:169`-ish), so the `check` job runs `make cluster-check` AND the dedicated `cluster_auth` job repeats it on the same PR; meanwhile `hiqlite_spike` + `cluster_auth` run on every non-docs PR (pure web changes included). Scope both via a scope key; drop the duplicate profile entry. Also rename the job — it now carries M1b **and** M1c (and after #90, the 3-voter store-contract suite). Acceptance: a web-only PR shows both jobs skipped; a cluster PR runs cluster-check once.

7. **iPad destination.** Two iPad-specific fixes shipped this week; CI still simulates only iPhone 16 Pro + Apple TV (`ci.yml:207-214`). Add an iPad simulator destination to `make apple-test`'s CI invocation (keep local defaults fast). Acceptance: CI log shows the iPad leg executing the `#if os(iOS)` layout tests.

8. **Flake policy, minimally.** The cluster gate historically flaked ~3 % (port TOCTOU is retried in-harness now; new 3-voter suite adds load). Before anyone adds a blind rerun in frustration: record per-job pass/fail + duration to a small ledger (overhaul M5's shape) and quarantine rules. Even a `gh run list` script committed under `scripts/` beats nothing. Acceptance: a written flake policy in docs/CI_TEST_OVERHAUL_PLAN.md M5 section marked "in effect".

## Don't

- Don't split rust-gate into per-suite jobs yet — that's overhaul M2 and needs the timeout groundwork first.
- Don't gate on `make apple-test` locally (`make check` intentionally stays client-free; WO-04 handles evidence discipline instead).
