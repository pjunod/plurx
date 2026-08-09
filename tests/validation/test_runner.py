from __future__ import annotations

import contextlib
import dataclasses
import io
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
import xml.etree.ElementTree as ET

from validation.ci_scope import (
    all_scope,
    is_docs_only,
    needs_rust_gate,
    resolve_scope,
    scope_for_paths,
)
from validation.runner import (
    CatalogError,
    CheckResult,
    changed_paths,
    execute_checks,
    glob_regex,
    lint_catalog,
    load_catalog,
    matches,
    select_named,
    select_points,
    selected_checks,
    write_reports,
)


ROOT = Path(__file__).resolve().parents[2]


CATALOG = """
version = 1

[settings]
profiles = ["commit", "full"]
always_checks = ["baseline"]

[[checks]]
id = "baseline"
title = "Baseline"
command = "true"
profiles = ["commit", "full"]

[[checks]]
id = "browser"
title = "Browser"
command = "true"
profiles = ["full"]

[[points]]
id = "core"
title = "Core"
contract = "Core stays true."
paths = ["src/**"]
checks = ["baseline"]

[[points]]
id = "web"
title = "Web"
contract = "Web stays true."
paths = ["web/**/*.js"]
checks = ["browser", "baseline"]
depends_on = ["core"]
"""


