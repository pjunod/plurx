# Historical regression coverage

A corrective commit normally proves itself by adding or changing a test in the
same patch. The entries in this directory cover the exceptions: the defect is
exercised by a current higher-level check, or the commit only corrected a
document and therefore has no runtime behavior to validate.

`scripts/history-audit` loads every `*.toml` file here, in file-name order, and
treats them as one ledger. Entry order carries no meaning.

## One entry per file

Each file holds exactly one `[[coverage]]` entry and is named after the first
commit it maps:

```
validation/regressions.d/<first-commit-prefix>-<slug>.toml
```

The audit rejects a file whose name does not start with its own first mapped
commit. Two corrective changes therefore cannot pick the same path, so adding a
mapping never touches a line another branch is also touching — the append
conflict that used to hit every open pull request on every merge to `main` is
structurally impossible rather than merely rare. Never reintroduce a shared
`validation/regressions.toml`; the loader refuses to run while one exists.

## Format

```toml
version = 1

[[coverage]]
commits = ["a1b2c3d4"]
points = ["playback.pipeline"]
checks = ["rust-gate"]
reason = "One sentence naming the current check that exercises the defect."
```

- `commits` — Git SHA prefixes (7–40 lowercase hex) of the corrective commits.
- `points` — functionality point IDs from `validation/points.toml`. Must
  intersect the points the commit's own paths resolve to.
- `checks` — check IDs that are real evidence for those points.
- `reason` — why the named checks exercise the defect. Required.
- `ignore` — `true` for a correction with no runtime behavior, such as a
  documentation-only fix. An ignored entry claims no points and no checks.

Unknown points, unknown checks, a check that is not evidence for its listed
points, a commit covered by two entries, and an unknown key all fail the audit.
The mapping is not an exemption: a corrective commit with neither a test nor a
mapping still fails.
