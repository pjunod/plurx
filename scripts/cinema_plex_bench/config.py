"""TOML configuration validation and deterministic scenario expansion."""

from __future__ import annotations

from dataclasses import dataclass
import math
from pathlib import Path
import random
import re
import tomllib
from typing import Any
from urllib.parse import urlsplit

from . import SCHEMA_VERSION


NAME = re.compile(r"^[a-z0-9][a-z0-9._-]*$")
OPERATIONS = {"startup", "seek", "random_seeks", "pause_resume", "concurrent_transcodes"}
PLAYBACK_MODES = {"direct_play", "transcode"}
MEASUREMENT_SCOPES = {"server_engine", "end_to_end"}


class ConfigError(ValueError):
    """The benchmark configuration is unsafe, ambiguous, or incomplete."""


@dataclass(frozen=True)
class BenchmarkConfig:
    path: Path
    document: dict[str, Any]
    media: dict[str, dict[str, Any]]
    scenarios: tuple[dict[str, Any], ...]
    servers: dict[str, dict[str, Any]]
    run: dict[str, Any]
    client: dict[str, Any]


def _object(value: Any, where: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ConfigError(f"{where} must be a TOML table")
    return value


def _name(value: Any, where: str) -> str:
    if not isinstance(value, str) or not NAME.fullmatch(value):
        raise ConfigError(f"{where} must match {NAME.pattern}")
    return value


def _string(value: Any, where: str, *, allow_empty: bool = False) -> str:
    if not isinstance(value, str) or (not allow_empty and not value.strip()):
        raise ConfigError(f"{where} must be a non-empty string")
    return value


def _number(value: Any, where: str, *, minimum: float = 0, strict: bool = True) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value):
        raise ConfigError(f"{where} must be a finite number")
    if (strict and value <= minimum) or (not strict and value < minimum):
        relation = "greater than" if strict else "at least"
        raise ConfigError(f"{where} must be {relation} {minimum}")
    return float(value)


def _integer(value: Any, where: str, *, minimum: int = 1) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        raise ConfigError(f"{where} must be an integer >= {minimum}")
    return value


def _command(value: Any, where: str) -> list[str] | None:
    if value is None:
        return None
    if not isinstance(value, list) or not value:
        raise ConfigError(f"{where} must be a non-empty array of argv strings")
    if any(not isinstance(part, str) or not part for part in value):
        raise ConfigError(f"{where} must contain only non-empty strings")
    return list(value)


def _base_url(value: Any, where: str) -> str:
    url = _string(value, where).rstrip("/")
    parsed = urlsplit(url)
    if parsed.scheme not in ("http", "https") or not parsed.netloc:
        raise ConfigError(f"{where} must be an absolute http(s) URL")
    if parsed.username is not None or parsed.password is not None:
        raise ConfigError(f"{where} must not embed credentials; use token_env")
    if parsed.query or parsed.fragment:
        raise ConfigError(f"{where} must not contain a query or fragment")
    return url


def _optional_string(table: dict[str, Any], key: str, where: str) -> None:
    value = table.get(key)
    if value is not None:
        _string(value, f"{where}.{key}")


def _validate_server(key: str, value: Any) -> dict[str, Any]:
    where = f"servers.{key}"
    server = dict(_object(value, where))
    runner = server.get("runner", key)
    if runner not in ("cinema", "plex"):
        raise ConfigError(f"{where}.runner must be 'cinema' or 'plex'")
    server["runner"] = runner
    server["base_url"] = _base_url(server.get("base_url"), f"{where}.base_url")
    server["token_env"] = _string(server.get("token_env"), f"{where}.token_env")
    _optional_string(server, "commit", where)
    _optional_string(server, "label", where)
    server["monitor_command"] = _command(server.get("monitor_command"), f"{where}.monitor_command")
    server["before_trial_command"] = _command(
        server.get("before_trial_command"), f"{where}.before_trial_command"
    )
    server["after_trial_command"] = _command(
        server.get("after_trial_command"), f"{where}.after_trial_command"
    )
    return server


