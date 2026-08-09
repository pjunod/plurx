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

6. **CI does the rest.** A `v*` tag runs the full gate, builds x86-64 and
   aarch64 binaries, and publishes a multi-arch image to GHCR tagged
   `{version}`, `{major}.{minor}`, and `latest`. Pushes to `main` build but do
   not publish, so releases are always deliberate.

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
