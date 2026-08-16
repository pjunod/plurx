"""Resource sampling and explicit operator hooks."""

from __future__ import annotations

import json
import math
import subprocess
from typing import Any


RESOURCE_FIELDS = (
    "cpu_percent",
    "gpu_percent",
    "rss_bytes",
    "storage_read_bytes",
    "storage_write_bytes",
    "network_rx_bytes",
    "network_tx_bytes",
)


class MonitorError(RuntimeError):
    pass


def _finite_number(value: Any, field: str) -> float | int | None:
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value):
        raise MonitorError(f"monitor field {field!r} must be a finite number or null")
    if value < 0:
        raise MonitorError(f"monitor field {field!r} must not be negative")
    return value


class ResourceMonitor:
    """One command invocation is one cumulative-counter snapshot."""

    def __init__(self, command: list[str] | None, timeout_seconds: float):
        self.command = command
        self.timeout_seconds = timeout_seconds

    def sample(self, context: dict[str, Any]) -> dict[str, float | int | None]:
        if not self.command:
            return {field: None for field in RESOURCE_FIELDS}
        try:
            result = subprocess.run(
                self.command,
                input=json.dumps(context, separators=(",", ":")),
                capture_output=True,
                text=True,
                timeout=self.timeout_seconds,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            raise MonitorError(f"resource monitor failed to run: {error}") from error
        if result.returncode != 0:
            detail = (result.stderr or "no diagnostic output").strip().splitlines()[-1:]
            raise MonitorError(f"resource monitor exited {result.returncode}: {' '.join(detail)}")
        try:
            document = json.loads(result.stdout)
        except json.JSONDecodeError as error:
            raise MonitorError("resource monitor did not print one JSON object") from error
        if not isinstance(document, dict):
            raise MonitorError("resource monitor output must be a JSON object")
        return {field: _finite_number(document.get(field), field) for field in RESOURCE_FIELDS}


def resource_delta(
    before: dict[str, float | int | None], after: dict[str, float | int | None]
) -> dict[str, Any]:
    result: dict[str, Any] = {}
    anomalies: list[str] = []
    for field in ("cpu_percent", "gpu_percent"):
        samples = [value for value in (before.get(field), after.get(field)) if value is not None]
        result[field] = sum(samples) / len(samples) if samples else None
    rss = [value for value in (before.get("rss_bytes"), after.get("rss_bytes")) if value is not None]
    result["rss_bytes"] = max(rss) if rss else None
    for field in (
        "storage_read_bytes",
        "storage_write_bytes",
        "network_rx_bytes",
        "network_tx_bytes",
    ):
        first, last = before.get(field), after.get(field)
        if first is None or last is None:
            result[field] = None
        elif last < first:
            result[field] = None
            anomalies.append(f"{field}_counter_reset")
        else:
            result[field] = last - first
    result["resource_anomalies"] = anomalies
    return result


def run_hook(command: list[str] | None, context: dict[str, Any], timeout_seconds: float) -> None:
    if not command:
        return
    try:
        result = subprocess.run(
            command,
            input=json.dumps(context, separators=(",", ":")),
            capture_output=True,
            text=True,
            timeout=timeout_seconds,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise MonitorError(f"trial hook failed to run: {error}") from error
    if result.returncode != 0:
        detail = (result.stderr or result.stdout or "no diagnostic output").strip().splitlines()[-1:]
        raise MonitorError(f"trial hook exited {result.returncode}: {' '.join(detail)}")
