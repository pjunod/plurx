"""Audit corrective commits against current validation evidence."""

from __future__ import annotations

import argparse
import dataclasses
import json
from pathlib import Path
import re
import subprocess
import sys
import tomllib

from validation.runner import Catalog, REPO_ROOT, load_catalog, matches


DEFAULT_COVERAGE = REPO_ROOT / "validation" / "regressions.toml"
DEFAULT_CLIENT_FIXES = REPO_ROOT / "tests" / "client-fixes.toml"
ISSUE_RE = re.compile(
    r"(^fix(?:\b|[(:])|^(?:perf(?:\([^)]*\))?:|address|hide|align|bind|delay|do not|fit|"
    r"harden|make .* (?:valid|playable)|map hls|normalize|put .* actually|"
    r"rebuild|refine|refresh|report (?:delivered|what)|reuse|right-align|route|"
    r"scope|signal|tighten|use latest)\b|"
    r"\b(?:bug|regression|crash|hang|freeze|broken|stale|race|wrong|failure|failed|"
    r"defect|bypass|lockout|clipped|clipping|fix(?:es|ed)?|stop(?:s|ped|ping)?|"
    r"avoid(?:s|ed|ing)?|correct(?:s|ed|ion)?|restore(?:s|d)?|prevent(?:s|ed|ing)?|"
    r"bound(?:s|ed|ing)?|keep(?:s|ing)?|remove(?:s|d|ing)?|withdraw(?:s|n)?|"
    r"omit(?:s|ted|ting)?|preserve(?:s|d|ing)?|repair(?:s|ed|ing)?|"
    r"refuse(?:s|d|ing)?|recover(?:y|ed|ing)?|survive(?:s|d|ing)?|fallback|"
    r"invalid|incompatible|compatible|truth|free each)\b|"
    r"^(?:roll back|revert unsupported|make .* fail|give .* back))",
    re.IGNORECASE,
)
TEST_ADDITION_RE = re.compile(
    r"^\+.*(?:#\[test\]|func test|@Test|assert(?:_eq|_ne)?!|XCTAssert|expect\(|"
    r"test\(|describe\(|it\(|\.golden)",
    re.MULTILINE,
)


class HistoryError(ValueError):
    """The regression coverage ledger is malformed."""


@dataclasses.dataclass(frozen=True)
class IssueCommit:
    sha: str
    subject: str
    paths: tuple[str, ...]
    point_ids: tuple[str, ...]
    direct_test_evidence: bool


@dataclasses.dataclass(frozen=True)
class CoverageEntry:
    commits: tuple[str, ...]
    points: tuple[str, ...]
    checks: tuple[str, ...]
    reason: str
    ignore: bool


@dataclasses.dataclass(frozen=True)
class ClientFixEntry:
    id: str
    commits: tuple[str, ...]
    source: str
    source_anchor: str
    test: str
    test_anchor: str


@dataclasses.dataclass(frozen=True)
class ClientFixLedger:
    enforce_after: str | None
    fixes: tuple[ClientFixEntry, ...]


@dataclasses.dataclass(frozen=True)
class HistoryReport:
    issues: tuple[IssueCommit, ...]
    covered_by_entry: tuple[str, ...]
    covered_by_anchor: tuple[str, ...]
    ignored: tuple[str, ...]
    errors: tuple[str, ...]

    @property
    def direct_count(self) -> int:
        return sum(issue.direct_test_evidence for issue in self.issues)

    @property
    def mapped_count(self) -> int:
        return len(self.covered_by_entry)

    @property
    def ignored_count(self) -> int:
        return len(self.ignored)

    @property
    def anchored_count(self) -> int:
        return len(self.covered_by_anchor)


def _strings(value: object, field: str) -> tuple[str, ...]:
    if value is None:
        return ()
    if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
        raise HistoryError(f"{field} must be an array of strings")
    return tuple(value)


