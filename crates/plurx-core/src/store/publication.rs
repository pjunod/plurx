//! Lease-fenced publication for cluster-wide background jobs.
//!
//! Readers still use the ordinary [`Store`](super::Store) boundary. Writers
//! receive this wrapper so the lease token is checked in the same transaction
//! as every durable mutation. Keeping the token behind a read lock also means
//! a renewal cannot replace it halfway through a publication call.

use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;

use crate::cluster::coordination::{unix_ms, Lease, StoreCoordinator};
use crate::domain::{BookMetadataPatch, MetadataPatch, NewItem, ProbeResult};
use crate::error::StoreError;

use super::{ReconcileOutcome, RootFingerprintStatus, Store};

#[derive(Clone)]
pub struct PublicationFence {
    state: Arc<RwLock<Option<Lease>>>,
    last: Arc<std::sync::RwLock<Lease>>,
}

impl PublicationFence {
    pub fn new(lease: Lease) -> Self {
        Self {
            state: Arc::new(RwLock::new(Some(lease.clone()))),
            last: Arc::new(std::sync::RwLock::new(lease)),
        }
    }

    pub async fn snapshot(&self) -> Option<Lease> {
        self.state.read().await.clone()
    }

    /// Renew while excluding publications from observing the predecessor token.
    /// The backend CAS may advance the revision before its response reaches
    /// this process, so releasing this write lock earlier would let a valid
    /// publication race with that response and present the just-stale token.
    pub async fn renew(
        &self,
        coordinator: &StoreCoordinator,
        ttl: Duration,
    ) -> Result<bool, StoreError> {
        let mut state = self.state.write().await;
        let Some(current) = state.clone() else {
            return Ok(false);
        };
        match coordinator.renew(&current, ttl).await {
            Ok(Some(replacement)) => {
                *self
                    .last
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = replacement.clone();
                *state = Some(replacement);
                Ok(true)
            }
            Ok(None) => {
                *state = None;
                Ok(false)
            }
            Err(error) => {
                *state = None;
                Err(error)
            }
        }
    }

    /// Self-fence after a failed renewal. Work may finish computing, but its
    /// next durable publication is rejected before it reaches the backend.
    pub async fn invalidate(&self, expected: &Lease) -> bool {
        let mut state = self.state.write().await;
        if state.as_ref() != Some(expected) {
            return false;
        }
        *state = None;
        true
    }
}

/// Read-through store view whose job-owned mutations are lease fenced.
pub struct PublicationStore<'a> {
    store: &'a dyn Store,
    fence: Option<PublicationFence>,
}

impl<'a> PublicationStore<'a> {
    pub fn unfenced(store: &'a dyn Store) -> Self {
        Self { store, fence: None }
    }

    pub fn fenced(store: &'a dyn Store, fence: PublicationFence) -> Self {
        Self {
            store,
            fence: Some(fence),
        }
    }

