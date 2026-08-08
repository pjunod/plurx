//! Server-side coalescing for active playback progress.
//!
//! Players are deliberately free to report more often than durable consensus
//! should commit. The first beat is written immediately, intermediate beats
//! replace one pending value, and the newest pending value is flushed at the
//! ten-second boundary. A newly completed item bypasses the wait so its
//! watched transition and notification stay synchronous.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use plurx_core::domain::WatchState;
use plurx_core::error::StoreError;
use plurx_core::store::Store;
use tokio::sync::Mutex;
use tokio::time::Instant;

const COMMIT_WINDOW: Duration = Duration::from_secs(10);
const RETRY_DELAY: Duration = Duration::from_secs(1);
const ENTRY_SWEEP_AT: usize = 1024;
type ProgressKey = (i64, i64);
type SharedEntry = Arc<Mutex<Entry>>;

#[derive(Clone, Copy, Debug)]
struct Pending {
    position_ms: i64,
    duration_ms: Option<i64>,
}

#[derive(Default)]
struct Entry {
    last_commit: Option<Instant>,
    committed: Option<WatchState>,
    pending: Option<Pending>,
    worker_running: bool,
}

/// The result returned to a request handler. A coalesced response is an
/// optimistic view of the latest beat; `committed` is exposed for the rate
/// contract and tests, not serialized on the wire.
#[derive(Clone, Debug)]
pub struct ProgressUpdate {
    pub watch: WatchState,
    pub committed: bool,
}

pub struct ProgressCoalescer {
    store: Arc<dyn Store>,
    entries: Mutex<HashMap<ProgressKey, SharedEntry>>,
    window: Duration,
    sweep_at: usize,
}

impl ProgressCoalescer {
    pub fn new(store: Arc<dyn Store>) -> Arc<Self> {
        Self::with_window(store, COMMIT_WINDOW)
    }

    fn with_window(store: Arc<dyn Store>, window: Duration) -> Arc<Self> {
        Self::with_limits(store, window, ENTRY_SWEEP_AT)
    }

    fn with_limits(store: Arc<dyn Store>, window: Duration, sweep_at: usize) -> Arc<Self> {
        Arc::new(Self {
            store,
            entries: Mutex::new(HashMap::new()),
            window,
            sweep_at,
        })
    }

    async fn entry(&self, key: ProgressKey) -> SharedEntry {
        let mut entries = self.entries.lock().await;
        if entries.len() >= self.sweep_at {
            let now = Instant::now();
            entries.retain(|_, entry| {
                // The map lock prevents a new caller from cloning this entry
                // while it is examined. Existing requests and flush workers
                // already own another Arc and therefore cannot be detached.
                if Arc::strong_count(entry) != 1 {
                    return true;
                }
                let Ok(slot) = entry.try_lock() else {
                    return true;
                };
                slot.worker_running
                    || slot.pending.is_some()
                    || slot
                        .last_commit
                        .is_some_and(|last| now.duration_since(last) < self.window)
            });
        }
        Arc::clone(
            entries
                .entry(key)
                .or_insert_with(|| Arc::new(Mutex::new(Entry::default()))),
        )
    }

    pub async fn put(
        self: &Arc<Self>,
        user_id: i64,
        item_id: i64,
        position_ms: i64,
        duration_ms: Option<i64>,
    ) -> Result<ProgressUpdate, StoreError> {
        let key = (user_id, item_id);
        let entry = self.entry(key).await;
        let mut slot = entry.lock().await;
        let now = Instant::now();
        let due = slot
            .last_commit
            .is_none_or(|last| now.duration_since(last) >= self.window);
        let position_ms = position_ms.max(0);
        let resolved_duration = slot
            .committed
            .as_ref()
            .and_then(|watch| watch.duration_ms)
            .or(duration_ms)
            .filter(|duration| *duration > 0);
        let newly_complete = slot.committed.as_ref().is_none_or(|watch| !watch.watched)
            && resolved_duration.is_some_and(|duration| {
                position_ms.saturating_mul(100) >= duration.saturating_mul(95)
            });

        if due || newly_complete {
            slot.pending = None;
            let watch = self
                .store
                .put_progress(user_id, item_id, position_ms, duration_ms)
                .await?;
            slot.last_commit = Some(Instant::now());
            slot.committed = Some(watch);
            return Ok(ProgressUpdate {
                watch,
                committed: true,
            });
        }

        slot.pending = Some(Pending {
            position_ms,
            duration_ms,
        });
        let committed = slot.committed.as_ref().ok_or_else(|| {
            StoreError::Database(
                "progress coalescer has a clock without committed state".to_owned(),
            )
        })?;
        let watch = WatchState {
            position_ms,
            duration_ms: resolved_duration,
            watched: committed.watched,
            updated_at: committed.updated_at,
        };
        if !slot.worker_running {
            slot.worker_running = true;
            let coalescer = Arc::clone(self);
            let worker_entry = Arc::clone(&entry);
            tokio::spawn(async move {
                coalescer.flush_loop(key, worker_entry).await;
            });
        }
        Ok(ProgressUpdate {
            watch,
            committed: false,
        })
    }

