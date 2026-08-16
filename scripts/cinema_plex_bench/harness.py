"""A/B orchestration, trial pairing, and raw evidence capture."""

from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
import json
import math
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
    "output_height",
    "output_bitrate_basis",
    "advertised_output_bitrate_kbps",
    "advertised_output_codecs",
    "advertised_output_resolution",
    "output_contract_verified",
    "output_contract_basis",
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
    "resource_anomalies",
    "pair_sequence",
    "pair_server_order",
    "server_order_index",
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


def validate_selection(
    config: BenchmarkConfig,
    only_servers: set[str] | None,
    only_scenarios: set[str] | None,
) -> None:
    for label, selected, available in (
        ("--server", only_servers, set(config.servers)),
        ("--scenario", only_scenarios, {item["id"] for item in config.scenarios}),
    ):
        if selected is None:
            continue
        if not selected:
            raise ConfigError(f"{label} selection must not be empty")
        unknown = selected - available
        if unknown:
            raise ConfigError(f"unknown {label} values: {', '.join(sorted(unknown))}")


def validate_cold_readiness(
    config: BenchmarkConfig,
    only_servers: set[str] | None,
    only_scenarios: set[str] | None,
) -> None:
    selected_servers = only_servers if only_servers is not None else set(config.servers)
    for scenario in config.scenarios:
        if only_scenarios is not None and scenario["id"] not in only_scenarios:
            continue
        if scenario["cache_state"] != "cold":
            continue
        missing = [
            key
            for key in selected_servers
            if not config.servers[key].get("before_trial_command")
        ]
        if missing:
            raise HarnessError(
                f"cold scenario {scenario['id']!r} requires before_trial_command for "
                + ", ".join(sorted(missing))
            )


def balanced_server_order(server_keys: list[str], pair_sequence: int) -> list[str]:
    order = list(server_keys)
    if pair_sequence % 2:
        order.reverse()
    return order


def validate_measurement_row(row: dict[str, Any]) -> None:
    missing = [field for field in RAW_REQUIRED_FIELDS if field not in row]
    if missing:
        raise HarnessError("measurement row is missing: " + ", ".join(missing))
    latency = row.get("latency_ms")
    if row.get("success") and (
        isinstance(latency, bool)
        or not isinstance(latency, (int, float))
        or not math.isfinite(latency)
        or latency < 0
    ):
        raise HarnessError("a successful measurement row requires finite non-negative latency_ms")
    if not isinstance(row.get("output_contract_verified"), bool):
        raise HarnessError("measurement row output_contract_verified must be boolean")
    if not isinstance(row.get("resource_anomalies"), list):
        raise HarnessError("measurement row resource_anomalies must be an array")
    order = row.get("pair_server_order")
    order_index = row.get("server_order_index")
    if (
        not isinstance(order, list)
        or isinstance(order_index, bool)
        or not isinstance(order_index, int)
        or order_index < 0
        or order_index >= len(order)
        or order[order_index] != row.get("server")
    ):
        raise HarnessError("measurement row has an invalid pair server order")


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


def safe_manifest_config(config: BenchmarkConfig) -> dict[str, Any]:
    """Return artifact-safe config without hook argv or accidental secret fields."""
    document = json.loads(
        json.dumps(
            {
                "schema_version": SCHEMA_VERSION,
                "run": config.run,
                "client": config.client,
                "servers": config.servers,
                "media": list(config.media.values()),
                "scenarios": list(config.scenarios),
            }
        )
    )
    command_fields = ("monitor_command", "before_trial_command", "after_trial_command")
    for server in document.get("servers", {}).values():
        for field in command_fields:
            command = server.get(field)
            if command is not None:
                server[field] = {
                    "configured": True,
                    "argument_count": len(command),
                    "argv_redacted": True,
                }

    def scrub(value: Any) -> Any:
        if isinstance(value, dict):
            cleaned: dict[str, Any] = {}
            for key, item in value.items():
                lowered = key.lower()
                if key != "token_env" and any(
                    marker in lowered
                    for marker in ("token", "password", "secret", "credential", "api_key")
                ):
                    cleaned[key] = "<redacted>"
                else:
                    cleaned[key] = scrub(item)
            return cleaned
        if isinstance(value, list):
            return [scrub(item) for item in value]
        return value

    return scrub(document)


