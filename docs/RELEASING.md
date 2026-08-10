# Versioning and releases

## What the numbers mean

plurx carries **one version for the whole workspace**. `plurx-core`,
`plurx-compat-plex`, and `plurxd` are not published to crates.io and are never
useful apart from each other, so a single number is what "a plurx release"
means. It lives in exactly one place:

```toml
# Cargo.toml
[workspace.package]
version = "0.2.7"
```

Every crate inherits it with `version.workspace = true`. The native apps keep
their store-facing marketing versions aligned with the workspace separately;
their monotonically increasing build counters advance whenever their release
paths change, and CI enforces that contract.

plurx follows [semantic versioning](https://semver.org/) under the 0.x rules,
which are worth stating plainly because they are not what 1.x users expect:

| Change | While `0.x` | After `1.0` |
| --- | --- | --- |
| Breaking API, config, or on-disk format change | **minor** — `0.1.0` → `0.2.0` | major |
| New feature, backwards compatible | **minor** — `0.1.0` → `0.2.0` | minor |
| Bug fix, no interface change | **patch** — `0.1.0` → `0.1.1` | patch |

So under 0.x a minor bump means "something may have moved" and a patch bump
means "nothing moved." That is the promise a self-hosted user actually needs:
it tells them whether `docker pull` is safe to do unattended.

The compatibility surface this covers is the HTTP API under `/api/v1`, the Plex
façade, the config file schema, and the SQLite schema. The web app ships inside
the binary and is versioned with it.

**1.0 is not a maturity badge, it's a promise.** It happens when the API and
on-disk format are stable enough to commit to not breaking them — realistically
once the HA cluster work (Phase 4) is wired up and the schema has settled.

## The build stamp

A version number cannot distinguish a tagged release from the forty commits
after it, which is exactly the situation most bug reports come from. So the
binary carries two strings:

- `version` — bare semver, e.g. `0.1.0`. Parseable; clients compare it.
- `build` — `git describe --tags --always --dirty`, e.g. `v0.1.0-14-gc0ffee`
  or `v0.1.0-14-gc0ffee-dirty`. Identifies the exact commit.

Both appear in `plurxd --version`, in the startup log, in `GET /api/v1/server`
and `GET /api/v1/system`, on the `plurx_build_info` metric, and in Settings →
Server (the build stamp is hidden there when it says nothing the version
doesn't).

`crates/plurxd/build.rs` produces the stamp and is not allowed to fail a build.
Without a git checkout — a source tarball, or the Docker context, which excludes
`.git` — it falls back to `unknown` unless `PLURX_BUILD_REF` is set:

```sh
docker build --build-arg PLURX_BUILD_REF="$(git describe --tags --always --dirty)" -t plurx/plurxd .
```

CI passes the tag name automatically when it publishes an image.

## Cutting a release

1. **Decide the number** using the table above.

2. **Update `Cargo.toml`**, then `cargo build` so `Cargo.lock` picks up the new
   version (it records workspace members).

3. **Move `CHANGELOG.md`'s `Unreleased` section into a dated release heading**
   and add the two link definitions at the bottom. Write entries for people
   running the server, not for people reading the diff.

4. **Run the gates.** `scripts/validate run --profile ci --all --strict` is the
   exact all-points CI contract: catalog, fmt, clippy, tests, embedded
   JavaScript, theme contrast, and Android unit/lint. Then run
   `make validate-full` for browser and container checks available on this
   machine; every unavailable check is recorded as a skip rather than disguised
   as a pass.

5. **Merge the release commit, then tag that exact green commit.** Open a pull
   request for the version and changelog changes, wait for its required checks,
   merge it, update local `main`, and run `make release-check` once more before
   creating the tag.

   ```sh
   git tag -a v0.2.7 -m "v0.2.7"
   git push origin v0.2.7
   ```

   The tag is `v` + the version. CI refuses to publish a tag that disagrees
   with `Cargo.toml`, so a mismatch fails before anything reaches a registry
   rather than after.

6. **CI does the rest.** A `v*` tag runs the full gate, then calls the same
   publication workflow used for recovery. That workflow peels the annotated
   tag once, builds x86-64 and aarch64 binaries inside the pinned Bookworm
   toolchain, and stamps both with the validated tag. It packages and smoke
   tests each platform by digest before assigning the GHCR tags `{version}`,
   `{major}.{minor}`, and `latest`. Pushes to `main` build but do not publish,
   so releases are always deliberate.

### Recovering a cancelled publication without moving the tag

A cancelled image build does not justify retagging a different commit. Run the
manual workflow from current `main` and pass the existing annotated tag; the
workflow resolves source and runtime files from that immutable tag rather than
from the branch that supplied the repaired workflow.

```bash
started=$(date -u +%Y-%m-%dT%H:%M:%SZ)   # bound the run lookup to this command
gh workflow run publish-release.yml --ref main \
  -f release_tag=v0.2.7                 # rebuild and verify the existing tag
run_id=$(gh run list --workflow publish-release.yml --branch main \
  --event workflow_dispatch --created ">=$started" --user @me --limit 10 \
  --json databaseId,displayTitle \
  --jq 'map(select(.displayTitle == "publish v0.2.7"))[0].databaseId')
                                           # capture this named dispatch only
gh run watch "$run_id" --exit-status     # require that exact run to succeed
image=ghcr.io/pjunod/plurx
version_digest=$(docker buildx imagetools inspect "$image:0.2.7" \
  | sed -n 's/^Digest:[[:space:]]*//p' | head -1)
for alias in 0.2.7 0.2 latest; do
  test "$(docker buildx imagetools inspect "$image:$alias" \
    | sed -n 's/^Digest:[[:space:]]*//p' | head -1)" = "$version_digest"
  test "$(docker buildx imagetools inspect --raw "$image:$alias" \
    | jq -r '.manifests[].platform | "\(.os)/\(.architecture)"' | sort)" \
    = "$(printf 'linux/amd64\nlinux/arm64')"
done
```

The workflow refuses lightweight tags, version or changelog mismatches, and a
remote tag that moves after source resolution. It pushes architecture images
without human-facing tags, smoke tests them, then creates the immutable version
index and its moving aliases. If the version index already exists, both of its
architecture bindings must pass the same source-label, version, and container
smoke checks before the workflow reuses it. `0.2` and `latest` move only when
the recovered version is not older than their verified current target, so a
late recovery cannot roll clients backward.

The successful workflow is the acceptance record: both platform jobs must
report `plurxd 0.2.7 (v0.2.7)`, both image configs must name the tag's peeled
source commit, and container smoke must pass on both. For a first publication,
`0.2.7`, `0.2`, and `latest` must resolve to the same two-platform index. For a
recovery after a newer release, the immutable `0.2.7` index must pass while the
newer moving aliases remain unchanged. Never delete or move the release tag to
make recovery pass; a mismatch is an incident to investigate, not an alias to
overwrite.

The weekly release-readiness workflow runs `make release-check`. A red run
means the current workspace version is already tagged or has no dated
changelog section; it is a visible prompt to prepare the next release, not a
reason to move or overwrite an existing tag.

## Checking what a build reports

```sh
make version          # what a build from this tree would stamp
plurxd --version      # what an existing binary reports
curl -s localhost:32400/api/v1/server | jq '{version, build}'
```
