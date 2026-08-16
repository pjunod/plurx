"""Configuration and deterministic scenario contract tests."""

from pathlib import Path
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from cinema_plex_bench.cli import v1_coverage  # noqa: E402
from cinema_plex_bench.config import ConfigError, load_config, seek_targets  # noqa: E402
from cinema_plex_bench.harness import (  # noqa: E402
    HarnessError,
    safe_manifest_config,
    validate_cold_readiness,
    validate_run_readiness,
    validate_selection,
)


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

    def test_binding_shape_must_match_the_configured_runner(self):
        source = (ROOT / "benchmarks/cinema-plex.example.toml").read_text(encoding="utf-8")
        source = source.replace("file_id = 1001", 'rating_key = "wrong-shape"', 1)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "bad.toml"
            path.write_text(source, encoding="utf-8")
            with self.assertRaisesRegex(ConfigError, "unknown field.*rating_key"):
                load_config(path)

    def test_plaintext_secret_fields_are_rejected(self):
        source = (ROOT / "benchmarks/cinema-plex.example.toml").read_text(encoding="utf-8")
        source = source.replace(
            'token_env = "PLEX_BENCH_TOKEN"',
            'token_env = "PLEX_BENCH_TOKEN"\ntoken = "plaintext-secret"',
            1,
        )
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "bad.toml"
            path.write_text(source, encoding="utf-8")
            with self.assertRaisesRegex(ConfigError, "unknown field.*token"):
                load_config(path)

    def test_manifest_redacts_every_hook_argument(self):
        source = (ROOT / "benchmarks/cinema-plex.example.toml").read_text(encoding="utf-8")
        source = source.replace(
            'token_env = "CINEMA_BENCH_TOKEN"',
            'token_env = "CINEMA_BENCH_TOKEN"\n'
            'monitor_command = ["sampler", "--password=plaintext-secret"]',
            1,
        )
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "safe.toml"
            path.write_text(source, encoding="utf-8")
            document = safe_manifest_config(load_config(path))
        rendered = str(document)
        self.assertNotIn("plaintext-secret", rendered)
        self.assertEqual(
            {"configured": True, "argument_count": 2, "argv_redacted": True},
            document["servers"]["cinema"]["monitor_command"],
        )

    def test_unknown_or_empty_selectors_fail_before_a_run(self):
        config = load_config(ROOT / "benchmarks/cinema-plex.example.toml")
        with self.assertRaisesRegex(ConfigError, "unknown --scenario"):
            validate_selection(config, None, {"typo"})
        with self.assertRaisesRegex(ConfigError, "must not be empty"):
            validate_selection(config, set(), None)

    def test_cold_scenario_without_hooks_fails_before_preflight(self):
        source = (ROOT / "benchmarks/cinema-plex.example.toml").read_text(encoding="utf-8")
        source = source.replace('cache_state = "warm"', 'cache_state = "cold"', 1)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "cold.toml"
            path.write_text(source, encoding="utf-8")
            config = load_config(path)
        with self.assertRaisesRegex(HarnessError, "requires before_trial_command"):
            validate_cold_readiness(config, None, {"startup-1080p-direct"})


if __name__ == "__main__":
    unittest.main()
