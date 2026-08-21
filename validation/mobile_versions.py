"""Enforce mobile release versions against the selected Git baseline."""

from __future__ import annotations

import dataclasses
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib
from collections.abc import Callable


REPO_ROOT = Path(__file__).resolve().parents[1]
SEMANTIC_VERSION = re.compile(
    r"(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)"
)

APPLE_RELEASE_PATHS = (
    "clients/apple/Sources/",
    "clients/apple/Resources/",
)
APPLE_RELEASE_FILES = frozenset({"clients/apple/project.yml"})

ANDROID_RELEASE_PATHS = ("clients/android/app/src/main/",)
ANDROID_RELEASE_FILES = frozenset(
    {
        "clients/android/app/build.gradle.kts",
        "clients/android/app/proguard-rules.pro",
        "clients/android/build.gradle.kts",
        "clients/android/gradle.properties",
        "clients/android/gradle/libs.versions.toml",
        "clients/android/settings.gradle.kts",
    }
)


class MobileVersionError(ValueError):
    """A mobile version declaration or comparison is invalid."""


@dataclasses.dataclass(frozen=True)
class MobileVersions:
    workspace: str
    android_name: str
    android_build: int
    apple_name: str
    apple_build: int
    apple_marketing_references: int
    apple_build_references: int


def _required_value(contents: str, pattern: str, label: str) -> str:
    matches = re.findall(pattern, contents, flags=re.MULTILINE)
    if len(matches) != 1:
        raise MobileVersionError(
            f"expected exactly one {label}, found {len(matches)}"
        )
    return matches[0]


def _positive_integer(value: str, label: str) -> int:
    if not re.fullmatch(r"[1-9]\d*", value):
        raise MobileVersionError(f"{label} must be a positive integer; found {value!r}")
    return int(value)


def read_versions(read: Callable[[str], str]) -> MobileVersions:
    try:
        workspace = tomllib.loads(read("Cargo.toml"))["workspace"]["package"][
            "version"
        ]
    except (KeyError, TypeError, tomllib.TOMLDecodeError) as exc:
        raise MobileVersionError(
            "Cargo.toml must declare workspace.package.version"
        ) from exc
    if not isinstance(workspace, str):
        raise MobileVersionError("workspace.package.version must be a string")

    android = read("clients/android/app/build.gradle.kts")
    apple = read("clients/apple/project.yml")
    android_name = _required_value(
        android,
        r'^\s*versionName\s*=\s*"([^"]+)"\s*$',
        "Android versionName",
    )
    android_build = _positive_integer(
        _required_value(
            android,
            r"^\s*versionCode\s*=\s*(\S+)\s*$",
            "Android versionCode",
        ),
        "Android versionCode",
    )
    apple_name = _required_value(
        apple,
        r'^\s*MARKETING_VERSION:\s*"([^"]+)"\s*$',
        "Apple MARKETING_VERSION",
    )
    apple_build = _positive_integer(
        _required_value(
            apple,
            r'^\s*CURRENT_PROJECT_VERSION:\s*"?([^"\s]+)"?\s*$',
            "Apple CURRENT_PROJECT_VERSION",
        ),
        "Apple CURRENT_PROJECT_VERSION",
    )

    return MobileVersions(
        workspace=workspace,
        android_name=android_name,
        android_build=android_build,
        apple_name=apple_name,
        apple_build=apple_build,
        apple_marketing_references=len(
            re.findall(
                r'^\s*CFBundleShortVersionString:\s*"\$\(MARKETING_VERSION\)"\s*$',
                apple,
                re.MULTILINE,
            )
        ),
        apple_build_references=len(
            re.findall(
                r'^\s*CFBundleVersion:\s*"\$\(CURRENT_PROJECT_VERSION\)"\s*$',
                apple,
                re.MULTILINE,
            )
        ),
    )


def _matches_release_path(
    path: str, prefixes: tuple[str, ...], files: frozenset[str]
) -> bool:
    return path in files or path.startswith(prefixes)


def _merge_target_hint(baseline_label: str | None, baseline_build: int) -> str:
    """Say which ref the counter has to clear, when that ref is the merge target.

    Without this the failure reads as "bump it once" and stays ambiguous about
    *which* baseline moved, which is exactly the confusion a base that advanced
    under an open branch produces.
    """
    if not baseline_label:
        return ""
    return (
        f". {baseline_label} is the merge target and already carries "
        f"{baseline_build}, so re-bump above the current base rather than above "
        "the commit this branch was cut from"
    )


