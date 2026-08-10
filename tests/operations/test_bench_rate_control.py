"""Contract tests for the Performance II N1 live-session harness."""

import hashlib
import json
import os
from pathlib import Path
import runpy
from types import SimpleNamespace
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
BENCH = runpy.run_path(str(ROOT / "scripts/bench"))
G = BENCH["rate_control_report"].__globals__
MISSING = object()


def file_sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def write_corpus(
    root,
    *,
    purpose="n1_acceptance",
    classes=("easy", "hard"),
    pinned=True,
    dynamic_range="sdr",
):
    fixtures = []
    references = {}
    for index, content_class in enumerate(classes):
        filename = f"{content_class}-{index}.mkv"
        reference = root / filename
        # Equal sizes deliberately keep size-only identity from self-certifying.
        reference.write_bytes(f"fixture-{content_class}-{index}".ljust(32, "!").encode())
        references[filename] = reference
        fixtures.append({
            "identity": f"{content_class}-{index}",
            "class": content_class,
            "dynamic_range": dynamic_range,
            "filename": filename,
            "reference": filename,
            "reference_sha256": file_sha(reference) if pinned else None,
            "trim": {"start_seconds": 2, "duration_seconds": 6},
            "rung": 1080,
        })
    path = root / "corpus.json"
    path.write_text(json.dumps({
        "version": 1,
        "purpose": purpose,
        "fixtures": fixtures,
    }), encoding="utf-8")
    return path, references


def write_server_manifest(root, references, overrides=None):
    overrides = overrides or {}
    path = root / "nynuc.sha256"
    lines = []
    for filename, reference in references.items():
        digest = overrides.get(filename, file_sha(reference))
        lines.append(f"{digest}  /srv/bench-media/{filename}")
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return path


class SessionApi:
    def __init__(
        self,
        *,
        vod=False,
        origin_ms=2000,
        unsafe=False,
        dynamic_range="sdr",
        start_encoder="qsv-h264",
        status_encoder="qsv-h264",
        recent_speed=2.0,
        delete_error=False,
        stuck_after_delete=False,
        initially_active=0,
    ):
        self.vod = vod
        self.origin_ms = origin_ms
        self.unsafe = unsafe
        self.dynamic_range = dynamic_range
        self.start_encoder = start_encoder
        self.status_encoder = status_encoder
        self.recent_speed = recent_speed
        self.delete_error = delete_error
        self.stuck_after_delete = stuck_after_delete
        self.active = initially_active
        self.deleted = False
        self.posted = False

    def call(self, path, method="GET", body=None):
        if path == "/system":
            return {"active_transcodes": self.active}
        if method == "POST":
            self.posted = True
            self.active = 1
            self.start_body = body
            return {
                "session_id": "session-capability",
                "encoder": self.start_encoder,
                "vod": self.vod,
                "media_origin_ms": self.origin_ms,
                "delivered_dynamic_range": self.dynamic_range,
                "ladder": [{"height": 1080, "total_kbps": 8160, "peak_kbps": 12160}],
            }
        if path.endswith("/status"):
            return {"recent_speed": self.recent_speed, "encoder": self.status_encoder}
        if method == "DELETE":
            if self.delete_error:
                raise RuntimeError("DELETE failed")
            self.deleted = True
            if not self.stuck_after_delete:
                self.active = 0
            return None
        raise AssertionError((method, path, body))

    def fetch(self, path):
        if path.endswith("index.m3u8"):
            first = "../server-secret.ts" if self.unsafe else "source-a.ts"
            return (
                "#EXTM3U\n"
                f"#EXTINF:2.0,\n{first}\n"
                "#EXTINF:2.0,\nsource-b.ts\n"
                "#EXTINF:2.0,\nsource-c.ts\n"
            ).encode()
        return ("served:" + path).encode()


