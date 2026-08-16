"""A/B orchestration, trial pairing, and raw evidence capture."""

from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import platform
import threading
import time
from typing import Any, Callable
import uuid

from . import HARNESS_VERSION, SCHEMA_VERSION
from .client import FfmpegClient
from .config import BenchmarkConfig, ConfigError, seek_targets
from .http import HttpClient
from .monitoring import RESOURCE_FIELDS, ResourceMonitor, resource_delta, run_hook
from .runners import ServerRunner, StreamHandle, build_runners


CODEC_ALIASES = {
    "avc": "h264",
    "avc1": "h264",
    "h265": "hevc",
    "x265": "hevc",
    "ac-3": "ac3",
    "e-ac-3": "eac3",
}

RAW_REQUIRED_FIELDS = (
    "server",
    "server_version",
    "server_commit",
    "client",
    "media",
    "playback_mode",
    "source_video_codec",
    "source_audio_codec",
    "source_bitrate_kbps",
    "output_video_codec",
    "output_audio_codec",
    "output_bitrate_kbps",
    "output_bitrate_basis",
    "advertised_output_bitrate_kbps",
    "advertised_output_codecs",
    "operation",
    "operation_variant",
    "measurement_scope",
    "latency_ms",
    "cpu_percent",
    "gpu_percent",
    "rss_bytes",
    "storage_read_bytes",
    "storage_write_bytes",
    "network_rx_bytes",
    "network_tx_bytes",
    "success",
)


class HarnessError(RuntimeError):
    pass


def validate_run_readiness(config: BenchmarkConfig) -> None:
    placeholders: list[str] = []
    if "REPLACE" in config.client["host"]:
        placeholders.append("client.host")
    for media_id, medium in config.media.items():
        if medium["sha256"] == "0" * 64:
            placeholders.append(f"media.{media_id}.sha256")
        if "REPLACE" in medium["title"]:
            placeholders.append(f"media.{media_id}.title")
    for server, settings in config.servers.items():
        if settings["base_url"].split("//", 1)[-1].split(":", 1)[0] in (
            "cinema-host",
            "plex-host",
        ):
            placeholders.append(f"servers.{server}.base_url")
    if placeholders:
        raise HarnessError(
            "replace template placeholders before a real run: " + ", ".join(placeholders)
        )


def validate_measurement_row(row: dict[str, Any]) -> None:
    missing = [field for field in RAW_REQUIRED_FIELDS if field not in row]
    if missing:
        raise HarnessError("measurement row is missing: " + ", ".join(missing))
    if row.get("success") and not isinstance(row.get("latency_ms"), (int, float)):
        raise HarnessError("a successful measurement row requires numeric latency_ms")


def _now() -> str:
    return datetime.now(timezone.utc).isoformat()


def _codec(value: Any) -> str | None:
    if value is None:
        return None
    normalized = str(value).strip().lower()
    return CODEC_ALIASES.get(normalized, normalized)


def _safe_error(error: BaseException, secrets: list[str]) -> str:
    detail = str(error).replace("\n", " ")
    for secret in secrets:
        if secret:
            detail = detail.replace(secret, "<redacted>")
    return detail[:2000]


def _operation_variant(
    scenario: dict[str, Any], target: dict[str, Any] | None
) -> str:
    operation = scenario["operation"]
    if operation == "seek" and target is not None:
        delta = target.get("delta_seconds")
        if delta is not None:
            return f"seek_{delta:+g}s"
        return f"seek_{target['target_seconds']:g}s"
    if operation == "random_seeks":
        return "random_seek"
    if operation == "concurrent_transcodes":
        return f"simultaneous_transcodes_{scenario['concurrency']}"
    return operation


def _verify_field(
    server: str,
    medium: dict[str, Any],
    actual: dict[str, Any],
    expected_field: str,
    actual_field: str,
) -> None:
    expected, observed = medium[expected_field], actual.get(actual_field)
    if observed is None:
        raise HarnessError(
            f"{server}/{medium['id']} did not report {actual_field}; fair A/B identity cannot be proven"
        )
    if expected_field.endswith("codec"):
        matches = _codec(expected) == _codec(observed)
    else:
        matches = expected == observed
    if not matches:
        raise HarnessError(
            f"{server}/{medium['id']} {actual_field}={observed!r}, expected {expected!r}"
        )


