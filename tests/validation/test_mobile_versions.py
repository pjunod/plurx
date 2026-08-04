from __future__ import annotations

from pathlib import Path
import subprocess
import tempfile
import unittest

from validation.mobile_versions import (
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


def apple_project(build: int) -> str:
    return f'''settings:
  base:
    MARKETING_VERSION: "0.2.2"
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
