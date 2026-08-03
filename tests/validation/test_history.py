from __future__ import annotations

import subprocess
from pathlib import Path
import tempfile
import textwrap
import unittest

from validation.history import audit_history
from validation.runner import load_catalog


CATALOG = """
version = 1

[settings]
profiles = ["commit"]
always_checks = ["baseline"]

[[checks]]
id = "baseline"
title = "Baseline"
command = "true"
profiles = ["commit"]

[[points]]
id = "app"
title = "Application"
contract = "The app works."
paths = ["src/**", "tests/**", "docs/**"]
checks = ["baseline"]
"""


class HistoryAuditCase(unittest.TestCase):
    def repository(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        subprocess.run(["git", "init", "-q"], cwd=root, check=True)
        subprocess.run(["git", "config", "user.name", "History Test"], cwd=root, check=True)
        subprocess.run(
            ["git", "config", "user.email", "history@example.invalid"], cwd=root, check=True
        )
        (root / "src").mkdir()
        (root / "tests").mkdir()
        (root / "docs").mkdir()
        catalog_path = root / "points.toml"
        catalog_path.write_text(CATALOG, encoding="utf-8")
        coverage_path = root / "regressions.toml"
        coverage_path.write_text("version = 1\n", encoding="utf-8")
        return root, load_catalog(catalog_path), coverage_path

    @staticmethod
    def commit(root: Path, subject: str):
        subprocess.run(["git", "add", "-A"], cwd=root, check=True)
        subprocess.run(["git", "commit", "-qm", subject], cwd=root, check=True)
        return subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=root, check=True, text=True,
            stdout=subprocess.PIPE
        ).stdout.strip()

    def test_fix_with_a_direct_regression_test_needs_no_ledger_entry(self):
        root, catalog, coverage = self.repository()
        (root / "src/app.rs").write_text("pub fn answer() -> u8 { 1 }\n", encoding="utf-8")
        self.commit(root, "feat: seed")
        (root / "src/app.rs").write_text(
            "pub fn answer() -> u8 { 2 }\n\n#[test]\nfn fixed_answer() { assert_eq!(answer(), 2); }\n",
            encoding="utf-8",
        )
        self.commit(root, "fix: return the corrected answer")

        report = audit_history(root, catalog, coverage)

        self.assertEqual(report.errors, ())
        self.assertEqual(report.mapped_count, 0)
        self.assertEqual(report.ignored_count, 0)
        self.assertEqual(report.direct_count, 1)

    def test_fix_without_a_test_must_name_current_check_evidence(self):
        root, catalog, coverage = self.repository()
        (root / "src/app.rs").write_text("pub fn answer() -> u8 { 1 }\n", encoding="utf-8")
        self.commit(root, "feat: seed")
        (root / "src/app.rs").write_text("pub fn answer() -> u8 { 2 }\n", encoding="utf-8")
        sha = self.commit(root, "fix: return the corrected answer")

        missing = audit_history(root, catalog, coverage)
        self.assertTrue(any(sha[:8] in error for error in missing.errors))

        coverage.write_text(
            textwrap.dedent(
                f"""
                version = 1

                [[coverage]]
                commits = ["{sha[:8]}"]
                points = ["app"]
                checks = ["baseline"]
                reason = "The current baseline exercises this generated behavior."
                """
            ),
            encoding="utf-8",
        )
        covered = audit_history(root, catalog, coverage)
        self.assertEqual(covered.errors, ())

    def test_unknown_checks_and_duplicate_commit_coverage_fail_loudly(self):
        root, catalog, coverage = self.repository()
        (root / "src/app.rs").write_text("pub fn answer() -> u8 { 1 }\n", encoding="utf-8")
        sha = self.commit(root, "fix: seed the corrected answer")
        coverage.write_text(
            textwrap.dedent(
                f"""
                version = 1

                [[coverage]]
                commits = ["{sha[:8]}"]
                points = ["app"]
                checks = ["missing"]
                reason = "First claim."

                [[coverage]]
                commits = ["{sha[:8]}"]
                points = ["app"]
                checks = ["baseline"]
                reason = "Duplicate claim."
                """
            ),
            encoding="utf-8",
        )

        report = audit_history(root, catalog, coverage)

        self.assertTrue(any("unknown check missing" in error for error in report.errors))
        self.assertTrue(any("covered more than once" in error for error in report.errors))

    def test_documentation_correction_can_be_explicitly_ignored(self):
        root, catalog, coverage = self.repository()
        (root / "docs/plan.md").write_text("Correct explanation.\n", encoding="utf-8")
        sha = self.commit(root, "docs: correct the explanation")
        coverage.write_text(
            textwrap.dedent(
                f"""
                version = 1

                [[coverage]]
                commits = ["{sha[:8]}"]
                reason = "Documentation only; no executable behavior changed."
                ignore = true
                """
            ),
            encoding="utf-8",
        )

        report = audit_history(root, catalog, coverage)

        self.assertEqual(report.errors, ())
        self.assertEqual(report.mapped_count, 0)
        self.assertEqual(report.ignored_count, 1)


if __name__ == "__main__":
    unittest.main()
