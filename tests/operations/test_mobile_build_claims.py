from __future__ import annotations

from pathlib import Path
import unittest

from validation.doc_versions import check_repository, validate_documented_builds


ROOT = Path(__file__).resolve().parents[2]


class MobileBuildClaimCase(unittest.TestCase):
    def test_repository_build_claims_match_release_inputs(self) -> None:
        self.assertEqual(check_repository(ROOT), ())

    def test_advancing_a_build_counter_without_docs_is_rejected(self) -> None:
        def read(path: str) -> str:
            contents = (ROOT / path).read_text(encoding="utf-8")
            if path == "clients/apple/project.yml":
                return contents.replace(
                    'CURRENT_PROJECT_VERSION: "41"',
                    'CURRENT_PROJECT_VERSION: "42"',
                )
            return contents

        errors = validate_documented_builds(read)

        self.assertTrue(
            any("claims Apple build 41" in error for error in errors), errors
        )
        self.assertTrue(any("STATUS.html" in error for error in errors), errors)


if __name__ == "__main__":
    unittest.main()
