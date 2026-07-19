//! The storage boundary.
//!
//! Everything that must survive a node (and later, replicate across the
//! cluster) goes through [`Store`]. Phase 0–2: [`SqliteStore`] on local disk.
//! Phase 3 spike / Phase 4: a raft-replicated backend implements this same
//! trait, and single-node mode becomes a 1-voter cluster — same code path.
//! See `docs/ARCHITECTURE.md` §2 and `docs/ROADMAP.md` Phase 3.

mod sqlite;

pub use sqlite::SqliteStore;

use async_trait::async_trait;

use crate::error::StoreError;

/// Well-known settings keys. Keys are dotted, lowercase, and owned by the
/// module that writes them.
pub mod keys {
    /// Stable unique id for this logical server. Generated on first startup,
    /// immutable thereafter; in a cluster it identifies the *cluster*, not a
    /// node (REQ-HA-5: one logical identity).
    pub const INSTANCE_ID: &str = "instance.id";
}

/// Abstract store for replicated durable state.
///
/// Contract notes for future backends:
/// - All operations are linearizable from the caller's perspective.
/// - `put_setting` acknowledged ⇒ durable (on a cluster: quorum-acked).
/// - Implementations must be cheap to clone via `Arc<dyn Store>` sharing.
#[async_trait]
pub trait Store: Send + Sync + 'static {
    /// Cheap liveness probe of the backing storage (drives `/readyz`).
    async fn ping(&self) -> Result<(), StoreError>;

    /// Read a server setting.
    async fn get_setting(&self, key: &str) -> Result<Option<String>, StoreError>;

    /// Write a server setting (upsert).
    async fn put_setting(&self, key: &str, value: &str) -> Result<(), StoreError>;

    /// The stable unique id of this logical server.
    async fn instance_id(&self) -> Result<String, StoreError>;
}
