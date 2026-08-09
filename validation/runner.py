"""Declarative functionality-point selection and validation execution."""

from __future__ import annotations

import argparse
import dataclasses
import datetime as dt
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time
import tomllib
import xml.etree.ElementTree as ET


REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_CATALOG = REPO_ROOT / "validation" / "points.toml"
ID_RE = re.compile(r"^[a-z][a-z0-9]*(?:[.-][a-z0-9]+)*$")
PLATFORMS = {"darwin", "linux", "windows"}


class CatalogError(ValueError):
    """The catalog does not satisfy the framework contract."""


@dataclasses.dataclass(frozen=True)
class Check:
    id: str
    title: str
    command: str
    profiles: tuple[str, ...]
    platforms: tuple[str, ...]
    requires: tuple[str, ...]
    requires_files: tuple[str, ...]
    preflight: str | None
    missing: str
    timeout_seconds: int
    skip_if_env: tuple[str, ...]


@dataclasses.dataclass(frozen=True)
class Point:
    id: str
    title: str
    contract: str
    paths: tuple[str, ...]
    checks: tuple[str, ...]
    depends_on: tuple[str, ...]


@dataclasses.dataclass(frozen=True)
class Catalog:
    path: Path
    profiles: tuple[str, ...]
    artifact_dir: str
    always_checks: tuple[str, ...]
    audit_paths: tuple[str, ...]
    audit_exclude: tuple[str, ...]
    allow_unmatched_globs: tuple[str, ...]
    checks: tuple[Check, ...]
    points: tuple[Point, ...]

    @property
    def check_map(self) -> dict[str, Check]:
        return {check.id: check for check in self.checks}

    @property
    def point_map(self) -> dict[str, Point]:
        return {point.id: point for point in self.points}


@dataclasses.dataclass(frozen=True)
class Selection:
    point_ids: tuple[str, ...]
    reasons: dict[str, tuple[str, ...]]
    paths: tuple[str, ...]


@dataclasses.dataclass
class CheckResult:
    id: str
    title: str
    status: str
    seconds: float
    returncode: int | None
    message: str
    log_path: str | None
    output: str


def _strings(value: object, field: str) -> tuple[str, ...]:
    if value is None:
        return ()
    if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
        raise CatalogError(f"{field} must be an array of strings")
    return tuple(value)


def load_catalog(path: Path = DEFAULT_CATALOG) -> Catalog:
    try:
        with path.open("rb") as handle:
            raw = tomllib.load(handle)
    except (OSError, tomllib.TOMLDecodeError) as exc:
        raise CatalogError(f"cannot read {path}: {exc}") from exc

    if raw.get("version") != 1:
        raise CatalogError("catalog version must be 1")

    settings = raw.get("settings", {})
    if not isinstance(settings, dict):
        raise CatalogError("settings must be a table")

    profiles = _strings(settings.get("profiles"), "settings.profiles")
    if not profiles:
        raise CatalogError("settings.profiles must not be empty")

    checks: list[Check] = []
    for index, item in enumerate(raw.get("checks", [])):
        where = f"checks[{index}]"
        if not isinstance(item, dict):
            raise CatalogError(f"{where} must be a table")
        try:
            timeout = int(item.get("timeout_seconds", 900))
        except (TypeError, ValueError) as exc:
            raise CatalogError(f"{where}.timeout_seconds must be an integer") from exc
        checks.append(
            Check(
                id=str(item.get("id", "")),
                title=str(item.get("title", "")),
                command=str(item.get("command", "")),
                profiles=_strings(item.get("profiles"), f"{where}.profiles"),
                platforms=_strings(
                    item.get("platforms", ["darwin", "linux", "windows"]),
                    f"{where}.platforms",
                ),
                requires=_strings(item.get("requires"), f"{where}.requires"),
                requires_files=_strings(
                    item.get("requires_files"), f"{where}.requires_files"
                ),
                preflight=(str(item["preflight"]) if "preflight" in item else None),
                missing=str(item.get("missing", "fail")),
                timeout_seconds=timeout,
                skip_if_env=_strings(item.get("skip_if_env"), f"{where}.skip_if_env"),
            )
        )

    points: list[Point] = []
    for index, item in enumerate(raw.get("points", [])):
        where = f"points[{index}]"
        if not isinstance(item, dict):
            raise CatalogError(f"{where} must be a table")
        points.append(
            Point(
                id=str(item.get("id", "")),
                title=str(item.get("title", "")),
                contract=str(item.get("contract", "")),
                paths=_strings(item.get("paths"), f"{where}.paths"),
                checks=_strings(item.get("checks"), f"{where}.checks"),
                depends_on=_strings(item.get("depends_on"), f"{where}.depends_on"),
            )
        )

    return Catalog(
        path=path,
        profiles=profiles,
        artifact_dir=str(settings.get("artifact_dir", "target/validation")),
        always_checks=_strings(settings.get("always_checks"), "settings.always_checks"),
        audit_paths=_strings(settings.get("audit_paths"), "settings.audit_paths"),
        audit_exclude=_strings(settings.get("audit_exclude"), "settings.audit_exclude"),
        allow_unmatched_globs=_strings(
            settings.get("allow_unmatched_globs"),
            "settings.allow_unmatched_globs",
        ),
        checks=tuple(checks),
        points=tuple(points),
    )


