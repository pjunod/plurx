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

/// Why an existing SQLite transaction cannot be copied into hiqlite's fixed
/// statement-list `txn()` API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransactionShape {
    BatchWrite,
    CompareAndSwap,
    ReadBranchWrite,
    ReadExpandWrite,
    WriteThenConditionalCleanup,
    WriteUntilStable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InteractiveTransaction {
    pub method: &'static str,
    pub shape: TransactionShape,
}

/// Exhaustive M1a inventory of current interactive SQLite transactions.
///
/// Keeping this as code makes the redesign list reviewable beside the CAS
/// primitive. The parity contract also pins the source-site count so a new
/// `unchecked_transaction()` cannot arrive without being classified here.
pub const INTERACTIVE_TRANSACTIONS: &[InteractiveTransaction] = &[
    InteractiveTransaction {
        method: "set_watched_tree",
        shape: TransactionShape::ReadExpandWrite,
    },
    InteractiveTransaction {
        method: "delete_files",
        shape: TransactionShape::BatchWrite,
    },
    InteractiveTransaction {
        method: "prune_empty_items",
        shape: TransactionShape::WriteUntilStable,
    },
    InteractiveTransaction {
        method: "claim_cache_entry",
        shape: TransactionShape::CompareAndSwap,
    },
    InteractiveTransaction {
        method: "forget_cache_entry",
        shape: TransactionShape::WriteThenConditionalCleanup,
    },
    InteractiveTransaction {
        method: "create_offline_package",
        shape: TransactionShape::ReadBranchWrite,
    },
    InteractiveTransaction {
        method: "renew_offline_package_for_user",
        shape: TransactionShape::ReadBranchWrite,
    },
    InteractiveTransaction {
        method: "claim_next_offline_package",
        shape: TransactionShape::ReadBranchWrite,
    },
    InteractiveTransaction {
        method: "put_offline_lease",
        shape: TransactionShape::ReadBranchWrite,
    },
    InteractiveTransaction {
        method: "offline_package_for_lease",
        shape: TransactionShape::ReadBranchWrite,
    },
];

const FORBIDDEN_IDENTIFIERS: &[&str] = &[
    "current_date",
    "current_time",
    "current_timestamp",
    "last_insert_rowid",
    "random",
    "randomblob",
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
    use super::*;

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
    fn every_interactive_transaction_is_named_once() {
        let mut methods = INTERACTIVE_TRANSACTIONS
            .iter()
            .map(|transaction| transaction.method)
            .collect::<Vec<_>>();
        let original_len = methods.len();
        methods.sort_unstable();
        methods.dedup();
        assert_eq!(methods.len(), original_len);
        assert_eq!(methods.len(), 10);
    }

    #[test]
    fn every_sqlite_transaction_site_is_classified() {
        let sqlite_sources = concat!(
            include_str!("sqlite/watch.rs"),
            include_str!("sqlite/media.rs"),
            include_str!("sqlite/cache.rs"),
            include_str!("sqlite/offline.rs"),
        );

        assert_eq!(
            sqlite_sources.matches("unchecked_transaction()").count(),
            INTERACTIVE_TRANSACTIONS.len(),
            "classify every interactive transaction before adding or removing a site",
        );
        for transaction in INTERACTIVE_TRANSACTIONS {
            assert!(
                sqlite_sources.contains(&format!("async fn {}", transaction.method)),
                "{} is not a current SQLite transaction method",
                transaction.method,
            );
        }
    }
}