class CatalogCase(unittest.TestCase):
    def load(self, content: str = CATALOG):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        path = Path(temporary.name) / "points.toml"
        path.write_text(content, encoding="utf-8")
        return load_catalog(path)

    def test_double_star_matches_zero_or_many_directories(self):
        pattern = glob_regex("web/**/*.js")
        self.assertIsNotNone(pattern.match("web/app.js"))
        self.assertIsNotNone(pattern.match("web/player/app.js"))
        self.assertIsNone(pattern.match("web/player/app.css"))

    def test_provider_change_expands_consumers_and_deduplicates_checks(self):
        catalog = self.load()
        selection = select_points(catalog, ("src/domain.py",))
        self.assertEqual(selection.point_ids, ("core", "web"))
        self.assertEqual(selection.reasons["web"], ("consumer:core",))
        self.assertEqual(
            tuple(check.id for check in selected_checks(catalog, selection, "full")),
            ("baseline", "browser"),
        )

    def test_consumer_change_does_not_point_upstream(self):
        catalog = self.load()
        selection = select_points(catalog, ("web/player/app.js",))
        self.assertEqual(selection.point_ids, ("web",))

    def test_apple_ui_test_change_does_not_select_web_layout(self):
        catalog = load_catalog(ROOT / "validation/points.toml")
        selection = select_points(
            catalog, ("clients/apple/Tests/AppleClientTests.swift",)
        )
        check_ids = {
            check.id for check in selected_checks(catalog, selection, profile="ci")
        }

        self.assertIn("apple-simulators", check_ids)
        self.assertNotIn("web-layout", check_ids)

    def test_ci_scope_keeps_expensive_jobs_on_their_affected_surfaces(self):
        catalog = load_catalog(ROOT / "validation/points.toml")

        operations = scope_for_paths(
            catalog, ("scripts/ship", "tests/operations/test_contracts.py")
        )
        # Scripts feed packaging and selection behavior the cargo suite pins,
        # so the Rust lane stays on — but no client or container surface does.
        self.assertTrue(operations["rust"])
        self.assertFalse(
            any(value for key, value in operations.items() if key != "rust")
        )

        android = scope_for_paths(
            catalog,
            ("clients/android/app/src/main/java/tv/plurx/app/player/PlayerScreen.kt",),
        )
        self.assertTrue(android["android_jvm"])
        self.assertTrue(android["android_device"])
        self.assertFalse(android["rust"])
        self.assertFalse(android["apple"])
        self.assertFalse(android["web_layout"])
        self.assertFalse(android["release_build"])
        self.assertFalse(android["container"])

        # The pull-request lane defers the server→client fan-out to the merge
        # queue: a server diff keeps the Rust, release, and container lanes and
        # runs no client simulator, JVM, or layout sweep on the PR itself.
        server = scope_for_paths(catalog, ("crates/plurxd/src/http/stream.rs",))
        self.assertTrue(server["rust"])
        self.assertTrue(server["release_build"])
        self.assertTrue(server["container"])
        self.assertFalse(server["apple"])
        self.assertFalse(server["android_jvm"])
        self.assertFalse(server["web_layout"])
        self.assertFalse(server["android_device"])
        self.assertFalse(server["hiqlite_spike"])
        self.assertFalse(server["cluster_auth"])

        web = scope_for_paths(catalog, ("crates/plurxd/src/web/app.js",))
        self.assertTrue(web["rust"])
        self.assertTrue(web["web_layout"])
        self.assertFalse(web["hiqlite_spike"])
        self.assertFalse(web["cluster_auth"])

        cluster = scope_for_paths(
            catalog, ("crates/plurx-core/src/store/hiqlite.rs",)
        )
        self.assertFalse(cluster["hiqlite_spike"])
        self.assertTrue(cluster["cluster_auth"])

        core = scope_for_paths(catalog, ("crates/plurx-core/src/domain.rs",))
        self.assertTrue(core["hiqlite_spike"])
        self.assertTrue(core["cluster_auth"])

        cluster_selection = select_points(
            catalog, ("crates/plurx-core/src/store/hiqlite.rs",)
        )
        cluster_ci_checks = {
            check.id
            for check in selected_checks(catalog, cluster_selection, profile="ci")
        }
        self.assertNotIn("cluster-auth", cluster_ci_checks)

        fallback = all_scope()
        executable_scope = (
            value for key, value in fallback.items() if key != "docs_only"
        )
        self.assertTrue(all(executable_scope))
        self.assertFalse(fallback["docs_only"])

    def test_client_only_diffs_skip_the_cargo_lane_but_keep_their_own(self):
        catalog = load_catalog(ROOT / "validation/points.toml")

        apple = scope_for_paths(
            catalog, ("clients/apple/Sources/PlayerSurface.swift",)
        )
        self.assertFalse(apple["rust"])
        self.assertTrue(apple["apple"])
        self.assertTrue(apple["mobile_version"])
        self.assertFalse(apple["android_jvm"])
        self.assertFalse(apple["android_device"])
        self.assertFalse(apple["web_layout"])
        self.assertFalse(apple["release_build"])
        self.assertFalse(apple["container"])

        # A Kotlin diff plus its own release notes is still client-only.
        mixed = scope_for_paths(
            catalog,
            (
                "clients/android/app/src/main/java/tv/plurx/app/MainActivity.kt",
                "clients/android/README.md",
                "docs/FEATURES.md",
            ),
        )
        self.assertFalse(mixed["rust"])
        self.assertTrue(mixed["android_jvm"])

        # The one file that compiles into BOTH native clients: editing the
        # shared wire fixture must re-run both client suites on the PR itself,
        # and it lives under tests/, so the Rust lane runs too.
        fixture = scope_for_paths(catalog, ("tests/contracts/native-api.json",))
        self.assertTrue(fixture["rust"])
        self.assertTrue(fixture["apple"])
        self.assertTrue(fixture["android_jvm"])

        self.assertTrue(needs_rust_gate(()))

    def test_merge_queue_events_fail_open_into_the_full_fan_out(self):
        for event in ("merge_group", "push"):
            with self.subTest(event=event):
                self.assertEqual(resolve_scope(event, None), all_scope())

    def test_ci_profile_splits_the_rust_gate_and_local_profiles_keep_it_whole(self):
        catalog = load_catalog(ROOT / "validation/points.toml")
        selection = select_points(catalog, ("crates/plurxd/src/http/stream.rs",))

        ci = {check.id for check in selected_checks(catalog, selection, "ci")}
        self.assertIn("rust-gate-ci", ci)
        self.assertNotIn("rust-gate", ci)
        # These re-run binaries rust-gate-ci already executes with identical
        # feature resolution; in CI they would be pure duplication.
        self.assertNotIn("api-wire", ci)
        self.assertNotIn("security-boundaries", ci)
        self.assertNotIn("user-journey", ci)

        commit = {check.id for check in selected_checks(catalog, selection, "commit")}
        self.assertIn("rust-gate", commit)
        self.assertNotIn("rust-gate-ci", commit)

        gate = catalog.check_map["rust-gate-ci"]
        self.assertEqual(gate.command, "make ci-rust-gate")

    def test_ci_scope_routes_documentation_only_diffs_to_fast_preflight(self):
        catalog = load_catalog(ROOT / "validation/points.toml")

        scope = scope_for_paths(
            catalog,
            (
                "docs/OFFLINE-VIEWING-PLAN.md",
                "docs/OFFLINE-VIEWING-REVIEW.md",
                "docs/STATUS.html",
                "validation/regressions.toml",
            ),
        )

        self.assertTrue(scope["docs_only"])
        executable_scope = (
            value for key, value in scope.items() if key != "docs_only"
        )
        self.assertFalse(any(executable_scope))
        self.assertTrue(is_docs_only((".github/PULL_REQUEST_TEMPLATE.md",)))
        self.assertFalse(is_docs_only(("clients/apple/Sources/Notes.md",)))
        self.assertFalse(is_docs_only(("crates/plurx-core/README.md",)))

    def test_ci_scope_does_not_treat_selector_changes_as_documentation(self):
        catalog = load_catalog(ROOT / "validation/points.toml")

        scope = scope_for_paths(
            catalog,
            ("docs/PLAYBACK.md", "validation/points.toml"),
        )

        self.assertFalse(scope["docs_only"])

        for scheduler_path in (
            ".github/workflows/ci.yml",
            "validation/ci_scope.py",
            "validation/points.toml",
            "validation/runner.py",
        ):
            with self.subTest(scheduler_path=scheduler_path):
                scheduler_scope = scope_for_paths(catalog, (scheduler_path,))
                selected = (
                    value
                    for key, value in scheduler_scope.items()
                    if key != "docs_only"
                )
                self.assertTrue(all(selected))
                self.assertFalse(scheduler_scope["docs_only"])

    def test_ci_scope_fails_open_for_mixed_documentation_and_code(self):
        catalog = load_catalog(ROOT / "validation/points.toml")

        scope = scope_for_paths(
            catalog,
            ("docs/PLAYBACK.md", "crates/plurxd/src/http/stream.rs"),
        )

        self.assertFalse(scope["docs_only"])
        self.assertTrue(scope["release_build"])
        self.assertTrue(scope["container"])

    def test_profile_keeps_mandatory_baseline_when_slow_check_is_ineligible(self):
        catalog = self.load()
        selection = select_named(catalog, ("web",))
        self.assertEqual(
            tuple(check.id for check in selected_checks(catalog, selection, "commit")),
            ("baseline",),
        )

    def test_lint_rejects_unknown_check(self):
        catalog = self.load(CATALOG.replace('checks = ["browser", "baseline"]', 'checks = ["missing"]'))
        errors = lint_catalog(catalog, audit=False)
        self.assertIn("point web references unknown check missing", errors)

    def test_lint_rejects_duplicate_ids_and_dependency_cycles(self):
        duplicate = self.load(CATALOG + """

[[points]]
id = "core"
title = "Duplicate core"
contract = "This must be rejected."
paths = ["duplicate/**"]
checks = ["baseline"]
""")
        self.assertIn("duplicate point id: core", lint_catalog(duplicate, audit=False))

        cycle_text = CATALOG.replace(
            'paths = ["src/**"]\nchecks = ["baseline"]',
            'paths = ["src/**"]\nchecks = ["baseline"]\ndepends_on = ["web"]',
        )
        cycle = self.load(cycle_text)
        self.assertTrue(
            any(error.startswith("point dependency cycle:") for error in lint_catalog(cycle, audit=False))
        )

    def test_unknown_named_point_is_an_error(self):
        catalog = self.load()
        with self.assertRaises(CatalogError):
            select_named(catalog, ("missing",))

    def test_repository_globs_do_not_let_single_star_cross_directories(self):
        self.assertTrue(matches("crates/core/src/lib.rs", ("crates/**",)))
        self.assertFalse(matches("crates/core/src/lib.rs", ("crates/*.rs",)))

    def test_braces_keep_related_path_triggers_reviewable(self):
        pattern = ("scripts/{bench,ship}",)
        self.assertTrue(matches("scripts/bench", pattern))
        self.assertTrue(matches("scripts/ship", pattern))
        self.assertFalse(matches("scripts/validate", pattern))

    def test_lint_rejects_a_literal_path_that_does_not_resolve(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "present.rs").write_text("// present\n", encoding="utf-8")
            catalog = self.load(
                CATALOG.replace(
                    'paths = ["src/**"]',
                    'paths = ["present.rs", "missing.rs"]',
                    1,
                )
            )
            errors = lint_catalog(catalog, repo_root=root, audit=False)

        self.assertIn(
            "point core references missing literal path 'missing.rs'",
            errors,
        )
        self.assertNotIn(
            "point core references missing literal path 'present.rs'",
            errors,
        )

    def test_lint_rejects_a_glob_that_matches_no_tracked_file(self):
        catalog = self.load(
            CATALOG.replace('paths = ["web/**/*.js"]', 'paths = ["renamed/**/*.js"]')
        )
        errors = lint_catalog(
            catalog,
            audit=False,
            tracked_paths=("src/lib.rs", "web/app.js"),
        )

        self.assertIn(
            "point web path glob matches no tracked file: 'renamed/**/*.js'",
            errors,
        )

    def test_forward_looking_glob_needs_an_exact_allowlist_entry(self):
        catalog = self.load(
            CATALOG.replace(
                'always_checks = ["baseline"]',
                'always_checks = ["baseline"]\nallow_unmatched_globs = ["future/**/*.md"]',
            ).replace('paths = ["web/**/*.js"]', 'paths = ["future/**/*.md"]')
        )
        errors = lint_catalog(
            catalog,
            audit=False,
            tracked_paths=("src/lib.rs",),
        )

        self.assertNotIn(
            "point web path glob matches no tracked file: 'future/**/*.md'",
            errors,
        )

    def test_missing_optional_prerequisite_is_visible_and_strict_can_fail_it(self):
        catalog = self.load()
        check = dataclasses.replace(
            catalog.check_map["browser"],
            requires_files=("does-not-exist",),
            missing="skip",
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with contextlib.redirect_stdout(io.StringIO()):
                optional = execute_checks(
                    (check,), root, root / "artifacts", strict=False, fail_fast=False
                )
                strict = execute_checks(
                    (check,), root, root / "artifacts", strict=True, fail_fast=False
                )
        self.assertEqual(optional[0].status, "skipped")
        self.assertEqual(strict[0].status, "failed")
        self.assertIn("missing files", optional[0].message)

    def test_staged_path_resolution_handles_added_and_renamed_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "-q"], cwd=root, check=True)
            subprocess.run(["git", "config", "user.name", "Validation Test"], cwd=root, check=True)
            subprocess.run(["git", "config", "user.email", "validation@example.invalid"], cwd=root, check=True)
            original = root / "old name.txt"
            original.write_text("old\n", encoding="utf-8")
            deleted = root / "deleted.txt"
            deleted.write_text("delete me\n", encoding="utf-8")
            subprocess.run(["git", "add", "old name.txt"], cwd=root, check=True)
            subprocess.run(["git", "add", "deleted.txt"], cwd=root, check=True)
            subprocess.run(["git", "commit", "-qm", "seed"], cwd=root, check=True)
            original.rename(root / "new name.txt")
            deleted.unlink()
            (root / "added.txt").write_text("new\n", encoding="utf-8")
            subprocess.run(["git", "add", "-A"], cwd=root, check=True)

            self.assertEqual(
                changed_paths(root, "staged"),
                ("added.txt", "deleted.txt", "new name.txt"),
            )

    def test_timeout_and_fail_fast_are_recorded_as_failures(self):
        catalog = self.load()
        timeout = dataclasses.replace(
            catalog.check_map["baseline"],
            command="python3 -c 'import time; time.sleep(2)'",
            timeout_seconds=1,
        )
        never = catalog.check_map["browser"]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with contextlib.redirect_stdout(io.StringIO()):
                results = execute_checks(
                    (timeout, never), root, root / "artifacts", strict=True, fail_fast=True
                )
        self.assertEqual(len(results), 1)
        self.assertEqual(results[0].status, "failed")
        self.assertEqual(results[0].returncode, 124)
        self.assertIn("timed out", results[0].output)

    def test_reports_preserve_point_status_and_parse_as_junit(self):
        catalog = self.load()
        selection = select_named(catalog, ("core",))
        results = [
            CheckResult(
                id="baseline",
                title="Baseline",
                status="passed",
                seconds=0.25,
                returncode=0,
                message="exit 0",
                log_path="logs/baseline.log",
                output="ok\n",
            )
        ]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            json_path, xml_path = write_reports(
                catalog, selection, "commit", results, root / "artifacts", root
            )
            report = json.loads(json_path.read_text(encoding="utf-8"))
            suite = ET.parse(xml_path).getroot()
        self.assertEqual(report["points"][0]["status"], "passed")
        self.assertEqual(report["checks"][0]["status"], "passed")
        self.assertEqual(suite.attrib["tests"], "1")
        self.assertEqual(suite.attrib["failures"], "0")


if __name__ == "__main__":
    unittest.main()