def verify_corpus(
    config: BenchmarkConfig, runners: dict[str, ServerRunner]
) -> dict[str, dict[str, dict[str, Any]]]:
    """Prove both catalogs point at media with the configured shape."""
    discovered: dict[str, dict[str, dict[str, Any]]] = {}
    for media_id, medium in config.media.items():
        discovered[media_id] = {}
        for server, runner in runners.items():
            actual = runner.media_info(medium)
            for expected_field, actual_field in (
                ("container", "source_container"),
                ("video_codec", "source_video_codec"),
                ("audio_codec", "source_audio_codec"),
                ("width", "source_width"),
                ("height", "source_height"),
            ):
                _verify_field(server, medium, actual, expected_field, actual_field)
            duration = actual.get("source_duration_seconds")
            if duration is None or abs(duration - medium["duration_seconds"]) > 2:
                raise HarnessError(
                    f"{server}/{media_id} duration={duration!r}s, expected "
                    f"{medium['duration_seconds']}s ±2s"
                )
            bitrate = actual.get("source_bitrate_kbps")
            tolerance = float(medium.get("bitrate_tolerance_percent", 15)) / 100
            expected_bitrate = medium["bitrate_kbps"]
            if bitrate is None or abs(bitrate - expected_bitrate) > expected_bitrate * tolerance:
                raise HarnessError(
                    f"{server}/{media_id} bitrate={bitrate!r} kb/s, expected "
                    f"{expected_bitrate} kb/s ±{tolerance:.0%}"
                )
            discovered[media_id][server] = actual
    return discovered


class RawWriter:
    def __init__(self, path: Path):
        path.parent.mkdir(parents=True, exist_ok=True)
        self.path = path
        self.handle = path.open("w", encoding="utf-8")
        self.lock = threading.Lock()

    def write(self, row: dict[str, Any]) -> None:
        validate_measurement_row(row)
        with self.lock:
            self.handle.write(json.dumps(row, sort_keys=True, allow_nan=False) + "\n")
            self.handle.flush()

    def close(self) -> None:
        self.handle.close()


