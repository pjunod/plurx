from __future__ import annotations

import json
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
    def test_swarm_runtime_keeps_network_quota_and_role_boundaries_explicit(self):
        config = json.loads(read("swarm/config.json"))

        self.assertIs(config["runtime"]["worker_network_access"], True)
        self.assertIs(config["queue"]["prefer_expiring_quota"], True)
        self.assertEqual(config["queue"]["diagnosis_role"], "troubleshooter")
        self.assertEqual(config["queue"]["diagnosis_fix_role"], "builder")
        self.assertEqual(
            config["workers"]["troubleshooter"]["role"], "troubleshooter"
        )
        troubleshooter = read("swarm/troubleshooter.txt")
        self.assertIn("Stay read-only", troubleshooter)
        self.assertIn("Root cause and confidence", troubleshooter)
        self.assertIn("diagnosis-fix", troubleshooter)
        for name, worker in config["workers"].items():
            with self.subTest(worker=name):
                self.assertNotIn("fallback_roles", worker)

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

    def test_docker_build_requires_the_profile5_renderer_used_at_runtime(self):
        dockerfile = read("Dockerfile")
        self.assertIn("-h filter=tonemapx", dockerfile)
        self.assertIn(
            "grep -q '^[[:space:]]*apply_dovi[[:space:]]'",
            dockerfile,
        )
        self.assertNotIn("-h filter=libplacebo", dockerfile)
        self.assertNotIn("apply_dolbyvision", dockerfile)

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
        self.assertIn("deploy/docker-compose.override.yml or deploy/.env", ship)
        self.assertNotIn("environment: block in deploy/docker-compose.yml", ship)

        deploy_readme = read("deploy/README.md")
        self.assertIn("There is no direct-install step for the fleet", deploy_readme)

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
        makefile = read("Makefile")
        self.assertIn("sudo xcodebuild -runFirstLaunch", workflow)
        self.assertEqual(workflow.count("xcrun simctl create"), 2)
        self.assertIn("SimDeviceType.iPhone-16-Pro", workflow)
        self.assertIn("xcrun simctl list devices available -j", workflow)
        self.assertIn('runtime.endswith("iOS-18-5")', workflow)
        self.assertIn('device["name"].startswith("iPad")', workflow)
        self.assertIn('device["udid"]', workflow)
        self.assertIn("SimDeviceType.Apple-TV-4K-3rd-generation-4K", workflow)
        self.assertIn('APPLE_IOS_SIM=platform=iOS Simulator,id=$ios_id', workflow)
        self.assertIn('APPLE_IPAD_SIM=platform=iOS Simulator,id=$ipad_id', workflow)
        self.assertIn('APPLE_TVOS_SIM=platform=tvOS Simulator,id=$tvos_id', workflow)
        self.assertIn('$${APPLE_IPAD_SIM:-}', makefile)
        self.assertLess(workflow.index("xcrun simctl create"), workflow.index("run: make apple-test"))

        # Each platform compiles once and every destination replays those
        # products: one iOS build-for-testing feeds the iPhone AND iPad
        # test-without-building runs, one tvOS build feeds Apple TV. A third
        # `test` (with building) invocation sneaking back in is the regression
        # this pins out.
        apple_target = makefile.split(".PHONY: apple-test", 1)[1].split(".PHONY:", 1)[0]
        self.assertEqual(apple_target.count("build-for-testing"), 2)
        self.assertEqual(apple_target.count("test-without-building"), 3)
        self.assertEqual(apple_target.count("-scheme plurx-iOS"), 3)
        self.assertEqual(apple_target.count("-scheme plurx-tvOS"), 2)
        self.assertNotRegex(
            apple_target, r"CODE_SIGNING_ALLOWED=NO test(?!-without-building)"
        )
        self.assertLess(
            apple_target.index("build-for-testing"),
            apple_target.index("test-without-building"),
        )
        self.assertEqual(apple_target.count('-derivedDataPath "$(APPLE_DERIVED_DATA)"'), 5)
        # The DerivedData cache in ci.yml must point at the same directory the
        # Makefile builds into, or it silently caches nothing.
        self.assertIn("APPLE_DERIVED_DATA := build/DerivedData", makefile)
        self.assertIn("path: clients/apple/build/DerivedData", workflow)

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
        self.assertIn("if: needs.scope.outputs.hiqlite_spike == 'true'", workflow)
        self.assertIn("if: needs.scope.outputs.cluster_auth == 'true'", workflow)
        self.assertIn("name: three-voter replicated store contracts", workflow)
        self.assertIn("if: needs.scope.outputs.rust == 'true'", workflow)
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
        self.assertEqual(
            workflow.count(
                "s|mirror+file:/etc/apt/apt-mirrors.txt|https://archive.ubuntu.com/ubuntu|g",
            ),
            4,
        )
        self.assertEqual(
            workflow.count("sudo apt-get -o Acquire::Retries=3"),
            8,
        )
        self.assertNotIn("sudo apt-get update", workflow)
        self.assertNotIn("sudo apt-get install", workflow)
        self.assertNotIn(
            "cargo build --release --workspace --target ${{ matrix.target }}",
            workflow,
        )

    def test_the_merge_queue_is_the_full_fan_out_and_prs_are_the_fast_lane(self):
        workflow = read(".github/workflows/ci.yml")
        lint = read(".github/workflows/lint.yml")
        makefile = read("Makefile")

        # Both required workflows must fire on merge_group, or enabling the
        # queue deadlocks every merge on a check that never reports.
        self.assertIn("\n  merge_group:\n", workflow)
        self.assertIn("\n  merge_group:\n", lint)
        self.assertIn(
            "if: always() && (github.event_name == 'pull_request' || github.event_name == 'merge_group')",
            workflow,
        )
        # Queue runs must never be cancelled by a later PR push.
        self.assertIn(
            "cancel-in-progress: ${{ github.event_name == 'pull_request' }}",
            workflow,
        )

        # The CI Rust gate splits, not shrinks: clippy stays in lint.yml, and
        # the excluded cluster member keeps its own dedicated job in ci.yml.
        gate = makefile.split(".PHONY: ci-rust-gate", 1)[1].split(".PHONY:", 1)[0]
        self.assertIn("--workspace --locked --exclude plurx-cluster-check", gate)
        self.assertIn("ci-rust-gate: fmt-check", gate)
        self.assertIn("make fmt-check lint", lint)
        self.assertIn("run: make cluster-check", workflow)

    def test_ci_caches_are_keyed_to_what_they_cache(self):
        workflow = read(".github/workflows/ci.yml")

        # The Playwright pip pin and the browser-bundle cache key must move
        # together, or a version bump silently reuses the wrong browsers.
        pip_pin = re.search(r"playwright==(\d+\.\d+\.\d+)", workflow)
        cache_pin = re.search(r"playwright-\$\{\{ runner\.os \}\}-(\d+\.\d+\.\d+)", workflow)
        self.assertIsNotNone(pip_pin)
        self.assertIsNotNone(cache_pin)
        self.assertEqual(pip_pin.group(1), cache_pin.group(1))

        # Both Android jobs reuse the GHCR toolchain image keyed on the
        # Dockerfile hash, and the Makefile honors the pre-pull instead of
        # rebuilding the SDK image from scratch.
        self.assertEqual(
            workflow.count("sha256sum clients/android/Dockerfile"), 2
        )
        self.assertEqual(workflow.count("PLURX_ANDROID_IMAGE_READY=1"), 4)
        makefile = read("Makefile")
        self.assertIn('if [ "$${PLURX_ANDROID_IMAGE_READY:-}" = "1" ]', makefile)

        # The emulator restores a cached AVD snapshot and never saves over it.
        self.assertIn("key: avd-35-google_apis-pixel_7_pro", workflow)
        self.assertIn("-no-snapshot-save", workflow)

        # The spike workspace's target dir must be inside its cache mapping,
        # or its 400-crate build runs cold every time.
        self.assertIn("spikes/hiqlite-m0 -> spikes/hiqlite-m0/target", workflow)

        # The docker smoke build keeps the GHA layer cache wired so the
        # ffmpeg runtime layers stop re-downloading on every run.
        docker = workflow.split("  docker:", 1)[1].split("\n  pr_gate:", 1)[0]
        self.assertIn("cache-from: type=gha", docker)
        self.assertIn("cache-to: type=gha,mode=min", docker)

    def test_release_registry_and_weekly_readiness_match_ci(self):
        ci = read(".github/workflows/ci.yml")
        publisher = read(".github/workflows/publish-release.yml")
        unraid = read("deploy/unraid-plurx.xml")
        readiness = read(".github/workflows/release-readiness.yml")

        self.assertIn("uses: ./.github/workflows/publish-release.yml", ci)
        self.assertIn("REGISTRY_IMAGE: ghcr.io/${{ github.repository }}", publisher)
        self.assertIn("<Repository>ghcr.io/pjunod/plurx:latest</Repository>", unraid)
        self.assertIn(
            "<Registry>https://github.com/pjunod/plurx/pkgs/container/plurx</Registry>",
            unraid,
        )
        self.assertIn('cron: "41 16 * * 1"', readiness)
        self.assertIn("run: make release-check", readiness)
        self.assertIn("fetch-depth: 0", readiness)

    def test_every_actions_job_has_an_explicit_timeout(self):
        for path in (
            ".github/workflows/ci.yml",
            ".github/workflows/lint.yml",
            ".github/workflows/publish-release.yml",
            ".github/workflows/release-readiness.yml",
            ".github/workflows/rust-audit.yml",
        ):
            with self.subTest(path=path):
                jobs = workflow_job_blocks(path)
                self.assertTrue(jobs, f"{path} has no jobs")
                missing = [
                    name for name, block in jobs.items()
                    if "\n    timeout-minutes:" not in block
                    and "\n    uses:" not in block
                ]
                self.assertEqual([], missing, f"jobs without timeouts in {path}")

    def test_ci_flake_ledger_records_real_job_outcomes_and_durations(self):
        script = ROOT / "scripts/ci-flake-report"
        subprocess.run([str(script), "--help"], check=True, stdout=subprocess.PIPE)
        ledger = json.loads(read("validation/ci-flake-ledger.json"))

        self.assertEqual(ledger["schema"], 1)
        self.assertEqual(ledger["repository"], "pjunod/plurx")
        self.assertEqual(ledger["workflow"], "ci.yml")
        self.assertGreaterEqual(ledger["source"]["completed_runs_returned"], 1)
        self.assertTrue(ledger["jobs"])
        self.assertTrue(ledger["summary"])
        for job in ledger["jobs"]:
            self.assertIsInstance(job["conclusion"], str)
            self.assertIsInstance(job["duration_seconds"], (int, float))
            self.assertGreaterEqual(job["duration_seconds"], 0)
            self.assertIsInstance(job["timestamp_anomaly"], bool)

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

    def test_android_player_renders_the_marker_display_label(self):
        player = read(
            "clients/android/app/src/main/java/tv/plurx/app/player/PlayerScreen.kt"
        )

        self.assertIn("Text(activeMarker.displayLabel", player)


if __name__ == "__main__":
    unittest.main()
