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

    def test_pgs_fuzzer_is_bounded_seeded_artifacted_and_gating(self) -> None:
        workflow = self.read(".github/workflows/validation-nightly.yml")
        self.assertIn(
            "cargo +nightly-2026-08-01 fuzz run inspect_sup fuzz/corpus/inspect_sup",
            workflow,
        )
        self.assertIn("-max_total_time=900", workflow)
        self.assertIn("fuzz/artifacts", workflow)
        self.assertIn("steps.pgs_fuzz.outcome == 'failure'", workflow)
        self.assertIn("exit 1", workflow)
        self.assertTrue((ROOT / "fuzz/corpus/inspect_sup/minimal-header").is_file())

    def test_pgs_periodic_refresh_and_completion_prune_remain_wired(self) -> None:
        apple = self.read("clients/apple/Sources/PlayerController.swift")
        server = self.read("crates/plurxd/src/pgs_overlay.rs")
        self.assertIn("PGSOverlayPolicy.periodicRefreshPosition", apple)
        self.assertIn("self.refreshPGSOverlayWindow(at: overlayPosition)", apple)
        self.assertEqual(server.count("prune(&root).await;"), 1)


if __name__ == "__main__":
    unittest.main()
