//! The storage boundary.
//!
//! Everything that must survive a node (and later, replicate across the
//! cluster) goes through the [`Store`] trait family. Phase 0–2:
//! [`SqliteStore`] on local disk. Phase 3 spike / Phase 4: a raft-replicated
//! backend implements these same traits, and single-node mode becomes a
//! 1-voter cluster — same code path. See `docs/ARCHITECTURE.md` §2.
//!
//! The trait is split by domain area purely for readability; consumers hold
//! one `Arc<dyn Store>`. Contract notes for future backends:
//! - Operations are linearizable from the caller's perspective.
//! - A write acknowledged ⇒ durable (on a cluster: quorum-acked).
//! - Implementations are shared via `Arc`, never cloned per-request.

mod sqlite;

use std::path::PathBuf;

pub use sqlite::SqliteStore;

use async_trait::async_trait;

use crate::domain::{
    InProgressItem, Item, ItemPage, ItemSort, Library, MediaFile, MetadataPatch, NewItem,
    NewLibrary, ProbeResult, RecentItem, User, WatchState,
};
use crate::error::StoreError;

/// Well-known settings keys. Keys are dotted, lowercase, and owned by the
/// module that writes them.
pub mod keys {
    /// Stable unique id for this logical server. Generated on first startup,
    /// immutable thereafter; in a cluster it identifies the *cluster*, not a
    /// node (REQ-HA-5: one logical identity).
    pub const INSTANCE_ID: &str = "instance.id";
    /// TMDB API key (set by the admin; empty/absent disables the agent).
    pub const TMDB_API_KEY: &str = "tmdb.api_key";
    /// Hardware-encoder preference for transcoding: "nvenc" | "qsv" | "vaapi"
    /// | "videotoolbox" | "software" | "" (automatic).
    pub const HWACCEL: &str = "transcode.hwaccel";
}

#[async_trait]
pub trait SettingsStore: Send + Sync + 'static {
    /// Cheap liveness probe of the backing storage (drives `/readyz`).
    async fn ping(&self) -> Result<(), StoreError>;
    async fn get_setting(&self, key: &str) -> Result<Option<String>, StoreError>;
    async fn put_setting(&self, key: &str, value: &str) -> Result<(), StoreError>;
    /// The stable unique id of this logical server.
    async fn instance_id(&self) -> Result<String, StoreError>;
}

#[async_trait]
pub trait UserStore: Send + Sync + 'static {
    async fn count_users(&self) -> Result<i64, StoreError>;
    async fn create_user(
        &self,
        username: &str,
        password_hash: &str,
        is_admin: bool,
    ) -> Result<User, StoreError>;
    async fn get_user(&self, id: i64) -> Result<Option<User>, StoreError>;
    async fn get_user_by_username(&self, username: &str) -> Result<Option<User>, StoreError>;
    async fn list_users(&self) -> Result<Vec<User>, StoreError>;
    async fn delete_user(&self, id: i64) -> Result<bool, StoreError>;

    /// Register a login token. Only the SHA-256 hash of the token is stored.
    async fn create_token(
        &self,
        token_hash: &str,
        user_id: i64,
        device: Option<&str>,
    ) -> Result<(), StoreError>;
    /// Resolve a token hash to its user (touching `last_seen_at`).
    async fn user_for_token(&self, token_hash: &str) -> Result<Option<User>, StoreError>;
    async fn delete_token(&self, token_hash: &str) -> Result<bool, StoreError>;
}

#[async_trait]
pub trait LibraryStore: Send + Sync + 'static {
    async fn create_library(&self, library: &NewLibrary) -> Result<Library, StoreError>;
    async fn update_library(
        &self,
        id: i64,
        library: &NewLibrary,
    ) -> Result<Option<Library>, StoreError>;
    async fn delete_library(&self, id: i64) -> Result<bool, StoreError>;
    async fn get_library(&self, id: i64) -> Result<Option<Library>, StoreError>;
    async fn list_libraries(&self) -> Result<Vec<Library>, StoreError>;
}

