"""Distribution summaries and Cinema-vs-Plex ratio reports."""

from __future__ import annotations

from collections import defaultdict
from datetime import datetime, timezone
import json
import math
from pathlib import Path
from typing import Any, Iterable

from . import SCHEMA_VERSION


GROUP_FIELDS = (
    "scenario_id",
    "operation",
    "operation_variant",
    "media",
    "playback_mode",
    "measurement_scope",
    "concurrency",
)


class ReportError(ValueError):
    pass


def percentile(values: Iterable[float], percent: float) -> float | None:
    """R/NumPy type-7 linear percentile; successful finite samples only."""
    ordered = sorted(float(value) for value in values if math.isfinite(float(value)))
    if not ordered:
        return None
    if len(ordered) == 1:
        return ordered[0]
    position = (len(ordered) - 1) * percent / 100
    lower = math.floor(position)
    upper = math.ceil(position)
    if lower == upper:
        return ordered[lower]
    fraction = position - lower
    return ordered[lower] * (1 - fraction) + ordered[upper] * fraction


def load_rows(path: str | Path) -> list[dict[str, Any]]:
    from .harness import HarnessError, validate_measurement_row

    rows: list[dict[str, Any]] = []
    source = Path(path)
    try:
        lines = source.read_text(encoding="utf-8").splitlines()
    except OSError as error:
        raise ReportError(f"cannot read {source}: {error}") from error
    for line_number, line in enumerate(lines, 1):
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError as error:
            raise ReportError(f"{source}:{line_number} is not JSON") from error
        if not isinstance(row, dict) or row.get("schema_version") != SCHEMA_VERSION:
            raise ReportError(f"{source}:{line_number} has an unsupported schema")
        if row.get("record_type") != "measurement":
            continue
        try:
            validate_measurement_row(row)
        except HarnessError as error:
            raise ReportError(f"{source}:{line_number} has an invalid measurement: {error}") from error
        rows.append(row)
    if not rows:
        raise ReportError(f"{source} contains no measurement rows")
    return rows


def _group_key(row: dict[str, Any]) -> tuple[Any, ...]:
    return tuple(row.get(field) for field in GROUP_FIELDS)


def summarize(rows: list[dict[str, Any]], source: str | None = None) -> dict[str, Any]:
    grouped: dict[tuple[Any, ...], list[dict[str, Any]]] = defaultdict(list)
    for row in rows:
        grouped[(_group_key(row), row.get("server"))].append(row)

    groups: list[dict[str, Any]] = []
    by_comparison: dict[tuple[Any, ...], dict[str, dict[str, Any]]] = defaultdict(dict)
    for (key, server), samples in sorted(grouped.items(), key=lambda item: str(item[0])):
        successful = [
            float(row["latency_ms"])
            for row in samples
            if row.get("success") and isinstance(row.get("latency_ms"), (int, float))
        ]
        failures = sum(not bool(row.get("success")) for row in samples)
        group = dict(zip(GROUP_FIELDS, key))
        group.update(
            {
                "server": server,
                "server_runner": samples[0].get("server_runner"),
                "server_version": samples[0].get("server_version"),
                "server_commit": samples[0].get("server_commit"),
                "samples": len(samples),
                "successful_samples": len(successful),
                "failures": failures,
                "failure_rate": failures / len(samples),
                "error_rate": failures / len(samples),
                "output_contract_verified": all(
                    row.get("output_contract_verified") is True for row in samples
                ),
                "latency_ms": {
                    "p50": percentile(successful, 50),
                    "p95": percentile(successful, 95),
                    "p99": percentile(successful, 99),
                },
            }
        )
        groups.append(group)
        runner = group["server_runner"]
        if runner in ("cinema", "plex"):
            by_comparison[key][runner] = group

    comparisons: list[dict[str, Any]] = []
    for key, pair in sorted(by_comparison.items(), key=lambda item: str(item[0])):
        if set(pair) != {"cinema", "plex"}:
            continue
        cinema, plex = pair["cinema"], pair["plex"]
        if not cinema["output_contract_verified"] or not plex["output_contract_verified"]:
            continue
        ratios: dict[str, float | None] = {}
        speedups: dict[str, float | None] = {}
        for percentile_name in ("p50", "p95", "p99"):
            cinema_value = cinema["latency_ms"][percentile_name]
            plex_value = plex["latency_ms"][percentile_name]
            ratios[percentile_name] = (
                cinema_value / plex_value
                if cinema_value is not None and plex_value not in (None, 0)
                else None
            )
            speedups[percentile_name] = (
                plex_value / cinema_value
                if plex_value is not None and cinema_value not in (None, 0)
                else None
            )
        comparison = dict(zip(GROUP_FIELDS, key))
        comparison.update(
            {
                "cinema_server": cinema["server"],
                "plex_server": plex["server"],
                "cinema_to_plex_latency_ratio": ratios,
                "plex_over_cinema_speedup": speedups,
                "cinema_failure_rate": cinema["failure_rate"],
                "plex_failure_rate": plex["failure_rate"],
                "failure_rate_difference": cinema["failure_rate"] - plex["failure_rate"],
            }
        )
        comparisons.append(comparison)

    return {
        "schema_version": SCHEMA_VERSION,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "source": source,
        "measurement_rows": len(rows),
        "groups": groups,
        "comparisons": comparisons,
    }