class FullApi:
    def __init__(
        self,
        references,
        *,
        hdr=None,
        probed=True,
        available=True,
        fact_overrides=None,
        active_counts=None,
        default_active=0,
        selected_encoder="qsv-h264",
    ):
        self.references = references
        self.hdr = hdr
        self.probed = probed
        self.available = available
        self.fact_overrides = fact_overrides or {}
        self.active_counts = list(active_counts or [])
        self.default_active = default_active
        self.selected_encoder = selected_encoder
        self.settings = {
            "transcode_rate_mode": "bitrate",
            "transcode_quality": None,
            "api_secret": "must-not-leak",
        }
        self.puts = []

    def call(self, path, method="GET", body=None):
        if path == "/system":
            active = self.active_counts.pop(0) if self.active_counts else self.default_active
            return {
                "name": "nynuc",
                "instance_id": "server-id",
                "version": "0.9.0",
                "build": "server-build",
                "built_at": "2026-08-09",
                "ffmpeg": "/usr/lib/jellyfin-ffmpeg/ffmpeg",
                "ffmpeg_version": "jellyfin-ffmpeg version production",
                "encoder_selected": self.selected_encoder,
                "active_transcodes": active,
            }
        if path == "/libraries":
            return [{"id": 9}]
        if path == "/libraries/9/items":
            return {"items": [{"id": 4}]}
        if path == "/items/4":
            files = []
            for index, (filename, reference) in enumerate(self.references.items(), 7):
                facts = {
                    "id": index,
                    "filename": filename,
                    "size": reference.stat().st_size,
                    "probed": self.probed,
                    "available": self.available,
                    "duration_ms": 60_000,
                    "container": "matroska",
                    "video_codec": "h264",
                    "width": 1920,
                    "height": 1080,
                    "bit_depth": 8,
                }
                if self.hdr is not MISSING:
                    facts["hdr"] = self.hdr
                for key, value in self.fact_overrides.items():
                    if value is MISSING:
                        facts.pop(key, None)
                    else:
                        facts[key] = value
                files.append(facts)
            return {"files": files}
        if path == "/settings" and method == "GET":
            return dict(self.settings)
        if path == "/settings" and method == "PUT":
            self.puts.append(dict(body))
            self.settings.update(body)
            return dict(self.settings)
        raise AssertionError((method, path, body))


def harness_args(root, corpus, server_manifest=None, *, modes="vbr,qvbr", only=None):
    return SimpleNamespace(
        modes=modes,
        vmaf_model="vmaf_v0.6.1",
        vmaf_subsample=1,
        rate_window=10.0,
        poll=0.001,
        capture_timeout=1.0,
        idle_timeout=0.005,
        settings_settle=0,
        corpus=str(corpus),
        server_sha256_manifest=str(server_manifest) if server_manifest else None,
        only=only,
        vmaf_ffmpeg="scorer-ffmpeg",
        base="http://admin:password@nynuc:32400?token=nope",
        token="top-secret-token",
        library=None,
        work_dir=str(root / "work"),
        json=str(root / "result.json"),
        quality=22,
        keep_artifacts=True,
    )


def passing_measurement(mode, encoder="qsv-h264"):
    return {
        "production_encoder": encoder,
        "start_response_encoder": encoder,
        "status_encoder": encoder,
        "delivered_dynamic_range": "sdr",
        "captured_media_seconds": 6.0,
        "bytes": 900 if mode == "qvbr" else 1000,
        "speed_p10": 1.9 if mode == "qvbr" else 2.0,
        "speed_p50": 2.0,
        "vmaf": 95.0,
        "peak_window_kbps": 100.0,
        "limits": {
            "theoretical_vbv_allowance_kbps": 90.0,
            "theoretical_vbv_role": "nonbinding_diagnostic_only",
            "advertised_peak_kbps": 12160.0,
            "binding_gate": "advertised_peak_kbps",
        },
    }


