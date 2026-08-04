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

from validation.ci_scope import all_scope, scope_for_paths
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
        self.assertFalse(any(operations.values()))

        android = scope_for_paths(
            catalog,
            ("clients/android/app/src/main/java/tv/plurx/app/player/PlayerScreen.kt",),
        )
        self.assertTrue(android["android_jvm"])
        self.assertTrue(android["android_device"])
        self.assertFalse(android["web_layout"])
        self.assertFalse(android["release_build"])
        self.assertFalse(android["container"])

        server = scope_for_paths(catalog, ("crates/plurxd/src/http/stream.rs",))
        self.assertTrue(server["apple"])
        self.assertTrue(server["android_jvm"])
        self.assertTrue(server["web_layout"])
        self.assertTrue(server["release_build"])
        self.assertTrue(server["container"])
        self.assertFalse(server["android_device"])

        self.assertTrue(all(all_scope().values()))

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
