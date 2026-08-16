"""Raw-schema, percentile, failure-rate, and A/B ratio tests."""

import json
from pathlib import Path
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from cinema_plex_bench.harness import (  # noqa: E402
    HarnessError,
    RAW_REQUIRED_FIELDS,
    validate_measurement_row,
)
from cinema_plex_bench.reporting import (  # noqa: E402
    ReportError,
    load_rows,
    markdown_report,
    percentile,
    summarize,
    write_report,
)


def row(server, runner, latency, success=True):
    document = {
        "schema_version": 2,
        "record_type": "measurement",
        "server": server,
        "server_runner": runner,
        "server_version": "1.2.3",
        "server_commit": "abc123",
        "client": "ffmpeg",
        "media": "fixture",
        "playback_mode": "transcode",
        "source_video_codec": "hevc",
        "source_audio_codec": "eac3",
        "source_bitrate_kbps": 60000,
        "output_video_codec": "h264",
        "output_audio_codec": "aac",
        "output_bitrate_kbps": 8000,
        "output_height": 1080,
        "output_bitrate_basis": "requested_transcode_contract",
        "advertised_output_bitrate_kbps": 8160,
        "advertised_output_codecs": "avc1.640028,mp4a.40.2",
        "advertised_output_resolution": "1920x1080",
        "output_contract_verified": True,
        "output_contract_basis": "fixture",
        "scenario_id": "startup-4k-to-1080p-transcode",
        "operation": "startup",
        "operation_variant": "startup",
        "measurement_scope": "end_to_end",
        "concurrency": 1,
        "latency_ms": latency,
        "cpu_percent": None,
        "gpu_percent": None,
        "rss_bytes": None,
        "storage_read_bytes": None,
        "storage_write_bytes": None,
        "network_rx_bytes": None,
        "network_tx_bytes": None,
        "resource_anomalies": [],
        "pair_sequence": 0,
        "pair_server_order": ["cinema", "plex"],
        "server_order_index": 0 if server == "cinema" else 1,
        "success": success,
    }
    validate_measurement_row(document)
    return document


class ReportingTests(unittest.TestCase):
    def test_type_seven_percentiles(self):
        values = [100, 200, 300, 400]
        self.assertEqual(250, percentile(values, 50))
        self.assertEqual(385, percentile(values, 95))
        self.assertEqual(397, percentile(values, 99))
        self.assertIsNone(percentile([], 95))

    def test_summary_uses_successes_for_latency_and_all_attempts_for_failures(self):
        rows = [
            row("cinema", "cinema", 100),
            row("cinema", "cinema", 200),
            row("cinema", "cinema", None, False),
            row("plex", "plex", 200),
            row("plex", "plex", 400),
        ]
        summary = summarize(rows)
        cinema = next(group for group in summary["groups"] if group["server"] == "cinema")
        self.assertEqual(150, cinema["latency_ms"]["p50"])
        self.assertEqual(1 / 3, cinema["failure_rate"])
        comparison = summary["comparisons"][0]
        self.assertEqual(0.5, comparison["cinema_to_plex_latency_ratio"]["p50"])
        self.assertEqual(2.0, comparison["plex_over_cinema_speedup"]["p50"])
        self.assertAlmostEqual(1 / 3, comparison["failure_rate_difference"])

    def test_raw_contract_rejects_a_missing_resource_field(self):
        document = row("cinema", "cinema", 10)
        document.pop("gpu_percent")
        with self.assertRaisesRegex(HarnessError, "gpu_percent"):
            validate_measurement_row(document)
        self.assertIn("network_tx_bytes", RAW_REQUIRED_FIELDS)

    def test_report_round_trip_writes_json_and_markdown(self):
        rows = [row("cinema", "cinema", 100), row("plex", "plex", 200)]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            raw = root / "raw.jsonl"
            raw.write_text("".join(json.dumps(item) + "\n" for item in rows), encoding="utf-8")
            summary = write_report(raw, root / "summary.json", root / "report.md")
            self.assertEqual(2, summary["measurement_rows"])
            report = (root / "report.md").read_text(encoding="utf-8")
            self.assertIn("0.50×", report)
            self.assertIn("Server/engine rows", report)
            parsed = json.loads((root / "summary.json").read_text(encoding="utf-8"))
            self.assertEqual(1, len(parsed["comparisons"]))

    def test_markdown_does_not_invent_a_ratio_when_one_side_failed(self):
        summary = summarize([row("cinema", "cinema", None, False), row("plex", "plex", 200)])
        self.assertIn("| — | — | — |", markdown_report(summary))

    def test_unverified_output_contract_suppresses_ratios(self):
        cinema = row("cinema", "cinema", 100)
        cinema["output_contract_verified"] = False
        summary = summarize([cinema, row("plex", "plex", 200)])
        self.assertEqual([], summary["comparisons"])

    def test_successful_nonfinite_latency_is_rejected(self):
        document = row("cinema", "cinema", 10)
        document["latency_ms"] = float("nan")
        with self.assertRaisesRegex(HarnessError, "finite non-negative"):
            validate_measurement_row(document)

    def test_schema_one_rows_have_an_explicit_version_boundary(self):
        document = row("cinema", "cinema", 10)
        document["schema_version"] = 1
        with tempfile.TemporaryDirectory() as directory:
            raw = Path(directory) / "old.jsonl"
            raw.write_text(json.dumps(document) + "\n", encoding="utf-8")
            with self.assertRaisesRegex(ReportError, "unsupported schema"):
                load_rows(raw)


if __name__ == "__main__":
    unittest.main()
