"""Configuration and deterministic scenario contract tests."""

from pathlib import Path
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from cinema_plex_bench.cli import v1_coverage  # noqa: E402
from cinema_plex_bench.config import ConfigError, load_config, seek_targets  # noqa: E402
from cinema_plex_bench.harness import HarnessError, validate_run_readiness  # noqa: E402


class ConfigTests(unittest.TestCase):
    def test_committed_example_covers_every_v1_case(self):
        config = load_config(ROOT / "benchmarks/cinema-plex.example.toml")
        covered, missing = v1_coverage(config)
        self.assertFalse(missing)
        self.assertIn("1/2/4/8 simultaneous transcodes", covered)
        self.assertIn("50 deterministic/random seeks", covered)

    def test_random_seek_targets_are_stable_and_in_bounds(self):
        config = load_config(ROOT / "benchmarks/cinema-plex.example.toml")
        scenario = next(item for item in config.scenarios if item["id"] == "seek-random-50-direct")
        medium = config.media[scenario["media"]]
        first = seek_targets(scenario, medium)
        second = seek_targets(scenario, medium)
        self.assertEqual(first, second)
        self.assertEqual(50, len(first))
        self.assertTrue(all(30 <= item["target_seconds"] <= 1770 for item in first))
        self.assertEqual(list(range(50)), [item["seek_index"] for item in first])

    def test_example_validates_as_a_plan_but_cannot_run_with_placeholders(self):
        config = load_config(ROOT / "benchmarks/cinema-plex.example.toml")
        with self.assertRaisesRegex(HarnessError, "replace template placeholders"):
            validate_run_readiness(config)

    def test_embedded_server_credentials_are_rejected(self):
        source = (ROOT / "benchmarks/cinema-plex.example.toml").read_text(encoding="utf-8")
        source = source.replace(
            'base_url = "http://cinema-host:32400"',
            'base_url = "http://admin:password@cinema-host:32400"',
        )
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "bad.toml"
            path.write_text(source, encoding="utf-8")
            with self.assertRaisesRegex(ConfigError, "must not embed credentials"):
                load_config(path)

    def test_pause_resume_cannot_claim_a_server_only_frame(self):
        source = (ROOT / "benchmarks/cinema-plex.example.toml").read_text(encoding="utf-8")
        marker = 'id = "pause-resume-direct"'
        before, after = source.split(marker, 1)
        after = after.replace(
            'measurement_scopes = ["end_to_end"]',
            'measurement_scopes = ["server_engine", "end_to_end"]',
            1,
        )
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "bad.toml"
            path.write_text(before + marker + after, encoding="utf-8")
            with self.assertRaisesRegex(ConfigError, "end-to-end only"):
                load_config(path)


if __name__ == "__main__":
    unittest.main()
