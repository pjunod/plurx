from __future__ import annotations

from pathlib import Path
import re
import unittest

from validation.doc_versions import check_repository, validate_documented_builds


ROOT = Path(__file__).resolve().parents[2]


class MobileBuildClaimCase(unittest.TestCase):
    def test_repository_build_claims_match_release_inputs(self) -> None:
        self.assertEqual(check_repository(ROOT), ())

    def test_advancing_a_build_counter_without_docs_is_rejected(self) -> None:
        project = (ROOT / "clients/apple/project.yml").read_text(encoding="utf-8")
        current = int(re.search(r'CURRENT_PROJECT_VERSION: "(\d+)"', project).group(1))

        def read(path: str) -> str:
            contents = (ROOT / path).read_text(encoding="utf-8")
            if path == "clients/apple/project.yml":
                return contents.replace(
                    f'CURRENT_PROJECT_VERSION: "{current}"',
                    f'CURRENT_PROJECT_VERSION: "{current + 1}"',
                )
            return contents

        errors = validate_documented_builds(read)

        self.assertTrue(
            any(f"claims Apple build {current}" in error for error in errors), errors
        )
        self.assertTrue(any("STATUS.html" in error for error in errors), errors)


class StatusHistoricalBuildMentionCase(unittest.TestCase):
    """docs/STATUS.html must be able to narrate past builds.

    The page carries sentences like "build 52 corrected the iPad mini failure"
    that are true when written and stay true after the next bump. Tense is
    invisible to the claim sweep, so a historical mention opts out explicitly
    with a `data-build-history` span. These cases pin both directions: the
    exemption works, and it is the only thing that grants it.
    """

    APPLE_BUILD = 60

    def _read(self, status_html: str):
        def read(path: str) -> str:
            if path == "docs/STATUS.html":
                return status_html
            contents = (ROOT / path).read_text(encoding="utf-8")
            if path == "clients/apple/project.yml":
                current = re.search(
                    r'CURRENT_PROJECT_VERSION: "(\d+)"', contents
                ).group(1)
                return contents.replace(
                    f'CURRENT_PROJECT_VERSION: "{current}"',
                    f'CURRENT_PROJECT_VERSION: "{self.APPLE_BUILD}"',
                )
            return contents

        return read

    def _status_errors(self, status_html: str) -> tuple[str, ...]:
        return tuple(
            error
            for error in validate_documented_builds(self._read(status_html))
            if "STATUS.html" in error
        )

    def test_a_marked_historical_mention_does_not_contradict_a_current_claim(
        self,
    ) -> None:
        status = (
            f"<p>Apple build {self.APPLE_BUILD} source, not yet uploaded</p>"
            "<p><span data-build-history>Apple build 52</span> corrected the "
            "reported iPad mini failure.</p>"
        )

        self.assertEqual(self._status_errors(status), ())

    def test_a_stale_current_claim_still_fails(self) -> None:
        status = f"<p>Apple build {self.APPLE_BUILD - 1} source, not yet uploaded</p>"

        errors = self._status_errors(status)

        self.assertTrue(errors, "a status page behind project.yml must be red")
        self.assertIn(str(self.APPLE_BUILD - 1), errors[0])

    def test_an_unmarked_historical_mention_is_still_swept(self) -> None:
        """The exemption is opt-in; prose alone never earns it."""
        status = (
            f"<p>Apple build {self.APPLE_BUILD} source, not yet uploaded</p>"
            "<p>Apple build 52 corrected the reported iPad mini failure.</p>"
        )

        errors = self._status_errors(status)

        self.assertTrue(errors, "an unmarked past build must still be checked")
        self.assertIn("52", errors[0])

    def test_a_marker_that_drifts_across_a_nested_span_fails_closed(self) -> None:
        """A marker that no longer wraps its sentence must not silently exempt."""
        status = (
            f"<p>Apple build {self.APPLE_BUILD} source, not yet uploaded</p>"
            "<p><span data-build-history><span class='t'>Apple build 52</span>"
            " corrected the iPad mini failure.</span></p>"
        )

        errors = self._status_errors(status)

        self.assertTrue(errors, "a marker spanning a nested <span> must not exempt")
        self.assertIn("52", errors[0])

    # Markup that is *not* the marker, and so must never earn the exemption.
    # The first three silently exempted a stale claim before the opening tag was
    # parsed as attributes rather than scanned as raw text: `data-build-history`
    # appearing anywhere in the tag was enough, including inside another
    # attribute's value or as the prefix of a different attribute's name.
    IMPOSTORS = {
        "the marker as another attribute's value": '<span title="data-build-history">',
        "the marker inside a longer value": '<span title="see data-build-history">',
        "an attribute name that merely starts with it": "<span data-build-history-note>",
        "an attribute name that merely ends with it": "<span x-data-build-history>",
        "a self-closing tag": "<span data-build-history/>",
        "a marker carrying a value": '<span data-build-history="false">',
        "the marker on a different element": "<div data-build-history>",
        "no marker at all": '<span class="t">',
    }

    # Markup that *is* the marker. Attribute order, an empty boolean value,
    # attribute-name case, and whitespace are all insignificant in HTML, and a
    # quoted value may legally contain `>` — which the raw-text pattern used to
    # choke on, rejecting a correctly marked sentence.
    MARKER_FORMS = {
        "bare": "<span data-build-history>",
        'empty value ""': '<span data-build-history="">',
        "empty value ''": "<span data-build-history=''>",
        "after another attribute": '<span class="t" data-build-history>',
        "before another attribute": '<span data-build-history class="t">',
        "beside a value containing >": '<span title="a>b" data-build-history>',
        "uppercase": "<SPAN DATA-BUILD-HISTORY>",
        "extra whitespace": "<span   data-build-history   >",
        "wrapped across lines": "<span\n  data-build-history\n>",
    }

    def _page_with(self, opening_tag: str) -> str:
        closing = "</div>" if opening_tag.startswith("<div") else "</span>"
        return (
            f"<p>Apple build {self.APPLE_BUILD} source, not yet uploaded</p>"
            f"<p>{opening_tag}Apple build 52{closing} corrected the reported "
            "iPad mini failure.</p>"
        )

    def test_only_the_real_marker_grants_the_exemption(self) -> None:
        """Markup that resembles the marker must not exempt a build mention.

        Each of these reads as an ordinary mention to a human, so treating one
        as an opt-out would let a stale current claim through the gate while
        looking like a deliberate history note in review.
        """
        for description, opening_tag in self.IMPOSTORS.items():
            with self.subTest(description):
                errors = self._status_errors(self._page_with(opening_tag))

                self.assertTrue(errors, f"{opening_tag} must not exempt build 52")
                self.assertIn("52", errors[0])

    def test_an_unclosed_marker_span_fails_closed(self) -> None:
        status = (
            f"<p>Apple build {self.APPLE_BUILD} source, not yet uploaded</p>"
            "<p><span data-build-history>Apple build 52 corrected the failure.</p>"
        )

        errors = self._status_errors(status)

        self.assertTrue(errors, "a marker with no closing tag must not exempt")
        self.assertIn("52", errors[0])

    def test_the_marker_is_honored_in_every_equivalent_html_form(self) -> None:
        """Attribute order, case, and whitespace are insignificant in HTML.

        A correctly marked sentence must not turn the gate red because a
        formatter rewrote the tag, since the only remedy left to that author is
        to reword true prose — exactly the failure this issue exists to remove.
        """
        for description, opening_tag in self.MARKER_FORMS.items():
            with self.subTest(description):
                self.assertEqual(self._status_errors(self._page_with(opening_tag)), ())

    # Marker-shaped characters that are not an element at all. The tokenizer
    # never produces a tag here, so a human reading the diff sees a comment, an
    # attribute value, or script text — nothing that looks like an opt-out —
    # while the raw-text pattern this replaced accepted all three and deleted
    # the visible prose between them.
    NON_ELEMENTS = {
        "an HTML comment": (
            "<!-- <span data-build-history> --><p>Apple build 52 stale</p>"
            "<!-- </span> -->"
        ),
        "another element's attribute value": (
            '<div title="<span data-build-history>">Apple build 52 stale</span></div>'
        ),
        "script text": (
            '<script>var s = "<span data-build-history>";</script>'
            "<p>Apple build 52 stale</p></span>"
        ),
        "style text": (
            '<style>/* <span data-build-history> */</style>'
            "<p>Apple build 52 stale</p></span>"
        ),
    }

    def test_marker_shaped_text_outside_element_context_never_exempts(self) -> None:
        """The marker must be a real start tag, not characters that spell one.

        Each fragment leaves `Apple build 52` visible on the rendered page, so
        exempting it would hide a stale current claim behind markup that reads
        as ordinary to both a browser and a reviewer.
        """
        for description, fragment in self.NON_ELEMENTS.items():
            with self.subTest(description):
                status = (
                    f"<p>Apple build {self.APPLE_BUILD} source, not yet uploaded</p>"
                    f"{fragment}"
                )

                errors = self._status_errors(status)

                self.assertTrue(errors, f"{description} must not exempt build 52")
                self.assertIn("52", errors[0])

    def test_the_exemption_ends_at_the_real_closing_tag(self) -> None:
        """A commented-out `</span>` neither closes the span nor extends it.

        The mirror of the case above: end-tag-shaped text must not end the
        exemption early, and the exemption must still stop at the author's own
        closing tag rather than running on into later prose.
        """
        status = (
            f"<p>Apple build {self.APPLE_BUILD} source, not yet uploaded</p>"
            "<p><span data-build-history>Apple build 52 <!-- </span> --></span>"
            " corrected the iPad mini failure.</p>"
            "<p>Apple build 51 stale</p>"
        )

        errors = self._status_errors(status)

        self.assertTrue(errors, "prose after the marked span is still swept")
        self.assertIn("51", errors[0])
        self.assertNotIn("52", errors[0])

    def test_the_shipped_status_page_uses_the_exemption_for_its_history(self) -> None:
        """Guards the real page, not just synthetic fixtures.

        The shipped `docs/STATUS.html` narrates the build that carried the
        offline-location fix. Bumping the counter must leave that sentence
        alone rather than forcing it to be reworded.
        """
        shipped = (ROOT / "docs/STATUS.html").read_text(encoding="utf-8")
        marked = re.findall(
            r"<span data-build-history>.*?</span>", shipped, re.DOTALL
        )
        self.assertTrue(marked, "the shipped page must mark its historical build")

        # Bump every *current* claim the way a real build bump would, leaving
        # the marked historical sentences exactly as written.
        bumped = shipped
        for index, span in enumerate(marked):
            bumped = bumped.replace(span, f"@@history-{index}@@")
        bumped = re.sub(
            r"Apple build [1-9]\d*", f"Apple build {self.APPLE_BUILD}", bumped
        )
        for index, span in enumerate(marked):
            bumped = bumped.replace(f"@@history-{index}@@", span)

        self.assertIn("Apple build 52", bumped, "history survived the bump verbatim")
        self.assertEqual(self._status_errors(bumped), ())


if __name__ == "__main__":
    unittest.main()
