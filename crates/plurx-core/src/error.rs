use std::path::PathBuf;

use thiserror::Error;

/// Errors from the storage layer.
///
/// Deliberately backend-agnostic: callers must not be able to tell whether
/// SQLite or a raft cluster produced the failure.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(String),

    #[error("schema migration failed: {0}")]
    Migration(String),

    #[error("storage task failed: {0}")]
    Task(String),
}

impl From<rusqlite::Error> for StoreError {
    fn from(err: rusqlite::Error) -> Self {
        StoreError::Database(err.to_string())
    }
}

/// Errors while loading configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("cannot read config file {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("invalid config file {path}: {source}")]
    Parse {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },

    #[error("invalid value in ${var}: {message}")]
    Env { var: String, message: String },
}