def load_coverage(path: Path = DEFAULT_COVERAGE) -> tuple[CoverageEntry, ...]:
    try:
        with path.open("rb") as handle:
            raw = tomllib.load(handle)
    except (OSError, tomllib.TOMLDecodeError) as exc:
        raise HistoryError(f"cannot read {path}: {exc}") from exc
    if raw.get("version") != 1:
        raise HistoryError("regression coverage version must be 1")

    entries: list[CoverageEntry] = []
    for index, item in enumerate(raw.get("coverage", [])):
        where = f"coverage[{index}]"
        if not isinstance(item, dict):
            raise HistoryError(f"{where} must be a table")
        entries.append(
            CoverageEntry(
                commits=_strings(item.get("commits"), f"{where}.commits"),
                points=_strings(item.get("points"), f"{where}.points"),
                checks=_strings(item.get("checks"), f"{where}.checks"),
                reason=str(item.get("reason", "")).strip(),
                ignore=bool(item.get("ignore", False)),
            )
        )
    return tuple(entries)


def load_client_fixes(path: Path) -> ClientFixLedger:
    if not path.exists():
        return ClientFixLedger(enforce_after=None, fixes=())
    try:
        with path.open("rb") as handle:
            raw = tomllib.load(handle)
    except (OSError, tomllib.TOMLDecodeError) as exc:
        raise HistoryError(f"cannot read {path}: {exc}") from exc
    if raw.get("version") != 1:
        raise HistoryError("client fix ledger version must be 1")
    enforce_after = raw.get("enforce_after")
    if enforce_after is not None and (
        not isinstance(enforce_after, str)
        or not re.fullmatch(r"[0-9a-f]{7,40}", enforce_after)
    ):
        raise HistoryError("client fix enforce_after must be a Git SHA prefix")

    entries: list[ClientFixEntry] = []
    for index, item in enumerate(raw.get("fixes", [])):
        where = f"fixes[{index}]"
        if not isinstance(item, dict):
            raise HistoryError(f"{where} must be a table")
        required = {
            "id",
            "commits",
            "source",
            "source_anchor",
            "test",
            "test_anchor",
        }
        if set(item) != required:
            raise HistoryError(
                f"{where} must contain exactly {', '.join(sorted(required))}"
            )
        entries.append(
            ClientFixEntry(
                id=str(item["id"]),
                commits=_strings(item["commits"], f"{where}.commits"),
                source=str(item["source"]),
                source_anchor=str(item["source_anchor"]),
                test=str(item["test"]),
                test_anchor=str(item["test_anchor"]),
            )
        )
    return ClientFixLedger(enforce_after=enforce_after, fixes=tuple(entries))


def _git(root: Path, *args: str) -> str:
    try:
        return subprocess.run(
            ("git", *args),
            cwd=root,
            check=True,
            text=True,
            errors="replace",
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        ).stdout
    except (OSError, subprocess.CalledProcessError) as exc:
        raise HistoryError(f"cannot inspect git history: {exc}") from exc


def _is_test_path(path: str) -> bool:
    return (
        path.startswith("tests/")
        or "/Tests/" in path
        or "/src/test/" in path
        or "/src/androidTest/" in path
    )


def discover_issues(
    root: Path,
    catalog: Catalog,
    explicit_prefixes: tuple[str, ...] = (),
) -> tuple[IssueCommit, ...]:
    issues: list[IssueCommit] = []
    history = _git(root, "log", "--no-merges", "--format=%H%x09%s")
    for line in history.splitlines():
        sha, subject = line.split("\t", 1)
        if not ISSUE_RE.search(subject) and not any(
            sha.startswith(prefix) for prefix in explicit_prefixes
        ):
            continue
        paths = tuple(
            path
            for path in _git(
                root,
                "diff-tree",
                "--root",
                "--no-commit-id",
                "--name-only",
                "-r",
                sha,
            ).splitlines()
            if path
        )
        points = tuple(
            point.id
            for point in catalog.points
            if any(matches(path, point.paths) for path in paths)
        )
        patch = _git(root, "show", "--format=", "--unified=0", "--no-ext-diff", sha)
        current_evidence_file = any(
            (root / path).is_file() for path in paths if _is_test_path(path)
        )
        inline_evidence = bool(TEST_ADDITION_RE.search(patch)) and any(
            (root / path).is_file() for path in paths
        )
        issues.append(
            IssueCommit(
                sha=sha,
                subject=subject,
                paths=paths,
                point_ids=points,
                direct_test_evidence=current_evidence_file or inline_evidence,
            )
        )
    return tuple(issues)