def _expand_braces(pattern: str) -> tuple[str, ...]:
    start = pattern.find("{")
    if start < 0:
        return (pattern,)
    end = pattern.find("}", start + 1)
    if end < 0:
        return (pattern,)
    choices = pattern[start + 1 : end].split(",")
    expanded: list[str] = []
    for choice in choices:
        expanded.extend(
            _expand_braces(pattern[:start] + choice + pattern[end + 1 :])
        )
    return tuple(expanded)


def _glob_fragment(pattern: str) -> str:
    pieces: list[str] = ["^"]
    index = 0
    while index < len(pattern):
        char = pattern[index]
        if char == "*":
            if index + 1 < len(pattern) and pattern[index + 1] == "*":
                index += 2
                if index < len(pattern) and pattern[index] == "/":
                    pieces.append("(?:.*/)?")
                    index += 1
                else:
                    pieces.append(".*")
                continue
            pieces.append("[^/]*")
        elif char == "?":
            pieces.append("[^/]")
        else:
            pieces.append(re.escape(char))
        index += 1
    pieces.append("$")
    return "".join(pieces)


def glob_regex(pattern: str) -> re.Pattern[str]:
    """Compile a repository-relative glob where ``**`` crosses directories."""

    normalized = normalize_path(pattern)
    alternatives = [_glob_fragment(item) for item in _expand_braces(normalized)]
    return re.compile("(?:" + "|".join(alternatives) + ")")


def normalize_path(path: str) -> str:
    normalized = path.replace("\\", "/")
    while normalized.startswith("./"):
        normalized = normalized[2:]
    return normalized.rstrip("/")


def matches(path: str, patterns: tuple[str, ...]) -> bool:
    normalized = normalize_path(path)
    return any(glob_regex(pattern).match(normalized) for pattern in patterns)


def _is_literal_path(pattern: str) -> bool:
    return not any(marker in pattern for marker in ("*", "?", "{"))