    async fn flush_loop(self: Arc<Self>, key: ProgressKey, entry: SharedEntry) {
        loop {
            let deadline = {
                let mut slot = entry.lock().await;
                if slot.pending.is_none() {
                    slot.worker_running = false;
                    return;
                }
                slot.last_commit
                    .map(|last| last + self.window)
                    .unwrap_or_else(Instant::now)
            };
            tokio::time::sleep_until(deadline).await;

            let mut slot = entry.lock().await;
            let Some(last) = slot.last_commit else {
                continue;
            };
            if Instant::now().duration_since(last) < self.window {
                continue;
            }
            let Some(pending) = slot.pending.take() else {
                continue;
            };
            match self
                .store
                .put_progress(key.0, key.1, pending.position_ms, pending.duration_ms)
                .await
            {
                Ok(watch) => {
                    slot.last_commit = Some(Instant::now());
                    slot.committed = Some(watch);
                }
                Err(error) => {
                    // A transient quorum loss must not discard the final beat.
                    slot.pending = Some(pending);
                    tracing::warn!(
                        user_id = key.0,
                        item_id = key.1,
                        error = %error,
                        "progress coalescer will retry the pending durable write"
                    );
                    drop(slot);
                    tokio::time::sleep(RETRY_DELAY).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use plurx_core::domain::{ItemKind, LibraryKind, NewItem, NewLibrary};
    use plurx_core::store::{LibraryStore, MediaStore, SqliteStore, UserStore};

    use super::*;

    async fn fixture() -> (Arc<dyn Store>, i64, i64) {
        let store = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let user = store
            .create_user("progress-coalescer", "hash", false)
            .await
            .expect("user");
        let library = store
            .create_library(&NewLibrary {
                name: "Progress".to_owned(),
                kind: LibraryKind::Movies,
                paths: vec![PathBuf::from("/progress")],
                anime: false,
            })
            .await
            .expect("library");
        let item = store
            .insert_item(&NewItem {
                library_id: library.id,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "Progress proof".to_owned(),
                year: None,
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("item");
        (store, user.id, item)
    }

    #[tokio::test(start_paused = true)]
    async fn intermediate_beats_coalesce_and_the_latest_flushes() {
        let (store, user_id, item_id) = fixture().await;
        let coalescer = ProgressCoalescer::with_window(Arc::clone(&store), COMMIT_WINDOW);

        let first = coalescer
            .put(user_id, item_id, 1_000, Some(100_000))
            .await
            .expect("first");
        assert!(first.committed);

        tokio::time::advance(Duration::from_secs(5)).await;
        let middle = coalescer
            .put(user_id, item_id, 5_000, Some(100_000))
            .await
            .expect("middle");
        assert!(!middle.committed);
        assert_eq!(middle.watch.position_ms, 5_000);
        assert_eq!(
            store
                .watch_state(user_id, item_id)
                .await
                .expect("watch")
                .expect("row")
                .position_ms,
            1_000
        );

        let latest = coalescer
            .put(user_id, item_id, 8_000, Some(100_000))
            .await
            .expect("latest");
        assert!(!latest.committed);
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            store
                .watch_state(user_id, item_id)
                .await
                .expect("watch")
                .expect("row")
                .position_ms,
            8_000,
            "the trailing flush keeps the newest coalesced beat"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_watched_transition_commits_synchronously() {
        let (store, user_id, item_id) = fixture().await;
        let coalescer = ProgressCoalescer::with_window(store, COMMIT_WINDOW);
        coalescer
            .put(user_id, item_id, 1_000, Some(100_000))
            .await
            .expect("first");
        tokio::time::advance(Duration::from_secs(2)).await;
        let finished = coalescer
            .put(user_id, item_id, 96_000, Some(100_000))
            .await
            .expect("finished");
        assert!(finished.committed);
        assert!(finished.watch.watched);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_stream_entries_do_not_accumulate_without_bound() {
        let (store, user_id, item_id) = fixture().await;
        let second_user = store
            .create_user("progress-coalescer-2", "hash", false)
            .await
            .expect("second user");
        let coalescer = ProgressCoalescer::with_limits(Arc::clone(&store), COMMIT_WINDOW, 1);

        coalescer
            .put(user_id, item_id, 1_000, Some(100_000))
            .await
            .expect("first stream");
        tokio::time::advance(COMMIT_WINDOW).await;
        coalescer
            .put(second_user.id, item_id, 2_000, Some(100_000))
            .await
            .expect("second stream");

        let entries = coalescer.entries.lock().await;
        assert_eq!(entries.len(), 1);
        assert!(!entries.contains_key(&(user_id, item_id)));
    }
}
