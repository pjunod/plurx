from __future__ import annotations

import subprocess
from pathlib import Path
import tempfile
import textwrap
import unittest

from validation.history import ISSUE_RE, HistoryError, audit_history, load_coverage
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
        coverage_dir = root / "regressions.d"
        coverage_dir.mkdir()
        return root, load_catalog(catalog_path), coverage_dir

    @staticmethod
    def write_coverage(coverage: Path, name: str, body: str) -> Path:
        """Write one `[[coverage]]` fragment, the way an author adds a mapping."""

        path = coverage / name
        path.write_text(
            "version = 1\n\n[[coverage]]\n" + textwrap.dedent(body).lstrip(),
            encoding="utf-8",
        )
        return path

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

        self.write_coverage(
            coverage,
            f"{sha[:8]}-app.toml",
            f"""
            commits = ["{sha[:8]}"]
            points = ["app"]
            checks = ["baseline"]
            reason = "The current baseline exercises this generated behavior."
            """,
        )
        covered = audit_history(root, catalog, coverage)
        self.assertEqual(covered.errors, ())

    def test_explicit_ledger_survives_a_non_corrective_squash_title(self):
        root, catalog, coverage = self.repository()
        (root / "src/app.rs").write_text(
            "pub fn imported() -> bool { true }\n", encoding="utf-8"
        )
        sha = self.commit(root, "Import state into a fresh target (#119)")
        self.write_coverage(
            coverage,
            f"{sha[:8]}-app.toml",
            f"""
            commits = ["{sha[:8]}"]
            points = ["app"]
            checks = ["baseline"]
            reason = "The current import contract exercises the squash-merged behavior."
            """,
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
                self.write_coverage(
                    coverage,
                    "candidate-app.toml",
                    f"""
                    commits = ["{prefix}"]
                    points = ["app"]
                    checks = ["baseline"]
                    reason = "Invalid prefixes must fail before history inspection."
                    """,
                )
                with self.assertRaises(HistoryError) as raised:
                    audit_history(root, catalog, coverage)
                self.assertEqual(
                    str(raised.exception),
                    "candidate-app.toml commits must contain Git SHA prefixes",
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
        self.write_coverage(
            coverage,
            f"{sha[:8]}-first.toml",
            f"""
            commits = ["{sha[:8]}"]
            points = ["app"]
            checks = ["missing"]
            reason = "First claim."
            """,
        )
        self.write_coverage(
            coverage,
            f"{sha[:8]}-second.toml",
            f"""
            commits = ["{sha[:8]}"]
            points = ["app"]
            checks = ["baseline"]
            reason = "Duplicate claim."
            """,
        )

        report = audit_history(root, catalog, coverage)

        self.assertTrue(any("unknown check missing" in error for error in report.errors))
        self.assertTrue(any("covered more than once" in error for error in report.errors))

    def test_documentation_correction_can_be_explicitly_ignored(self):
        root, catalog, coverage = self.repository()
        (root / "docs/plan.md").write_text("Correct explanation.\n", encoding="utf-8")
        sha = self.commit(root, "docs: correct the explanation")
        self.write_coverage(
            coverage,
            f"{sha[:8]}-non-runtime.toml",
            f"""
            commits = ["{sha[:8]}"]
            reason = "Documentation only; no executable behavior changed."
            ignore = true
            """,
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


class CoverageDirectoryCase(unittest.TestCase):
    """The regression ledger must survive concurrent corrective changes.

    A shared append-only file made every merge to `main` conflict every other
    open pull request that carried a mapping, and clearing that conflict with a
    rebase stranded the reviewer's exact-SHA approval. One entry per file makes
    the collision impossible instead of merely rare.
    """

    @staticmethod
    def git(root: Path, *arguments: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["git", *arguments], cwd=root, check=True, text=True, stdout=subprocess.PIPE
        )

    @staticmethod
    def fragment(root: Path, name: str, commit: str, reason: str) -> None:
        (root / "validation/regressions.d" / name).write_text(
            textwrap.dedent(
                f"""
                version = 1

                [[coverage]]
                commits = ["{commit}"]
                points = ["app"]
                checks = ["baseline"]
                reason = "{reason}"
                """
            ).lstrip(),
            encoding="utf-8",
        )

    def repository(self) -> Path:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        self.git(root, "init", "-q", "-b", "main")
        self.git(root, "config", "user.name", "Ledger Test")
        self.git(root, "config", "user.email", "ledger@example.invalid")
        (root / "validation/regressions.d").mkdir(parents=True)
        self.fragment(root, "0000aa11-seed.toml", "0000aa11", "Seeded before either branch.")
        self.git(root, "add", "-A")
        self.git(root, "commit", "-qm", "seed the ledger")
        return root

    def branch_adding(self, root: Path, name: str, fragment: str, commit: str) -> None:
        self.git(root, "checkout", "-q", "-b", name, "main")
        self.fragment(root, fragment, commit, f"Added independently on {name}.")
        self.git(root, "add", "-A")
        self.git(root, "commit", "-qm", f"fix: map {commit}")

    def merged(self, root: Path, name: str, first: str, second: str) -> tuple[str, ...]:
        self.git(root, "checkout", "-q", "-b", name, "main")
        for branch in (first, second):
            merge = subprocess.run(
                ["git", "merge", "--no-edit", "-q", branch],
                cwd=root,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
            )
            self.assertEqual(
                merge.returncode,
                0,
                f"merging {branch} into {name} conflicted:\n{merge.stdout}",
            )
        entries = load_coverage(root / "validation/regressions.d")
        return tuple(entry.commits[0] for entry in entries)

    def test_independent_mappings_merge_cleanly_in_either_order(self):
        root = self.repository()
        self.branch_adding(root, "alpha", "1111bb22-app.toml", "1111bb22")
        self.branch_adding(root, "beta", "2222cc33-app.toml", "2222cc33")

        expected = ("0000aa11", "1111bb22", "2222cc33")
        self.assertEqual(self.merged(root, "alpha-first", "alpha", "beta"), expected)
        self.assertEqual(self.merged(root, "beta-first", "beta", "alpha"), expected)

    def test_a_shared_append_only_ledger_is_what_conflicted(self):
        root = self.repository()
        shared = root / "validation/shared.toml"
        shared.write_text("version = 1\n", encoding="utf-8")
        self.git(root, "add", "-A")
        self.git(root, "commit", "-qm", "seed a shared tail")
        for branch, commit in (("gamma", "3333dd44"), ("delta", "4444ee55")):
            self.git(root, "checkout", "-q", "-b", branch, "main")
            shared.write_text(
                shared.read_text(encoding="utf-8")
                + f'\n[[coverage]]\ncommits = ["{commit}"]\n',
                encoding="utf-8",
            )
            self.git(root, "add", "-A")
            self.git(root, "commit", "-qm", f"fix: map {commit}")

        self.git(root, "checkout", "-q", "gamma")
        merge = subprocess.run(
            ["git", "merge", "--no-edit", "-q", "delta"],
            cwd=root,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )

        self.assertNotEqual(merge.returncode, 0)
        self.assertIn("<<<<<<<", shared.read_text(encoding="utf-8"))

    def test_a_fragment_must_be_named_after_its_first_mapped_commit(self):
        root = self.repository()
        self.fragment(root, "unrelated-name.toml", "5555ff66", "Named after nothing.")

        with self.assertRaises(HistoryError) as raised:
            load_coverage(root / "validation/regressions.d")

        self.assertIn("5555ff66", str(raised.exception))

    def test_a_fragment_holds_exactly_one_entry(self):
        root = self.repository()
        path = root / "validation/regressions.d/6666aa77-app.toml"
        self.fragment(root, path.name, "6666aa77", "First.")
        path.write_text(
            path.read_text(encoding="utf-8")
            + '\n[[coverage]]\ncommits = ["7777bb88"]\nreason = "Second."\nignore = true\n',
            encoding="utf-8",
        )

        with self.assertRaises(HistoryError) as raised:
            load_coverage(root / "validation/regressions.d")

        self.assertIn("exactly one", str(raised.exception))

    def test_a_reintroduced_shared_ledger_is_refused(self):
        root = self.repository()
        (root / "validation/regressions.toml").write_text(
            "version = 1\n", encoding="utf-8"
        )

        with self.assertRaises(HistoryError) as raised:
            load_coverage(root / "validation/regressions.d")

        self.assertIn("no longer the regression ledger", str(raised.exception))


if __name__ == "__main__":
    unittest.main()