def patched_harness(measure=passing_measurement):
    real_sha = BENCH["sha256_file"]

    def capture(_api, fixture, mode, *_args):
        return {"mode": mode, "fixture": fixture}

    def measured(capture_result, *_args):
        return measure(capture_result["mode"])

    return {
        "scoring_executable": lambda _path: "/scoring/ffmpeg",
        "probe_vmaf_scorer": lambda *_args: {"score": 100.0, "diagnostics": ["libvmaf"]},
        "libvmaf_fingerprint": lambda *_args: {"filter_help_sha256": "a" * 64},
        "tool_build": lambda _path: "ffmpeg scoring build",
        "scorer_build_configuration": lambda _path: "configuration: --enable-libvmaf",
        "sha256_file": lambda path: "b" * 64 if str(path) == "/scoring/ffmpeg" else real_sha(path),
        "capture_disk_preflight": lambda *_args: {"free_bytes_before": 1_000_000_000},
        "capture_live_session": capture,
        "measured_capture": measured,
    }


def capture_fixture():
    return {
        "identity": "easy", "file_id": 7, "rung": 1080,
        "start_seconds": 2.0, "duration_seconds": 6.0,
    }


class RateControlBenchCase(unittest.TestCase):
    def test_full_manifest_binds_its_sha_and_requires_balanced_pinned_sdr_halves(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path, _ = write_corpus(root)
            loaded = BENCH["load_rate_control_corpus"](path)
            self.assertEqual(loaded["manifest_sha256"], file_sha(path))
            self.assertEqual([fixture["class"] for fixture in loaded["fixtures"]], ["easy", "hard"])
            self.assertTrue(all(fixture["reference_sha256_pinned"] for fixture in loaded["fixtures"]))
            self.assertTrue(all(fixture["dynamic_range"] == "sdr" for fixture in loaded["fixtures"]))

    def test_full_manifest_rejects_minimal_unbalanced_and_null_hash_corpora(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            minimal, _ = write_corpus(root, classes=("easy",))
            with self.assertRaisesRegex(BENCH["BenchError"], "equal easy and hard"):
                BENCH["load_rate_control_corpus"](minimal)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            unpinned, _ = write_corpus(root, pinned=False)
            with self.assertRaisesRegex(BENCH["BenchError"], "pinned reference_sha256"):
                BENCH["load_rate_control_corpus"](unpinned)

    def test_manifest_rejects_unknown_dynamic_range_and_wrong_reference_sha(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path, _ = write_corpus(root, dynamic_range=None)
            with self.assertRaisesRegex(BENCH["BenchError"], "dynamic_range"):
                BENCH["load_rate_control_corpus"](path)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path, _ = write_corpus(root)
            document = json.loads(path.read_text())
            document["fixtures"][0]["reference_sha256"] = "0" * 64
            path.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaisesRegex(BENCH["BenchError"], "mismatch"):
                BENCH["load_rate_control_corpus"](path)

    def test_manifest_rejects_nonfinite_trim_numbers(self):
        for field, value, message in (
            ("start_seconds", float("nan"), "start_seconds"),
            ("start_seconds", float("inf"), "start_seconds"),
            ("duration_seconds", float("nan"), "positive number"),
            ("duration_seconds", float("inf"), "positive number"),
        ):
            with self.subTest(field=field, value=value), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                path, _ = write_corpus(root)
                document = json.loads(path.read_text())
                document["fixtures"][0]["trim"][field] = value
                path.write_text(json.dumps(document), encoding="utf-8")
                with self.assertRaisesRegex(BENCH["BenchError"], message):
                    BENCH["load_rate_control_corpus"](path)

    def test_checked_in_manifest_is_explicitly_smoke_only(self):
        path = ROOT / "scripts/perf2-rate-control-smoke-corpus.json"
        document = json.loads(path.read_text())
        self.assertEqual(document["purpose"], "vbr_smoke")
        self.assertEqual({fixture["class"] for fixture in document["fixtures"]}, {"easy", "hard"})
        self.assertTrue(all(fixture["dynamic_range"] == "sdr" for fixture in document["fixtures"]))
        self.assertNotIn("ffmpeg_args", path.read_text())

    def test_server_sha256_manifest_is_fail_closed_and_self_identifying(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _, references = write_corpus(root)
            path = write_server_manifest(root, references)
            loaded = BENCH["load_server_sha256_manifest"](path)
            self.assertEqual(loaded["manifest_sha256"], file_sha(path))
            self.assertEqual(loaded["by_filename"]["easy-0.mkv"], file_sha(references["easy-0.mkv"]))
            path.write_text("not sha256sum\n", encoding="utf-8")
            with self.assertRaisesRegex(BENCH["BenchError"], "invalid sha256sum"):
                BENCH["load_server_sha256_manifest"](path)

    def test_full_comparison_requires_server_manifest_and_rejects_same_size_wrong_digest(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            corpus, references = write_corpus(root)
            report = BENCH["rate_control_report"](
                harness_args(root, corpus, None), api=mock.Mock()
            )
            self.assertIn("--server-sha256-manifest", report["failures"][0]["detail"])
            wrong = write_server_manifest(
                root, references, {"easy-0.mkv": file_sha(references["hard-1.mkv"])}
            )
            report = BENCH["rate_control_report"](
                harness_args(root, corpus, wrong), api=mock.Mock()
            )
            self.assertIn("differs from pinned", report["failures"][0]["detail"])

    def test_explicit_scorer_ignores_production_ffmpeg_environment(self):
        with tempfile.TemporaryDirectory() as directory:
            scorer = Path(directory) / "scorer"
            scorer.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            scorer.chmod(0o755)
            with mock.patch.dict(os.environ, {"PLURX_FFMPEG": "/production/ffmpeg"}):
                self.assertEqual(BENCH["scoring_executable"](str(scorer)), str(scorer.resolve()))

    def test_vmaf_probe_requires_exactly_one_finite_score(self):
        for stderr, message in (
            ("libvmaf active\n", "0 scores"),
            ("VMAF score: 99\nVMAF score: 100\n", "2 scores"),
            ("VMAF score: nan\n", "non-finite"),
        ):
            with self.subTest(stderr=stderr):
                result = SimpleNamespace(returncode=0, stdout="", stderr=stderr)
                with mock.patch.dict(G, {"sh": mock.Mock(return_value=result)}):
                    with self.assertRaisesRegex(BENCH["BenchError"], message):
                        BENCH["probe_vmaf_scorer"]("/scoring/ffmpeg", "vmaf_v0.6.1")

    def test_vmaf_measurement_requires_exactly_one_finite_score(self):
        for stderr, message in (
            ("libvmaf active\n", "0 scores"),
            ("VMAF score: 99\nVMAF score: 100\n", "2 scores"),
            ("VMAF score: inf\n", "non-finite"),
        ):
            with self.subTest(stderr=stderr):
                result = SimpleNamespace(returncode=0, stdout="", stderr=stderr)
                with mock.patch.dict(G, {"sh": mock.Mock(return_value=result)}):
                    with self.assertRaisesRegex(BENCH["BenchError"], message):
                        BENCH["score_vmaf"](
                            "/scoring/ffmpeg", "capture.m3u8", "reference.mkv",
                            2.0, 6.0, "vmaf_v0.6.1", 1,
                        )

    def test_failed_scorer_probe_precedes_every_server_action(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            corpus, _ = write_corpus(root, purpose="vbr_smoke", pinned=False)
            api = mock.Mock()
            patches = {
                "scoring_executable": lambda _path: "/scoring/ffmpeg",
                "probe_vmaf_scorer": mock.Mock(side_effect=BENCH["BenchError"]("0 scores")),
            }
            with mock.patch.dict(G, patches):
                report = BENCH["rate_control_report"](
                    harness_args(root, corpus, modes="vbr"), api=api
                )
            self.assertFalse(report["passed"])
            api.call.assert_not_called()

    def test_full_acceptance_requires_canonical_model_and_ten_second_window(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            corpus, references = write_corpus(root)
            server_manifest = write_server_manifest(root, references)
            cases = (
                ("vmaf_model", "other-model", "vmaf_v0.6.1 exactly"),
                ("rate_window", 12.0, "rate-window 10.0 exactly"),
            )
            for field, value, message in cases:
                with self.subTest(field=field):
                    args = harness_args(root, corpus, server_manifest)
                    setattr(args, field, value)
                    api = mock.Mock()
                    report = BENCH["rate_control_report"](args, api=api)
                    self.assertFalse(report["passed"])
                    self.assertIn(message, report["failures"][0]["detail"])
                    api.call.assert_not_called()

    def test_cli_float_arguments_reject_nonfinite_and_wrong_signs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            corpus, _ = write_corpus(root, purpose="vbr_smoke", pinned=False)
            cases = (
                ("rate_window", float("nan")),
                ("poll", float("inf")),
                ("capture_timeout", float("nan")),
                ("idle_timeout", 0.0),
                ("settings_settle", float("inf")),
                ("settings_settle", -1.0),
            )
            for field, value in cases:
                with self.subTest(field=field, value=value):
                    args = harness_args(root, corpus, modes="vbr")
                    setattr(args, field, value)
                    api = mock.Mock()
                    report = BENCH["rate_control_report"](args, api=api)
                    self.assertFalse(report["passed"])
                    self.assertIn("finite", report["failures"][0]["detail"])
                    api.call.assert_not_called()

    def test_capture_uses_real_session_bytes_safe_names_sdr_and_stable_encoder(self):
        with tempfile.TemporaryDirectory() as directory:
            api = SessionApi()
            capture = BENCH["capture_live_session"](
                api, capture_fixture(), "vbr", Path(directory), 0.001, 1, 0.01,
                "qsv-h264",
            )
            self.assertNotIn("copy", api.start_body)
            self.assertTrue(api.deleted)
            self.assertEqual(capture["status_encoder"], "qsv-h264")
            self.assertEqual(capture["delivered_dynamic_range"], "sdr")
            playlist = capture["playlist_path"].read_text()
            self.assertIn("segment-00000.ts", playlist)
            self.assertNotIn("source-a.ts", playlist)
            BENCH["cleanup_capture"](capture)

    def test_capture_rejects_cached_unsafe_unknown_dr_and_encoder_change(self):
        cases = (
            (SessionApi(vod=True), "cached"),
            (SessionApi(unsafe=True), "unsafe"),
            (SessionApi(dynamic_range=None), "prove SDR"),
            (SessionApi(dynamic_range="hdr10"), "prove SDR"),
            (SessionApi(status_encoder="software-h264"), "encoder changed"),
        )
        for api, message in cases:
            with self.subTest(message=message), tempfile.TemporaryDirectory() as directory:
                with self.assertRaisesRegex(BENCH["BenchError"], message):
                    BENCH["capture_live_session"](
                        api, capture_fixture(), "vbr", Path(directory), 0.001, 1, 0.01,
                        "qsv-h264",
                    )
                self.assertTrue(api.deleted)

    def test_capture_rejects_nonfinite_playlist_duration_and_status_speed(self):
        for api, message in (
            (SessionApi(recent_speed=float("nan")), "recent_speed"),
            (SessionApi(recent_speed=float("inf")), "recent_speed"),
        ):
            with self.subTest(message=message), tempfile.TemporaryDirectory() as directory:
                with self.assertRaisesRegex(BENCH["BenchError"], message):
                    BENCH["capture_live_session"](
                        api, capture_fixture(), "vbr", Path(directory), 0.001, 1, 0.01,
                        "qsv-h264",
                    )
                self.assertTrue(api.deleted)
        for duration in ("nan", "inf", "0", "-1"):
            with self.subTest(duration=duration):
                with self.assertRaisesRegex(BENCH["BenchError"], "duration"):
                    BENCH["parse_playlist"](
                        f"#EXTM3U\n#EXTINF:{duration},\nsegment.ts\n"
                    )

    def test_capture_must_use_system_selected_encoder(self):
        api = SessionApi(start_encoder="software-h264", status_encoder="software-h264")
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(BENCH["BenchError"], "/system selected"):
                BENCH["capture_live_session"](
                    api, capture_fixture(), "vbr", Path(directory), 0.001, 1, 0.01,
                    "qsv-h264",
                )
        self.assertTrue(api.deleted)

    def test_capture_surfaces_failed_delete_and_idle_timeout(self):
        for api, message in (
            (SessionApi(delete_error=True), "DELETE failed"),
            (SessionApi(stuck_after_delete=True), "idle timeout"),
        ):
            with self.subTest(message=message), tempfile.TemporaryDirectory() as directory:
                with self.assertRaisesRegex((RuntimeError, BENCH["BenchError"]), message):
                    BENCH["capture_live_session"](
                        api, capture_fixture(), "vbr", Path(directory), 0.001, 1, 0.002,
                        "qsv-h264",
                    )

    def test_capture_rejects_reference_origin_misalignment(self):
        api = SessionApi(origin_ms=2100)
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(BENCH["BenchError"], "media_origin_ms"):
                BENCH["capture_live_session"](
                    api, capture_fixture(), "vbr", Path(directory), 0.001, 1, 0.01,
                    "qsv-h264",
                )
        self.assertTrue(api.deleted)

    def test_viewer_race_refuses_capture_before_session_creation(self):
        api = SessionApi(initially_active=1)
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(BENCH["BenchError"], "exclusivity was lost"):
                BENCH["capture_live_session"](
                    api, capture_fixture(), "vbr", Path(directory), 0.001, 1, 0.01,
                    "qsv-h264",
                )
        self.assertFalse(api.posted)

    def test_server_source_hdr_is_fail_closed_when_unknown_or_non_sdr(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            corpus, references = write_corpus(root)
            fixtures = BENCH["load_rate_control_corpus"](corpus)["fixtures"]
            for hdr, message in ((MISSING, "unknown"), ("hdr10", "not SDR")):
                with self.subTest(hdr=hdr):
                    with self.assertRaisesRegex(BENCH["BenchError"], message):
                        BENCH["resolve_fixture_file_ids"](
                            FullApi(references, hdr=hdr), fixtures, None
                        )

    def test_server_source_requires_real_probed_available_video_facts(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            corpus, references = write_corpus(root)
            fixtures = BENCH["load_rate_control_corpus"](corpus)["fixtures"]
            cases = (
                (FullApi(references, probed=False), "definitively probed"),
                (FullApi(references, probed=None), "definitively probed"),
                (FullApi(references, probed=MISSING), "definitively probed"),
                (FullApi(references, available=False), "definitively available"),
                (FullApi(references, available=MISSING), "definitively available"),
                (FullApi(references, fact_overrides={"video_codec": None}), "video_codec"),
                (FullApi(references, fact_overrides={"width": MISSING}), "width"),
                (FullApi(references, fact_overrides={"height": 0}), "height"),
                (FullApi(references, fact_overrides={"bit_depth": None}), "bit_depth"),
                (FullApi(references, fact_overrides={"duration_ms": None}), "duration_ms"),
                (FullApi(references, fact_overrides={"duration_ms": 1_000}), "trim ends"),
            )
            for api, message in cases:
                with self.subTest(message=message):
                    with self.assertRaisesRegex(BENCH["BenchError"], message):
                        BENCH["resolve_fixture_file_ids"](api, fixtures, None)

    def test_report_refuses_missing_selected_production_encoder(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            corpus, references = write_corpus(root)
            server_manifest = write_server_manifest(root, references)
            with mock.patch.dict(G, patched_harness()):
                report = BENCH["rate_control_report"](
                    harness_args(root, corpus, server_manifest),
                    api=FullApi(references, selected_encoder=None),
                )
            self.assertFalse(report["passed"])
            self.assertIn("selected production encoder", report["failures"][0]["detail"])

    def test_full_comparison_records_provenance_settings_and_separate_scorer(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            corpus, references = write_corpus(root)
            server_manifest = write_server_manifest(root, references)
            api = FullApi(references)
            args = harness_args(root, corpus, server_manifest)
            with mock.patch.dict(G, patched_harness()):
                report = BENCH["rate_control_report"](args, api=api)

            self.assertTrue(report["passed"], report)
            self.assertTrue(report["acceptance"]["eligible"])
            self.assertEqual(report["acceptance"]["required_vmaf_model"], "vmaf_v0.6.1")
            self.assertEqual(report["acceptance"]["required_rate_window_seconds"], 10.0)
            self.assertEqual(report["corpus"]["manifest_sha256"], file_sha(corpus))
            self.assertEqual(
                report["corpus"]["server_sha256_manifest"]["manifest_sha256"],
                file_sha(server_manifest),
            )
            self.assertEqual(api.puts, [
                {"transcode_rate_mode": "bitrate"},
                {"transcode_rate_mode": "quality", "transcode_quality": 22},
                {"transcode_rate_mode": "bitrate", "transcode_quality": None},
            ])
            self.assertEqual(
                report["mode_settings"]["qvbr"]["verified_settings_response"],
                {"transcode_rate_mode": "quality", "transcode_quality": 22},
            )
            self.assertTrue(all(
                fixture["server_file_identity"]["probed"] is True
                and fixture["server_file_identity"]["available"] is True
                for fixture in report["fixtures"]
            ))
            self.assertIn("does not prove", report["mode_settings"]["qvbr"]["evidence_scope"])
            rendered = json.dumps(report, sort_keys=True)
            self.assertNotIn(args.token, rendered)
            self.assertNotIn("must-not-leak", rendered)
            self.assertFalse(report["production"]["vmaf_scorer_used_for_encoding"])
            self.assertEqual(report["vmaf_scorer"]["role"], "scoring_only_never_production_encoding")

    def test_full_subset_is_diagnostic_nonzero_even_when_measurements_pass(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            corpus, references = write_corpus(root)
            server_manifest = write_server_manifest(root, references)
            with mock.patch.dict(G, patched_harness()):
                report = BENCH["rate_control_report"](
                    harness_args(root, corpus, server_manifest, only="easy-0"),
                    api=FullApi(references),
                )
            self.assertFalse(report["passed"])
            self.assertFalse(report["acceptance"]["eligible"])
            self.assertEqual(report["acceptance"]["scope"], "diagnostic_subset_non_acceptance")
            self.assertIn("diagnostic_subset_not_acceptance", {
                failure["code"] for failure in report["failures"]
            })

    def test_viewer_race_between_modes_blocks_next_mutation_then_restores(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            corpus, references = write_corpus(root)
            server_manifest = write_server_manifest(root, references)
            # Initial system check, VBR mutation, QVBR mutation refusal, restore.
            api = FullApi(references, active_counts=[0, 0, 1, 0])
            with mock.patch.dict(G, patched_harness()):
                report = BENCH["rate_control_report"](
                    harness_args(root, corpus, server_manifest), api=api
                )
            self.assertFalse(report["passed"])
            self.assertEqual(api.puts, [
                {"transcode_rate_mode": "bitrate"},
                {"transcode_rate_mode": "bitrate", "transcode_quality": None},
            ])

    def test_restore_waits_for_viewer_to_finish_then_restores(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            corpus, references = write_corpus(root)
            server_manifest = write_server_manifest(root, references)
            # Initial check, VBR update, quality update, restore sees viewer then idle.
            api = FullApi(references, active_counts=[0, 0, 0, 1, 0])
            with mock.patch.dict(G, patched_harness()):
                report = BENCH["rate_control_report"](
                    harness_args(root, corpus, server_manifest), api=api
                )
            self.assertTrue(report["passed"], report)
            self.assertEqual(api.puts[-1], {
                "transcode_rate_mode": "bitrate",
                "transcode_quality": None,
            })

    def test_restore_timeout_emits_manual_body_without_claiming_restore(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            corpus, references = write_corpus(root)
            server_manifest = write_server_manifest(root, references)
            api = FullApi(
                references,
                active_counts=[0, 0, 0],
                default_active=1,
            )
            with mock.patch.dict(G, patched_harness()):
                report = BENCH["rate_control_report"](
                    harness_args(root, corpus, server_manifest), api=api
                )
            failure = next(
                failure for failure in report["failures"]
                if failure["code"] == "setting_restore_failed"
            )
            self.assertFalse(report["passed"])
            self.assertIn("idle timeout", failure["detail"])
            self.assertEqual(failure["required_manual_restore"], {
                "transcode_rate_mode": "bitrate",
                "transcode_quality": None,
            })
            self.assertNotIn("must-not-leak", json.dumps(failure, sort_keys=True))
            self.assertEqual(api.puts, [
                {"transcode_rate_mode": "bitrate"},
                {"transcode_rate_mode": "quality", "transcode_quality": 22},
            ])
            self.assertEqual(api.settings["transcode_rate_mode"], "quality")

    def test_capture_failure_still_restores_original_rate_settings(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            corpus, references = write_corpus(root)
            server_manifest = write_server_manifest(root, references)
            api = FullApi(references)
            patches = patched_harness()
            patches["capture_live_session"] = mock.Mock(
                side_effect=BENCH["BenchError"]("capture failed")
            )
            with mock.patch.dict(G, patches):
                report = BENCH["rate_control_report"](
                    harness_args(root, corpus, server_manifest), api=api
                )
            self.assertFalse(report["passed"])
            self.assertIn("capture failed", report["failures"][0]["detail"])
            self.assertEqual(api.puts, [
                {"transcode_rate_mode": "bitrate"},
                {"transcode_rate_mode": "bitrate", "transcode_quality": None},
            ])

    def test_gates_use_advertised_peak_only_and_check_encoder_identity(self):
        fixture = {
            "identity": "easy",
            "class": "easy",
            "modes": {
                "vbr": passing_measurement("vbr", "qsv-h264"),
                "qvbr": passing_measurement("qvbr", "software-h264"),
            },
        }
        fixture["modes"]["qvbr"].update({
            "bytes": 1001,
            "speed_p10": 0.89,
            "vmaf": 94.9,
            "peak_window_kbps": 12161.0,
        })
        failures = BENCH["evaluate_rate_control"]([fixture])
        codes = {failure["code"] for failure in failures}
        self.assertEqual(codes, {
            "advertised_peak_exceeded", "encoder_identity_mismatch", "vmaf_regression",
            "easy_bytes_regression", "speed_regression",
        })
        self.assertNotIn("vbv_peak_exceeded", codes)
        # VBR is over its lower theoretical diagnostic but under advertised;
        # it must not receive any peak failure.
        self.assertFalse(any(
            failure["code"] == "advertised_peak_exceeded" and failure["mode"] == "vbr"
            for failure in failures
        ))

    def test_rolling_rate_matches_complete_served_segment_windows(self):
        segments = [
            {"duration_seconds": 3.0, "bytes": 3000},
            {"duration_seconds": 3.0, "bytes": 6000},
            {"duration_seconds": 3.0, "bytes": 9000},
            {"duration_seconds": 1.0, "bytes": 100_000},
        ]
        self.assertEqual(BENCH["rolling_segment_peak"](segments, 6.0), (20.0, 3.0))

    def test_twelve_second_window_can_mask_a_failing_ten_second_peak(self):
        segments = [
            {"duration_seconds": 5.0, "bytes": 5000},
            {"duration_seconds": 5.0, "bytes": 5000},
            {"duration_seconds": 2.0, "bytes": 1},
        ]
        peak_10, _ = BENCH["rolling_segment_peak"](segments, 10.0)
        peak_12, _ = BENCH["rolling_segment_peak"](segments, 12.0)
        self.assertGreater(peak_10, 7.5)
        self.assertLess(peak_12, 7.0)

    def test_stable_json_artifact_is_sorted_and_nonzero(self):
        with tempfile.TemporaryDirectory() as directory:
            first = Path(directory) / "first.json"
            second = Path(directory) / "second.json"
            document = {"z": [2, 1], "a": {"passed": False}}
            BENCH["write_json"](first, document)
            BENCH["write_json"](second, document)
            self.assertGreater(first.stat().st_size, 0)
            self.assertEqual(first.read_bytes(), second.read_bytes())
            with self.assertRaises(ValueError):
                BENCH["write_json"](Path(directory) / "nan.json", {"value": float("nan")})


if __name__ == "__main__":
    unittest.main()
