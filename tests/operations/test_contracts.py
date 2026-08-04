from __future__ import annotations

import json
import os
from pathlib import Path
import re
import runpy
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


class OperationsContractCase(unittest.TestCase):
    def test_compose_keeps_identity_data_and_host_ports_explicit(self):
        compose = read("deploy/docker-compose.yml")
        self.assertRegex(compose, r"(?m)^name: plurx$")
        self.assertIn('${PLURX_DATA:-/srv/plurx}:/var/lib/plurx', compose)
        self.assertIn('${PLURX_HTTP_PORT:-32400}:32400', compose)
        self.assertIn('${PLURX_GDM_PORT:-32414}:32414/udp', compose)
        self.assertIn('user: "${PUID:-1000}:${PGID:-1000}"', compose)
        self.assertIn('PLURX_BUILD_REF: ${PLURX_BUILD_REF:-}', compose)

    def test_discovery_uses_host_network_without_stealing_it_from_the_server(self):
        compose = read("deploy/docker-compose.yml")
        server, discovery = compose.split("  plurx-discovery:", 1)
        self.assertNotRegex(server, r"(?m)^    network_mode: host$")
        self.assertIn("network_mode: host", discovery)
        configured_origin = '"http://127.0.0.1:${PLURX_HTTP_PORT:-32400}"'
        configured_bind = '"127.0.0.1:${PLURX_HTTP_PORT:-32400}"'
        self.assertIn(f"PLURX_DISCOVERY_SERVER_URL: {configured_origin}", discovery)
        self.assertIn(f"PLURX_BIND: {configured_bind}", discovery)

    def test_docker_up_preserves_override_discovery_and_stamps_the_build(self):
        result = subprocess.run(
            ["make", "-n", "docker-up"],
            cwd=ROOT,
            check=True,
            text=True,
            stdout=subprocess.PIPE,
        )
        command = result.stdout
        self.assertIn("cd deploy && PLURX_BUILD_REF=", command)
        self.assertIn("docker compose up -d --build", command)
        self.assertNotIn("-f deploy/docker-compose.yml", command)

        dockerfile = read("Dockerfile")
        compose = read("deploy/docker-compose.yml")
        self.assertIn('ARG PLURX_BUILD_REF=""', dockerfile)
        self.assertIn("ENV PLURX_BUILD_REF=${PLURX_BUILD_REF}", dockerfile)
        self.assertIn("PLURX_BUILD_REF: ${PLURX_BUILD_REF:-}", compose)

    def test_docker_build_frees_each_ffmpeg_download_before_the_next(self):
        dockerfile = read("Dockerfile")
        distro_install = dockerfile.index("intel-media-va-driver-non-free")
        first_clean = dockerfile.index("apt-get clean", distro_install)
        jellyfin_install = dockerfile.index("apt-get install -y --no-install-recommends jellyfin-ffmpeg7")
        second_clean = dockerfile.index("apt-get clean", jellyfin_install)

        self.assertLess(distro_install, first_clean)
        self.assertLess(first_clean, jellyfin_install)
        self.assertLess(jellyfin_install, second_clean)
        self.assertEqual(dockerfile.count("&& apt-get clean"), 2)

    def test_ship_routes_real_mobile_targets_through_ansible(self):
        ship = read("scripts/ship")
        project = read("clients/apple/project.yml")
        android = read("clients/android/app/build.gradle.kts")
        android_tasks = read("deploy/ansible/tasks/mobile-android.yml")
        apple_tasks = read("deploy/ansible/tasks/mobile-apple.yml")
        apple_target_tasks = read("deploy/ansible/tasks/mobile-apple-target.yml")
        upload_options = read("deploy/ansible/ExportOptionsUpload.plist")
        subprocess.run(["bash", "-n", str(ROOT / "scripts/ship")], check=True)
        self.assertIn("plurx-iOS:", project)
        self.assertIn("plurx-tvOS:", project)
        self.assertIn('"$ROOT/deploy/ansible/mobile.yml"', ship)
        self.assertIn('--tags "$MOBILE_TAGS"', ship)
        self.assertIn("- plurx-iOS", apple_tasks)
        self.assertIn("- plurx-tvOS", apple_tasks)
        self.assertIn("- testDebugUnitTest", android_tasks)
        self.assertIn("- :app:lintDebug", android_tasks)
        self.assertIn("- :app:assembleDebug", android_tasks)
        self.assertIn("'-exportArchive'", apple_target_tasks)
        self.assertIn("ExportOptionsUpload.plist", apple_target_tasks)
        self.assertIn("<key>destination</key><string>upload</string>", upload_options)
        self.assertIn("alias(libs.plugins.android.application)", android)

    def test_ship_selects_mobile_tags_and_optional_vars_file(self):
        with tempfile.TemporaryDirectory() as temporary:
            environment = os.environ.copy()
            environment["PLURX_ANSIBLE_INVENTORY"] = str(
                Path(temporary) / "inventory.yml"
            )
            environment["PLURX_MOBILE_VARS_FILE"] = str(
                Path(temporary) / "mobile.yml"
            )
            result = subprocess.run(
                [str(ROOT / "scripts/ship"), "--apple", "--android", "--dry-run"],
                cwd=ROOT,
                env=environment,
                check=True,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
            )

        output = result.stdout
        self.assertIn("targets: apple,android", output)
        self.assertIn("deploy/ansible/mobile.yml --tags apple,android", output)
        self.assertIn(f"-e @{temporary}/mobile.yml", output)

    @unittest.skipUnless(shutil.which("ansible-playbook"), "ansible is not installed")
    def test_android_playbook_targets_each_physical_device_once(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = Path(temporary)
            android_root = fixture / "clients/android"
            android_root.mkdir(parents=True)
            (fixture / "Cargo.toml").write_text("[workspace]\n", encoding="utf-8")
            install_log = fixture / "installs.log"
            gradle_log = fixture / "gradle.log"

            gradlew = android_root / "gradlew"
            gradlew.write_text(
                f"""#!/bin/sh
mkdir -p app/build/outputs/apk/debug
: > app/build/outputs/apk/debug/app-debug.apk
printf '%s\\n' "$*" > '{gradle_log}'
""",
                encoding="utf-8",
            )
            gradlew.chmod(0o755)

            adb = fixture / "adb"
            adb.write_text(
                f"""#!/bin/sh
case "$1" in
  devices)
    printf 'List of devices attached\\nemulator-5554\\tdevice\\nwireless-a\\tdevice\\nwireless-b\\tdevice\\nlenovo-endpoint\\tdevice\\n'
    ;;
  -s)
    endpoint="$2"
    if [ "$3" = shell ]; then
      case "$endpoint" in
        wireless-a|wireless-b) printf 'DUPLICATE\\r\\n' ;;
        lenovo-endpoint) printf 'HA263LBP\\r\\n' ;;
        *) exit 2 ;;
      esac
    elif [ "$3" = install ]; then
      printf '%s\\n' "$endpoint" >> '{install_log}'
      printf 'Success\\n'
    else
      exit 4
    fi
    ;;
  *) exit 3 ;;
esac
""",
                encoding="utf-8",
            )
            adb.chmod(0o755)

            inventory = fixture / "inventory.yml"
            inventory.write_text(
                """all:
  children:
    control:
      hosts:
        localhost:
          ansible_connection: local
          ansible_python_interpreter: "{{ ansible_playbook_python }}"
""",
                encoding="utf-8",
            )
            variables = fixture / "vars.json"
            variables.write_text(
                json.dumps(
                    {
                        "plurx_repo": str(fixture),
                        "plurx_android_adb": str(adb),
                        "plurx_android_required_serials": [
                            "DUPLICATE",
                            "HA263LBP",
                        ],
                    }
                ),
                encoding="utf-8",
            )

            environment = os.environ.copy()
            environment["ANSIBLE_LOCAL_TEMP"] = str(fixture / "ansible-local")
            environment["ANSIBLE_REMOTE_TEMP"] = str(fixture / "ansible-remote")
            ansible = subprocess.run(
                [
                    "ansible-playbook",
                    "-i",
                    str(inventory),
                    str(ROOT / "deploy/ansible/mobile.yml"),
                    "--tags",
                    "android",
                    "-e",
                    f"@{variables}",
                ],
                cwd=ROOT,
                env=environment,
                check=False,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
            )
            self.assertEqual(ansible.returncode, 0, ansible.stdout)

            self.assertEqual(
                gradle_log.read_text(encoding="utf-8").strip(),
                "--no-daemon testDebugUnitTest :app:lintDebug :app:assembleDebug",
            )
            self.assertEqual(
                install_log.read_text(encoding="utf-8").splitlines(),
                ["wireless-a", "lenovo-endpoint"],
            )

    @unittest.skipUnless(shutil.which("ansible-playbook"), "ansible is not installed")
    def test_apple_playbook_tests_both_platforms_before_direct_upload(self):
        with tempfile.TemporaryDirectory() as temporary:
            fixture = Path(temporary)
            apple_root = fixture / "clients/apple"
            ansible_root = fixture / "deploy/ansible"
            bin_root = fixture / "bin"
            apple_root.mkdir(parents=True)
            ansible_root.mkdir(parents=True)
            bin_root.mkdir()
            (fixture / "Cargo.toml").write_text("[workspace]\n", encoding="utf-8")
            (apple_root / "ExportOptions.plist").write_text("plist\n", encoding="utf-8")
            (ansible_root / "ExportOptionsUpload.plist").write_text(
                "plist\n", encoding="utf-8"
            )
            private_key = fixture / "AuthKey_TEST.p8"
            private_key.write_text("private fixture\n", encoding="utf-8")
            xcode_log = fixture / "xcodebuild.log"

            xcodegen = bin_root / "xcodegen"
            xcodegen.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            xcodegen.chmod(0o755)
            xcodebuild = bin_root / "xcodebuild"
            xcodebuild.write_text(
                f"#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{xcode_log}'\n",
                encoding="utf-8",
            )
            xcodebuild.chmod(0o755)

            inventory = fixture / "inventory.yml"
            inventory.write_text(
                """all:
  children:
    control:
      hosts:
        localhost:
          ansible_connection: local
          ansible_python_interpreter: "{{ ansible_playbook_python }}"
""",
                encoding="utf-8",
            )
            variables = fixture / "vars.json"
            variables.write_text(
                json.dumps(
                    {
                        "plurx_repo": str(fixture),
                        "plurx_apple_key_id": "TEST",
                        "plurx_apple_issuer_id": "ISSUER",
                        "plurx_apple_private_key": str(private_key),
                    }
                ),
                encoding="utf-8",
            )

            environment = os.environ.copy()
            environment["ANSIBLE_LOCAL_TEMP"] = str(fixture / "ansible-local")
            environment["ANSIBLE_REMOTE_TEMP"] = str(fixture / "ansible-remote")
            environment["PATH"] = f"{bin_root}{os.pathsep}{environment['PATH']}"
            ansible = subprocess.run(
                [
                    "ansible-playbook",
                    "-i",
                    str(inventory),
                    str(ROOT / "deploy/ansible/mobile.yml"),
                    "--tags",
                    "apple",
                    "-e",
                    f"@{variables}",
                ],
                cwd=ROOT,
                env=environment,
                check=False,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
            )
            self.assertEqual(ansible.returncode, 0, ansible.stdout)

            commands = xcode_log.read_text(encoding="utf-8").splitlines()
            self.assertEqual(len(commands), 6)
            self.assertIn("-scheme plurx-iOS", commands[0])
            self.assertTrue(commands[0].endswith(" test"))
            self.assertIn("-scheme plurx-tvOS", commands[1])
            self.assertTrue(commands[1].endswith(" test"))
            self.assertIn("-scheme plurx-iOS", commands[2])
            self.assertTrue(commands[2].endswith(" archive"))
            self.assertIn("ExportOptionsUpload.plist", commands[3])
            self.assertIn("-scheme plurx-tvOS", commands[4])
            self.assertTrue(commands[4].endswith(" archive"))
            self.assertIn("ExportOptionsUpload.plist", commands[5])
            for command in commands[2:]:
                self.assertIn("-authenticationKeyID TEST", command)
                self.assertIn("-authenticationKeyIssuerID ISSUER", command)

    def test_ci_provisions_concrete_apple_devices_before_testing(self):
        workflow = read(".github/workflows/ci.yml")
        self.assertIn("sudo xcodebuild -runFirstLaunch", workflow)
        self.assertEqual(workflow.count("xcrun simctl create"), 2)
        self.assertIn("SimDeviceType.iPhone-16-Pro", workflow)
        self.assertIn("SimDeviceType.Apple-TV-4K-3rd-generation-4K", workflow)
        self.assertIn('APPLE_IOS_SIM=platform=iOS Simulator,id=$ios_id', workflow)
        self.assertIn('APPLE_TVOS_SIM=platform=tvOS Simulator,id=$tvos_id', workflow)
        self.assertLess(workflow.index("xcrun simctl create"), workflow.index("run: make apple-test"))

    def test_pr_ci_selects_expensive_surfaces_and_has_one_aggregate_gate(self):
        workflow = read(".github/workflows/ci.yml")

        self.assertIn("python3 -m validation.ci_scope", workflow)
        self.assertIn("name: PR validation gate", workflow)
        self.assertIn("PLURX_SKIP_UI_BASELINE: 1", workflow)
        self.assertIn("PLURX_SKIP_ANDROID_JVM: 1", workflow)
        self.assertIn("if: needs.scope.outputs.apple == 'true'", workflow)
        self.assertIn("if: needs.scope.outputs.android_device == 'true'", workflow)
        self.assertIn("if: needs.scope.outputs.web_layout == 'true'", workflow)
        self.assertIn("if: needs.scope.outputs.release_build == 'true'", workflow)
        self.assertIn("needs: scope", workflow)
        self.assertNotIn("github.event_name == 'pull_request' && github.ref == 'refs/heads/main'", workflow)

        coverage = workflow.split("  coverage:", 1)[1].split("\n  build:", 1)[0]
        self.assertIn("if: github.ref == 'refs/heads/main'", coverage)

        docker = workflow.split("  docker:", 1)[1].split("\n  pr_gate:", 1)[0]
        self.assertNotIn("needs: check", docker)

    def test_container_smoke_keeps_non_root_state_port_and_cleanup_contracts(self):
        smoke = read("scripts/container-smoke")
        subprocess.run(["sh", "-n", str(ROOT / "scripts/container-smoke")], check=True)
        self.assertIn('chmod 0777 "$scratch"', smoke)
        self.assertIn("trap cleanup EXIT HUP INT TERM", smoke)
        self.assertIn("--publish 127.0.0.1:0:32400", smoke)
        self.assertIn('container_user="$(docker image inspect', smoke)
        self.assertIn('if [ "$container_user" != "plurx" ]', smoke)
        self.assertIn("--user 0:0", smoke)
        self.assertIn("-c 'chmod -R a+rwX /scratch'", smoke)
        self.assertEqual(len(re.findall(r'docker port "\$name" 32400/tcp', smoke)), 2)
        self.assertIn('curl -fsS "$base/readyz"', smoke)
        self.assertIn('curl -fsS "$base/metrics"', smoke)
        self.assertIn('test "$instance_before" = "$instance_after"', smoke)

    def test_perf_report_counts_copy_video_as_a_real_session(self):
        namespace = runpy.run_path(str(ROOT / "scripts/perf-report"))
        lines = []
        logs = [
            {"message": "transcode ffmpeg args: encoder=software pipeline=cpu"},
            {"message": "copy-video HLS ffmpeg args: -c:v copy"},
        ]

        namespace["log_section"](lines.append, logs, {}, None)

        report = "\n".join(lines)
        self.assertIn("1 transcode · 1 copy-video", report)
        self.assertIn("most recent copy-video command", report)


if __name__ == "__main__":
    unittest.main()
