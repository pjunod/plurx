from __future__ import annotations

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
PROVE = ROOT / "scripts/prove-fix"

# Records the CARGO_TARGET_DIR each proof phase actually sees, then writes an
# artifact into it the way a real cargo build would.
RECORDING_PROOF = (
    "import os\n"
    "from pathlib import Path\n"
    "target = os.environ.get('CARGO_TARGET_DIR', '')\n"
    "with Path(os.environ['PROVE_FIX_RECORD']).open('a', encoding='utf-8') as handle:\n"
    "    handle.write(target + '\\n')\n"
    "if target:\n"
    "    Path(target).mkdir(parents=True, exist_ok=True)\n"
    "    (Path(target) / 'artifact').write_text('built\\n', encoding='utf-8')\n"
    "raise SystemExit(0 if Path('src/value.txt').read_text() == 'after\\n' else 1)\n"
)


class ProveFixHarness:
    """Builds a throwaway repository whose one test is sensitive to the fix."""

    def repository(self) -> tuple[tempfile.TemporaryDirectory[str], Path, str]:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        (root / "src").mkdir()
        (root / "checks").mkdir()
        (root / "src/value.txt").write_text("before\n", encoding="utf-8")
        (root / "checks/proof.py").write_text(
            "from pathlib import Path\n"
            "raise SystemExit(0 if Path('src/value.txt').read_text() == 'after\\n' else 1)\n",
            encoding="utf-8",
        )
        subprocess.run(["git", "init", "-q"], cwd=root, check=True)
        subprocess.run(["git", "config", "user.name", "Proof Test"], cwd=root, check=True)
        subprocess.run(
            ["git", "config", "user.email", "proof@example.invalid"],
            cwd=root,
            check=True,
        )
        subprocess.run(["git", "add", "-A"], cwd=root, check=True)
        subprocess.run(["git", "commit", "-qm", "seed"], cwd=root, check=True)
        base = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            cwd=root,
            check=True,
            text=True,
            stdout=subprocess.PIPE,
        ).stdout.strip()
        (root / "src/value.txt").write_text("after\n", encoding="utf-8")
        subprocess.run(["git", "add", "-A"], cwd=root, check=True)
        subprocess.run(["git", "commit", "-qm", "fix: change value"], cwd=root, check=True)
        return temporary, root, base

    def invoke(
        self,
        root: Path,
        base: str,
        *,
        environment: dict[str, str] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                str(PROVE),
                "--repo",
                str(root),
                "--command",
                "python3 checks/proof.py",
                base,
                "-",
                "src/value.txt",
            ],
            cwd=root,
            env=environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )


class ProveFixCase(ProveFixHarness, unittest.TestCase):
    def test_sensitive_test_passes_the_revert_protocol(self) -> None:
        _, root, base = self.repository()

        result = self.invoke(root, base)

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("retained test rejects", result.stdout)
        self.assertEqual((root / "src/value.txt").read_text(encoding="utf-8"), "after\n")

    def test_vacuous_test_is_rejected_even_when_it_was_edited(self) -> None:
        _, root, base = self.repository()
        (root / "checks/proof.py").write_text("raise SystemExit(0)\n", encoding="utf-8")
        subprocess.run(["git", "add", "-A"], cwd=root, check=True)
        subprocess.run(["git", "commit", "-qm", "test: neuter proof"], cwd=root, check=True)

        result = self.invoke(root, base)

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("test stayed green", result.stdout)


class ProveFixTargetIsolationCase(ProveFixHarness, unittest.TestCase):
    def recording_run(
        self, inherited: str | None
    ) -> tuple[Path, list[str], subprocess.CompletedProcess[str]]:
        _, root, base = self.repository()
        (root / "checks/proof.py").write_text(RECORDING_PROOF, encoding="utf-8")
        subprocess.run(["git", "add", "-A"], cwd=root, check=True)
        subprocess.run(["git", "commit", "-qm", "test: record target dir"], cwd=root, check=True)
        record = root / "target-dirs.txt"

        environment = os.environ.copy()
        environment["PROVE_FIX_RECORD"] = str(record)
        if inherited is None:
            environment.pop("CARGO_TARGET_DIR", None)
        else:
            environment["CARGO_TARGET_DIR"] = inherited

        result = self.invoke(root, base, environment=environment)
        self.assertEqual(result.returncode, 0, result.stdout)
        observed = record.read_text(encoding="utf-8").split()
        self.assertEqual(len(observed), 2, result.stdout)
        return root, observed, result

    def test_inherited_target_dir_is_displaced_by_a_sibling(self) -> None:
        # Both proof phases must build somewhere the ordinary gate never reads;
        # otherwise the reverted-source build is left newer than the corrected
        # checkout and cargo serves it to the next `make validate`.
        inherited = Path(tempfile.mkdtemp())
        self.addCleanup(subprocess.run, ["rm", "-rf", str(inherited)])
        expected = inherited.with_name(f"{inherited.name}-prove-fix")
        self.addCleanup(subprocess.run, ["rm", "-rf", str(expected)])

        _, observed, result = self.recording_run(str(inherited))

        self.assertEqual(observed, [str(expected)] * 2, result.stdout)
        self.assertTrue((expected / "artifact").is_file(), result.stdout)
        self.assertEqual(list(inherited.iterdir()), [], "prove-fix wrote into the inherited dir")

    def test_absent_inherited_target_dir_still_yields_a_disposable_one(self) -> None:
        _, observed, result = self.recording_run(None)

        self.assertNotEqual(observed[0], "", result.stdout)
        self.assertEqual(observed, [observed[0]] * 2, result.stdout)
        self.assertIn("plurx-prove-fix-", observed[0], result.stdout)
        self.assertFalse(Path(observed[0]).exists(), "disposable target dir outlived the clone")


if __name__ == "__main__":
    unittest.main()