def validate_output_contract(
    server: str,
    scenario: dict[str, Any],
    stream: StreamHandle,
    readiness: dict[str, Any],
    probe: dict[str, Any],
) -> dict[str, Any]:
    expected_height = scenario["output_height"]
    expected_bitrate = scenario["output_bitrate_kbps"]
    tolerance_percent = scenario["output_bitrate_tolerance_percent"]
    actual_height = probe.get("probed_output_height")
    video_codec = _codec(probe.get("probed_output_video_codec"))
    audio_codec = _codec(probe.get("probed_output_audio_codec"))
    if actual_height != expected_height:
        raise HarnessError(
            f"{server}/{scenario['id']} delivered height {actual_height!r}, "
            f"expected {expected_height}"
        )
    if video_codec != "h264" or audio_codec != "aac":
        raise HarnessError(
            f"{server}/{scenario['id']} delivered codecs {video_codec!r}/{audio_codec!r}, "
            "expected h264/aac"
        )
    advertised_bitrate = readiness.get("advertised_output_bitrate_kbps")
    if advertised_bitrate is None:
        advertised_bitrate = stream.details.get("advertised_output_bitrate_kbps")
    if not isinstance(advertised_bitrate, (int, float)) or isinstance(
        advertised_bitrate, bool
    ):
        raise HarnessError(
            f"{server}/{scenario['id']} did not advertise an output bitrate"
        )
    tolerance = expected_bitrate * tolerance_percent / 100
    if abs(float(advertised_bitrate) - expected_bitrate) > tolerance:
        raise HarnessError(
            f"{server}/{scenario['id']} advertised {advertised_bitrate:g} kb/s, expected "
            f"{expected_bitrate} kb/s ±{tolerance_percent:g}%"
        )
    return {
        "output_height": actual_height,
        "output_video_codec": video_codec,
        "output_audio_codec": audio_codec,
        "output_bitrate_kbps": expected_bitrate,
        "output_bitrate_basis": "requested_transcode_contract",
        "advertised_output_bitrate_kbps": advertised_bitrate,
        "advertised_output_codecs": readiness.get("advertised_output_codecs"),
        "advertised_output_resolution": readiness.get("advertised_output_resolution"),
        "output_contract_verified": True,
        "output_contract_basis": stream.details.get(
            "output_contract_basis", "hls_metadata_and_ffprobe"
        ),
        "output_bitrate_tolerance_percent": tolerance_percent,
        **probe,
    }


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
        validate_selection(config, only_servers, only_scenarios)
        validate_cold_readiness(config, only_servers, only_scenarios)
        self.http = HttpClient(config.run["timeout_seconds"])
        self.runners = build_runners(config, self.http, only_servers)
        secrets = [runner.token for runner in self.runners.values()]
        self.client = FfmpegClient(
            config.client["ffmpeg"],
            config.run["timeout_seconds"],
            secrets,
            config.client["ffprobe"],
        )
        self.secrets = secrets
        self.identities = {key: runner.identity() for key, runner in self.runners.items()}
        self.discovered = verify_corpus(config, self.runners)
        self.monitors = {
            key: ResourceMonitor(runner.config.get("monitor_command"), config.run["timeout_seconds"])
            for key, runner in self.runners.items()
        }
        self.output_contracts = self._verify_output_contracts()
        self.writer = RawWriter(output_dir / "raw.jsonl")

    def _selected_scenarios(self) -> list[dict[str, Any]]:
        return [
            scenario
            for scenario in self.config.scenarios
            if self.only_scenarios is None or scenario["id"] in self.only_scenarios
        ]

    def _verify_output_contracts(self) -> dict[str, dict[str, dict[str, Any]]]:
        contracts: dict[str, dict[str, dict[str, Any]]] = {}
        preflight_cache: dict[tuple[Any, ...], dict[str, Any]] = {}
        for scenario in self._selected_scenarios():
            medium = self.config.media[scenario["media"]]
            contracts[scenario["id"]] = {}
            for server, runner in self.runners.items():
                if scenario["playback_mode"] == "direct_play":
                    contracts[scenario["id"]][server] = {
                        "output_height": medium["height"],
                        "output_video_codec": _codec(medium["video_codec"]),
                        "output_audio_codec": _codec(medium["audio_codec"]),
                        "output_bitrate_kbps": medium["bitrate_kbps"],
                        "output_bitrate_basis": "source_catalog",
                        "advertised_output_bitrate_kbps": medium["bitrate_kbps"],
                        "advertised_output_codecs": None,
                        "advertised_output_resolution": (
                            f"{medium['width']}x{medium['height']}"
                        ),
                        "output_contract_verified": True,
                        "output_contract_basis": "verified_source_catalog",
                        "output_bitrate_tolerance_percent": medium.get(
                            "bitrate_tolerance_percent", 15
                        ),
                    }
                    continue
                cache_key = (
                    server,
                    medium["id"],
                    scenario["output_height"],
                    scenario["output_bitrate_kbps"],
                    scenario["output_bitrate_tolerance_percent"],
                )
                contract = preflight_cache.get(cache_key)
                if contract is None:
                    stream: StreamHandle | None = None
                    try:
                        stream = runner.prepare_stream(
                            medium,
                            scenario,
                            0,
                            f"contract-preflight-{uuid.uuid4()}",
                        )
                        readiness = runner.wait_ready(
                            stream, self.config.run["timeout_seconds"]
                        )
                        probe = self.client.probe_stream(stream)
                        contract = validate_output_contract(
                            server, scenario, stream, readiness, probe
                        )
                    finally:
                        if stream is not None:
                            runner.close_stream(stream)
                    preflight_cache[cache_key] = contract
                contracts[scenario["id"]][server] = dict(contract)
        return contracts

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
        pair_sequence: int,
        pair_server_order: list[str],
        server_order_index: int,
    ) -> dict[str, Any]:
        identity = self.identities[server]
        actual = self.discovered[medium["id"]][server]
        contract = self.output_contracts[scenario["id"]][server]
        row: dict[str, Any] = {
            "schema_version": SCHEMA_VERSION,
            "record_type": "measurement",
            "run_id": self.run_id,
            "pair_id": pair_id,
            "pair_sequence": pair_sequence,
            "pair_server_order": pair_server_order,
            "server_order_index": server_order_index,
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
            "output_video_codec": contract["output_video_codec"],
            "output_audio_codec": contract["output_audio_codec"],
            "output_bitrate_kbps": contract["output_bitrate_kbps"],
            "output_height": contract["output_height"],
            "output_bitrate_basis": contract["output_bitrate_basis"],
            "advertised_output_bitrate_kbps": contract[
                "advertised_output_bitrate_kbps"
            ],
            "advertised_output_codecs": contract["advertised_output_codecs"],
            "advertised_output_resolution": contract["advertised_output_resolution"],
            "output_contract_verified": contract["output_contract_verified"],
            "output_contract_basis": contract["output_contract_basis"],
            "output_bitrate_tolerance_percent": contract[
                "output_bitrate_tolerance_percent"
            ],
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
            "resource_anomalies": [],
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

    @staticmethod
    def _apply_runtime_contract(
        row: dict[str, Any], stream: StreamHandle, scenario: dict[str, Any]
    ) -> None:
        row["details"].update(stream.details)
        if scenario["playback_mode"] != "transcode":
            return
        row["output_contract_verified"] = False
        if stream.output_height != scenario["output_height"]:
            raise HarnessError(
                f"{row['server']}/{scenario['id']} prepared height {stream.output_height!r}, "
                f"expected {scenario['output_height']}"
            )
        advertised = row["details"].get("advertised_output_bitrate_kbps")
        if advertised is not None:
            expected = scenario["output_bitrate_kbps"]
            tolerance_percent = scenario["output_bitrate_tolerance_percent"]
            tolerance = expected * tolerance_percent / 100
            if not isinstance(advertised, (int, float)) or isinstance(advertised, bool):
                raise HarnessError("advertised output bitrate must be numeric")
            if abs(float(advertised) - expected) > tolerance:
                raise HarnessError(
                    f"{row['server']}/{scenario['id']} advertised {advertised:g} kb/s, "
                    f"expected {expected} kb/s ±{tolerance_percent:g}%"
                )
            row["advertised_output_bitrate_kbps"] = advertised
        for field in ("advertised_output_codecs", "advertised_output_resolution"):
            value = row["details"].get(field)
            if value is not None:
                row[field] = value
        row["output_contract_verified"] = True

    def _single(
        self,
        server: str,
        medium: dict[str, Any],
        scenario: dict[str, Any],
        scope: str,
        iteration: int,
        pair_id: str,
        target: dict[str, Any] | None,
        pair_sequence: int,
        pair_server_order: list[str],
        server_order_index: int,
    ) -> dict[str, Any]:
        runner = self.runners[server]
        row = self._base_row(
            server,
            medium,
            scenario,
            scope,
            iteration,
            pair_id,
            target,
            pair_sequence=pair_sequence,
            pair_server_order=pair_server_order,
            server_order_index=server_order_index,
        )
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
            self._apply_runtime_contract(row, stream, scenario)
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
                row["resource_anomalies"] = ["resource_monitor_failed"]
        return row

    def _concurrent(
        self,
        server: str,
        medium: dict[str, Any],
        scenario: dict[str, Any],
        scope: str,
        iteration: int,
        pair_id: str,
        pair_sequence: int,
        pair_server_order: list[str],
        server_order_index: int,
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
                pair_sequence=pair_sequence,
                pair_server_order=pair_server_order,
                server_order_index=server_order_index,
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
                self._apply_runtime_contract(row, stream, scenario)
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
            resources = {
                **{field: None for field in RESOURCE_FIELDS},
                "resource_anomalies": ["resource_monitor_failed"],
            }
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
            "config": safe_manifest_config(self.config),
            "controller": {
                "platform": platform.platform(),
                "python": platform.python_version(),
                "pid": os.getpid(),
                "ffmpeg": self.client.version,
                "ffprobe": self.client.probe_version,
            },
            "servers": self.identities,
            "discovered_media": self.discovered,
            "verified_output_contracts": self.output_contracts,
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
                pair_sequence = 0
                for iteration in range(iterations):
                    for scope in scenario["measurement_scopes"]:
                        for target_index, target in enumerate(targets):
                            server_order = balanced_server_order(
                                list(self.runners), pair_sequence
                            )
                            pair_id = (
                                f"{self.run_id}:{scenario['id']}:{iteration}:{scope}:{target_index}"
                            )
                            for server_order_index, server in enumerate(server_order):
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
                                        pair_sequence,
                                        server_order,
                                        server_order_index,
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
                                            pair_sequence,
                                            server_order,
                                            server_order_index,
                                        )
                                    ]
                                for row in rows:
                                    self.writer.write(row)
                                    measurement_rows += 1
                                cooldown = self.config.run["cooldown_seconds"]
                                if cooldown:
                                    time.sleep(cooldown)
                            pair_sequence += 1
            self._write_manifest("complete", measurement_rows)
        except Exception:
            self._write_manifest("failed", measurement_rows)
            raise
        finally:
            self.writer.close()
        return self.writer.path
