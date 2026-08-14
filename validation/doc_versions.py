"""Pin current mobile build claims in user-facing repository status documents."""

from __future__ import annotations

from collections.abc import Callable
from pathlib import Path
import re

from validation.mobile_versions import REPO_ROOT, read_versions


def _one(contents: str, pattern: str, label: str) -> int:
    matches = re.findall(pattern, contents, re.MULTILINE)
    if len(matches) != 1:
        raise ValueError(f"expected exactly one {label}, found {len(matches)}")
    return int(matches[0])


# A build number in docs/STATUS.html is a *current* claim by default: the sweep
# in `validate_documented_builds` requires every mention to match project.yml,
# so a status page that quietly falls behind a build bump is red before a PR
# opens. But the page also has to narrate history — "build 52 corrected the
# reported iPad mini failure" is true and stays true after the bump — and tense
# is invisible to a regex. A historical mention therefore opts out explicitly:
#
#     <span data-build-history>Apple build 52</span> corrects the reported ...
#
# Marking is deliberately the exception, not the rule, so the exemption is
# granted only by the real marker: a bare `data-build-history` attribute (or one
# with an empty value, which is how HTML formatters serialize a boolean
# attribute) on a `<span>`. Recognizing it demands parsing the whole opening tag
# rather than scanning the raw text for the marker's name, because a substring
# match also fires on things that are not the marker — `title="data-build-history"`
# carries it as an attribute *value*, `data-build-history-note` is a different
# attribute that merely starts with it — and each of those would silently exempt
# a stale claim. Everything else fails closed and returns the mention to the
# sweep: an unmarked mention, a marker on a malformed or self-closing tag, a
# marker with a value, an unclosed span, and a marker that drifts across a
# nested `<span>` and so no longer wraps its own sentence.
#
# HTML attribute names are ASCII case-insensitive, so `DATA-BUILD-HISTORY` is
# the same marker; `re.IGNORECASE` matches the language rather than relaxing the
# rule.
_ATTRIBUTE = r"""[^\s"'>/=]+(?:\s*=\s*(?:"[^"]*"|'[^']*'|[^\s"'`=<>]+))?"""
_MARKER_ATTRIBUTE = r"""data-build-history(?:\s*=\s*(?:""|''))?"""
_HISTORICAL_BUILD_SPAN = re.compile(
    rf"<span(?:\s+{_ATTRIBUTE})*\s+{_MARKER_ATTRIBUTE}(?:\s+{_ATTRIBUTE})*\s*>"
    r"(?:(?!</?span\b).)*</span\s*>",
    re.DOTALL | re.IGNORECASE,
)


def _current_claims_only(contents: str) -> str:
    """`contents` with every explicitly historical build span removed."""
    return _HISTORICAL_BUILD_SPAN.sub(" ", contents)


def validate_documented_builds(read: Callable[[str], str]) -> tuple[str, ...]:
    versions = read_versions(read)
    claims = {
        "clients/apple/README.md": _one(
            read("clients/apple/README.md"),
            r"^> Status:.*?build `([1-9]\d*)`",
            "Apple README current build claim",
        ),
        "clients/android/README.md": _one(
            read("clients/android/README.md"),
            r"^> Status:.*?build `([1-9]\d*)`",
            "Android README current build claim",
        ),
        "docs/APPLE-CLIENT-PARITY.md": _one(
            read("docs/APPLE-CLIENT-PARITY.md"),
            r"^> Status .*?Apple build ([1-9]\d*)\.",
            "Apple parity current build claim",
        ),
    }
    errors: list[str] = []
    for path in ("clients/apple/README.md", "docs/APPLE-CLIENT-PARITY.md"):
        if claims[path] != versions.apple_build:
            errors.append(
                f"{path} claims Apple build {claims[path]}; "
                f"project.yml declares {versions.apple_build}"
            )
    if claims["clients/android/README.md"] != versions.android_build:
        errors.append(
            "clients/android/README.md claims Android build "
            f"{claims['clients/android/README.md']}; build.gradle.kts declares "
            f"{versions.android_build}"
        )

    status_claims = {
        int(value)
        for value in re.findall(
            r"Apple build ([1-9]\d*)",
            _current_claims_only(read("docs/STATUS.html")),
            re.IGNORECASE,
        )
    }
    if not status_claims:
        errors.append("docs/STATUS.html has no current Apple build claim")
    elif status_claims != {versions.apple_build}:
        errors.append(
            "docs/STATUS.html Apple build claims must all match project.yml "
            f"({versions.apple_build}); found {sorted(status_claims)}"
        )
    return tuple(errors)


def check_repository(root: Path = REPO_ROOT) -> tuple[str, ...]:
    return validate_documented_builds(
        lambda path: (root / path).read_text(encoding="utf-8")
    )
