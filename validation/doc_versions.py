"""Pin current mobile build claims in user-facing repository status documents."""

from __future__ import annotations

from collections.abc import Callable
from html.parser import HTMLParser
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
# attribute) on a real `<span>` element that closes around its own sentence.
#
# Both halves of that sentence are load-bearing, and neither survives a pattern
# match against the document's raw text. The attribute has to be read at
# attribute-name position, because the marker's *name* also appears in markup
# that is not the marker — `title="data-build-history"` carries it as a value,
# `data-build-history-note` is a different attribute that merely starts with it.
# And the tag has to be a tag: marker-shaped characters also occur where no
# element exists at all — inside an HTML comment, inside another element's
# quoted attribute value, inside `<script>` or `<style>` text. Each of those
# reads as ordinary markup to a human, so treating one as an opt-out would let a
# visible stale claim through a gate whose diff looked like nothing.
#
# The document is therefore parsed. `HTMLParser` reports a start tag only where
# the HTML tokenizer finds one, so comments, attribute values, and raw-text
# elements never produce a marker, and attribute names arrive already
# lowercased, which is exactly HTML's own ASCII case-insensitivity rather than a
# relaxed rule. Everything that is not an unambiguous marked span fails closed
# and returns the mention to the sweep: an unmarked mention, a marker on any
# other element, a marker carrying a value, a marker on a self-closing or
# otherwise malformed tag, an unclosed span, and a marker that drifts across a
# nested `<span>` and so no longer wraps its own sentence.
_MARKER = "data-build-history"


class _HistoricalSpanFinder(HTMLParser):
    """The document offsets that marked historical spans genuinely cover.

    Each open `<span>` is tracked as `[start, spoiled]`: `start` is the offset
    of its opening tag when that tag carries the marker and `None` otherwise,
    and `spoiled` records that another `<span>` opened inside it, which is the
    nesting case the marker does not cover. A span is exempt only when it closes
    with its own marker intact, so an unclosed one is dropped at EOF.
    """

    def __init__(self) -> None:
        super().__init__(convert_charrefs=False)

    def spans(self, contents: str) -> tuple[tuple[int, int], ...]:
        self.reset()
        # `getpos` reports (line, column); the sweep works in flat offsets.
        self._line_starts = [0]
        for line in contents.split("\n"):
            self._line_starts.append(self._line_starts[-1] + len(line) + 1)
        self._open: list[list] = []
        self._spans: list[tuple[int, int]] = []
        self.feed(contents)
        self.close()
        return tuple(self._spans)

    def _offset(self) -> int:
        line, column = self.getpos()
        return self._line_starts[line - 1] + column

    def _note_nesting(self) -> None:
        for entry in self._open:
            entry[1] = True

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        if tag != "span":
            return
        self._note_nesting()
        marked = any(name == _MARKER and not value for name, value in attrs)
        self._open.append([self._offset() if marked else None, False])

    def handle_startendtag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        # `<span … />` opens nothing here: a marker on it fails closed, and it
        # still counts as nesting inside any span already open.
        if tag == "span":
            self._note_nesting()

    def handle_endtag(self, tag: str) -> None:
        if tag != "span" or not self._open:
            return
        start, spoiled = self._open.pop()
        if start is not None and not spoiled:
            self._spans.append((start, self._offset()))


def _current_claims_only(contents: str) -> str:
    """`contents` with every explicitly historical build span removed.

    The opening tag is removed with the content it wraps, so a build number in
    the marked span's own attributes goes with it. Marked spans never nest, so
    the removed regions are disjoint.
    """
    kept: list[str] = []
    cursor = 0
    for start, end in _HistoricalSpanFinder().spans(contents):
        kept.append(contents[cursor:start])
        kept.append(" ")
        cursor = end
    kept.append(contents[cursor:])
    return "".join(kept)


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
