//! Guardrails shared by every replicated-store slice.
//!
//! hiqlite replays SQL and bound parameters on each voter. Values that depend
//! on the executing connection, wall clock, or random generator therefore do
//! not belong in that dialect: the leader computes them once and binds them.

use std::fmt;

/// SQL whose replay cannot depend on a voter's clock, RNG, or connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplicatedSql<'a>(&'a str);

impl<'a> ReplicatedSql<'a> {
    /// Validate one statement before it can reach a replicated writer.
    pub fn new(sql: &'a str) -> Result<Self, ReplicatedSqlError> {
        if let Some(identifier) = first_forbidden_identifier(sql) {
            return Err(ReplicatedSqlError { identifier });
        }
        Ok(Self(sql))
    }

    pub fn as_str(self) -> &'a str {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplicatedSqlError {
    identifier: String,
}

impl ReplicatedSqlError {
    pub fn identifier(&self) -> &str {
        &self.identifier
    }
}

impl fmt::Display for ReplicatedSqlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "replicated SQL must bind leader-computed values; `{}` is forbidden",
            self.identifier
        )
    }
}

impl std::error::Error for ReplicatedSqlError {}

/// The only two valid outcomes of a single-row compare-and-swap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CasOutcome {
    Applied,
    Stale,
}

/// Interpret the affected-row count returned by the guarded write.
///
/// A stale expected value is not a transport error: hiqlite returns `Ok(0)`.
/// More than one changed row means the purported single-row fence is broken.
pub fn classify_cas(rows_affected: u64) -> Result<CasOutcome, CasCardinalityError> {
    match rows_affected {
        0 => Ok(CasOutcome::Stale),
        1 => Ok(CasOutcome::Applied),
        actual => Err(CasCardinalityError { actual }),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CasCardinalityError {
    actual: u64,
}

impl CasCardinalityError {
    pub fn actual(self) -> u64 {
        self.actual
    }
}

impl fmt::Display for CasCardinalityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "single-row compare-and-swap changed {} rows",
            self.actual
        )
    }
}

impl std::error::Error for CasCardinalityError {}

/// Shape of one explicit production transaction boundary in the SQLite
/// backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransactionShape {
    BatchWrite,
    BranchOnRowsAffected,
    ReadBranchWrite,
    ReadExpandWrite,
    VerbatimBatch,
    WriteUntilStable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TransactionMechanism {
    RawBeginBatch,
    RusqliteTransaction,
}