class BenchmarkHarness:
    def __init__(
        self,
        config: BenchmarkConfig,
        output_dir: Path,
        *,
        only_servers: set[str] | None = None,
        only_scenarios: set[str] | None = None,
        iterations_override: int | None = None,
        progress: Callable[[str], None] = print,
    ):
        self.config = config
        self.output_dir = output_dir
        self.only_servers = only_servers
        self.only_scenarios = only_scenarios
        self.iterations_override = iterations_override
        self.progress = progress
        self.run_id = str(uuid.uuid4())
        validate_run_readiness(config)
        self.http = HttpClient(config.run["timeout_seconds"])
        self.runners = build_runners(config, self.http)
        if only_servers is not None:
            unknown = only_servers - set(self.runners)
            if unknown:
                raise ConfigError(f"unknown --server values: {', '.join(sorted(unknown))}")
            self.runners = {key: value for key, value in self.runners.items() if key in only_servers}
        secrets = [runner.token for runner in self.runners.values()]
        self.client = FfmpegClient(config.client["ffmpeg"], config.run["timeout_seconds"], secrets)
        self.secrets = secrets
        self.identities = {key: runner.identity() for key, runner in self.runners.items()}
        self.discovered = verify_corpus(config, self.runners)
        self.monitors = {
            key: ResourceMonitor(runner.config.get("monitor_command"), config.run["timeout_seconds"])
            for key, runner in self.runners.items()
        }
        self.writer = RawWriter(output_dir / "raw.jsonl")

    def _base_row(
        self,
        server: str,
        medium: dict[str, Any],
        scenario: dict[str, Any],
        scope: str,
        iteration: int,
        pair_id: str,
        target: dict[str, Any] | None,
        *,
        concurrency_index: int | None = None,
    ) -> dict[str, Any]:
        identity = self.identities[server]
        actual = self.discovered[medium["id"]][server]
        row: dict[str, Any] = {
            "schema_version": SCHEMA_VERSION,
            "record_type": "measurement",
            "run_id": self.run_id,
            "pair_id": pair_id,
            "recorded_at": _now(),
            **identity,
            "server_runner": self.runners[server].config["runner"],
            "client": self.config.client["name"],
            "client_version": (
                self.client.version
                if self.config.client["version"] == "auto"
                else self.config.client["version"]
            ),
            "client_host": self.config.client["host"],
            "media": medium["id"],
            "media_title": medium["title"],
            "media_sha256": medium["sha256"],
            "source_container": actual.get("source_container"),
            "source_video_codec": actual.get("source_video_codec"),
            "source_audio_codec": actual.get("source_audio_codec"),
            "source_bitrate_kbps": actual.get("source_bitrate_kbps"),
            "source_width": actual.get("source_width"),
            "source_height": actual.get("source_height"),
            "playback_mode": scenario["playback_mode"],
            "output_video_codec": (
                medium["video_codec"] if scenario["playback_mode"] == "direct_play" else "h264"
            ),
            "output_audio_codec": (
                medium["audio_codec"] if scenario["playback_mode"] == "direct_play" else "aac"
            ),
            "output_bitrate_kbps": (
                medium["bitrate_kbps"]
                if scenario["playback_mode"] == "direct_play"
                else scenario["output_bitrate_kbps"]
            ),
            "output_bitrate_basis": (
                "source_catalog"
                if scenario["playback_mode"] == "direct_play"
                else "requested_transcode_contract"
            ),
            "advertised_output_bitrate_kbps": (
                medium["bitrate_kbps"] if scenario["playback_mode"] == "direct_play" else None
            ),
            "advertised_output_codecs": None,
            "scenario_id": scenario["id"],
            "operation": scenario["operation"],
            "operation_variant": _operation_variant(scenario, target),
            "measurement_scope": scope,
            "cache_state": scenario["cache_state"],
            "iteration": iteration,
            "concurrency": scenario.get("concurrency", 1),
            "concurrency_index": concurrency_index,
            "seek_index": target.get("seek_index") if target else None,
            "seek_target_seconds": target.get("target_seconds") if target else None,
            "seek_delta_seconds": target.get("delta_seconds") if target else None,
            "latency_ms": None,
            "success": False,
            "error_type": None,
            "error": None,
            "resource_scope": "batch" if scenario["operation"] == "concurrent_transcodes" else "trial",
            **{field: None for field in RESOURCE_FIELDS},
            "details": {},
        }
        return row

    def _context(
        self,
        row: dict[str, Any],
        phase: str,
    ) -> dict[str, Any]:
        return {
            "schema_version": SCHEMA_VERSION,
            "phase": phase,
            "run_id": self.run_id,
            "pair_id": row["pair_id"],
            "server": row["server"],
            "scenario_id": row["scenario_id"],
            "operation": row["operation"],
            "operation_variant": row["operation_variant"],
            "measurement_scope": row["measurement_scope"],
            "media": row["media"],
            "playback_mode": row["playback_mode"],
            "seek_target_seconds": row["seek_target_seconds"],
            "seek_delta_seconds": row["seek_delta_seconds"],
            "iteration": row["iteration"],
            "concurrency": row["concurrency"],
            "cache_state": row["cache_state"],
        }

    def _single(
        self,
        server: str,
        medium: dict[str, Any],
        scenario: dict[str, Any],
        scope: str,
        iteration: int,
        pair_id: str,
        target: dict[str, Any] | None,
    ) -> dict[str, Any]:
        runner = self.runners[server]
        row = self._base_row(server, medium, scenario, scope, iteration, pair_id, target)
        context = self._context(row, "before")
        stream: StreamHandle | None = None
        before = {field: None for field in RESOURCE_FIELDS}
        after = dict(before)
        try:
            run_hook(
                runner.config.get("before_trial_command"),
                context,
                self.config.run["timeout_seconds"],
            )
            before = self.monitors[server].sample({**context, "sample_phase": "before"})
            started = time.monotonic()
            start_seconds = float(target["target_seconds"]) if target else 0.0
            stream = runner.prepare_stream(medium, scenario, start_seconds, pair_id)
            if scenario["operation"] == "pause_resume":
                measured = self.client.pause_resume(
                    stream, scenario["warmup_seconds"], scenario["pause_seconds"]
                )
                row["latency_ms"] = measured["resume_latency_ms"]
                row["details"].update(measured)
                row["details"]["setup_to_resume_complete_ms"] = (
                    time.monotonic() - started
                ) * 1000
            elif scope == "server_engine":
                if target is not None and scenario["playback_mode"] == "direct_play":
                    # A temporal seek has no honest byte-range offset without
                    # parsing the source index. Use the same demuxer on both
                    # sides and stop before video decode.
                    row["details"].update(self.client.read_first_packet(stream))
                else:
                    row["details"].update(
                        runner.wait_ready(stream, self.config.run["timeout_seconds"])
                    )
                row["latency_ms"] = (time.monotonic() - started) * 1000
            else:
                row["details"].update(self.client.decode_first_frame(stream))
                row["latency_ms"] = (time.monotonic() - started) * 1000
            row["details"].update(stream.details)
            row["advertised_output_bitrate_kbps"] = row["details"].get(
                "advertised_output_bitrate_kbps", row["advertised_output_bitrate_kbps"]
            )
            row["advertised_output_codecs"] = row["details"].get(
                "advertised_output_codecs"
            )
            row["success"] = True
        except Exception as error:
            row["error_type"] = type(error).__name__
            row["error"] = _safe_error(error, self.secrets)
        finally:
            if stream is not None:
                runner.close_stream(stream)
            try:
                after = self.monitors[server].sample({**context, "sample_phase": "after"})
                row.update(resource_delta(before, after))
                run_hook(
                    runner.config.get("after_trial_command"),
                    {**context, "phase": "after"},
                    self.config.run["timeout_seconds"],
                )
            except Exception as error:
                row["success"] = False
                row["error_type"] = type(error).__name__
                row["error"] = _safe_error(error, self.secrets)
        return row

    def _concurrent(
        self,
        server: str,
        medium: dict[str, Any],
        scenario: dict[str, Any],
        scope: str,
        iteration: int,
        pair_id: str,
    ) -> list[dict[str, Any]]:
        runner = self.runners[server]
        count = scenario["concurrency"]
        rows = [
            self._base_row(
                server,
                medium,
                scenario,
                scope,
                iteration,
                pair_id,
                None,
                concurrency_index=index,
            )
            for index in range(count)
        ]
        context = self._context(rows[0], "before")
        barrier = threading.Barrier(count + 1)
        before = {field: None for field in RESOURCE_FIELDS}
        batch_error: BaseException | None = None

        def worker(index: int) -> dict[str, Any]:
            row = rows[index]
            stream: StreamHandle | None = None
            barrier.wait()
            started = time.monotonic()
            try:
                stream = runner.prepare_stream(
                    medium, scenario, 0, f"{pair_id}-{index}"
                )
                if scope == "server_engine":
                    row["details"].update(
                        runner.wait_ready(stream, self.config.run["timeout_seconds"])
                    )
                    row["latency_ms"] = (time.monotonic() - started) * 1000
                    remaining = scenario["observe_seconds"] - (time.monotonic() - started)
                    if remaining > 0:
                        time.sleep(remaining)
                else:
                    measured = self.client.decode_for(stream, scenario["observe_seconds"])
                    row["latency_ms"] = (
                        measured.pop("first_frame_monotonic") - started
                    ) * 1000
                    row["details"].update(measured)
                row["details"].update(stream.details)
                row["advertised_output_bitrate_kbps"] = row["details"].get(
                    "advertised_output_bitrate_kbps", row["advertised_output_bitrate_kbps"]
                )
                row["advertised_output_codecs"] = row["details"].get(
                    "advertised_output_codecs"
                )
                row["success"] = True
            except Exception as error:
                row["error_type"] = type(error).__name__
                row["error"] = _safe_error(error, self.secrets)
            finally:
                if stream is not None:
                    runner.close_stream(stream)
            return row

        try:
            run_hook(
                runner.config.get("before_trial_command"),
                context,
                self.config.run["timeout_seconds"],
            )
            before = self.monitors[server].sample({**context, "sample_phase": "before"})
            with ThreadPoolExecutor(max_workers=count, thread_name_prefix="cinema-plex-bench") as pool:
                futures = [pool.submit(worker, index) for index in range(count)]
                barrier.wait()
                rows = [future.result() for future in futures]
        except Exception as error:
            batch_error = error
            try:
                barrier.abort()
            except threading.BrokenBarrierError:
                pass
        try:
            after = self.monitors[server].sample({**context, "sample_phase": "after"})
            resources = resource_delta(before, after)
            run_hook(
                runner.config.get("after_trial_command"),
                {**context, "phase": "after"},
                self.config.run["timeout_seconds"],
            )
        except Exception as error:
            resources = {field: None for field in RESOURCE_FIELDS}
            batch_error = error
        for row in rows:
            row.update(resources)
            if batch_error is not None:
                row["success"] = False
                row["error_type"] = type(batch_error).__name__
                row["error"] = _safe_error(batch_error, self.secrets)
        return rows

    def _scenario_targets(
        self, scenario: dict[str, Any], medium: dict[str, Any]
    ) -> list[dict[str, Any] | None]:
        if scenario["operation"] in ("seek", "random_seeks"):
            return list(seek_targets(scenario, medium))
        return [None]

    def _manifest(self, status: str, measurement_rows: int) -> dict[str, Any]:
        return {
            "schema_version": SCHEMA_VERSION,
            "harness_version": HARNESS_VERSION,
            "run_id": self.run_id,
            "status": status,
            "recorded_at": _now(),
            "config_path": str(self.config.path),
            "config": self.config.document,
            "controller": {
                "platform": platform.platform(),
                "python": platform.python_version(),
                "pid": os.getpid(),
                "ffmpeg": self.client.version,
            },
            "servers": self.identities,
            "discovered_media": self.discovered,
            "measurement_rows": measurement_rows,
            "raw_results": "raw.jsonl",
        }

    def _write_manifest(self, status: str, rows: int) -> None:
        (self.output_dir / "manifest.json").write_text(
            json.dumps(self._manifest(status, rows), indent=2, sort_keys=True, allow_nan=False)
            + "\n",
            encoding="utf-8",
        )

    def run(self) -> Path:
        measurement_rows = 0
        self._write_manifest("running", measurement_rows)
        try:
            for scenario in self.config.scenarios:
                if self.only_scenarios is not None and scenario["id"] not in self.only_scenarios:
                    continue
                if scenario["cache_state"] == "cold":
                    missing = [
                        key
                        for key, runner in self.runners.items()
                        if not runner.config.get("before_trial_command")
                    ]
                    if missing:
                        raise HarnessError(
                            f"cold scenario {scenario['id']!r} requires before_trial_command for "
                            + ", ".join(missing)
                        )
                medium = self.config.media[scenario["media"]]
                iterations = self.iterations_override or scenario["iterations"]
                targets = self._scenario_targets(scenario, medium)
                for iteration in range(iterations):
                    server_order = list(self.runners)
                    if iteration % 2:
                        server_order.reverse()
                    for scope in scenario["measurement_scopes"]:
                        for target_index, target in enumerate(targets):
                            pair_id = (
                                f"{self.run_id}:{scenario['id']}:{iteration}:{scope}:{target_index}"
                            )
                            for server in server_order:
                                label = (
                                    f"{scenario['id']} · {scope} · iteration {iteration + 1}/{iterations} "
                                    f"· {server}"
                                )
                                if target is not None:
                                    label += f" · seek {target['target_seconds']:.3f}s"
                                self.progress(label)
                                if scenario["operation"] == "concurrent_transcodes":
                                    rows = self._concurrent(
                                        server,
                                        medium,
                                        scenario,
                                        scope,
                                        iteration,
                                        pair_id,
                                    )
                                else:
                                    rows = [
                                        self._single(
                                            server,
                                            medium,
                                            scenario,
                                            scope,
                                            iteration,
                                            pair_id,
                                            target,
                                        )
                                    ]
                                for row in rows:
                                    self.writer.write(row)
                                    measurement_rows += 1
                                cooldown = self.config.run["cooldown_seconds"]
                                if cooldown:
                                    time.sleep(cooldown)
            self._write_manifest("complete", measurement_rows)
        except Exception:
            self._write_manifest("failed", measurement_rows)
            raise
        finally:
            self.writer.close()
        return self.writer.path