def _metric(value: float | None, suffix: str = "", precision: int = 1) -> str:
    return "—" if value is None else f"{value:.{precision}f}{suffix}"


def markdown_report(summary: dict[str, Any]) -> str:
    lines = [
        "# Cinema vs Plex benchmark report",
        "",
        f"Generated from {summary['measurement_rows']} raw measurement rows. Latency percentiles",
        "use successful samples; failure and error rates use every attempt. A Cinema/Plex",
        "latency ratio below 1.00 means Cinema was faster for that percentile.",
        "",
        "## Latency and failures",
        "",
        "| Scenario | Operation | Scope | Server | Samples | Failures | p50 ms | p95 ms | p99 ms |",
        "|---|---|---|---:|---:|---:|---:|---:|---:|",
    ]
    for group in summary["groups"]:
        latency = group["latency_ms"]
        lines.append(
            "| {scenario} | {operation} | {scope} | {server} | {samples} | {failures} ({rate:.1%}) | "
            "{p50} | {p95} | {p99} |".format(
                scenario=group["scenario_id"],
                operation=group["operation_variant"],
                scope=group["measurement_scope"],
                server=group["server"],
                samples=group["samples"],
                failures=group["failures"],
                rate=group["failure_rate"],
                p50=_metric(latency["p50"]),
                p95=_metric(latency["p95"]),
                p99=_metric(latency["p99"]),
            )
        )
    lines.extend(
        [
            "",
            "## Cinema-to-Plex ratios",
            "",
            "| Scenario | Operation | Scope | p50 | p95 | p99 | Failure-rate delta |",
            "|---|---|---|---:|---:|---:|---:|",
        ]
    )
    for comparison in summary["comparisons"]:
        ratios = comparison["cinema_to_plex_latency_ratio"]
        lines.append(
            "| {scenario} | {operation} | {scope} | {p50} | {p95} | {p99} | {failure:+.1%} |".format(
                scenario=comparison["scenario_id"],
                operation=comparison["operation_variant"],
                scope=comparison["measurement_scope"],
                p50=_metric(ratios["p50"], "×", 2),
                p95=_metric(ratios["p95"], "×", 2),
                p99=_metric(ratios["p99"], "×", 2),
                failure=comparison["failure_rate_difference"],
            )
        )
    lines.extend(
        [
            "",
            "## How to read it",
            "",
            "Server/engine rows stop at direct bytes, a target source packet, or a completed",
            "HLS segment. End-to-end rows stop when the common FFmpeg client decodes its first",
            "video frame. Do not compare rows across scopes. Treat a lower latency with a higher",
            "failure rate as a regression until the failures are explained.",
            "",
        ]
    )
    return "\n".join(lines)


def write_report(
    raw_path: str | Path, json_path: str | Path, markdown_path: str | Path
) -> dict[str, Any]:
    rows = load_rows(raw_path)
    summary = summarize(rows, str(raw_path))
    json_output = Path(json_path)
    markdown_output = Path(markdown_path)
    json_output.parent.mkdir(parents=True, exist_ok=True)
    markdown_output.parent.mkdir(parents=True, exist_ok=True)
    json_output.write_text(
        json.dumps(summary, indent=2, sort_keys=True, allow_nan=False) + "\n",
        encoding="utf-8",
    )
    markdown_output.write_text(markdown_report(summary), encoding="utf-8")
    return summary