impl TransactionMechanism {
    #[cfg(test)]
    fn source_marker(self) -> &'static str {
        match self {
            Self::RawBeginBatch => "BEGIN;",
            Self::RusqliteTransaction => "unchecked_transaction",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SqliteTransactionSite {
    pub module: &'static str,
    pub method: &'static str,
    pub is_async: bool,
    pub mechanism: TransactionMechanism,
    pub shape: TransactionShape,
}

/// Exhaustive M1a inventory of explicit production SQLite transactions.
///
/// This is not the whole replicated-port work list. Untransacted
/// read-branch-write flows, `RETURNING`, connection-local generated ids, and
/// Rust-driven backfills remain separate audit populations. Keeping explicit
/// boundaries here makes their port shape reviewable beside the CAS primitive.
pub const SQLITE_TRANSACTION_SITES: &[SqliteTransactionSite] = &[
    SqliteTransactionSite {
        module: "watch.rs",
        method: "set_watched_tree",
        is_async: true,
        mechanism: TransactionMechanism::RusqliteTransaction,
        shape: TransactionShape::ReadExpandWrite,
    },
    SqliteTransactionSite {
        module: "media.rs",
        method: "delete_files",
        is_async: true,
        mechanism: TransactionMechanism::RusqliteTransaction,
        shape: TransactionShape::BatchWrite,
    },
    SqliteTransactionSite {
        module: "media.rs",
        method: "reconcile_library",
        is_async: true,
        mechanism: TransactionMechanism::RusqliteTransaction,
        shape: TransactionShape::ReadBranchWrite,
    },
    SqliteTransactionSite {
        module: "media.rs",
        method: "prune_empty_items",
        is_async: true,
        mechanism: TransactionMechanism::RusqliteTransaction,
        shape: TransactionShape::WriteUntilStable,
    },
    SqliteTransactionSite {
        module: "cache.rs",
        method: "claim_cache_entry",
        is_async: true,
        mechanism: TransactionMechanism::RusqliteTransaction,
        shape: TransactionShape::VerbatimBatch,
    },
    SqliteTransactionSite {
        module: "cache.rs",
        method: "forget_cache_entry",
        is_async: true,
        mechanism: TransactionMechanism::RusqliteTransaction,
        shape: TransactionShape::VerbatimBatch,
    },
    SqliteTransactionSite {
        module: "offline.rs",
        method: "create_offline_package",
        is_async: true,
        mechanism: TransactionMechanism::RusqliteTransaction,
        shape: TransactionShape::ReadBranchWrite,
    },
    SqliteTransactionSite {
        module: "offline.rs",
        method: "renew_offline_package_for_user",
        is_async: true,
        mechanism: TransactionMechanism::RusqliteTransaction,
        shape: TransactionShape::BranchOnRowsAffected,
    },
    SqliteTransactionSite {
        module: "offline.rs",
        method: "claim_next_offline_package",
        is_async: true,
        mechanism: TransactionMechanism::RusqliteTransaction,
        shape: TransactionShape::ReadBranchWrite,
    },
    SqliteTransactionSite {
        module: "offline.rs",
        method: "put_offline_lease",
        is_async: true,
        mechanism: TransactionMechanism::RusqliteTransaction,
        shape: TransactionShape::ReadBranchWrite,
    },
    SqliteTransactionSite {
        module: "offline.rs",
        method: "offline_package_for_lease",
        is_async: true,
        mechanism: TransactionMechanism::RusqliteTransaction,
        shape: TransactionShape::ReadBranchWrite,
    },
    SqliteTransactionSite {
        module: "mod.rs",
        method: "migrate",
        is_async: false,
        mechanism: TransactionMechanism::RawBeginBatch,
        shape: TransactionShape::ReadBranchWrite,
    },
];

/// hiqlite 0.14.0 `store/state_machine/sqlite/state_machine.rs:401-510`
/// registers a panicking scalar-function stub for each name on its writer.
#[cfg(test)]
const HIQLITE_GUARDED_IDENTIFIERS: &[&str] = &[
    "date",
    "datetime",
    "julianday",
    "now",
    "random",
    "randomblob",
    "strftime",
    "time",
    "timediff",
    "unixepoch",
];

const FORBIDDEN_IDENTIFIERS: &[&str] = &[
    "changes",
    "current_date",
    "current_time",
    "current_timestamp",
    "date",
    "datetime",
    "julianday",
    "last_insert_rowid",
    "now",
    "random",
    "randomblob",
    "sqlite_version",
    "strftime",
    "time",
    "timediff",
    "total_changes",
    "unixepoch",
];

fn first_forbidden_identifier(sql: &str) -> Option<String> {
    let bytes = sql.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\'' => index = skip_quoted(bytes, index + 1, b'\''),
            b'"' => index = skip_quoted(bytes, index + 1, b'"'),
            b'`' => index = skip_quoted(bytes, index + 1, b'`'),
            b'[' => index = skip_bracket_identifier(bytes, index + 1),
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                index = skip_line_comment(bytes, index + 2)
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = skip_block_comment(bytes, index + 2)
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                let start = index;
                index += 1;
                while bytes
                    .get(index)
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                {
                    index += 1;
                }
                let identifier = sql[start..index].to_ascii_lowercase();
                if FORBIDDEN_IDENTIFIERS.contains(&identifier.as_str()) {
                    return Some(identifier);
                }
            }
            _ => index += 1,
        }
    }
    None
}

fn skip_quoted(bytes: &[u8], mut index: usize, quote: u8) -> usize {
    while index < bytes.len() {
        if bytes[index] == quote {
            if bytes.get(index + 1) == Some(&quote) {
                index += 2;
            } else {
                return index + 1;
            }
        } else {
            index += 1;
        }
    }
    index
}

fn skip_bracket_identifier(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && bytes[index] != b']' {
        index += 1;
    }
    (index + 1).min(bytes.len())
}

fn skip_line_comment(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && !matches!(bytes[index], b'\n' | b'\r') {
        index += 1;
    }
    index
}

