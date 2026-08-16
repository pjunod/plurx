"""Server-adapter and resource-accounting contract tests."""

from pathlib import Path
import sys
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from urllib.parse import parse_qs, urlsplit


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from cinema_plex_bench.monitoring import resource_delta  # noqa: E402
from cinema_plex_bench.runners import (  # noqa: E402
    CinemaRunner,
    PlexRunner,
    build_runners,
)


MEDIUM = {
    "id": "fixture",
    "width": 3840,
    "height": 2160,
    "video_codec": "hevc",
    "audio_codec": "eac3",
    "bitrate_kbps": 60000,
    "bindings": {
        "cinema": {"file_id": 41},
        "plex": {"rating_key": "99", "media_index": 0, "part_index": 0},
    },
}

TRANSCODE = {
    "playback_mode": "transcode",
    "output_height": 1080,
    "output_bitrate_kbps": 8000,
}


class FakeCinemaHttp:
    def __init__(self):
        self.posts = []

    def json(self, url, **kwargs):
        if url.endswith("/api/v1/server"):
            return {"name": "lab", "version": "0.2.7", "build": "gabc", "instance_id": "i"}
        if "/decision?" in url:
            return {
                "source": {
                    "container": "mkv",
                    "video_codec": "hevc",
                    "bitrate": 60_000_000,
                    "width": 3840,
                    "height": 2160,
                    "duration_ms": 3_600_000,
                },
                "audio": [{"codec": "eac3"}],
            }
        self.posts.append((url, kwargs))
        return {
            "session_id": "session-id",
            "playlist_url": "/api/v1/hls/session-id/index.m3u8",
            "encoder": "qsv-h264",
            "media_origin_ms": 300000,
            "vod": False,
        }

    def request(self, *args, **kwargs):
        return b"", args[0], {}


class FakePlexHttp:
    def request(self, url, **kwargs):
        if url.endswith("/identity"):
            payload = b'<MediaContainer machineIdentifier="plex-id" version="1.41.0" />'
        elif "/library/metadata/99" in url:
            payload = b'''<MediaContainer><Video duration="3600000"><Media container="mkv"
                videoCodec="hevc" audioCodec="eac3" bitrate="60000" width="3840" height="2160">
                <Part key="/library/parts/7/file"><Stream streamType="2" codec="eac3" /></Part>
                </Media></Video></MediaContainer>'''
        else:
            payload = b""
        return payload, url, {}


class RunnerTests(unittest.TestCase):
    def test_cinema_runner_uses_session_start_and_capability_playlist(self):
        http = FakeCinemaHttp()
        runner = CinemaRunner(
            "cinema",
            {"base_url": "http://cinema:32400", "runner": "cinema"},
            "secret",
            http,
        )
        self.assertEqual("gabc", runner.identity()["server_commit"])
        self.assertEqual(60000, runner.media_info(MEDIUM)["source_bitrate_kbps"])
        stream = runner.prepare_stream(MEDIUM, TRANSCODE, 300, "trial")
        self.assertEqual("http://cinema:32400/api/v1/hls/session-id/index.m3u8", stream.url)
        self.assertEqual({}, stream.headers)
        body = http.posts[0][1]["json_body"]
        self.assertEqual(300, body["start"])
        self.assertEqual(1080, body["height"])

    def test_plex_runner_builds_forced_hls_without_putting_token_in_url(self):
        runner = PlexRunner(
            "plex",
            {"base_url": "http://plex:32400", "runner": "plex", "label": "Plex"},
            "top-secret",
            FakePlexHttp(),
        )
        self.assertEqual("1.41.0", runner.identity()["server_version"])
        self.assertEqual("/library/parts/7/file", runner.media_info(MEDIUM)["part_key"])
        stream = runner.prepare_stream(MEDIUM, TRANSCODE, 300, "trial")
        parsed = urlsplit(stream.url)
        query = parse_qs(parsed.query)
        self.assertEqual(["0"], query["directPlay"])
        self.assertEqual(["0"], query["directStream"])
        self.assertEqual(["300.000"], query["offset"])
        self.assertEqual(["1920x1080"], query["videoResolution"])
        self.assertNotIn("top-secret", stream.url)
        self.assertEqual("top-secret", stream.headers["X-Plex-Token"])
        self.assertIn("videoCodec=h264", stream.headers["X-Plex-Client-Profile-Extra"])
        self.assertIn("audioCodec=aac", stream.headers["X-Plex-Client-Profile-Extra"])

    def test_resource_counters_become_trial_deltas(self):
        before = {
            "cpu_percent": 20,
            "gpu_percent": 30,
            "rss_bytes": 100,
            "storage_read_bytes": 1000,
            "storage_write_bytes": 2000,
            "network_rx_bytes": 3000,
            "network_tx_bytes": 4000,
        }
        after = {
            "cpu_percent": 40,
            "gpu_percent": 50,
            "rss_bytes": 150,
            "storage_read_bytes": 1600,
            "storage_write_bytes": 2400,
            "network_rx_bytes": 3900,
            "network_tx_bytes": 4700,
        }
        measured = resource_delta(before, after)
        self.assertEqual(30, measured["cpu_percent"])
        self.assertEqual(40, measured["gpu_percent"])
        self.assertEqual(150, measured["rss_bytes"])
        self.assertEqual(600, measured["storage_read_bytes"])
        self.assertEqual(700, measured["network_tx_bytes"])
        self.assertEqual([], measured["resource_anomalies"])

    def test_counter_reset_is_null_and_explicit_instead_of_fabricated_zero(self):
        measured = resource_delta(
            {"storage_read_bytes": 1000},
            {"storage_read_bytes": 100},
        )
        self.assertIsNone(measured["storage_read_bytes"])
        self.assertIn("storage_read_bytes_counter_reset", measured["resource_anomalies"])

    def test_single_server_build_requires_only_the_selected_token(self):
        config = SimpleNamespace(
            servers={
                "cinema": {
                    "runner": "cinema",
                    "base_url": "http://cinema:32400",
                    "token_env": "ONLY_CINEMA_TOKEN",
                },
                "plex": {
                    "runner": "plex",
                    "base_url": "http://plex:32400",
                    "token_env": "MISSING_PLEX_TOKEN",
                },
            }
        )
        with patch.dict("os.environ", {"ONLY_CINEMA_TOKEN": "secret"}, clear=True):
            runners = build_runners(config, object(), {"cinema"})
        self.assertEqual(["cinema"], list(runners))


if __name__ == "__main__":
    unittest.main()
