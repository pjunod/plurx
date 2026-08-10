from __future__ import annotations

from pathlib import Path
import unittest

from validation.release_aliases import alias_action
from validation.release_dockerfile import render


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github/workflows/publish-release.yml"


class ReleasePublicationContractCase(unittest.TestCase):
    def test_release_source_is_immutable_and_bookworm_compatible(self):
        workflow = WORKFLOW.read_text(encoding="utf-8")

        self.assertIn("workflow_call:", workflow)
        self.assertIn("workflow_dispatch:", workflow)
        self.assertIn('^v[0-9]+\\.[0-9]+\\.[0-9]+$', workflow)
        self.assertIn('git cat-file -t "refs/tags/$RELEASE_TAG"', workflow)
        self.assertIn('refs/tags/$RELEASE_TAG^{commit}', workflow)
        self.assertIn('EVENT_REF" != refs/heads/main', workflow)
        self.assertIn("ref: ${{ github.sha }}", workflow)
        self.assertIn("ref: ${{ needs.resolve.outputs.packaging_sha }}", workflow)
        self.assertIn("container: rust:1.97.1-bookworm", workflow)
        binary = workflow.split("\n  binary:\n", 1)[1].split("\n  image:\n", 1)[0]
        self.assertIn("defaults:\n      run:\n        shell: bash", binary)
        self.assertIn("cargo build --locked --release -p plurxd", workflow)
        self.assertIn(
            "PLURX_BUILD_REF: ${{ needs.resolve.outputs.release_tag }}",
            workflow,
        )
        self.assertIn("GLIBC_$max_glibc; Bookworm provides 2.36", workflow)

    def test_aliases_wait_for_both_smoked_platform_digests(self):
        workflow = WORKFLOW.read_text(encoding="utf-8")
        image = workflow.split("\n  image:\n", 1)[1].split("\n  publish:\n", 1)[0]
        publish = workflow.split("\n  publish:\n", 1)[1]

        self.assertIn("push-by-digest=true", image)
        self.assertNotIn("cache-from:", image)
        self.assertNotIn("cache-to:", image)
        self.assertLess(
            image.index("Verify the pushed platform image"),
            image.index("release-image-digest-${{ matrix.arch }}"),
        )
        self.assertIn("needs: [resolve, image, reuse]", publish)
        self.assertIn("Reconfirm the remote tag has not moved", publish)
        self.assertIn("appeared after source resolution", publish)
        self.assertIn("registry state is indeterminate", publish)
        self.assertIn('grep -Fq "$ref" "$err"', workflow)
        self.assertNotIn("manifest unknown|name unknown|not found", workflow)
        self.assertIn("group: publish-release-aliases", publish)
        self.assertIn("python3 -m validation.release_aliases", publish)
        self.assertIn("verified-release-index-*", publish)
        self.assertIn('test "$candidate_digest" = "$verified_amd64"', publish)
        self.assertIn('test "$published_amd64" = "$(cat digests/amd64)"', publish)
        self.assertIn('test "$published_arm64" = "$(cat digests/arm64)"', publish)
        self.assertIn('source_ref="$REGISTRY_IMAGE@$candidate_digest"', publish)
        self.assertIn("for arch in amd64 arm64", publish)
        self.assertIn("{{.Os}}/{{.Architecture}}", publish)
        self.assertIn("org.opencontainers.image.source", publish)
        self.assertIn('scripts/container-smoke "$image_ref" >&2', publish)
        self.assertIn('test "$alias_digest" = "$immutable_digest"', publish)
        self.assertIn('remote_commit=$(printf', publish)
        self.assertIn('test "$remote_commit" = "$revision"', publish)
        self.assertNotIn("=$(alias_version", publish)
        self.assertIn(
            'alias_version "$minor_ref" minor.json minor_existing minor_before',
            publish,
        )
        self.assertIn("timeout-minutes: 30", publish)
        self.assertNotIn(
            '.manifests[].platform | select(.os == "linux")',
            workflow,
        )
        self.assertIn("if [ \"$minor_action\" = keep ]", publish)
        self.assertIn("if [ \"$latest_action\" = keep ]", publish)
        self.assertEqual(workflow.count("packages: write"), 2)

        reuse = workflow.split("\n  reuse:\n", 1)[1].split("\n  publish:\n", 1)[0]
        self.assertIn("version_exists == 'true'", reuse)
        self.assertIn("platform.architecture == $arch", reuse)
        self.assertIn("verified-release-index-${{ matrix.arch }}", reuse)
        self.assertIn("{{.Os}}/{{.Architecture}}", reuse)
        self.assertIn('verified_ref="$REGISTRY_IMAGE@$index_digest"', reuse)
        self.assertIn('test "$current_digest" = "$index_digest"', reuse)
        self.assertIn("scripts/container-smoke", reuse)

    def test_release_aliases_advance_monotonically(self):
        self.assertEqual(alias_action("minor", "0.2.7", None), "advance")
        self.assertEqual(alias_action("minor", "0.2.7", "0.2.7"), "advance")
        self.assertEqual(alias_action("minor", "0.2.7", "0.2.8"), "keep")
        self.assertEqual(alias_action("latest", "0.2.7", "0.3.0"), "keep")
        self.assertEqual(alias_action("latest", "0.3.0", "0.2.9"), "advance")
        with self.assertRaisesRegex(ValueError, "unrelated version"):
            alias_action("minor", "0.2.7", "0.3.0")
        with self.assertRaisesRegex(ValueError, "invalid release version"):
            alias_action("latest", "0.2.7", "main")

    def test_generated_dockerfile_keeps_only_the_tagged_runtime(self):
        source = """# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS build
RUN cargo build --release && cp target/release/plurxd /plurxd
FROM debian:bookworm-slim
RUN apt-get update
COPY --from=build /plurxd /usr/local/bin/plurxd
ENTRYPOINT [\"plurxd\"]
"""

        generated = render(source)

        self.assertIn("FROM debian:bookworm-slim", generated)
        self.assertIn(
            "COPY --chmod=0755 release-bin/plurxd /usr/local/bin/plurxd",
            generated,
        )
        self.assertNotIn("FROM rust:", generated)
        self.assertNotIn("cargo build", generated)
        self.assertNotIn("COPY --from=build", generated)

    def test_generator_refuses_an_unrecognized_runtime_contract(self):
        with self.assertRaisesRegex(ValueError, "one Bookworm runtime stage"):
            render("FROM alpine:3.22\n")


if __name__ == "__main__":
    unittest.main()
