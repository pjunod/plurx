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
    "rust",
    "apple",
    "android_jvm",
    "android_device",
    "web_layout",
    "release_build",
    "container",
    "mobile_version",
    "hiqlite_spike",
    "cluster_auth",
    "docs_only",
)

# A selector or aggregate-workflow edit can change which evidence appears at
# all. It must exercise every routed surface instead of trusting the routing it
# is in the middle of changing. `runner.py` owns diff resolution, glob
# matching, and point selection, so it is selector code exactly like this file.
FULL_CI_PATHS = (
    ".github/workflows/ci.yml",
    "validation/ci_scope.py",
    "validation/points.toml",
    "validation/runner.py",
)

# Documentation can still be executable evidence: validation unit tests pin
# inventories and links between docs and source anchors. The regression map is
# also allowed because it is consumed only by the history audit that remains in
# preflight; unlike the catalog and selector code, it cannot select or suppress
# a runtime suite. Keep the required workflow alive for these PRs, but route
# them through fast contracts rather than compilers and environment suites.
DOCS_ONLY_PATHS = (
    "**/*.md",
    ".github/**/*.md",
    "docs/**",
    "LICENSE",
    "LICENSE.*",
    "NOTICE",
    "NOTICE.*",
    "validation/regressions.toml",
)

# Markdown under a shipped source tree is still a release-path change. Client
# build counters and crate packaging rules own those trees independently of a
# file's extension, so the documentation lane must never hide them.
SHIPPED_SOURCE_PATHS = (
    "clients/**",
    "crates/**",
)

# The pull-request lane is the iteration loop, so client suites run only when
# a diff can reach their compiled sources; the impact graph's server→client
# fan-out still runs where it belongs — on every merge_group and push event,
# which resolve through all_scope() before a commit can land on main. The one
# cross-surface file that compiles into BOTH native clients is the shared wire
# fixture: editing it re-runs both client suites on the PR itself.
APPLE_PATHS = (
    "clients/apple/**",
    "tests/contracts/native-api.json",
)

ANDROID_JVM_PATHS = (
    "clients/android/**",
    "tests/contracts/native-api.json",
)

# The layout golden is exercised by booting the real server, so its inputs are
# the embedded web sources and the tooling that drives or grades the sweep —
# the `web.experience` point's compiled surface, minus prose.
WEB_LAYOUT_PATHS = (
    "brand/tokens.css",
    "crates/plurxd/src/http/web.rs",
    "crates/plurxd/src/web/**",
    "scripts/contrast-*",
    "scripts/js-check",
    "scripts/themes-proposed.json",
    "scripts/ui-baseline",
    "tests/ui-structure.golden",
)

# The cargo gate cannot be affected by native-client sources: a Kotlin or
# Swift diff compiles nothing under crates/. Everything else — scripts,
# deploy, validation, vendor — keeps the Rust lane, because those trees feed
# build, packaging, or selection behavior the workspace suite pins.
RUST_EXEMPT_PATHS = ("clients/**",)

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

    scope = {key: True for key in SCOPE_KEYS}
    scope["docs_only"] = False
    return scope


def is_docs_only(paths: tuple[str, ...]) -> bool:
    """Return true only when a non-empty diff cannot change shipped code."""

    return bool(paths) and all(
        matches(path, DOCS_ONLY_PATHS)
        and not matches(path, SHIPPED_SOURCE_PATHS)
        for path in paths
    )


def needs_rust_gate(paths: tuple[str, ...]) -> bool:
    """Fail open unless every changed path provably skips the cargo suite."""

    if not paths:
        return True
    return not all(
        matches(path, RUST_EXEMPT_PATHS)
        or (
            matches(path, DOCS_ONLY_PATHS)
            and not matches(path, SHIPPED_SOURCE_PATHS)
        )
        for path in paths
    )


def scope_for_paths(catalog: Catalog, paths: tuple[str, ...]) -> dict[str, bool]:
    """Map changed paths to independently runnable CI surfaces."""

    if is_docs_only(paths):
        return {key: key == "docs_only" for key in SCOPE_KEYS}
    if any(matches(path, FULL_CI_PATHS) for path in paths):
        return all_scope()

    selection = select_points(catalog, paths)
    check_ids = {
        check.id for check in selected_checks(catalog, selection, profile="ci")
    }
    point_ids = set(selection.point_ids)
    return {
        "rust": needs_rust_gate(paths),
        "apple": any(matches(path, APPLE_PATHS) for path in paths),
        "android_jvm": any(matches(path, ANDROID_JVM_PATHS) for path in paths),
        "android_device": any(matches(path, ANDROID_DEVICE_PATHS) for path in paths),
        "web_layout": any(matches(path, WEB_LAYOUT_PATHS) for path in paths),
        "release_build": any(matches(path, RELEASE_BUILD_PATHS) for path in paths),
        "container": any(matches(path, CONTAINER_PATHS) for path in paths),
        "mobile_version": "mobile-version" in check_ids,
        "hiqlite_spike": "core.media" in point_ids,
        "cluster_auth": "cluster.auth" in point_ids,
        "docs_only": False,
    }


def resolve_scope(event: str, base: str | None) -> dict[str, bool]:
    """Scope pull requests; run everything for merge_group, push, and tags.

    The merge queue is the enforcement point: a `merge_group` event lands here
    with a non-PR event name and fails open into `all_scope()`, so the full
    cross-surface fan-out always runs between a green PR and main.
    """

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