def _validate_media(value: Any, index: int, server_keys: set[str]) -> dict[str, Any]:
    where = f"media[{index}]"
    medium = dict(_object(value, where))
    medium["id"] = _name(medium.get("id"), f"{where}.id")
    medium["title"] = _string(medium.get("title", medium["id"]), f"{where}.title")
    medium["duration_seconds"] = _number(
        medium.get("duration_seconds"), f"{where}.duration_seconds"
    )
    for field in ("width", "height", "bitrate_kbps"):
        medium[field] = _integer(medium.get(field), f"{where}.{field}")
    for field in ("container", "video_codec", "audio_codec", "sha256"):
        medium[field] = _string(medium.get(field), f"{where}.{field}")
    if not re.fullmatch(r"[0-9a-f]{64}", medium["sha256"]):
        raise ConfigError(f"{where}.sha256 must be a lowercase SHA-256 digest")

    bindings = _object(medium.get("bindings"), f"{where}.bindings")
    missing = server_keys - set(bindings)
    if missing:
        raise ConfigError(f"{where}.bindings is missing server(s): {', '.join(sorted(missing))}")
    for server_key in server_keys:
        binding = _object(bindings[server_key], f"{where}.bindings.{server_key}")
        runner = binding.get("runner")
        if runner == "cinema" or "file_id" in binding:
            _integer(binding.get("file_id"), f"{where}.bindings.{server_key}.file_id")
        elif runner == "plex" or "rating_key" in binding:
            _string(str(binding.get("rating_key", "")), f"{where}.bindings.{server_key}.rating_key")
            for field in ("media_index", "part_index"):
                if field in binding:
                    _integer(binding[field], f"{where}.bindings.{server_key}.{field}", minimum=0)
            _optional_string(binding, "part_key", f"{where}.bindings.{server_key}")
        else:
            raise ConfigError(
                f"{where}.bindings.{server_key} needs file_id (Cinema) or rating_key (Plex)"
            )
    return medium


def _scope_list(value: Any, where: str) -> list[str]:
    if not isinstance(value, list) or not value:
        raise ConfigError(f"{where} must be a non-empty array")
    scopes = []
    for scope in value:
        if scope not in MEASUREMENT_SCOPES:
            raise ConfigError(
                f"{where} contains {scope!r}; expected server_engine or end_to_end"
            )
        if scope not in scopes:
            scopes.append(scope)
    return scopes


def _validate_scenario(
    value: Any, index: int, media_ids: set[str], default_iterations: int
) -> dict[str, Any]:
    where = f"scenarios[{index}]"
    scenario = dict(_object(value, where))
    scenario["id"] = _name(scenario.get("id"), f"{where}.id")
    operation = scenario.get("operation")
    if operation not in OPERATIONS:
        raise ConfigError(f"{where}.operation must be one of {', '.join(sorted(OPERATIONS))}")
    medium = _name(scenario.get("media"), f"{where}.media")
    if medium not in media_ids:
        raise ConfigError(f"{where}.media refers to unknown media {medium!r}")
    mode = scenario.get("playback_mode")
    if mode not in PLAYBACK_MODES:
        raise ConfigError(f"{where}.playback_mode must be direct_play or transcode")
    if operation == "concurrent_transcodes" and mode != "transcode":
        raise ConfigError(f"{where} concurrent_transcodes requires playback_mode='transcode'")
    scenario["iterations"] = _integer(
        scenario.get("iterations", default_iterations), f"{where}.iterations"
    )
    scenario["measurement_scopes"] = _scope_list(
        scenario.get("measurement_scopes", ["server_engine", "end_to_end"]),
        f"{where}.measurement_scopes",
    )
    scenario["cache_state"] = _string(
        scenario.get("cache_state", "warm"), f"{where}.cache_state"
    )
    if scenario["cache_state"] not in ("warm", "cold", "mixed"):
        raise ConfigError(f"{where}.cache_state must be warm, cold, or mixed")

    for field in ("output_height", "output_bitrate_kbps", "concurrency", "seek_count"):
        if field in scenario:
            scenario[field] = _integer(scenario[field], f"{where}.{field}")
    for field in (
        "baseline_position_seconds",
        "warmup_seconds",
        "pause_seconds",
        "observe_seconds",
        "seek_min_seconds",
        "seek_max_seconds",
    ):
        if field in scenario:
            scenario[field] = _number(
                scenario[field], f"{where}.{field}", minimum=0, strict=False
            )
    for field in ("seek_deltas_seconds", "seek_positions_seconds"):
        if field in scenario:
            values = scenario[field]
            if not isinstance(values, list) or not values:
                raise ConfigError(f"{where}.{field} must be a non-empty number array")
            scenario[field] = [
                _number(item, f"{where}.{field}[{i}]", minimum=-math.inf, strict=False)
                for i, item in enumerate(values)
            ]
    if operation == "startup":
        pass
    elif operation == "seek":
        if not scenario.get("seek_deltas_seconds") and not scenario.get("seek_positions_seconds"):
            raise ConfigError(f"{where} seek requires seek_deltas_seconds or seek_positions_seconds")
    elif operation == "random_seeks":
        scenario["seek_count"] = _integer(scenario.get("seek_count", 50), f"{where}.seek_count")
        scenario["seed"] = _integer(scenario.get("seed", 20260815), f"{where}.seed", minimum=0)
    elif operation == "pause_resume":
        scenario["warmup_seconds"] = _number(
            scenario.get("warmup_seconds", 5), f"{where}.warmup_seconds", minimum=0
        )
        scenario["pause_seconds"] = _number(
            scenario.get("pause_seconds", 5), f"{where}.pause_seconds", minimum=0
        )
        if "server_engine" in scenario["measurement_scopes"]:
            raise ConfigError(f"{where} pause_resume is end-to-end only")
    elif operation == "concurrent_transcodes":
        scenario["concurrency"] = _integer(
            scenario.get("concurrency"), f"{where}.concurrency"
        )
        scenario["observe_seconds"] = _number(
            scenario.get("observe_seconds", 8), f"{where}.observe_seconds", minimum=0
        )
    if mode == "transcode":
        scenario["output_height"] = _integer(
            scenario.get("output_height"), f"{where}.output_height"
        )
        scenario["output_bitrate_kbps"] = _integer(
            scenario.get("output_bitrate_kbps"), f"{where}.output_bitrate_kbps"
        )
    return scenario