def _git_paths(repo_root: Path) -> tuple[str, ...]:
    command = [
        "git",
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
    ]
    try:
        result = subprocess.run(
            command,
            cwd=repo_root,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
    except (OSError, subprocess.CalledProcessError) as exc:
        raise CatalogError(f"cannot audit tracked files: {exc}") from exc
    return tuple(
        normalize_path(part.decode("utf-8", errors="surrogateescape"))
        for part in result.stdout.split(b"\0")
        if part
    )


def _git_tracked_paths(repo_root: Path) -> tuple[str, ...]:
    try:
        result = subprocess.run(
            ["git", "ls-files", "-z", "--cached"],
            cwd=repo_root,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
    except (OSError, subprocess.CalledProcessError) as exc:
        raise CatalogError(f"cannot audit tracked path globs: {exc}") from exc
    return tuple(
        normalize_path(part.decode("utf-8", errors="surrogateescape"))
        for part in result.stdout.split(b"\0")
        if part
    )


def lint_catalog(
    catalog: Catalog,
    repo_root: Path = REPO_ROOT,
    *,
    audit: bool = True,
    tracked_paths: tuple[str, ...] | None = None,
) -> list[str]:
    errors: list[str] = []
    check_ids = [check.id for check in catalog.checks]
    point_ids = [point.id for point in catalog.points]

    if not catalog.checks:
        errors.append("catalog has no checks")
    if not catalog.points:
        errors.append("catalog has no functionality points")

    for kind, ids in (("check", check_ids), ("point", point_ids)):
        seen: set[str] = set()
        for item_id in ids:
            if not ID_RE.fullmatch(item_id):
                errors.append(f"invalid {kind} id: {item_id!r}")
            if item_id in seen:
                errors.append(f"duplicate {kind} id: {item_id}")
            seen.add(item_id)

    known_profiles = set(catalog.profiles)
    known_checks = set(check_ids)
    known_points = set(point_ids)
    if tracked_paths is None and audit:
        tracked_paths = _git_tracked_paths(repo_root)
    if tracked_paths is not None:
        tracked_paths = tuple(normalize_path(path) for path in tracked_paths)

    declared_patterns = {pattern for point in catalog.points for pattern in point.paths}
    allowed_unmatched = set(catalog.allow_unmatched_globs)
    for pattern in catalog.allow_unmatched_globs:
        if _is_literal_path(pattern):
            errors.append(f"allow_unmatched_globs entry is not a glob: {pattern!r}")
        if pattern not in declared_patterns:
            errors.append(
                f"allow_unmatched_globs entry is not used by any point: {pattern!r}"
            )

    for check in catalog.checks:
        prefix = f"check {check.id or '<missing>'}"
        if not check.title:
            errors.append(f"{prefix} has no title")
        if not check.command:
            errors.append(f"{prefix} has no command")
        if not check.profiles:
            errors.append(f"{prefix} has no profiles")
        for profile in check.profiles:
            if profile not in known_profiles:
                errors.append(f"{prefix} references unknown profile {profile}")
        for platform in check.platforms:
            if platform not in PLATFORMS:
                errors.append(f"{prefix} references unknown platform {platform}")
        if check.missing not in {"fail", "skip"}:
            errors.append(f"{prefix}.missing must be 'fail' or 'skip'")
        if check.timeout_seconds <= 0:
            errors.append(f"{prefix}.timeout_seconds must be positive")

    for check_id in catalog.always_checks:
        if check_id not in known_checks:
            errors.append(f"always_checks references unknown check {check_id}")

    for point in catalog.points:
        prefix = f"point {point.id or '<missing>'}"
        if not point.title:
            errors.append(f"{prefix} has no title")
        if not point.contract:
            errors.append(f"{prefix} has no contract")
        if not point.paths:
            errors.append(f"{prefix} has no path triggers")
        if not point.checks:
            errors.append(f"{prefix} has no checks")
        for pattern in point.paths:
            try:
                glob_regex(pattern)
            except re.error as exc:
                errors.append(f"{prefix} has invalid path glob {pattern!r}: {exc}")
            if _is_literal_path(pattern) and not (repo_root / normalize_path(pattern)).is_file():
                errors.append(f"{prefix} references missing literal path {pattern!r}")
            if (
                tracked_paths is not None
                and not _is_literal_path(pattern)
                and pattern not in allowed_unmatched
                and not any(matches(path, (pattern,)) for path in tracked_paths)
            ):
                errors.append(f"{prefix} path glob matches no tracked file: {pattern!r}")
        for check_id in point.checks:
            if check_id not in known_checks:
                errors.append(f"{prefix} references unknown check {check_id}")
        for dependency in point.depends_on:
            if dependency not in known_points:
                errors.append(f"{prefix} depends on unknown point {dependency}")
            if dependency == point.id:
                errors.append(f"{prefix} depends on itself")

    visiting: set[str] = set()
    visited: set[str] = set()

    def visit(point_id: str, trail: tuple[str, ...]) -> None:
        if point_id in visiting:
            errors.append("point dependency cycle: " + " -> ".join((*trail, point_id)))
            return
        if point_id in visited or point_id not in catalog.point_map:
            return
        visiting.add(point_id)
        for dependency in catalog.point_map[point_id].depends_on:
            visit(dependency, (*trail, point_id))
        visiting.remove(point_id)
        visited.add(point_id)

    for point_id in point_ids:
        visit(point_id, ())

    used_checks = set(catalog.always_checks)
    for point in catalog.points:
        used_checks.update(point.checks)
    for check_id in known_checks - used_checks:
        errors.append(f"check {check_id} is not used by any point or always_checks")

    if audit and catalog.audit_paths:
        for path in _git_paths(repo_root):
            if not matches(path, catalog.audit_paths):
                continue
            if matches(path, catalog.audit_exclude):
                continue
            if not any(matches(path, point.paths) for point in catalog.points):
                errors.append(f"audited file has no functionality point: {path}")

    return errors


def select_points(catalog: Catalog, paths: tuple[str, ...]) -> Selection:
    reasons: dict[str, list[str]] = {}
    normalized = tuple(dict.fromkeys(normalize_path(path) for path in paths if path))

    for point in catalog.points:
        matched = [path for path in normalized if matches(path, point.paths)]
        if matched:
            reasons[point.id] = [f"path:{path}" for path in matched]

    _expand_consumers(catalog, reasons)

    ordered = tuple(point.id for point in catalog.points if point.id in reasons)
    return Selection(
        point_ids=ordered,
        reasons={point_id: tuple(reasons[point_id]) for point_id in ordered},
        paths=normalized,
    )


def select_all(catalog: Catalog) -> Selection:
    return Selection(
        point_ids=tuple(point.id for point in catalog.points),
        reasons={point.id: ("all",) for point in catalog.points},
        paths=(),
    )


def select_named(catalog: Catalog, point_ids: tuple[str, ...]) -> Selection:
    unknown = [point_id for point_id in point_ids if point_id not in catalog.point_map]
    if unknown:
        raise CatalogError("unknown functionality point(s): " + ", ".join(unknown))
    reasons = {point_id: ["explicit"] for point_id in point_ids}
    _expand_consumers(catalog, reasons)
    ordered = tuple(point.id for point in catalog.points if point.id in reasons)
    return Selection(ordered, {key: tuple(reasons[key]) for key in ordered}, ())


def _expand_consumers(catalog: Catalog, reasons: dict[str, list[str]]) -> None:
    """Add every consumer that can be affected by an already-selected point.

    ``A depends_on B`` means a change to B can break A. Impact therefore walks
    the reverse edge, from the changed provider to its consumers. Walking from
    A to B instead only retests providers when a leaf changes and misses the
    cascade this catalog exists to expose.
    """

    consumers: dict[str, list[str]] = {point.id: [] for point in catalog.points}
    for point in catalog.points:
        for dependency in point.depends_on:
            consumers[dependency].append(point.id)

    queue = list(reasons)
    while queue:
        provider_id = queue.pop(0)
        for consumer_id in consumers[provider_id]:
            reason = f"consumer:{provider_id}"
            if consumer_id not in reasons:
                reasons[consumer_id] = [reason]
                queue.append(consumer_id)
            elif reason not in reasons[consumer_id]:
                reasons[consumer_id].append(reason)


def selected_checks(
    catalog: Catalog, selection: Selection, profile: str
) -> tuple[Check, ...]:
    if profile not in catalog.profiles:
        raise CatalogError(
            f"unknown profile {profile!r}; choose from {', '.join(catalog.profiles)}"
        )
    wanted = set(catalog.always_checks)
    for point_id in selection.point_ids:
        wanted.update(catalog.point_map[point_id].checks)
    return tuple(
        check
        for check in catalog.checks
        if check.id in wanted and profile in check.profiles
    )


def _current_platform() -> str:
    if sys.platform.startswith("linux"):
        return "linux"
    if sys.platform == "darwin":
        return "darwin"
    if sys.platform.startswith("win"):
        return "windows"
    return sys.platform


def _missing_reason(check: Check, repo_root: Path) -> str | None:
    missing_tools = [tool for tool in check.requires if shutil.which(tool) is None]
    missing_files = [path for path in check.requires_files if not (repo_root / path).exists()]
    pieces: list[str] = []
    if missing_tools:
        pieces.append("missing tools: " + ", ".join(missing_tools))
    if missing_files:
        pieces.append("missing files: " + ", ".join(missing_files))
    return "; ".join(pieces) if pieces else None


def _run_shell(
    command: str,
    repo_root: Path,
    timeout_seconds: int,
    environment_overrides: dict[str, str] | None = None,
) -> tuple[int, str, float]:
    started = time.monotonic()
    environment = os.environ.copy()
    environment["PLURX_VALIDATION"] = "1"
    environment.update(environment_overrides or {})
    try:
        completed = subprocess.run(
            command,
            cwd=repo_root,
            env=environment,
            shell=True,
            executable="/bin/sh" if os.name != "nt" else None,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=timeout_seconds,
        )
        return completed.returncode, completed.stdout, time.monotonic() - started
    except subprocess.TimeoutExpired as exc:
        output = exc.stdout or ""
        if isinstance(output, bytes):
            output = output.decode("utf-8", errors="replace")
        output += f"\nvalidation timed out after {timeout_seconds}s\n"
        return 124, output, time.monotonic() - started


def execute_checks(
    checks: tuple[Check, ...],
    repo_root: Path,
    artifact_dir: Path,
    *,
    strict: bool,
    fail_fast: bool,
    environment: dict[str, str] | None = None,
) -> list[CheckResult]:
    logs_dir = artifact_dir / "logs"
    logs_dir.mkdir(parents=True, exist_ok=True)
    results: list[CheckResult] = []
    platform = _current_platform()

    for check in checks:
        print(f"\n==> {check.id} · {check.title}", flush=True)

        env_skip = next((name for name in check.skip_if_env if os.environ.get(name)), None)
        if env_skip:
            message = f"skipped because {env_skip} is set"
            print(f"    SKIP {message}", flush=True)
            results.append(CheckResult(check.id, check.title, "skipped", 0, None, message, None, ""))
            continue

        if platform not in check.platforms:
            message = f"requires platform {'/'.join(check.platforms)}; this is {platform}"
            print(f"    SKIP {message}", flush=True)
            results.append(CheckResult(check.id, check.title, "skipped", 0, None, message, None, ""))
            continue

        missing = _missing_reason(check, repo_root)
        if not missing and check.preflight:
            code, output, _ = _run_shell(
                check.preflight, repo_root, 30, environment
            )
            if code != 0:
                detail = output.strip().splitlines()
                missing = "preflight failed" + (f": {detail[-1]}" if detail else "")

        if missing:
            status = "failed" if strict or check.missing == "fail" else "skipped"
            print(f"    {status.upper()} {missing}", flush=True)
            results.append(CheckResult(check.id, check.title, status, 0, None, missing, None, ""))
            if fail_fast and status == "failed":
                break
            continue

        print(f"    $ {check.command}", flush=True)
        returncode, output, seconds = _run_shell(
            check.command, repo_root, check.timeout_seconds, environment
        )
        log_path = logs_dir / f"{check.id}.log"
        log_path.write_text(output, encoding="utf-8")
        if output:
            print(output, end="" if output.endswith("\n") else "\n", flush=True)
        status = "passed" if returncode == 0 else "failed"
        message = f"exit {returncode} in {seconds:.1f}s"
        print(f"    {status.upper()} {message}", flush=True)
        results.append(
            CheckResult(
                check.id,
                check.title,
                status,
                seconds,
                returncode,
                message,
                str(log_path.relative_to(repo_root))
                if log_path.is_relative_to(repo_root)
                else str(log_path),
                output,
            )
        )
        if fail_fast and status == "failed":
            break

    return results


def point_results(
    catalog: Catalog,
    selection: Selection,
    profile: str,
    check_results: list[CheckResult],
) -> list[dict[str, object]]:
    by_id = {result.id: result for result in check_results}
    rows: list[dict[str, object]] = []
    for point_id in selection.point_ids:
        point = catalog.point_map[point_id]
        eligible = [
            check_id
            for check_id in point.checks
            if profile in catalog.check_map[check_id].profiles
        ]
        statuses = [by_id[check_id].status for check_id in eligible if check_id in by_id]
        if not eligible:
            status = "not-covered"
        elif any(item == "failed" for item in statuses):
            status = "failed"
        elif statuses and all(item == "passed" for item in statuses):
            status = "passed"
        elif statuses:
            status = "partial"
        else:
            status = "not-run"
        rows.append(
            {
                "id": point.id,
                "title": point.title,
                "contract": point.contract,
                "status": status,
                "checks": eligible,
                "reasons": list(selection.reasons[point.id]),
            }
        )
    return rows


def _git_ref(repo_root: Path) -> str:
    try:
        completed = subprocess.run(
            ["git", "rev-parse", "--short", "HEAD"],
            cwd=repo_root,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        return completed.stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        return "unknown"


def write_reports(
    catalog: Catalog,
    selection: Selection,
    profile: str,
    results: list[CheckResult],
    artifact_dir: Path,
    repo_root: Path,
) -> tuple[Path, Path]:
    artifact_dir.mkdir(parents=True, exist_ok=True)
    points = point_results(catalog, selection, profile, results)
    report = {
        "schema_version": 1,
        "generated_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "git_ref": _git_ref(repo_root),
        "profile": profile,
        "paths": list(selection.paths),
        "points": points,
        "checks": [
            {
                "id": result.id,
                "title": result.title,
                "status": result.status,
                "seconds": round(result.seconds, 3),
                "returncode": result.returncode,
                "message": result.message,
                "log_path": result.log_path,
            }
            for result in results
        ],
    }
    json_path = artifact_dir / "report.json"
    json_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    suite = ET.Element(
        "testsuite",
        {
            "name": "plurx.validation",
            "tests": str(len(results)),
            "failures": str(sum(result.status == "failed" for result in results)),
            "skipped": str(sum(result.status == "skipped" for result in results)),
            "time": f"{sum(result.seconds for result in results):.3f}",
        },
    )
    for result in results:
        case = ET.SubElement(
            suite,
            "testcase",
            {
                "classname": "validation",
                "name": result.id,
                "time": f"{result.seconds:.3f}",
            },
        )
        if result.status == "failed":
            failure = ET.SubElement(case, "failure", {"message": result.message})
            failure.text = result.output[-12000:]
        elif result.status == "skipped":
            ET.SubElement(case, "skipped", {"message": result.message})
        output = ET.SubElement(case, "system-out")
        output.text = (result.output[-2000:] if result.output else result.message)
    xml_path = artifact_dir / "junit.xml"
    ET.ElementTree(suite).write(xml_path, encoding="utf-8", xml_declaration=True)
    return json_path, xml_path


def changed_paths(repo_root: Path, mode: str, base: str | None = None) -> tuple[str, ...]:
    if mode == "staged":
        command = [
            "git",
            "diff",
            "--cached",
            "--name-only",
            "-z",
            "--diff-filter=ACMRD",
        ]
    elif mode == "changed-from" and base:
        command = [
            "git",
            "diff",
            "--name-only",
            "-z",
            "--diff-filter=ACMRD",
            f"{base}...HEAD",
        ]
    else:
        raise CatalogError(f"unsupported change mode: {mode}")
    try:
        result = subprocess.run(
            command,
            cwd=repo_root,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
    except (OSError, subprocess.CalledProcessError) as exc:
        detail = getattr(exc, "stderr", b"")
        if isinstance(detail, bytes):
            detail = detail.decode("utf-8", errors="replace")
        raise CatalogError(f"cannot resolve changed paths: {detail or exc}") from exc
    return tuple(
        normalize_path(part.decode("utf-8", errors="surrogateescape"))
        for part in result.stdout.split(b"\0")
        if part
    )


def _selection_from_args(args: argparse.Namespace, catalog: Catalog) -> Selection:
    if args.point:
        return select_named(catalog, tuple(args.point))
    if args.paths:
        return select_points(catalog, tuple(args.paths))
    if args.staged:
        return select_points(catalog, changed_paths(REPO_ROOT, "staged"))
    if args.changed_from:
        return select_points(
            catalog, changed_paths(REPO_ROOT, "changed-from", args.changed_from)
        )
    return select_all(catalog)


def _validation_environment(args: argparse.Namespace) -> dict[str, str]:
    if args.staged:
        return {"PLURX_VALIDATION_MODE": "staged"}
    if args.changed_from:
        return {
            "PLURX_VALIDATION_MODE": "changed-from",
            "PLURX_VALIDATION_BASE": args.changed_from,
        }
    if args.paths:
        return {"PLURX_VALIDATION_MODE": "paths"}
    if args.point:
        return {"PLURX_VALIDATION_MODE": "point"}
    return {"PLURX_VALIDATION_MODE": "all"}


def print_plan(catalog: Catalog, selection: Selection, profile: str) -> None:
    checks = selected_checks(catalog, selection, profile)
    print(f"profile: {profile}")
    print(f"functionality points: {len(selection.point_ids)}")
    for point_id in selection.point_ids:
        point = catalog.point_map[point_id]
        reason = ", ".join(selection.reasons[point_id])
        eligible = [
            check_id
            for check_id in point.checks
            if profile in catalog.check_map[check_id].profiles
        ]
        coverage = ", ".join(eligible) if eligible else "no check in this profile"
        print(f"  {point.id:<24} {point.title}")
        print(f"    because {reason}")
        print(f"    checks  {coverage}")
    print(f"checks to run: {len(checks)} (shared checks are deduplicated)")
    for check in checks:
        print(f"  {check.id:<24} {check.title}")


def _add_selection_arguments(parser: argparse.ArgumentParser) -> None:
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--all", action="store_true", help="select every point (default)")
    group.add_argument("--staged", action="store_true", help="select from the staged diff")
    group.add_argument("--changed-from", metavar="REF", help="select from REF...HEAD")
    group.add_argument("--paths", nargs="+", help="select from explicit repository paths")
    group.add_argument("--point", action="append", help="select a point by id; repeatable")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="scripts/validate",
        description="Select and run checks from functionality-point contracts.",
    )
    parser.add_argument("--catalog", type=Path, default=DEFAULT_CATALOG)
    subparsers = parser.add_subparsers(dest="command", required=True)

    subparsers.add_parser("lint", help="validate catalog structure and path coverage")
    subparsers.add_parser("list", help="list functionality points and their checks")

    plan = subparsers.add_parser("plan", help="show impacted points and checks")
    plan.add_argument("--profile", default="commit")
    _add_selection_arguments(plan)

    run = subparsers.add_parser("run", help="execute impacted checks")
    run.add_argument("--profile", default="commit")
    run.add_argument("--strict", action="store_true", help="fail on missing prerequisites")
    run.add_argument("--fail-fast", action="store_true")
    run.add_argument("--artifact-dir", type=Path)
    run.add_argument("--no-report", action="store_true")
    _add_selection_arguments(run)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        catalog = load_catalog(args.catalog)
        errors = lint_catalog(catalog, REPO_ROOT)
        if errors:
            print("invalid validation catalog:", file=sys.stderr)
            for error in errors:
                print(f"  - {error}", file=sys.stderr)
            return 2

        if args.command == "lint":
            audited = sum(
                matches(path, catalog.audit_paths) and not matches(path, catalog.audit_exclude)
                for path in _git_paths(REPO_ROOT)
            )
            print(
                f"catalog ok: {len(catalog.points)} points · "
                f"{len(catalog.checks)} checks · {audited} audited files"
            )
            return 0

        if args.command == "list":
            for point in catalog.points:
                print(f"{point.id:<24} {point.title}")
                print(f"  checks: {', '.join(point.checks)}")
                if point.depends_on:
                    print(f"  depends on: {', '.join(point.depends_on)}")
            return 0

        selection = _selection_from_args(args, catalog)
        if selection.paths and not selection.point_ids:
            print("warning: no functionality point matched the selected paths", file=sys.stderr)
        print_plan(catalog, selection, args.profile)
        if args.command == "plan":
            return 0

        checks = selected_checks(catalog, selection, args.profile)
        artifact_dir = args.artifact_dir or (REPO_ROOT / catalog.artifact_dir)
        if not artifact_dir.is_absolute():
            artifact_dir = REPO_ROOT / artifact_dir
        results = execute_checks(
            checks,
            REPO_ROOT,
            artifact_dir,
            strict=args.strict,
            fail_fast=args.fail_fast,
            environment=_validation_environment(args),
        )
        if not args.no_report:
            json_path, xml_path = write_reports(
                catalog, selection, args.profile, results, artifact_dir, REPO_ROOT
            )
            print(f"\nevidence: {json_path} · {xml_path}")
        failures = sum(result.status == "failed" for result in results)
        skipped = sum(result.status == "skipped" for result in results)
        passed = sum(result.status == "passed" for result in results)
        print(f"result: {passed} passed · {failures} failed · {skipped} skipped")
        return 1 if failures else 0
    except CatalogError as exc:
        print(f"validation error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
