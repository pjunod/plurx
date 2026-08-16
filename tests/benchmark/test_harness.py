"""Adversarial orchestration and output-contract regression tests."""

from pathlib import Path
import sys
import unittest


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from cinema_plex_bench.harness import (  # noqa: E402
    BenchmarkHarness,
    HarnessError,
    balanced_server_order,
    validate_output_contract,
)
from cinema_plex_bench.runners import StreamHandle  # noqa: E402


def transcode_stream(advertised_bitrate=8160):
    return StreamHandle(
        url="http://server/output.m3u8",
        headers={},
        session_id="session",
        client_seek_seconds=0,
        playback_mode="transcode",
        output_video_codec="h264",
        output_audio_codec="aac",
        output_bitrate_kbps=8000,
        output_height=1080,
        details={
            "advertised_output_bitrate_kbps": advertised_bitrate,
            "output_contract_basis": "fixture",
        },
    )


def scenario(output_bitrate=8000):
    return {
        "id": "transcode-fixture",
        "playback_mode": "transcode",
        "output_height": 1080,
        "output_bitrate_kbps": output_bitrate,
        "output_bitrate_tolerance_percent": 5,
    }


PROBE = {
    "probed_output_video_codec": "h264",
    "probed_output_audio_codec": "aac",
    "probed_output_width": 1920,
    "probed_output_height": 1080,
}


class HarnessTests(unittest.TestCase):
    def test_fifty_pairs_balance_first_server_exactly(self):
        orders = [balanced_server_order(["cinema", "plex"], index) for index in range(50)]
        self.assertEqual(25, sum(order[0] == "cinema" for order in orders))
        self.assertEqual(25, sum(order[0] == "plex" for order in orders))
        self.assertEqual(["cinema", "plex"], orders[0])
        self.assertEqual(["plex", "cinema"], orders[1])

    def test_matching_transcode_contract_is_verified(self):
        contract = validate_output_contract(
            "cinema", scenario(), transcode_stream(), {}, PROBE
        )
        self.assertTrue(contract["output_contract_verified"])
        self.assertEqual(8160, contract["advertised_output_bitrate_kbps"])
        self.assertEqual("h264", contract["output_video_codec"])

    def test_cinema_fixed_rung_cannot_be_compared_to_a_lower_plex_request(self):
        with self.assertRaisesRegex(HarnessError, "advertised 8160.*expected 4000"):
            validate_output_contract(
                "cinema",
                scenario(output_bitrate=4000),
                transcode_stream(),
                {},
                PROBE,
            )

    def test_wrong_delivered_codec_fails_the_preflight(self):
        probe = {**PROBE, "probed_output_video_codec": "hevc"}
        with self.assertRaisesRegex(HarnessError, "expected h264/aac"):
            validate_output_contract(
                "plex", scenario(), transcode_stream(), {}, probe
            )

    def test_runtime_contract_mismatch_clears_the_verified_flag(self):
        row = {
            "server": "cinema",
            "details": {},
            "output_contract_verified": True,
        }
        with self.assertRaisesRegex(HarnessError, "advertised 12000"):
            BenchmarkHarness._apply_runtime_contract(
                row, transcode_stream(advertised_bitrate=12000), scenario()
            )
        self.assertFalse(row["output_contract_verified"])


if __name__ == "__main__":
    unittest.main()