def validate_versions(
    current: MobileVersions,
    *,
    baseline: MobileVersions | None = None,
    scope_baseline: MobileVersions | None = None,
    changed_paths: tuple[str, ...] = (),
    baseline_label: str | None = None,
) -> tuple[str, ...]:
    errors: list[str] = []

    if not SEMANTIC_VERSION.fullmatch(current.workspace):
        errors.append(
            "workspace version must be MAJOR.MINOR.PATCH; "
            f"found {current.workspace!r}"
        )
    if current.android_name != current.workspace:
        errors.append(
            "Android versionName must match the workspace version "
            f"({current.workspace}); found {current.android_name}"
        )
    if current.apple_name != current.workspace:
        errors.append(
            "Apple MARKETING_VERSION must match the workspace version "
            f"({current.workspace}); found {current.apple_name}"
        )
    if current.android_build > 2_100_000_000:
        errors.append("Android versionCode exceeds Google Play's 2100000000 ceiling")
    if current.apple_marketing_references != 2:
        errors.append(
            "the iOS and tvOS targets must both stamp MARKETING_VERSION "
            f"(found {current.apple_marketing_references} references)"
        )
    if current.apple_build_references != 2:
        errors.append(
            "the iOS and tvOS targets must both stamp CURRENT_PROJECT_VERSION "
            f"(found {current.apple_build_references} references)"
        )

    if baseline is None:
        return tuple(errors)

    # "Did this branch bump the release?" is a property of the branch, so it is
    # answered against the branch point even when the counters are answered
    # against the merge target. Reading it off the target instead conflates "the
    # branch released" with "the target released", which reds every open branch
    # the moment a release lands and tells each one to bump counters it never
    # touched.
    scope = baseline if scope_baseline is None else scope_baseline
    workspace_changed = current.workspace != scope.workspace
    apple_changed = workspace_changed or any(
        _matches_release_path(path, APPLE_RELEASE_PATHS, APPLE_RELEASE_FILES)
        for path in changed_paths
    )
    android_changed = workspace_changed or any(
        _matches_release_path(path, ANDROID_RELEASE_PATHS, ANDROID_RELEASE_FILES)
        for path in changed_paths
    )

    if apple_changed and current.apple_build <= baseline.apple_build:
        errors.append(
            "Apple release inputs changed, so CURRENT_PROJECT_VERSION must increase "
            f"above {baseline.apple_build}; found {current.apple_build}"
            + _merge_target_hint(baseline_label, baseline.apple_build)
        )
    if android_changed and current.android_build <= baseline.android_build:
        errors.append(
            "Android release inputs changed, so versionCode must increase "
            f"above {baseline.android_build}; found {current.android_build}"
            + _merge_target_hint(baseline_label, baseline.android_build)
        )

    return tuple(errors)


def _git(root: Path, *arguments: str) -> str:
    try:
        return subprocess.run(
            ["git", *arguments],
            cwd=root,
            check=True,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        ).stdout
    except (OSError, subprocess.CalledProcessError) as exc:
        detail = getattr(exc, "stderr", "")
        raise MobileVersionError(
            f"git {' '.join(arguments)} failed: {detail or exc}"
        ) from exc


def _git_reader(root: Path, ref: str) -> Callable[[str], str]:
    return lambda path: _git(root, "show", f"{ref}:{path}")


def _filesystem_reader(root: Path) -> Callable[[str], str]:
    return lambda path: (root / path).read_text(encoding="utf-8")


def _changed_paths(root: Path, *diff_arguments: str) -> tuple[str, ...]:
    output = _git(
        root,
        "diff",
        "--name-only",
        "-z",
        "--diff-filter=ACMRD",
        *diff_arguments,
    )
    return tuple(path for path in output.split("\0") if path)


def check_repository(
    root: Path = REPO_ROOT,
    *,
    mode: str = "all",
    base: str | None = None,
    merge_target: str | None = None,
) -> tuple[str, ...]:
    """Compare the working revision's build counters against a Git baseline.

    ``base`` scopes *which* release inputs the branch touched, and is the branch
    point. ``merge_target`` supplies the counters to beat, and is the tip of the
    branch this change will merge into. They are the same commit only while the
    target has not moved; once it has, only the target tip answers the question
    the store cares about, which is whether the counter is still an increase if
    this merged right now.

    Scope is the whole of scope: both the changed paths and the workspace-version
    comparison are read against ``base``. Only the counters move to
    ``merge_target``.
    """
    if merge_target and mode != "changed-from":
        raise MobileVersionError(
            "a merge target is only meaningful for changed-from validation; "
            f"mode is {mode}"
        )

    baseline_label: str | None = None
    scope_baseline: MobileVersions | None = None
    if mode == "staged":
        changed = _changed_paths(root, "--cached", "HEAD")
        current = read_versions(_git_reader(root, ""))
        baseline = read_versions(_git_reader(root, "HEAD"))
    elif mode == "changed-from":
        if not base:
            raise MobileVersionError("changed-from validation requires a Git base")
        changed = _changed_paths(root, f"{base}...HEAD")
        current = read_versions(_git_reader(root, "HEAD"))
        baseline = scope_baseline = read_versions(_git_reader(root, base))
        if merge_target:
            # Scope stays on the branch point read above; only the counters move
            # to the target. A missing or unreadable merge target fails the check
            # rather than falling back to the branch point. Failing open here
            # reproduces the exact defect this baseline exists to catch: a green
            # result on a tree that cannot ship.
            baseline = read_versions(_git_reader(root, merge_target))
            baseline_label = merge_target
    elif mode in {"all", "paths", "point"}:
        changed = ()
        current = read_versions(_filesystem_reader(root))
        baseline = None
    else:
        raise MobileVersionError(f"unsupported validation mode: {mode}")

    return validate_versions(
        current,
        baseline=baseline,
        scope_baseline=scope_baseline,
        changed_paths=changed,
        baseline_label=baseline_label,
    )


def main() -> int:
    mode = os.environ.get("PLURX_VALIDATION_MODE", "all")
    base = os.environ.get("PLURX_VALIDATION_BASE")
    merge_target = os.environ.get("PLURX_VALIDATION_MERGE_TARGET") or None
    try:
        errors = check_repository(mode=mode, base=base, merge_target=merge_target)
    except (MobileVersionError, OSError) as exc:
        print(f"mobile version validation failed: {exc}", file=sys.stderr)
        return 1

    if errors:
        print("mobile version validation failed:", file=sys.stderr)
        for error in errors:
            print(f"  - {error}", file=sys.stderr)
        return 1

    print("mobile versions and changed-app build counters are valid")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
