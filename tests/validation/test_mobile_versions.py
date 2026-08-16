from __future__ import annotations

from pathlib import Path
import subprocess
import tempfile
import unittest

from validation.mobile_versions import (
    MobileVersionError,
    MobileVersions,
    check_repository,
    validate_versions,
)
from validation.runner import load_catalog, select_points, selected_checks


ROOT = Path(__file__).resolve().parents[2]


def versions(
    *,
    workspace: str = "0.2.2",
    apple_name: str | None = None,
    apple_build: int = 10,
    android_name: str | None = None,
    android_build: int = 5,
) -> MobileVersions:
    return MobileVersions(
        workspace=workspace,
        android_name=android_name or workspace,
        android_build=android_build,
        apple_name=apple_name or workspace,
        apple_build=apple_build,
        apple_marketing_references=2,
        apple_build_references=2,
    )


def apple_project(build: int, name: str = "0.2.2") -> str:
    return f'''settings:
  base:
    MARKETING_VERSION: "{name}"
    CURRENT_PROJECT_VERSION: "{build}"
targets:
  ios:
    info:
      properties:
        CFBundleShortVersionString: "$(MARKETING_VERSION)"
        CFBundleVersion: "$(CURRENT_PROJECT_VERSION)"
  tvos:
    info:
      properties:
        CFBundleShortVersionString: "$(MARKETING_VERSION)"
        CFBundleVersion: "$(CURRENT_PROJECT_VERSION)"
'''


def git(root: Path, *arguments: str) -> str:
    return subprocess.run(
        ["git", *arguments],
        cwd=root,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    ).stdout.strip()


def android_gradle(build: int, name: str = "0.2.2") -> str:
    return f'namespace = "org.plurx"\nversionCode = {build}\nversionName = "{name}"\n'


def cargo_workspace(version: str = "0.2.2", *, dependency: bool = False) -> str:
    manifest = f'[workspace]\n[workspace.package]\nversion = "{version}"\n'
    if dependency:
        manifest += '\n[workspace.dependencies]\nserde = "1"\n'
    return manifest


def seed_repository(root: Path, *, apple_build: int, android_build: int) -> None:
    """Lay down a minimal repo on a ``main`` branch with both apps present."""
    (root / "clients/apple/Sources").mkdir(parents=True)
    (root / "clients/android/app/src/main").mkdir(parents=True)
    (root / "Cargo.toml").write_text(cargo_workspace(), encoding="utf-8")
    (root / "clients/apple/project.yml").write_text(
        apple_project(apple_build), encoding="utf-8"
    )
    (root / "clients/apple/Sources/App.swift").write_text(
        "let release = 1\n", encoding="utf-8"
    )
    (root / "clients/android/app/build.gradle.kts").write_text(
        android_gradle(android_build), encoding="utf-8"
    )
    (root / "clients/android/app/src/main/AndroidManifest.xml").write_text(
        "<manifest />\n", encoding="utf-8"
    )
    git(root, "init", "-q")
    git(root, "checkout", "-qb", "main")
    git(root, "config", "user.name", "Version Test")
    git(root, "config", "user.email", "version@example.invalid")
    git(root, "add", "-A")
    git(root, "commit", "-qm", "seed")


