# Vendored s3-simple 0.8.0

This directory is the crates.io `s3-simple` 0.8.0 package, licensed under
Apache-2.0. Plurx carries one dependency-only patch: `quick-xml` is raised
from 0.39 to 0.41 because RustSec marks 0.39.4 as vulnerable.

Hiqlite 0.14 enables cryptr's S3 feature even when plurx builds hiqlite with
only `macros` and `sqlite`. That makes this otherwise unused package part of
the production resolution. Keeping the source unchanged and patching the
compatible XML parser constraint avoids a RustSec exception while preserving
the upstream API. Remove this vendor when upstream hiqlite stops enabling the
unused path or s3-simple publishes a fixed release.

Source: <https://crates.io/crates/s3-simple/0.8.0>

The original `LICENSE` and `README.md` are retained beside this note.
