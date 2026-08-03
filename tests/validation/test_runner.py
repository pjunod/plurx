from __future__ import annotations

import contextlib
import dataclasses
import io
import json
from pathlib import Path
import tempfile
import unittest
import xml.etree.ElementTree as ET

from validation.runner import (
    CatalogError,
    CheckResult,
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

    def test_selection_expands_dependencies_and_deduplicates_checks(self):
        catalog = self.load()
        selection = select_points(catalog, ("web/player/app.js",))
        self.assertEqual(selection.point_ids, ("core", "web"))
        self.assertEqual(
            tuple(check.id for check in selected_checks(catalog, selection, "full")),
            ("baseline", "browser"),
        )

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
