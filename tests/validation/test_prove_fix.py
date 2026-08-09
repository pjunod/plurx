from __future__ import annotations

from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
PROVE = ROOT / "scripts/prove-fix"


class ProveFixCase(unittest.TestCase):
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

    def invoke(self, root: Path, base: str) -> subprocess.CompletedProcess[str]:
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
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )

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


if __name__ == "__main__":
    unittest.main()