fn skip_block_comment(bytes: &[u8], mut index: usize) -> usize {
    while index + 1 < bytes.len() {
        if bytes[index] == b'*' && bytes[index + 1] == b'/' {
            return index + 2;
        }
        index += 1;
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    use super::*;

    const SQLITE_MODULES: &[(&str, &str)] = &[
        ("apikeys.rs", include_str!("sqlite/apikeys.rs")),
        ("cache.rs", include_str!("sqlite/cache.rs")),
        ("library.rs", include_str!("sqlite/library.rs")),
        ("media.rs", include_str!("sqlite/media.rs")),
        ("mod.rs", include_str!("sqlite/mod.rs")),
        ("offline.rs", include_str!("sqlite/offline.rs")),
        ("outbox.rs", include_str!("sqlite/outbox.rs")),
        ("trakt.rs", include_str!("sqlite/trakt.rs")),
        ("users.rs", include_str!("sqlite/users.rs")),
        ("watch.rs", include_str!("sqlite/watch.rs")),
    ];

    fn production_source(source: &str) -> &str {
        source
            .split_once("\n#[cfg(test)]")
            .map_or(source, |(code, _)| code)
    }

    fn source_for(module: &str) -> &'static str {
        SQLITE_MODULES
            .iter()
            .find_map(|(name, source)| (*name == module).then_some(production_source(source)))
            .unwrap_or_else(|| panic!("unregistered SQLite module {module}"))
    }

    fn method_source(site: &SqliteTransactionSite) -> &'static str {
        let source = source_for(site.module);
        let keyword = if site.is_async { "async fn" } else { "fn" };
        let declaration = format!("    {keyword} {}(", site.method);
        let start = source
            .find(&declaration)
            .unwrap_or_else(|| panic!("missing {declaration} in {}", site.module));
        let tail = &source[start + declaration.len()..];
        let next_async = tail.find("\n    async fn ");
        let next_sync = tail.find("\n    fn ");
        let end = [next_async, next_sync]
            .into_iter()
            .flatten()
            .min()
            .map_or(source.len(), |offset| start + declaration.len() + offset);
        &source[start..end]
    }

    #[test]
    fn replicated_sql_covers_every_hiqlite_panicking_function() {
        let forbidden = FORBIDDEN_IDENTIFIERS
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let missing = HIQLITE_GUARDED_IDENTIFIERS
            .iter()
            .copied()
            .filter(|identifier| !forbidden.contains(identifier))
            .collect::<Vec<_>>();
        assert!(missing.is_empty(), "hiqlite guardrail gaps: {missing:?}");
    }

    #[test]
    fn replicated_sql_requires_leader_bound_values() {
        for identifier in FORBIDDEN_IDENTIFIERS {
            let sql = format!("INSERT INTO facts(value) VALUES ({identifier}())");
            let error = ReplicatedSql::new(&sql).expect_err("forbidden value");
            assert_eq!(error.identifier(), *identifier);
        }

        let error = ReplicatedSql::new("INSERT INTO facts(value) VALUES (CURRENT_TIMESTAMP)")
            .expect_err("unguarded clock keyword");
        assert_eq!(error.identifier(), "current_timestamp");
    }

    #[test]
    fn quoted_data_and_comments_do_not_trigger_the_policy() {
        let sql = ReplicatedSql::new(
            "INSERT INTO facts(name, note) VALUES ($1, 'unixepoch()'); \
             -- last_insert_rowid() documents the old implementation\n\
             UPDATE facts SET \"current_timestamp\" = $2 WHERE name = $1",
        )
        .expect("only executable identifiers are governed");
        assert!(sql.as_str().starts_with("INSERT"));
    }

    #[test]
    fn compare_and_swap_requires_exactly_one_changed_row() {
        assert_eq!(classify_cas(0), Ok(CasOutcome::Stale));
        assert_eq!(classify_cas(1), Ok(CasOutcome::Applied));
        assert_eq!(classify_cas(2).expect_err("too many").actual(), 2);
    }

    #[test]
    fn every_sqlite_transaction_is_named_once() {
        let mut methods = SQLITE_TRANSACTION_SITES
            .iter()
            .map(|site| (site.module, site.method))
            .collect::<Vec<_>>();
        let original_len = methods.len();
        methods.sort_unstable();
        methods.dedup();
        assert_eq!(methods.len(), original_len);
        assert_eq!(methods.len(), 12);
    }

    #[test]
    fn every_sqlite_transaction_site_is_classified() {
        let source_directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/store/sqlite");
        let actual_modules = std::fs::read_dir(source_directory)
            .expect("read SQLite module directory")
            .map(|entry| entry.expect("SQLite module entry").path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
            .map(|path| {
                path.file_name()
                    .expect("SQLite module filename")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<BTreeSet<_>>();
        let registered_modules = SQLITE_MODULES
            .iter()
            .map(|(module, _)| (*module).to_owned())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            actual_modules, registered_modules,
            "register every SQLite module"
        );

        let observed = SQLITE_MODULES
            .iter()
            .flat_map(|(module, source)| {
                let source = production_source(source);
                [
                    (
                        (*module, TransactionMechanism::RusqliteTransaction),
                        source
                            .matches(TransactionMechanism::RusqliteTransaction.source_marker())
                            .count(),
                    ),
                    (
                        (*module, TransactionMechanism::RawBeginBatch),
                        source
                            .matches(TransactionMechanism::RawBeginBatch.source_marker())
                            .count(),
                    ),
                ]
            })
            .filter(|(_, count)| *count > 0)
            .collect::<BTreeMap<_, _>>();
        let mut expected = BTreeMap::new();
        for site in SQLITE_TRANSACTION_SITES {
            *expected.entry((site.module, site.mechanism)).or_insert(0) += 1;
            assert_eq!(
                method_source(site)
                    .matches(site.mechanism.source_marker())
                    .count(),
                1,
                "{}.{} must own exactly one classified transaction boundary",
                site.module,
                site.method,
            );
        }
        assert_eq!(
            observed, expected,
            "classify every production transaction boundary"
        );
    }
}
