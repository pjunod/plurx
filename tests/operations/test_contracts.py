from __future__ import annotations

import os
from pathlib import Path
import re
import runpy
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def workflow_job_blocks(path: str) -> dict[str, str]:
    jobs = read(path).split("\njobs:\n", 1)[1]
    starts = list(re.finditer(r"(?m)^  ([a-zA-Z0-9_-]+):\n", jobs))
    return {
        match.group(1): jobs[match.start() : starts[index + 1].start()]
        if index + 1 < len(starts)
        else jobs[match.start() :]
        for index, match in enumerate(starts)
    }


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
        subprocess.run(["bash", "-n", str(ROOT / "scripts/ship")], check=True)
        self.assertIn("plurx-iOS:", project)
        self.assertIn("plurx-tvOS:", project)
        self.assertIn('"$ANSIBLE_DIR/mobile.yml"', ship)
        self.assertIn('--tags "$MOBILE_TAGS"', ship)
        self.assertNotIn("deploy/ansible", ship)
        self.assertIn("alias(libs.plugins.android.application)", android)

    def test_ship_selects_mobile_tags_and_optional_vars_file(self):
        with tempfile.TemporaryDirectory() as temporary:
            environment = os.environ.copy()
            environment["PLURX_ANSIBLE_DIR"] = temporary
            environment["PLURX_ANSIBLE_INVENTORY"] = str(
                Path(temporary) / "inventory.yml"
            )
            environment["PLURX_MOBILE_VARS_FILE"] = str(
                Path(temporary) / "vars.yml"
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
        self.assertIn(f"{temporary}/mobile.yml --tags apple,android", output)
        self.assertIn(f"-e @{temporary}/vars.yml", output)

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
        self.assertIn("name: fast policy and contract preflight", workflow)
        self.assertIn("name: mobile release version", workflow)
        self.assertIn("name: Check mobile release hygiene", workflow)
        self.assertLess(
            workflow.index("name: Audit corrective-history evidence"),
            workflow.index("name: Check validation catalog and contract unit tests"),
        )
        self.assertIn("name: PR validation gate", workflow)
        self.assertIn("PLURX_SKIP_UI_BASELINE: 1", workflow)
        self.assertIn("PLURX_SKIP_ANDROID_JVM: 1", workflow)
        self.assertIn("if: needs.scope.outputs.apple == 'true'", workflow)
        self.assertIn("if: needs.scope.outputs.android_device == 'true'", workflow)
        self.assertIn("if: needs.scope.outputs.web_layout == 'true'", workflow)
        self.assertIn("if: needs.scope.outputs.release_build == 'true'", workflow)
        self.assertIn("if: needs.scope.outputs.docs_only != 'true'", workflow)
        self.assertIn("needs: [scope, preflight]", workflow)
        self.assertIn("PREFLIGHT_RESULT: ${{ needs.preflight.result }}", workflow)
        self.assertIn(
            "MOBILE_VERSION_RESULT: ${{ needs.mobile_version.result }}",
            workflow,
        )
        self.assertIn("HIQLITE_SPIKE_RESULT: ${{ needs.hiqlite_spike.result }}", workflow)
        self.assertIn("CLUSTER_AUTH_RESULT: ${{ needs.cluster_auth.result }}", workflow)
        pr_gate = workflow.split("  pr_gate:", 1)[1]
        self.assertIn("      - mobile_version", pr_gate)
        self.assertIn("      - hiqlite_spike", pr_gate)
        self.assertIn("      - cluster_auth", pr_gate)
        self.assertIn("needs: scope", workflow)
        self.assertNotIn("github.event_name == 'pull_request' && github.ref == 'refs/heads/main'", workflow)

        mobile = workflow.split("  mobile_version:", 1)[1].split("\n  preflight:", 1)[0]
        self.assertIn("needs: scope", mobile)
        self.assertNotIn("needs: [scope, preflight]", mobile)
        apple = workflow.split("\n  apple:\n", 1)[1].split(
            "\n  android_device:\n", 1
        )[0]
        self.assertIn("needs: [scope, preflight]", apple)
        self.assertNotIn("mobile_version", apple)

        coverage = workflow.split("  coverage:", 1)[1].split("\n  build:", 1)[0]
        self.assertIn("if: github.ref == 'refs/heads/main'", coverage)

        docker = workflow.split("  docker:", 1)[1].split("\n  pr_gate:", 1)[0]
        self.assertNotIn("needs: check", docker)
        self.assertIn("needs: [scope, preflight]", docker)

        lint = read(".github/workflows/lint.yml")
        self.assertIn("Select the documentation-only fast path", lint)
        self.assertIn("steps.scope.outputs.docs_only != 'true'", lint)

        self.assertIn(
            "cargo build --release -p plurxd --target ${{ matrix.target }}",
            workflow,
        )
        self.assertNotIn(
            "cargo build --release --workspace --target ${{ matrix.target }}",
            workflow,
        )

    def test_every_actions_job_has_an_explicit_timeout(self):
        for path in (
            ".github/workflows/ci.yml",
            ".github/workflows/lint.yml",
            ".github/workflows/rust-audit.yml",
        ):
            with self.subTest(path=path):
                jobs = workflow_job_blocks(path)
                self.assertTrue(jobs, f"{path} has no jobs")
                missing = [
                    name for name, block in jobs.items()
                    if "\n    timeout-minutes:" not in block
                ]
                self.assertEqual([], missing, f"jobs without timeouts in {path}")

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