class MergeTargetBaselineCase(unittest.TestCase):
    """The counter has to clear what the merge target ships, not the branch point.

    Reproduces the shape that let PR #294 sit green and unmergeable: the branch
    bumped its counter to N for its own feature, then an unrelated release bump
    landed the same N on the target. Both sides wrote the same literal, so Git
    auto-merged with no conflict and no diff evidence, and the release-hygiene
    check kept comparing against the branch point where the counter was still
    N-1.
    """

    def collide(
        self, root: Path, *, branch_edit: str, gradle: str, project: str
    ) -> str:
        """Bump one app to the same counter on both `main` and `feature`.

        Returns the branch point, which is what CI records as the base sha.
        """
        seed_repository(root, apple_build=10, android_build=26)
        branch_point = git(root, "rev-parse", "HEAD")

        git(root, "checkout", "-qb", "feature")
        (root / branch_edit).write_text("changed by the feature\n", encoding="utf-8")
        (root / "clients/android/app/build.gradle.kts").write_text(
            gradle, encoding="utf-8"
        )
        (root / "clients/apple/project.yml").write_text(project, encoding="utf-8")
        git(root, "add", "-A")
        git(root, "commit", "-qm", "feature work and its build bump")

        git(root, "checkout", "-q", "main")
        (root / "clients/android/app/build.gradle.kts").write_text(
            gradle, encoding="utf-8"
        )
        (root / "clients/apple/project.yml").write_text(project, encoding="utf-8")
        git(root, "add", "-A")
        git(root, "commit", "-qm", "unrelated operator release bump")

        git(root, "checkout", "-q", "feature")
        return branch_point

    def assert_collision_is_invisible_to_git(self, root: Path) -> None:
        git(root, "checkout", "-qb", "merge-probe", "feature")
        merge = subprocess.run(
            ["git", "merge", "--no-edit", "main"],
            cwd=root,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        self.assertEqual(merge.returncode, 0, merge.stdout)
        self.assertNotIn(
            "<<<<<<<", (root / "clients/android/app/build.gradle.kts").read_text()
        )
        git(root, "checkout", "-q", "feature")

    def test_android_counter_the_target_already_ships_fails_against_the_target(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            branch_point = self.collide(
                root,
                branch_edit="clients/android/app/src/main/AndroidManifest.xml",
                gradle=android_gradle(27),
                project=apple_project(10),
            )
            self.assert_collision_is_invisible_to_git(root)

            # The defect: baselined on the branch point, 27 > 26 and the branch
            # is green on a tree that cannot ship.
            self.assertEqual(
                check_repository(root, mode="changed-from", base=branch_point), ()
            )

            errors = check_repository(
                root, mode="changed-from", base=branch_point, merge_target="main"
            )
            failure = "\n".join(errors)
            self.assertIn("versionCode must increase", failure)
            self.assertIn("above 27", failure)
            self.assertIn("found 27", failure)
            self.assertIn("main is the merge target", failure)
            self.assertNotIn("CURRENT_PROJECT_VERSION", failure)

            # Re-bumping above the target, not above the branch point, clears it.
            (root / "clients/android/app/build.gradle.kts").write_text(
                android_gradle(28), encoding="utf-8"
            )
            git(root, "commit", "-qam", "re-bump above the current base")
            self.assertEqual(
                check_repository(
                    root, mode="changed-from", base=branch_point, merge_target="main"
                ),
                (),
            )

    def test_apple_counter_the_target_already_ships_fails_against_the_target(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            branch_point = self.collide(
                root,
                branch_edit="clients/apple/Sources/App.swift",
                gradle=android_gradle(26),
                project=apple_project(11),
            )

            self.assertEqual(
                check_repository(root, mode="changed-from", base=branch_point), ()
            )

            errors = check_repository(
                root, mode="changed-from", base=branch_point, merge_target="main"
            )
            failure = "\n".join(errors)
            self.assertIn("CURRENT_PROJECT_VERSION must increase", failure)
            self.assertIn("above 11", failure)
            self.assertIn("found 11", failure)
            self.assertIn("main is the merge target", failure)
            self.assertNotIn("versionCode must increase", failure)

    def test_the_target_scopes_only_the_counter_not_which_app_changed(self):
        """A target bump alone does not obligate an untouched app to re-bump."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            seed_repository(root, apple_build=10, android_build=26)
            branch_point = git(root, "rev-parse", "HEAD")

            git(root, "checkout", "-qb", "feature")
            (root / "docs").mkdir()
            (root / "docs/NOTES.md").write_text("prose only\n", encoding="utf-8")
            git(root, "add", "-A")
            git(root, "commit", "-qm", "documentation only")

            git(root, "checkout", "-q", "main")
            (root / "clients/android/app/build.gradle.kts").write_text(
                android_gradle(27), encoding="utf-8"
            )
            git(root, "commit", "-qam", "operator release bump")
            git(root, "checkout", "-q", "feature")

            self.assertEqual(
                check_repository(
                    root, mode="changed-from", base=branch_point, merge_target="main"
                ),
                (),
            )

    def release_lands_on_the_target(self, root: Path) -> str:
        """Ship release 0.2.3 on `main` under a branch that touched no client.

        The branch appends a workspace dependency and nothing else. `Cargo.toml`
        is in the `mobile.versioning` point paths, so this is the shape of an
        ordinary dependency pull request, not a client change.
        """
        seed_repository(root, apple_build=10, android_build=26)
        branch_point = git(root, "rev-parse", "HEAD")

        git(root, "checkout", "-qb", "feature")
        (root / "Cargo.toml").write_text(
            cargo_workspace(dependency=True), encoding="utf-8"
        )
        git(root, "add", "-A")
        git(root, "commit", "-qm", "add a workspace dependency")

        git(root, "checkout", "-q", "main")
        (root / "Cargo.toml").write_text(cargo_workspace("0.2.3"), encoding="utf-8")
        (root / "clients/android/app/build.gradle.kts").write_text(
            android_gradle(27, "0.2.3"), encoding="utf-8"
        )
        (root / "clients/apple/project.yml").write_text(
            apple_project(11, "0.2.3"), encoding="utf-8"
        )
        git(root, "add", "-A")
        git(root, "commit", "-qm", "release 0.2.3")
        git(root, "checkout", "-q", "feature")
        return branch_point

    def test_a_release_on_the_target_does_not_obligate_a_branch_that_shipped_none(self):
        """Whether *this branch* released is read at the branch point, not the target.

        `workspace_changed` is a scope input — it answers "did this branch ship a
        release", which is a property of the branch. Reading it off the merge
        target instead conflates that with "the target shipped a release", so
        every pull request open across a release goes red and is told to bump two
        store counters it never touched.
        """
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            branch_point = self.release_lands_on_the_target(root)

            self.assertEqual(
                check_repository(
                    root, mode="changed-from", base=branch_point, merge_target="main"
                ),
                (),
            )

    def test_the_target_release_still_raises_the_bar_for_a_branch_that_did_touch(self):
        """Scoping on the branch point must not weaken the counter comparison.

        The obvious wrong fix for the test above is to stop consulting the target
        at all. This pins the other side: the same target release, but a branch
        that genuinely edits an Android release input still has to clear the
        counter the target ships, not the one its branch point had.
        """
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            branch_point = self.release_lands_on_the_target(root)

            (root / "clients/android/app/src/main/AndroidManifest.xml").write_text(
                "<manifest android:label=\"feature\" />\n", encoding="utf-8"
            )
            git(root, "commit", "-qam", "android feature work")

            errors = check_repository(
                root, mode="changed-from", base=branch_point, merge_target="main"
            )
            failure = "\n".join(errors)
            self.assertIn("versionCode must increase", failure)
            self.assertIn("above 27", failure)
            self.assertIn("main is the merge target", failure)
            # Apple was never touched by this branch, and the target's release
            # bump is not this branch's, so Apple stays silent.
            self.assertNotIn("CURRENT_PROJECT_VERSION must increase", failure)

    def test_a_branch_that_ships_its_own_release_is_still_scoped_in(self):
        """A workspace bump made *by the branch* still obligates both counters."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            seed_repository(root, apple_build=10, android_build=26)
            branch_point = git(root, "rev-parse", "HEAD")

            git(root, "checkout", "-qb", "feature")
            (root / "Cargo.toml").write_text(cargo_workspace("0.2.3"), encoding="utf-8")
            (root / "clients/android/app/build.gradle.kts").write_text(
                android_gradle(26, "0.2.3"), encoding="utf-8"
            )
            (root / "clients/apple/project.yml").write_text(
                apple_project(10, "0.2.3"), encoding="utf-8"
            )
            git(root, "add", "-A")
            git(root, "commit", "-qm", "release 0.2.3 from the branch")

            errors = check_repository(
                root, mode="changed-from", base=branch_point, merge_target="main"
            )
            failure = "\n".join(errors)
            self.assertIn("versionCode must increase", failure)
            self.assertIn("CURRENT_PROJECT_VERSION must increase", failure)

    def test_an_unreadable_merge_target_fails_instead_of_falling_back(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            seed_repository(root, apple_build=10, android_build=26)
            branch_point = git(root, "rev-parse", "HEAD")

            with self.assertRaises(MobileVersionError):
                check_repository(
                    root,
                    mode="changed-from",
                    base=branch_point,
                    merge_target="origin/never-fetched",
                )

    def test_a_merge_target_without_a_branch_diff_is_rejected_not_ignored(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            seed_repository(root, apple_build=10, android_build=26)

            with self.assertRaises(MobileVersionError):
                check_repository(root, mode="all", merge_target="main")


class MobileVersionCase(unittest.TestCase):
    def test_mobile_marketing_versions_still_match_the_workspace_release(self):
        errors = validate_versions(
            versions(apple_name="0.2.1", android_name="0.3.0")
        )

        self.assertTrue(any("Android versionName must match" in error for error in errors))
        self.assertTrue(
            any("Apple MARKETING_VERSION must match" in error for error in errors)
        )

    def test_each_changed_app_requires_only_its_own_build_counter(self):
        baseline = versions()

        apple_errors = validate_versions(
            versions(),
            baseline=baseline,
            changed_paths=("clients/apple/Sources/PlayerView.swift",),
        )
        self.assertTrue(
            any(
                "CURRENT_PROJECT_VERSION must increase" in error
                for error in apple_errors
            )
        )
        self.assertEqual(
            validate_versions(
                versions(apple_build=11),
                baseline=baseline,
                changed_paths=("clients/apple/Sources/PlayerView.swift",),
            ),
            (),
        )

        android_errors = validate_versions(
            versions(),
            baseline=baseline,
            changed_paths=("clients/android/app/src/main/AndroidManifest.xml",),
        )
        self.assertTrue(any("versionCode must increase" in error for error in android_errors))
        self.assertEqual(
            validate_versions(
                versions(android_build=6),
                baseline=baseline,
                changed_paths=("clients/android/app/src/main/AndroidManifest.xml",),
            ),
            (),
        )

    def test_tests_and_documentation_do_not_require_a_build_increment(self):
        baseline = versions()
        self.assertEqual(
            validate_versions(
                versions(),
                baseline=baseline,
                changed_paths=(
                    "clients/apple/Tests/AppleClientTests.swift",
                    "clients/android/app/src/test/AppVersionTest.kt",
                    "clients/apple/README.md",
                ),
            ),
            (),
        )

    def test_workspace_release_change_updates_and_advances_both_apps(self):
        baseline = versions()
        unchanged_builds = validate_versions(
            versions(workspace="0.2.3"),
            baseline=baseline,
            changed_paths=("Cargo.toml",),
        )
        self.assertTrue(
            any(
                "CURRENT_PROJECT_VERSION must increase" in error
                for error in unchanged_builds
            )
        )
        self.assertTrue(
            any("versionCode must increase" in error for error in unchanged_builds)
        )

        self.assertEqual(
            validate_versions(
                versions(workspace="0.2.3", apple_build=11, android_build=6),
                baseline=baseline,
                changed_paths=("Cargo.toml",),
            ),
            (),
        )

    def test_staged_check_reads_the_index_not_unstaged_version_edits(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "clients/apple/Sources").mkdir(parents=True)
            (root / "clients/android/app").mkdir(parents=True)
            (root / "Cargo.toml").write_text(
                '[workspace]\n[workspace.package]\nversion = "0.2.2"\n',
                encoding="utf-8",
            )
            (root / "clients/apple/project.yml").write_text(
                apple_project(10), encoding="utf-8"
            )
            (root / "clients/apple/Sources/App.swift").write_text(
                "let release = 1\n", encoding="utf-8"
            )
            (root / "clients/android/app/build.gradle.kts").write_text(
                'versionCode = 5\nversionName = "0.2.2"\n', encoding="utf-8"
            )
            subprocess.run(["git", "init", "-q"], cwd=root, check=True)
            subprocess.run(
                ["git", "config", "user.name", "Version Test"], cwd=root, check=True
            )
            subprocess.run(
                ["git", "config", "user.email", "version@example.invalid"],
                cwd=root,
                check=True,
            )
            subprocess.run(["git", "add", "-A"], cwd=root, check=True)
            subprocess.run(["git", "commit", "-qm", "seed"], cwd=root, check=True)
            base = subprocess.run(
                ["git", "rev-parse", "HEAD"],
                cwd=root,
                check=True,
                text=True,
                stdout=subprocess.PIPE,
            ).stdout.strip()

            (root / "clients/apple/Sources/App.swift").write_text(
                "let release = 2\n", encoding="utf-8"
            )
            subprocess.run(
                ["git", "add", "clients/apple/Sources/App.swift"], cwd=root, check=True
            )
            (root / "clients/apple/project.yml").write_text(
                apple_project(11), encoding="utf-8"
            )

            errors = check_repository(root, mode="staged")
            self.assertTrue(
                any(
                    "CURRENT_PROJECT_VERSION must increase" in error
                    for error in errors
                )
            )

            subprocess.run(
                ["git", "add", "clients/apple/project.yml"], cwd=root, check=True
            )
            self.assertEqual(check_repository(root, mode="staged"), ())
            subprocess.run(
                ["git", "commit", "-qm", "change apple app"], cwd=root, check=True
            )
            self.assertEqual(
                check_repository(root, mode="changed-from", base=base), ()
            )

    def test_catalog_routes_only_release_inputs_to_the_version_gate(self):
        catalog = load_catalog(ROOT / "validation/points.toml")

        def check_ids(path: str) -> set[str]:
            selection = select_points(catalog, (path,))
            return {
                check.id
                for check in selected_checks(catalog, selection, profile="commit")
            }

        self.assertIn(
            "mobile-version", check_ids("clients/apple/Sources/PlayerView.swift")
        )
        self.assertIn(
            "mobile-version",
            check_ids("clients/android/app/src/main/AndroidManifest.xml"),
        )
        self.assertIn("mobile-version", check_ids("Cargo.toml"))
        self.assertNotIn(
            "mobile-version", check_ids("clients/apple/Tests/AppleClientTests.swift")
        )
        self.assertNotIn(
            "mobile-version",
            check_ids("clients/android/app/src/test/AppVersionTest.kt"),
        )


if __name__ == "__main__":
    unittest.main()
