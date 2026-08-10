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
        self.assertIn("name: report-only weekly mutation spot-check", workflow)
        self.assertIn("name: nightly-mutation-evidence", workflow)

    def test_pgs_fuzzer_is_bounded_seeded_artifacted_and_gating(self) -> None:
        workflow = self.read(".github/workflows/validation-nightly.yml")
        self.assertIn(
            "cargo +nightly-2026-08-01 fuzz run inspect_sup fuzz/corpus/inspect_sup",
            workflow,
        )
        self.assertIn("-max_total_time=900", workflow)
        self.assertIn("fuzz/artifacts", workflow)
        self.assertIn("steps.pgs_fuzz.outcome == 'failure'", workflow)
        self.assertIn("name: bounded PGS parser fuzz campaign", workflow)
        self.assertIn("name: nightly-pgs-fuzz-evidence", workflow)
        self.assertIn("target/validation/pgs-fuzz.log", workflow)
        self.assertIn("seed_pgs_crash:", workflow)
        self.assertIn("PLURX_FUZZ_SEEDED_CRASH", workflow)
        self.assertIn(
            'seeded_crash_enabled(std::env::var_os("PLURX_FUZZ_SEEDED_CRASH").as_deref())',
            self.read("fuzz/fuzz_targets/inspect_sup.rs"),
        )
        self.assertIn(
            "only_the_explicit_seed_enables_the_crash_proof",
            self.read("fuzz/src/lib.rs"),
        )
        self.assertIn("test --manifest-path fuzz/Cargo.toml --lib", workflow)
        self.assertIn("exit 1", workflow)
        self.assertTrue((ROOT / "fuzz/corpus/inspect_sup/minimal-header").is_file())

    def test_nightly_deep_fuzz_and_mutation_jobs_are_independent(self) -> None:
        workflow = self.read(".github/workflows/validation-nightly.yml")
        deep = workflow.split("\n  deep-validation:\n", 1)[1].split("\n  pgs-fuzz:\n", 1)[0]
        fuzz = workflow.split("\n  pgs-fuzz:\n", 1)[1].split("\n  mutation:\n", 1)[0]
        mutation = workflow.split("\n  mutation:\n", 1)[1]

        self.assertIn("run: make validate-nightly", deep)
        self.assertNotIn("cargo-fuzz", deep)
        self.assertNotIn("cargo-mutants", deep)
        self.assertIn("cargo-fuzz", fuzz)
        self.assertNotIn("needs:", fuzz)
        self.assertIn("cargo-mutants", mutation)
        self.assertNotIn("needs:", mutation)

    def test_pgs_periodic_refresh_and_completion_prune_remain_wired(self) -> None:
        apple = self.read("clients/apple/Sources/PlayerController.swift")
        server = self.read("crates/plurxd/src/pgs_overlay.rs")
        self.assertIn("PGSOverlayPolicy.periodicRefreshPosition", apple)
        self.assertIn("self.refreshPGSOverlayWindow(at: overlayPosition)", apple)
        self.assertEqual(server.count("prune(&root).await;"), 1)

    def test_media_origin_and_remote_seek_consumption_remain_wired(self) -> None:
        android = self.read("clients/android/app/src/main/java/tv/plurx/app/player/Controller.kt")
        android_screen = self.read(
            "clients/android/app/src/main/java/tv/plurx/app/player/PlayerScreen.kt"
        )
        apple = self.read("clients/apple/Sources/PlayerController.swift")
        apple_view = self.read("clients/apple/Sources/PlayerView.swift")
        hls = self.read("crates/plurxd/src/http/hls.rs")

        self.assertIn("return realMediaPositionMs(", android)
        self.assertIn("val timeline = sessionPlaybackTimeline(hls, requestedStartMs = ms)", android)
        self.assertIn(".setTransferListener(progressiveMediaOrigin)", android)
        self.assertIn("HiddenSeekAccumulator(plan.durationMs)", android_screen)
        self.assertIn("hiddenSeekAccumulator.consume()?.let(controller::seekTo)", android_screen)
        self.assertIn("nextBaseMs = Self.sessionMediaOriginMs(hls, requestedStartMs: startMs)", apple)

        remote_seek = apple_view.split("private func seekFromRemote", 1)[1].split("#endif", 1)[0]
        self.assertLess(remote_seek.index("controller.skip"), remote_seek.index("revealControlsFromRemote"))
        self.assertIn("skipped Apple HEVC tier normalization", hls)


if __name__ == "__main__":
    unittest.main()