def load_config(path: str | Path) -> BenchmarkConfig:
    config_path = Path(path).resolve()
    try:
        document = tomllib.loads(config_path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise ConfigError(f"cannot read {config_path}: {error}") from error
    if document.get("schema_version") != SCHEMA_VERSION:
        raise ConfigError(f"schema_version must be {SCHEMA_VERSION}")

    run = dict(_object(document.get("run", {}), "run"))
    run["iterations"] = _integer(run.get("iterations", 30), "run.iterations")
    run["timeout_seconds"] = _number(run.get("timeout_seconds", 120), "run.timeout_seconds")
    run["cooldown_seconds"] = _number(
        run.get("cooldown_seconds", 2), "run.cooldown_seconds", minimum=0, strict=False
    )
    run["output_dir"] = _string(run.get("output_dir", "target/cinema-plex-bench"), "run.output_dir")

    client = dict(_object(document.get("client", {}), "client"))
    client["name"] = _string(client.get("name", "ffmpeg"), "client.name")
    client["version"] = _string(client.get("version", "auto"), "client.version")
    client["host"] = _string(client.get("host", "controller"), "client.host")
    client["ffmpeg"] = _string(client.get("ffmpeg", "ffmpeg"), "client.ffmpeg")

    raw_servers = _object(document.get("servers"), "servers")
    if len(raw_servers) != 2:
        raise ConfigError("servers must contain exactly two A/B server tables")
    servers = {_name(key, "servers key"): _validate_server(key, value) for key, value in raw_servers.items()}
    runners = sorted(server["runner"] for server in servers.values())
    if runners != ["cinema", "plex"]:
        raise ConfigError("servers must configure exactly one Cinema runner and one Plex runner")

    raw_media = document.get("media")
    if not isinstance(raw_media, list) or not raw_media:
        raise ConfigError("media must be a non-empty array of tables")
    media_list = [_validate_media(value, i, set(servers)) for i, value in enumerate(raw_media)]
    media = {medium["id"]: medium for medium in media_list}
    if len(media) != len(media_list):
        raise ConfigError("media ids must be unique")

    raw_scenarios = document.get("scenarios")
    if not isinstance(raw_scenarios, list) or not raw_scenarios:
        raise ConfigError("scenarios must be a non-empty array of tables")
    scenarios = tuple(
        _validate_scenario(value, i, set(media), run["iterations"])
        for i, value in enumerate(raw_scenarios)
    )
    if len({scenario["id"] for scenario in scenarios}) != len(scenarios):
        raise ConfigError("scenario ids must be unique")

    return BenchmarkConfig(config_path, document, media, scenarios, servers, run, client)


def seek_targets(scenario: dict[str, Any], medium: dict[str, Any]) -> list[dict[str, Any]]:
    """Return stable absolute targets with their operation labels."""
    duration = float(medium["duration_seconds"])
    operation = scenario["operation"]
    targets: list[dict[str, Any]] = []
    if operation == "seek":
        baseline = float(scenario.get("baseline_position_seconds", 0))
        for delta in scenario.get("seek_deltas_seconds", []):
            target = baseline + float(delta)
            targets.append({"target_seconds": target, "delta_seconds": float(delta)})
        for target in scenario.get("seek_positions_seconds", []):
            targets.append({"target_seconds": float(target), "delta_seconds": None})
    elif operation == "random_seeks":
        low = float(scenario.get("seek_min_seconds", 30))
        high = float(scenario.get("seek_max_seconds", duration - 30))
        if high <= low:
            raise ConfigError(
                f"scenario {scenario['id']!r} has no random-seek window inside {duration}s"
            )
        generator = random.Random(scenario["seed"])
        targets = [
            {"target_seconds": generator.uniform(low, high), "delta_seconds": None}
            for _ in range(scenario["seek_count"])
        ]
    for index, target in enumerate(targets):
        value = target["target_seconds"]
        if value < 0 or value >= duration:
            raise ConfigError(
                f"scenario {scenario['id']!r} seek target {value}s is outside media duration {duration}s"
            )
        target["seek_index"] = index
    return targets
