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

/// Errors from media inspection.
#[derive(Debug, Error)]
pub enum ProbeError {
    #[error("could not run ffprobe: {0}")]
    Spawn(String),

    #[error("ffprobe failed for {path} (exit {code:?})")]
    Failed { path: String, code: Option<i32> },

    #[error("could not parse ffprobe output: {0}")]
    Parse(String),
}

/// Errors from password hashing / token generation.
#[derive(Debug, Error)]
pub enum AuthError {
    #[error("random source failed: {0}")]
    Rng(String),

    #[error("hashing failed: {0}")]
    Hash(String),
}

/// Errors from metadata providers.
#[derive(Debug, Error)]
pub enum MetadataError {
    #[error("http error: {0}")]
    Http(String),

    #[error("provider returned status {0}")]
    Status(u16),

    #[error("could not parse provider response: {0}")]
    Parse(String),

    #[error("no metadata provider configured")]
    NotConfigured,
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
