from __future__ import annotations

import subprocess
from pathlib import Path
import tempfile
import textwrap
import unittest

from validation.history import ISSUE_RE, HistoryError, audit_history
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
paths = ["src/**", "crates/**", "clients/**", "tests/**", "docs/**"]
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
        (root / "crates").mkdir()
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

    def test_explicit_ledger_survives_a_non_corrective_squash_title(self):
        root, catalog, coverage = self.repository()
        (root / "src/app.rs").write_text(
            "pub fn imported() -> bool { true }\n", encoding="utf-8"
        )
        sha = self.commit(root, "Import state into a fresh target (#119)")
        coverage.write_text(
            textwrap.dedent(
                f"""
                version = 1

                [[coverage]]
                commits = ["{sha[:8]}"]
                points = ["app"]
                checks = ["baseline"]
                reason = "The current import contract exercises the squash-merged behavior."
                """
            ),
            encoding="utf-8",
        )

        report = audit_history(root, catalog, coverage)

        self.assertEqual(report.errors, ())
        self.assertEqual(report.mapped_count, 1)

    def test_client_anchor_survives_a_non_corrective_squash_title(self):
        root, catalog, coverage = self.repository()
        (root / "clients").mkdir()
        (root / "clients/app.swift").write_text(
            "func restoredPolicy() {}\n", encoding="utf-8"
        )
        (root / "tests/client.swift").write_text(
            "func testRestoredPolicy() {}\n", encoding="utf-8"
        )
        subject = "Persist saved client policy (#120)"
        self.assertIsNone(ISSUE_RE.search(subject))
        sha = self.commit(root, subject)
        (root / "tests/client-fixes.toml").write_text(
            textwrap.dedent(
                f"""
                version = 1

                [[fixes]]
                id = "client.restored-policy"
                commits = ["{sha[:8]}"]
                source = "clients/app.swift"
                source_anchor = "restoredPolicy"
                test = "tests/client.swift"
                test_anchor = "testRestoredPolicy"
                """
            ),
            encoding="utf-8",
        )

        report = audit_history(root, catalog, coverage)

        self.assertEqual(report.errors, ())
        self.assertEqual(report.anchored_count, 1)

    def test_commit_prefixes_must_be_stable_lowercase_shas(self):
        root, catalog, coverage = self.repository()
        (root / "src/app.rs").write_text(
            "pub fn answer() -> u8 { 1 }\n", encoding="utf-8"
        )
        self.commit(root, "feat: seed")
        for prefix in ("", "a", "123456", "ABCDEF0", "123456g", "a" * 41):
            with self.subTest(prefix=prefix):
                coverage.write_text(
                    textwrap.dedent(
                        f"""
                        version = 1

                        [[coverage]]
                        commits = ["{prefix}"]
                        points = ["app"]
                        checks = ["baseline"]
                        reason = "Invalid prefixes must fail before history inspection."
                        """
                    ),
                    encoding="utf-8",
                )
                with self.assertRaises(HistoryError) as raised:
                    audit_history(root, catalog, coverage)
                self.assertEqual(
                    str(raised.exception),
                    "coverage[0].commits must contain Git SHA prefixes",
                )

    def test_one_commit_cannot_use_two_client_anchor_prefixes(self):
        root, catalog, coverage = self.repository()
        (root / "clients").mkdir()
        (root / "clients/app.swift").write_text(
            "func restoredPolicy() {}\n", encoding="utf-8"
        )
        (root / "tests/client.swift").write_text(
            "func testRestoredPolicy() {}\n", encoding="utf-8"
        )
        sha = self.commit(root, "fix(client): restore saved policy")
        rows = []
        for identifier, prefix in (("short", sha[:8]), ("long", sha[:12])):
            rows.append(
                textwrap.dedent(
                    f"""
                    [[fixes]]
                    id = "client.{identifier}"
                    commits = ["{prefix}"]
                    source = "clients/app.swift"
                    source_anchor = "restoredPolicy"
                    test = "tests/client.swift"
                    test_anchor = "testRestoredPolicy"
                    """
                )
            )
        (root / "tests/client-fixes.toml").write_text(
            "version = 1\n" + "".join(rows), encoding="utf-8"
        )

        report = audit_history(root, catalog, coverage)

        self.assertTrue(any("anchored more than once" in error for error in report.errors))

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

    def test_runtime_fix_cannot_hide_behind_an_unrelated_test_edit(self):
        root, catalog, coverage = self.repository()
        (root / "crates/app.rs").write_text(
            "pub fn answer() -> u8 { 1 }\n", encoding="utf-8"
        )
        base = self.commit(root, "feat: seed runtime")
        (root / "crates/app.rs").write_text(
            "pub fn answer() -> u8 { 2 }\n", encoding="utf-8"
        )
        (root / "tests/unrelated.rs").write_text(
            "#[test]\nfn unrelated() { assert_eq!(1, 1); }\n", encoding="utf-8"
        )
        (root / "tests/client-fixes.toml").write_text(
            textwrap.dedent(
                f"""
                version = 1
                enforce_after = "{base[:8]}"
                """
            ),
            encoding="utf-8",
        )
        sha = self.commit(root, "Address runtime review feedback")

        report = audit_history(root, catalog, coverage)

        self.assertTrue(
            any(
                sha[:8] in error and "needs an explicit" in error
                for error in report.errors
            ),
            report.errors,
        )


if __name__ == "__main__":
    unittest.main()
