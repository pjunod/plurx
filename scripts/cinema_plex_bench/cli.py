"""Command-line interface for the Cinema-vs-Plex benchmark suite."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
import sys
from typing import Any

from .config import BenchmarkConfig, ConfigError, load_config, seek_targets
from .harness import (
    BenchmarkHarness,
    HarnessError,
    validate_cold_readiness,
    validate_run_readiness,
    validate_selection,
)
from .reporting import ReportError, write_report


def v1_coverage(config: BenchmarkConfig) -> tuple[set[str], set[str]]:
    covered: set[str] = set()
    concurrency: set[int] = set()
    for scenario in config.scenarios:
        medium = config.media[scenario["media"]]
        operation = scenario["operation"]
        mode = scenario["playback_mode"]
        if operation == "startup" and mode == "direct_play":
            if medium["height"] == 1080:
                covered.add("1080p direct-play startup")
            if medium["height"] >= 2160:
                covered.add("4K direct-play startup")
        if operation == "startup" and mode == "transcode":
            if medium["height"] == 1080 and scenario.get("output_height") == 1080:
                covered.add("1080p transcode startup")
            if medium["height"] >= 2160 and scenario.get("output_height") == 1080:
                covered.add("4K-to-1080p transcode startup")
        if operation == "seek":
            deltas = {round(abs(value)) for value in scenario.get("seek_deltas_seconds", [])}
            signs = {value > 0 for value in scenario.get("seek_deltas_seconds", [])}
            if 30 in deltas and signs == {False, True}:
                covered.add("±30-second seeks")
            if 300 in deltas:
                covered.add("5-minute seeks")
        if operation == "random_seeks" and scenario.get("seek_count", 0) >= 50:
            covered.add("50 deterministic/random seeks")
        if operation == "pause_resume":
            covered.add("pause/resume")
        if operation == "concurrent_transcodes":
            concurrency.add(scenario["concurrency"])
    if {1, 2, 4, 8}.issubset(concurrency):
        covered.add("1/2/4/8 simultaneous transcodes")
    required = {
        "1080p direct-play startup",
        "4K direct-play startup",
        "1080p transcode startup",
        "4K-to-1080p transcode startup",
        "±30-second seeks",
        "5-minute seeks",
        "50 deterministic/random seeks",
        "pause/resume",
        "1/2/4/8 simultaneous transcodes",
    }
    return covered, required - covered


def validation_document(config: BenchmarkConfig) -> dict[str, Any]:
    covered, missing = v1_coverage(config)
    scenarios = []
    for scenario in config.scenarios:
        medium = config.media[scenario["media"]]
        targets = seek_targets(scenario, medium) if scenario["operation"] in ("seek", "random_seeks") else []
        samples_per_iteration = (
            scenario.get("concurrency", 1)
            * max(1, len(targets))
            * len(scenario["measurement_scopes"])
        )
        scenarios.append(
            {
                "id": scenario["id"],
                "operation": scenario["operation"],
                "media": scenario["media"],
                "iterations": scenario["iterations"],
                "measurement_scopes": scenario["measurement_scopes"],
                "samples_per_server": samples_per_iteration * scenario["iterations"],
                "seek_targets_seconds": [round(target["target_seconds"], 6) for target in targets],
            }
        )
    return {
        "config": str(config.path),
        "servers": list(config.servers),
        "media": list(config.media),
        "scenarios": scenarios,
        "v1_coverage": sorted(covered),
        "v1_missing": sorted(missing),
    }


def _csv_set(values: list[str] | None) -> set[str] | None:
    if not values:
        return None
    return {
        item.strip()
        for value in values
        for item in value.split(",")
        if item.strip()
    }


def _default_output(config: BenchmarkConfig) -> Path:
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return Path(config.run["output_dir"]) / stamp


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Reproducible Cinema/plurx-vs-Plex playback benchmark",
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    validate = subparsers.add_parser("validate", help="validate and expand a scenario TOML")
    validate.add_argument("--config", required=True)
    validate.add_argument(
        "--require-v1", action="store_true", help="fail unless every v1 benchmark case is present"
    )

    run = subparsers.add_parser("run", help="run real A/B measurements")
    run.add_argument("--config", required=True)
    run.add_argument("--output-dir", default=None, help="exact artifact directory")
    run.add_argument("--server", action="append", help="limit to a server key; repeat or comma-separate")
    run.add_argument(
        "--scenario", action="append", help="limit to a scenario id; repeat or comma-separate"
    )
    run.add_argument("--iterations", type=int, default=None, help="override every scenario iteration count")

    report = subparsers.add_parser("report", help="regenerate reports from raw JSONL")
    report.add_argument("--input", required=True, help="raw.jsonl")
    report.add_argument("--json", default=None, help="summary JSON output")
    report.add_argument("--markdown", default=None, help="Markdown report output")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        if args.command == "report":
            raw = Path(args.input)
            json_path = Path(args.json) if args.json else raw.with_name("summary.json")
            markdown_path = Path(args.markdown) if args.markdown else raw.with_name("report.md")
            summary = write_report(raw, json_path, markdown_path)
            print(f"measurements: {summary['measurement_rows']}")
            print(f"summary: {json_path}")
            print(f"report: {markdown_path}")
            return 0

        config = load_config(args.config)
        if args.command == "validate":
            document = validation_document(config)
            print(json.dumps(document, indent=2, sort_keys=True))
            if args.require_v1 and document["v1_missing"]:
                print("missing v1 coverage: " + ", ".join(document["v1_missing"]), file=sys.stderr)
                return 2
            return 0

        if args.iterations is not None and args.iterations < 1:
            raise ConfigError("--iterations must be >= 1")
        validate_run_readiness(config)
        only_servers = _csv_set(args.server)
        only_scenarios = _csv_set(args.scenario)
        validate_selection(config, only_servers, only_scenarios)
        validate_cold_readiness(config, only_servers, only_scenarios)
        output = Path(args.output_dir) if args.output_dir else _default_output(config)
        output.mkdir(parents=True, exist_ok=False)
        harness = BenchmarkHarness(
            config,
            output,
            only_servers=only_servers,
            only_scenarios=only_scenarios,
            iterations_override=args.iterations,
        )
        raw_path = harness.run()
        summary = write_report(raw_path, output / "summary.json", output / "report.md")
        print(f"measurements: {summary['measurement_rows']}")
        print(f"raw: {raw_path}")
        print(f"report: {output / 'report.md'}")
        return 0
    except (ConfigError, HarnessError, ReportError, RuntimeError, OSError) as error:
        print(f"benchmark error: {error}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        print("benchmark interrupted; completed raw rows remain in the output directory", file=sys.stderr)
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