    pub fn raw(&self) -> &'a dyn Store {
        self.store
    }

    async fn token(&self) -> Result<tokio::sync::OwnedRwLockReadGuard<Option<Lease>>, StoreError> {
        let fence = self.fence.as_ref().ok_or_else(|| {
            StoreError::Task("fenced publication requested without a lease".to_owned())
        })?;
        Ok(Arc::clone(&fence.state).read_owned().await)
    }

    fn invalidated(&self) -> StoreError {
        let lease = self
            .fence
            .as_ref()
            .map(|fence| {
                fence
                    .last
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
            })
            .expect("invalidated is called only for a fenced publisher");
        StoreError::FenceRejected {
            resource: lease.resource,
            owner_node_id: lease.owner_node_id,
            fence: lease.fence,
        }
    }

    pub async fn put_setting(&self, key: &str, value: &str) -> Result<(), StoreError> {
        if self.fence.is_none() {
            return self.store.put_setting(key, value).await;
        }
        let token = self.token().await?;
        let lease = token.as_ref().ok_or_else(|| self.invalidated())?;
        self.store
            .put_setting_fenced(key, value, lease, unix_ms()?)
            .await
    }

    pub async fn mark_library_scanned(&self, id: i64, refreshed: bool) -> Result<(), StoreError> {
        if self.fence.is_none() {
            return self.store.mark_library_scanned(id, refreshed).await;
        }
        let token = self.token().await?;
        let lease = token.as_ref().ok_or_else(|| self.invalidated())?;
        self.store
            .mark_library_scanned_fenced(id, refreshed, lease, unix_ms()?)
            .await
    }

    pub async fn insert_item(&self, item: &NewItem) -> Result<i64, StoreError> {
        if self.fence.is_none() {
            return self.store.insert_item(item).await;
        }
        let token = self.token().await?;
        let lease = token.as_ref().ok_or_else(|| self.invalidated())?;
        self.store.insert_item_fenced(item, lease, unix_ms()?).await
    }

    pub async fn apply_metadata(
        &self,
        item_id: i64,
        patch: &MetadataPatch,
    ) -> Result<(), StoreError> {
        if self.fence.is_none() {
            return self.store.apply_metadata(item_id, patch).await;
        }
        let token = self.token().await?;
        let lease = token.as_ref().ok_or_else(|| self.invalidated())?;
        self.store
            .apply_metadata_fenced(item_id, patch, lease, unix_ms()?)
            .await
    }

    pub async fn apply_book_metadata(
        &self,
        item_id: i64,
        patch: &BookMetadataPatch,
    ) -> Result<(), StoreError> {
        if self.fence.is_none() {
            return self.store.apply_book_metadata(item_id, patch).await;
        }
        let token = self.token().await?;
        let lease = token.as_ref().ok_or_else(|| self.invalidated())?;
        self.store
            .apply_book_metadata_fenced(item_id, patch, lease, unix_ms()?)
            .await
    }

    pub async fn set_nfo_seeded(&self, item_id: i64) -> Result<(), StoreError> {
        if self.fence.is_none() {
            return self.store.set_nfo_seeded(item_id).await;
        }
        let token = self.token().await?;
        let lease = token.as_ref().ok_or_else(|| self.invalidated())?;
        self.store
            .set_nfo_seeded_fenced(item_id, lease, unix_ms()?)
            .await
    }

    pub async fn upsert_file(
        &self,
        item_id: i64,
        path: &str,
        size: i64,
        mtime: i64,
        probe: &ProbeResult,
    ) -> Result<i64, StoreError> {
        if self.fence.is_none() {
            return self
                .store
                .upsert_file(item_id, path, size, mtime, probe)
                .await;
        }
        let token = self.token().await?;
        let lease = token.as_ref().ok_or_else(|| self.invalidated())?;
        self.store
            .upsert_file_fenced(item_id, path, size, mtime, probe, lease, unix_ms()?)
            .await
    }

    pub async fn ensure_library_root_fingerprint(
        &self,
        library_id: i64,
        fingerprint: &str,
        allow_establish: bool,
    ) -> Result<RootFingerprintStatus, StoreError> {
        if self.fence.is_none() {
            return self
                .store
                .ensure_library_root_fingerprint(library_id, fingerprint, allow_establish)
                .await;
        }
        let token = self.token().await?;
        let lease = token.as_ref().ok_or_else(|| self.invalidated())?;
        self.store
            .ensure_library_root_fingerprint_fenced(
                library_id,
                fingerprint,
                allow_establish,
                lease,
                unix_ms()?,
            )
            .await
    }

    pub async fn reconcile_library(
        &self,
        library_id: i64,
        root_fingerprint: &str,
        gone_file_ids: &[i64],
        prune_limit: u64,
    ) -> Result<ReconcileOutcome, StoreError> {
        if self.fence.is_none() {
            return self
                .store
                .reconcile_library(library_id, root_fingerprint, gone_file_ids, prune_limit)
                .await;
        }
        let token = self.token().await?;
        let lease = token.as_ref().ok_or_else(|| self.invalidated())?;
        self.store
            .reconcile_library_fenced(
                library_id,
                root_fingerprint,
                gone_file_ids,
                prune_limit,
                lease,
                unix_ms()?,
            )
            .await
    }

    pub async fn claim_cache_entry(
        &self,
        recipe_hash: &str,
        file_id: i64,
        recipe_version: i64,
        node_id: &str,
        relative_dir: &str,
    ) -> Result<bool, StoreError> {
        if self.fence.is_none() {
            return self
                .store
                .claim_cache_entry(recipe_hash, file_id, recipe_version, node_id, relative_dir)
                .await;
        }
        let token = self.token().await?;
        let lease = token.as_ref().ok_or_else(|| self.invalidated())?;
        self.store
            .claim_cache_entry_fenced(
                recipe_hash,
                file_id,
                recipe_version,
                node_id,
                relative_dir,
                lease,
                unix_ms()?,
            )
            .await
    }

    pub async fn touch_cache_claim(
        &self,
        recipe_hash: &str,
        node_id: &str,
    ) -> Result<(), StoreError> {
        if self.fence.is_none() {
            return self.store.touch_cache_claim(recipe_hash, node_id).await;
        }
        let token = self.token().await?;
        let lease = token.as_ref().ok_or_else(|| self.invalidated())?;
        self.store
            .touch_cache_claim_fenced(recipe_hash, node_id, lease, unix_ms()?)
            .await
    }

    pub async fn complete_cache_entry(
        &self,
        recipe_hash: &str,
        node_id: &str,
        relative_dir: &str,
        bytes: i64,
    ) -> Result<(), StoreError> {
        if self.fence.is_none() {
            return self
                .store
                .complete_cache_entry(recipe_hash, node_id, bytes)
                .await;
        }
        let token = self.token().await?;
        let lease = token.as_ref().ok_or_else(|| self.invalidated())?;
        self.store
            .complete_cache_entry_fenced(
                recipe_hash,
                node_id,
                relative_dir,
                bytes,
                lease,
                unix_ms()?,
            )
            .await
    }

    pub async fn forget_cache_entry(
        &self,
        recipe_hash: &str,
        node_id: &str,
        storage_class: &str,
    ) -> Result<(), StoreError> {
        if self.fence.is_none() {
            self.store
                .forget_cache_entry(recipe_hash, node_id, storage_class)
                .await?;
            return Ok(());
        }
        let token = self.token().await?;
        let lease = token.as_ref().ok_or_else(|| self.invalidated())?;
        self.store
            .forget_cache_entry_fenced(recipe_hash, node_id, storage_class, lease, unix_ms()?)
            .await
    }
}

impl Deref for PublicationStore<'_> {
    type Target = dyn Store;

    fn deref(&self) -> &Self::Target {
        self.store
    }
}