def audit_history(
    root: Path = REPO_ROOT,
    catalog: Catalog | None = None,
    coverage_path: Path = DEFAULT_COVERAGE,
    client_fixes_path: Path | None = None,
) -> HistoryReport:
    catalog = catalog or load_catalog()
    entries = load_coverage(coverage_path)
    explicit_prefixes = tuple(
        prefix for entry in entries for prefix in entry.commits
    )
    issues = discover_issues(root, catalog, explicit_prefixes)
    client_fixes = load_client_fixes(
        client_fixes_path or root / "tests" / "client-fixes.toml"
    )
    errors: list[str] = []
    by_sha = {issue.sha: issue for issue in issues}
    resolved: dict[str, CoverageEntry] = {}
    anchored: set[str] = set()

    for index, entry in enumerate(entries):
        where = f"coverage[{index}]"
        if not entry.commits:
            errors.append(f"{where} has no commits")
        if not entry.reason:
            errors.append(f"{where} has no reason")
        if entry.ignore and (entry.points or entry.checks):
            errors.append(f"{where} ignore entries cannot claim points or checks")
        if not entry.ignore and not entry.checks:
            errors.append(f"{where} has no checks")
        for point in entry.points:
            if point not in catalog.point_map:
                errors.append(f"{where} references unknown point {point}")
        for check in entry.checks:
            if check not in catalog.check_map:
                errors.append(f"{where} references unknown check {check}")

        for prefix in entry.commits:
            matches_sha = tuple(sha for sha in by_sha if sha.startswith(prefix))
            if len(matches_sha) != 1:
                errors.append(
                    f"{where} commit {prefix} matches {len(matches_sha)} audited commits"
                )
                continue
            sha = matches_sha[0]
            issue = by_sha[sha]
            if sha in resolved:
                errors.append(f"corrective commit {prefix} is covered more than once")
                continue
            resolved[sha] = entry
            if (
                not entry.ignore
                and issue.point_ids
                and not set(issue.point_ids).intersection(entry.points)
            ):
                errors.append(
                    f"{where} commit {prefix} maps to {', '.join(issue.point_ids)}; "
                    f"entry claims {', '.join(entry.points) or 'no point'}"
                )

        allowed_checks = set(catalog.always_checks)
        for point in entry.points:
            if point in catalog.point_map:
                allowed_checks.update(catalog.point_map[point].checks)
        for check in entry.checks:
            if check in catalog.check_map and check not in allowed_checks:
                errors.append(
                    f"{where} check {check} is not evidence for its listed point(s)"
                )

    anchor_ids: set[str] = set()
    anchor_prefixes: set[str] = set()
    for index, entry in enumerate(client_fixes.fixes):
        where = f"fixes[{index}]"
        if not entry.id or entry.id in anchor_ids:
            errors.append(f"{where} has a missing or duplicate id {entry.id!r}")
        anchor_ids.add(entry.id)
        for path, anchor, kind in (
            (entry.source, entry.source_anchor, "source"),
            (entry.test, entry.test_anchor, "test"),
        ):
            target = root / path
            if not target.is_file():
                errors.append(f"{where} missing {kind} path {path}")
            elif anchor not in target.read_text(encoding="utf-8"):
                errors.append(f"{where} lost {kind} anchor {anchor!r} in {path}")
        for prefix in entry.commits:
            if prefix in anchor_prefixes:
                errors.append(f"client fix commit {prefix} is anchored more than once")
                continue
            anchor_prefixes.add(prefix)
            matches_sha = tuple(sha for sha in by_sha if sha.startswith(prefix))
            if len(matches_sha) != 1:
                errors.append(
                    f"{where} commit {prefix} matches {len(matches_sha)} corrective commits"
                )
                continue
            sha = matches_sha[0]
            if sha in resolved:
                errors.append(f"corrective commit {prefix} has both ledger and anchor evidence")
                continue
            anchored.add(sha)

    explicit_commits: set[str] = set()
    if client_fixes.enforce_after:
        try:
            explicit_commits.update(
                _git(root, "rev-list", f"{client_fixes.enforce_after}..HEAD").splitlines()
            )
        except HistoryError:
            errors.append(
                f"client fix enforce_after does not resolve: {client_fixes.enforce_after}"
            )

    for issue in issues:
        client_anchor_required = issue.sha in explicit_commits and any(
            path.startswith("clients/") for path in issue.paths
        )
        if client_anchor_required:
            if issue.sha not in anchored:
                errors.append(
                    f"corrective client commit {issue.sha[:8]} needs a "
                    f"tests/client-fixes.toml anchor row: {issue.subject}"
                )
            continue
        runtime_mapping_required = issue.sha in explicit_commits and any(
            path.startswith("crates/") for path in issue.paths
        )
        if runtime_mapping_required:
            if issue.sha not in resolved and issue.sha not in anchored:
                errors.append(
                    f"corrective runtime commit {issue.sha[:8]} needs an explicit "
                    f"regressions.toml mapping or client-fix anchor: {issue.subject}"
                )
            continue
        if issue.point_ids and issue.direct_test_evidence:
            continue
        if issue.sha not in resolved:
            missing = "functionality point" if not issue.point_ids else "regression evidence"
            errors.append(
                f"corrective commit {issue.sha[:8]} has no {missing}: {issue.subject}"
            )

    return HistoryReport(
        issues=issues,
        covered_by_entry=tuple(
            sorted(sha for sha, entry in resolved.items() if not entry.ignore)
        ),
        covered_by_anchor=tuple(sorted(anchored)),
        ignored=tuple(sorted(sha for sha, entry in resolved.items() if entry.ignore)),
        errors=tuple(errors),
    )


