//! Configuration: sane defaults, one optional TOML file, env overrides.
//! (REQ-OPS-2: defaults + file + env; settings edited at runtime live in the
//! Store, not here — this file covers only what's needed before the Store opens.)

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::ConfigError;

/// Default HTTP port. Deliberately near — but never colliding with — the
/// 32400-era ports ex-Plex users already have muscle memory for.
pub const DEFAULT_PORT: u16 = 32400;
/// Default Raft replication port, adjacent to the public HTTP API.
pub const DEFAULT_RAFT_PORT: u16 = 32401;
/// Default authenticated node-to-node API port.
pub const DEFAULT_CLUSTER_API_PORT: u16 = 32402;

const DEFAULT_CONFIG_PATHS: &[&str] = &["plurx.toml", "/etc/plurx/plurx.toml"];

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    /// Forward-compatible cluster settings. M0 reads these without changing
    /// the production SQLite backend; later clustering releases activate them.
    pub cluster: ClusterConfig,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Human-visible server name (cluster-wide identity comes later; REQ-HA-5).
    pub name: String,
    /// Address the HTTP API binds to.
    pub bind: SocketAddr,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            name: "plurx".to_owned(),
            bind: SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT)),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    /// Directory for the database and caches. Created if missing.
    pub data_dir: PathBuf,
}

impl Default for StorageConfig {
    fn default() -> Self {
        StorageConfig {
            data_dir: PathBuf::from("./data"),
        }
    }
}

/// Configuration reserved for the embedded cluster backend.
///
/// Unlike the surrounding config sections this intentionally tolerates
/// unknown keys. An M0 binary must be able to read a config written by a later
/// clustering release during rollback, while typos in the existing server and
/// storage sections must continue to fail closed.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct ClusterConfig {
    /// Raft replication listener. It is not part of the public HTTP API.
    pub raft_bind: SocketAddr,
    /// Authenticated node-to-node request listener.
    pub api_bind: SocketAddr,
    /// Reachable address advertised to peers; empty means derive it locally.
    pub advertise_host: String,
    /// Single-use join-token file; empty means bootstrap/reopen one voter.
    pub join_token_file: PathBuf,
    /// Required network boundary when inter-node transport is not using TLS.
    pub trusted_network: String,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            raft_bind: SocketAddr::from(([0, 0, 0, 0], DEFAULT_RAFT_PORT)),
            api_bind: SocketAddr::from(([0, 0, 0, 0], DEFAULT_CLUSTER_API_PORT)),
            advertise_host: String::new(),
            join_token_file: PathBuf::new(),
            trusted_network: String::new(),
        }
    }
}

impl Config {
    /// Load configuration.
    ///
    /// Precedence (lowest → highest): built-in defaults, TOML file, `PLURX_*`
    /// env vars. An explicitly given path must exist; the default locations
    /// (`./plurx.toml`, `/etc/plurx/plurx.toml`) are used only if present.
    pub fn load(explicit_path: Option<&Path>) -> Result<Config, ConfigError> {
        let mut config = match explicit_path {
            Some(path) => Self::from_file(path)?,
            None => match DEFAULT_CONFIG_PATHS
                .iter()
                .map(Path::new)
                .find(|p| p.is_file())
            {
                Some(path) => Self::from_file(path)?,
                None => Config::default(),
            },
        };
        config.apply_env()?;
        Ok(config)
    }

    fn from_file(path: &Path) -> Result<Config, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source: Box::new(source),
        })
    }

    fn apply_env(&mut self) -> Result<(), ConfigError> {
        if let Some(name) = env_var("PLURX_SERVER_NAME") {
            self.server.name = name;
        }
        if let Some(bind) = env_var("PLURX_BIND") {
            self.server.bind = bind.parse().map_err(|_| ConfigError::Env {
                var: "PLURX_BIND".to_owned(),
                message: format!("`{bind}` is not a socket address (e.g. 0.0.0.0:{DEFAULT_PORT})"),
            })?;
        }
        if let Some(dir) = env_var("PLURX_DATA_DIR") {
            self.storage.data_dir = PathBuf::from(dir);
        }
        Ok(())
    }
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let config = Config::default();
        assert_eq!(config.server.bind.port(), DEFAULT_PORT);
        assert_eq!(config.server.name, "plurx");
        assert_eq!(config.storage.data_dir, PathBuf::from("./data"));
        assert_eq!(config.cluster.raft_bind.port(), DEFAULT_RAFT_PORT);
        assert_eq!(config.cluster.api_bind.port(), DEFAULT_CLUSTER_API_PORT);
    }

    #[test]
    fn file_overrides_defaults_and_rejects_unknown_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plurx.toml");

        std::fs::write(
            &path,
            "[server]\nname = \"den\"\nbind = \"127.0.0.1:9999\"\n",
        )
        .expect("write config");
        let config = Config::load(Some(&path)).expect("load");
        assert_eq!(config.server.name, "den");
        assert_eq!(config.server.bind.port(), 9999);
        // Unspecified sections keep defaults.
        assert_eq!(config.storage.data_dir, PathBuf::from("./data"));

        std::fs::write(&path, "[server]\nnmae = \"typo\"\n").expect("write config");
        assert!(matches!(
            Config::load(Some(&path)),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn cluster_section_tolerates_future_keys_without_weakening_existing_sections() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plurx.toml");

        std::fs::write(
            &path,
            "[cluster]\nraft_bind = \"127.0.0.1:42401\"\nsome_future_key = 1\n",
        )
        .expect("write future cluster config");
        let config = Config::load(Some(&path)).expect("future cluster key is tolerated");
        assert_eq!(config.cluster.raft_bind.port(), 42401);

        std::fs::write(
            &path,
            "[cluster]\nsome_future_key = 1\n[server]\nnmae = \"typo\"\n",
        )
        .expect("write strict server config");
        assert!(matches!(
            Config::load(Some(&path)),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn explicit_missing_path_errors() {
        assert!(matches!(
            Config::load(Some(Path::new("/nonexistent/plurx.toml"))),
            Err(ConfigError::Read { .. })
        ));
    }
}
