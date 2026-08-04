"""Select the expensive CI jobs justified by a pull-request diff."""

from __future__ import annotations

import argparse
import sys

from validation.runner import (
    Catalog,
    CatalogError,
    REPO_ROOT,
    changed_paths,
    load_catalog,
    matches,
    select_points,
    selected_checks,
)


SCOPE_KEYS = (
    "apple",
    "android_jvm",
    "android_device",
    "web_layout",
    "release_build",
    "container",
)

# Device tests prove Android UI, focus, and packaging behavior. Server-only
# changes can select the Android consumer through the impact graph, but they do
# not change those on-device contracts and therefore do not justify an emulator.
ANDROID_DEVICE_PATHS = (
    "clients/android/Dockerfile",
    "clients/android/app/build.gradle.kts",
    "clients/android/app/src/main/**",
    "clients/android/app/src/androidTest/**",
    "clients/android/build.gradle.kts",
    "clients/android/gradle.properties",
    "clients/android/gradle/**",
    "clients/android/gradlew",
    "clients/android/settings.gradle.kts",
)

# Cross-target release builds are evidence about the Rust server and its
# toolchain. Native-client and documentation changes cannot affect them.
RELEASE_BUILD_PATHS = (
    "Cargo.lock",
    "Cargo.toml",
    "crates/**",
    "rust-toolchain.toml",
)

# The container smoke test additionally owns image, Compose, runtime-config,
# and lifecycle behavior.
CONTAINER_PATHS = (
    ".dockerignore",
    "Cargo.lock",
    "Cargo.toml",
    "Dockerfile",
    "crates/**",
    "deploy/**",
    "plurx.example.toml",
    "rust-toolchain.toml",
    "scripts/container-smoke",
)


def all_scope() -> dict[str, bool]:
    """Fail open when a diff cannot be trusted or the event is not a PR."""

    return {key: True for key in SCOPE_KEYS}


def scope_for_paths(catalog: Catalog, paths: tuple[str, ...]) -> dict[str, bool]:
    """Map changed paths to independently runnable CI surfaces."""

    selection = select_points(catalog, paths)
    check_ids = {
        check.id for check in selected_checks(catalog, selection, profile="ci")
    }
    return {
        "apple": "apple-simulators" in check_ids,
        "android_jvm": "android-jvm" in check_ids,
        "android_device": any(matches(path, ANDROID_DEVICE_PATHS) for path in paths),
        "web_layout": "web-layout" in check_ids,
        "release_build": any(matches(path, RELEASE_BUILD_PATHS) for path in paths),
        "container": any(matches(path, CONTAINER_PATHS) for path in paths),
    }


def resolve_scope(event: str, base: str | None) -> dict[str, bool]:
    """Use impact selection for PRs and exhaustive jobs after merge or on tags."""

    if event != "pull_request":
        return all_scope()
    if not base:
        print("ci-scope: no pull-request base; enabling every job", file=sys.stderr)
        return all_scope()

    try:
        paths = changed_paths(REPO_ROOT, "changed-from", base)
    except CatalogError as exc:
        print(f"ci-scope: cannot resolve diff ({exc}); enabling every job", file=sys.stderr)
        return all_scope()
    return scope_for_paths(load_catalog(), paths)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="python3 -m validation.ci_scope",
        description="Emit GitHub Actions booleans for impact-selected CI jobs.",
    )
    parser.add_argument("--event", required=True)
    parser.add_argument("--base")
    args = parser.parse_args(argv)

    for key, enabled in resolve_scope(args.event, args.base).items():
        print(f"{key}={'true' if enabled else 'false'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