def _write_report(path: Path, report: HistoryReport) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    mapped = set(report.covered_by_entry)
    anchored = set(report.covered_by_anchor)
    ignored = set(report.ignored)
    payload = {
        "summary": {
            "corrective_commits": len(report.issues),
            "direct_test_evidence": report.direct_count,
            "explicit_check_mapping": report.mapped_count,
            "client_fix_anchor": report.anchored_count,
            "non_runtime_correction": report.ignored_count,
            "errors": len(report.errors),
        },
        "commits": [
            {
                "commit": issue.sha,
                "subject": issue.subject,
                "points": list(issue.point_ids),
                "coverage": (
                    "client-fix-anchor"
                    if issue.sha in anchored
                    else "direct-test"
                    if issue.direct_test_evidence
                    else "non-runtime"
                    if issue.sha in ignored
                    else "explicit-check"
                ),
                "explicitly_mapped": issue.sha in mapped,
                "explicitly_anchored": issue.sha in anchored,
            }
            for issue in report.issues
        ],
        "errors": list(report.errors),
    }
    path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Verify every historical corrective commit has validation evidence."
    )
    parser.add_argument("--report", type=Path, help="write the machine-readable audit JSON")
    args = parser.parse_args(argv)
    try:
        report = audit_history()
    except HistoryError as exc:
        print(f"history audit error: {exc}", file=sys.stderr)
        return 2
    if args.report:
        _write_report(args.report, report)
    if report.errors:
        print("historical regression coverage is incomplete:", file=sys.stderr)
        for error in report.errors:
            print(f"  - {error}", file=sys.stderr)
        return 1
    print(
        "history ok: "
        f"{len(report.issues)} corrective commits · "
        f"{report.direct_count} direct test changes · "
        f"{report.mapped_count} explicit current-check mappings · "
        f"{report.anchored_count} client-fix anchors · "
        f"{report.ignored_count} non-runtime corrections"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
