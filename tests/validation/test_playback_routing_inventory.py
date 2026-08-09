from __future__ import annotations

import re
import tomllib
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
MANIFEST = ROOT / "tests/playback/routing-decisions.toml"
CLIENT_FIXES = ROOT / "tests/client-fixes.toml"
PLAYBACK_DOC = ROOT / "docs/PLAYBACK.md"
START = "<!-- playback-routing-inventory:start -->"
END = "<!-- playback-routing-inventory:end -->"


class PlaybackRoutingInventoryTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.catalog = tomllib.loads(MANIFEST.read_text(encoding="utf-8"))
        cls.decisions = cls.catalog["decisions"]

    def test_inventory_is_unique_and_covers_every_documented_route(self) -> None:
        ids = [entry["id"] for entry in self.decisions]
        self.assertEqual(len(ids), len(set(ids)), "routing decision ids must be unique")
        self.assertGreaterEqual(
            len(ids),
            20,
            "an accidentally truncated routing inventory must fail loudly",
        )

        document = PLAYBACK_DOC.read_text(encoding="utf-8")
        self.assertIn(START, document)
        self.assertIn(END, document)
        section = document.split(START, 1)[1].split(END, 1)[0]
        documented = re.findall(r"^\|\s*`([a-z0-9.-]+)`\s*\|", section, re.MULTILINE)
        self.assertEqual(
            set(ids),
            set(documented),
            "the executable catalog and PLAYBACK.md must list the same routing forks",
        )
        self.assertEqual(
            len(documented),
            len(set(documented)),
            "PLAYBACK.md lists a routing id more than once",
        )

    def test_every_route_has_live_source_and_test_anchors(self) -> None:
        required = {"id", "source", "source_anchor", "test", "test_anchor"}
        for entry in self.decisions:
            with self.subTest(route=entry.get("id", "(missing id)")):
                self.assertEqual(required, set(entry))
                for path_key, anchor_key in (
                    ("source", "source_anchor"),
                    ("test", "test_anchor"),
                ):
                    path = ROOT / entry[path_key]
                    self.assertTrue(path.is_file(), f"missing {path_key}: {path}")
                    contents = path.read_text(encoding="utf-8")
                    self.assertIn(
                        entry[anchor_key],
                        contents,
                        f"{entry['id']} lost {anchor_key} in {entry[path_key]}",
                    )

    def test_client_fix_anchors_are_unique_live_and_commit_bound(self) -> None:
        catalog = tomllib.loads(CLIENT_FIXES.read_text(encoding="utf-8"))
        self.assertEqual(catalog.get("version"), 1)
        self.assertRegex(catalog.get("enforce_after", ""), r"^[0-9a-f]{7,40}$")
        fixes = catalog["fixes"]
        ids = [entry["id"] for entry in fixes]
        self.assertEqual(len(ids), len(set(ids)), "client fix ids must be unique")
        self.assertGreaterEqual(len(fixes), 3)
        required = {
            "id",
            "commits",
            "source",
            "source_anchor",
            "test",
            "test_anchor",
        }
        commit_claims: set[str] = set()
        for entry in fixes:
            with self.subTest(client_fix=entry.get("id", "(missing id)")):
                self.assertEqual(required, set(entry))
                self.assertTrue(entry["commits"])
                for commit in entry["commits"]:
                    self.assertRegex(commit, r"^[0-9a-f]{7,40}$")
                    self.assertNotIn(commit, commit_claims, "commit has two client fix rows")
                    commit_claims.add(commit)
                for path_key, anchor_key in (
                    ("source", "source_anchor"),
                    ("test", "test_anchor"),
                ):
                    path = ROOT / entry[path_key]
                    self.assertTrue(path.is_file(), f"missing {path_key}: {path}")
                    self.assertIn(
                        entry[anchor_key],
                        path.read_text(encoding="utf-8"),
                        f"{entry['id']} lost {anchor_key} in {entry[path_key]}",
                    )

    def test_web_policy_tests_are_in_the_commit_gate(self) -> None:
        makefile = (ROOT / "Makefile").read_text(encoding="utf-8")
        self.assertIn("node tests/playback/web-policy.test.js", makefile)

        validation = tomllib.loads(
            (ROOT / "validation/points.toml").read_text(encoding="utf-8")
        )
        checks = {entry["id"]: entry for entry in validation["checks"]}
        self.assertEqual(checks["web-static"]["command"], "make web-check")
        self.assertIn("node", checks["web-static"]["requires"])

        points = {entry["id"]: entry for entry in validation["points"]}
        playback = points["playback.pipeline"]
        self.assertIn("crates/plurxd/src/web/playback-policy.js", playback["paths"])
        self.assertIn("web-static", playback["checks"])


if __name__ == "__main__":
    unittest.main()
