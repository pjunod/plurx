from __future__ import annotations

from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[2]


class EvidenceWorkflowCase(unittest.TestCase):
    def read(self, path: str) -> str:
        return (ROOT / path).read_text(encoding="utf-8")

    def test_fix_evidence_is_label_opt_in_and_report_only(self) -> None:
        workflow = self.read(".github/workflows/fix-evidence.yml")
        self.assertIn("'fixes-behavior'", workflow)
        self.assertIn("scripts/prove-fix", workflow)
        self.assertIn("continue-on-error: true", workflow)
        self.assertIn("This is report-only", workflow)
        self.assertIn("timeout-minutes: 60", workflow)

    def test_nightly_mutation_scope_is_bounded_and_artifacted(self) -> None:
        workflow = self.read(".github/workflows/validation-nightly.yml")
        self.assertIn("git log --since=7.days --name-only", workflow)
        self.assertIn("cargo mutants --timeout 300", workflow)
        self.assertIn("--file", workflow)
        self.assertIn("timeout-minutes: 60", workflow)
        self.assertIn("continue-on-error: true", workflow)
        self.assertIn("target/mutants", workflow)
        self.assertIn("GITHUB_STEP_SUMMARY", workflow)


if __name__ == "__main__":
    unittest.main()