#[async_trait]
pub trait MediaStore: Send + Sync + 'static {
    // --- item placement (scanner) ---
    async fn find_movie(
        &self,
        library_id: i64,
        title: &str,
        year: Option<i32>,
    ) -> Result<Option<Item>, StoreError>;
    async fn find_show(
        &self,
        library_id: i64,
        title: &str,
        year: Option<i32>,
    ) -> Result<Option<Item>, StoreError>;
    async fn find_season(
        &self,
        show_id: i64,
        season_number: i32,
    ) -> Result<Option<Item>, StoreError>;
    async fn find_episode(
        &self,
        season_id: i64,
        episode_number: i32,
    ) -> Result<Option<Item>, StoreError>;
    async fn insert_item(&self, item: &NewItem) -> Result<i64, StoreError>;

    // --- browse ---
    async fn get_item(&self, id: i64) -> Result<Option<Item>, StoreError>;
    async fn get_item_children(&self, parent_id: i64) -> Result<Vec<Item>, StoreError>;
    async fn list_top_items(
        &self,
        library_id: i64,
        sort: ItemSort,
        offset: i64,
        limit: i64,
    ) -> Result<ItemPage, StoreError>;
    async fn recently_added(
        &self,
        library_id: Option<i64>,
        limit: i64,
    ) -> Result<Vec<RecentItem>, StoreError>;
    async fn search_items(&self, query: &str, limit: i64) -> Result<Vec<RecentItem>, StoreError>;

    // --- metadata enrichment ---
    async fn apply_metadata(&self, item_id: i64, patch: &MetadataPatch) -> Result<(), StoreError>;
    /// Movies and shows that have no TMDB match yet.
    async fn items_needing_metadata(
        &self,
        library_id: Option<i64>,
    ) -> Result<Vec<Item>, StoreError>;
    /// All episodes of a show (across seasons), for bulk episode enrichment.
    async fn episodes_for_show(&self, show_id: i64) -> Result<Vec<Item>, StoreError>;

    // --- files ---
    async fn get_file_by_path(&self, path: &str) -> Result<Option<MediaFile>, StoreError>;
    async fn upsert_file(
        &self,
        item_id: i64,
        path: &str,
        size: i64,
        mtime: i64,
        probe: &ProbeResult,
    ) -> Result<i64, StoreError>;
    async fn get_file(&self, id: i64) -> Result<Option<MediaFile>, StoreError>;
    async fn files_for_item(&self, item_id: i64) -> Result<Vec<MediaFile>, StoreError>;
    /// All known file paths in a library (for vanished-file detection).
    async fn library_file_paths(&self, library_id: i64) -> Result<Vec<(i64, PathBuf)>, StoreError>;
    async fn delete_files(&self, ids: &[i64]) -> Result<u64, StoreError>;
    /// Remove items left childless/file-less after a scan. Returns rows removed.
    async fn prune_empty_items(&self, library_id: i64) -> Result<u64, StoreError>;
}

#[async_trait]
pub trait WatchStore: Send + Sync + 'static {
    async fn watch_state(
        &self,
        user_id: i64,
        item_id: i64,
    ) -> Result<Option<WatchState>, StoreError>;
    /// Batch lookup for annotating item lists.
    async fn watch_map(
        &self,
        user_id: i64,
        item_ids: &[i64],
    ) -> Result<Vec<(i64, WatchState)>, StoreError>;
    /// Record playback progress; crossing 95% marks watched automatically.
    async fn put_progress(
        &self,
        user_id: i64,
        item_id: i64,
        position_ms: i64,
        duration_ms: Option<i64>,
    ) -> Result<WatchState, StoreError>;
    async fn set_watched(
        &self,
        user_id: i64,
        item_id: i64,
        watched: bool,
    ) -> Result<(), StoreError>;
    async fn continue_watching(
        &self,
        user_id: i64,
        limit: i64,
    ) -> Result<Vec<InProgressItem>, StoreError>;
}

/// The full storage boundary — what plurxd holds as `Arc<dyn Store>`.
pub trait Store:
    SettingsStore + UserStore + LibraryStore + MediaStore + WatchStore + Send + Sync + 'static
{
}

impl<T> Store for T where
    T: SettingsStore + UserStore + LibraryStore + MediaStore + WatchStore + Send + Sync + 'static
{
}
