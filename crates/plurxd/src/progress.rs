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
const MAX_FLUSH_ATTEMPTS: u32 = 8;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
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
    retry_attempts: u32,
}

/// The result returned to a request handler. `watch` is always a coherent
/// durable row; reported position/duration are separate so side effects can
/// follow the current heartbeat without serializing a half-old optimistic
/// `WatchState` on the wire.
#[derive(Clone, Debug)]
pub struct ProgressUpdate {
    pub watch: WatchState,
    pub reported_position_ms: i64,
    pub reported_duration_ms: Option<i64>,
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
            sweep_at: sweep_at.max(1),
        })
    }

    async fn entry(&self, key: ProgressKey) -> Option<SharedEntry> {
        let mut entries = self.entries.lock().await;
        if let Some(entry) = entries.get(&key) {
            return Some(Arc::clone(entry));
        }
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
        if entries.len() >= self.sweep_at {
            // The map is a hard cap, not merely a sweep trigger. A new stream
            // writes through until an idle slot becomes reclaimable.
            return None;
        }
        let entry = Arc::new(Mutex::new(Entry::default()));
        entries.insert(key, Arc::clone(&entry));
        Some(entry)
    }

    pub async fn put(
        self: &Arc<Self>,
        user_id: i64,
        item_id: i64,
        position_ms: i64,
        duration_ms: Option<i64>,
    ) -> Result<ProgressUpdate, StoreError> {
        let key = (user_id, item_id);
        let durable = self.store.watch_state(user_id, item_id).await?;
        let known_duration = self
            .store
            .files_for_item(item_id)
            .await?
            .into_iter()
            .filter_map(|file| file.duration_ms)
            .filter(|duration| *duration > 0)
            .max();
        let Some(entry) = self.entry(key).await else {
            let watch = self
                .store
                .put_progress(user_id, item_id, position_ms, duration_ms)
                .await?;
            return Ok(ProgressUpdate {
                reported_position_ms: watch.position_ms,
                reported_duration_ms: watch.duration_ms,
                watch,
                committed: true,
            });
        };
        let mut slot = entry.lock().await;
        if slot.committed != durable {
            // A non-coalescer writer moved the row. Any pending value was
            // observed before that write and is no longer safe to replay.
            slot.pending = None;
            slot.last_commit = None;
            slot.committed = durable;
            slot.retry_attempts = 0;
        }
        let now = Instant::now();
        let due = slot
            .last_commit
            .is_none_or(|last| now.duration_since(last) >= self.window);
        let position_ms = position_ms.max(0);
        let resolved_duration = slot
            .committed
            .as_ref()
            .and_then(|watch| watch.duration_ms)
            .or(known_duration)
            .or(duration_ms)
            .filter(|duration| *duration > 0);
        let position_ms = resolved_duration
            .map(|duration| position_ms.min(duration))
            .unwrap_or(position_ms);
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
                reported_position_ms: watch.position_ms,
                reported_duration_ms: watch.duration_ms,
                watch,
                committed: true,
            });
        }

        slot.pending = Some(Pending {
            position_ms,
            duration_ms,
        });
        slot.retry_attempts = 0;
        let watch = *slot.committed.as_ref().ok_or_else(|| {
            StoreError::Database(
                "progress coalescer has a clock without committed state".to_owned(),
            )
        })?;
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
            reported_position_ms: position_ms,
            reported_duration_ms: resolved_duration,
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
                .put_progress_if_current(
                    key.0,
                    key.1,
                    slot.committed
                        .as_ref()
                        .expect("pending requires committed state"),
                    pending.position_ms,
                    pending.duration_ms,
                )
                .await
            {
                Ok(Some(watch)) => {
                    slot.last_commit = Some(Instant::now());
                    slot.committed = Some(watch);
                    slot.retry_attempts = 0;
                }
                Ok(None) => {
                    // Another durable writer changed this row while the beat
                    // waited. Adopt that state and discard the stale beat;
                    // overwriting a manual unwatch or an offline/Trakt merge
                    // would be worse than losing one intermediate heartbeat.
                    slot.committed = match self.store.watch_state(key.0, key.1).await {
                        Ok(current) => current,
                        Err(error) => {
                            slot.retry_attempts = slot.retry_attempts.saturating_add(1);
                            if slot.retry_attempts >= MAX_FLUSH_ATTEMPTS {
                                slot.worker_running = false;
                                tracing::error!(
                                    user_id = key.0,
                                    item_id = key.1,
                                    attempts = slot.retry_attempts,
                                    error = %error,
                                    "giving up refreshing a stale progress entry"
                                );
                                return;
                            }
                            slot.pending = Some(pending);
                            tracing::warn!(
                                user_id = key.0,
                                item_id = key.1,
                                error = %error,
                                "progress coalescer could not refresh after a competing write"
                            );
                            let delay = retry_delay(slot.retry_attempts);
                            drop(slot);
                            tokio::time::sleep(delay).await;
                            continue;
                        }
                    };
                    slot.last_commit = slot.committed.as_ref().map(|_| Instant::now());
                }
                Err(error) => {
                    slot.retry_attempts = slot.retry_attempts.saturating_add(1);
                    if slot.retry_attempts >= MAX_FLUSH_ATTEMPTS {
                        slot.worker_running = false;
                        tracing::error!(
                            user_id = key.0,
                            item_id = key.1,
                            attempts = slot.retry_attempts,
                            error = %error,
                            "giving up a coalesced progress write after bounded retries"
                        );
                        return;
                    }
                    slot.pending = Some(pending);
                    tracing::warn!(
                        user_id = key.0,
                        item_id = key.1,
                        error = %error,
                        "progress coalescer will retry the pending durable write"
                    );
                    let delay = retry_delay(slot.retry_attempts);
                    drop(slot);
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    /// Flush every accepted trailing beat immediately during graceful
    /// shutdown. Request draining stops new callers before this runs, and each
    /// entry mutex excludes its background worker while the final CAS is in
    /// flight.
    pub async fn drain(&self) -> Result<usize, StoreError> {
        let entries = self
            .entries
            .lock()
            .await
            .iter()
            .map(|(key, entry)| (*key, Arc::clone(entry)))
            .collect::<Vec<_>>();
        let mut flushed = 0;
        for (key, entry) in entries {
            let mut slot = entry.lock().await;
            let Some(pending) = slot.pending.take() else {
                continue;
            };
            let expected = slot.committed.as_ref().ok_or_else(|| {
                StoreError::Database(
                    "progress coalescer has pending state without a durable baseline".to_owned(),
                )
            })?;
            match self
                .store
                .put_progress_if_current(
                    key.0,
                    key.1,
                    expected,
                    pending.position_ms,
                    pending.duration_ms,
                )
                .await
            {
                Ok(Some(watch)) => {
                    slot.committed = Some(watch);
                    slot.last_commit = Some(Instant::now());
                    flushed += 1;
                }
                Ok(None) => {
                    slot.committed = self.store.watch_state(key.0, key.1).await?;
                    slot.last_commit = slot.committed.as_ref().map(|_| Instant::now());
                }
                Err(error) => {
                    slot.pending = Some(pending);
                    return Err(error);
                }
            }
        }
        Ok(flushed)
    }
}

fn retry_delay(attempt: u32) -> Duration {
    Duration::from_secs(
        1_u64
            .checked_shl(attempt.saturating_sub(1))
            .unwrap_or(u64::MAX),
    )
    .min(MAX_RETRY_DELAY)
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
        assert_eq!(middle.watch.position_ms, 1_000);
        assert_eq!(middle.reported_position_ms, 5_000);
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
    async fn a_trailing_beat_never_overwrites_a_competing_manual_write() {
        let (store, user_id, item_id) = fixture().await;
        let coalescer = ProgressCoalescer::with_window(Arc::clone(&store), COMMIT_WINDOW);
        coalescer
            .put(user_id, item_id, 1_000, Some(100_000))
            .await
            .expect("leading beat");
        coalescer
            .put(user_id, item_id, 5_000, Some(100_000))
            .await
            .expect("pending beat");
        store
            .set_watched(user_id, item_id, false)
            .await
            .expect("manual unwatch");
        let restarted = coalescer
            .put(user_id, item_id, 2_000, Some(100_000))
            .await
            .expect("restart beat");
        assert!(!restarted.watch.watched);
        assert_eq!(restarted.watch.position_ms, 2_000);

        tokio::time::advance(COMMIT_WINDOW).await;
        tokio::task::yield_now().await;
        let watch = store
            .watch_state(user_id, item_id)
            .await
            .expect("watch state")
            .expect("watch row");
        assert_eq!(watch.position_ms, 2_000);
        assert!(!watch.watched);
    }

    #[tokio::test(start_paused = true)]
    async fn a_repaired_probe_makes_the_watched_crossing_synchronous() {
        let (store, user_id, item_id) = fixture().await;
        let coalescer = ProgressCoalescer::with_window(Arc::clone(&store), COMMIT_WINDOW);
        coalescer
            .put(user_id, item_id, 1_000, None)
            .await
            .expect("leading beat without duration");
        store
            .upsert_file(
                item_id,
                "/progress/repaired.mkv",
                100,
                1,
                &plurx_core::domain::ProbeResult {
                    duration_ms: Some(100_000),
                    ..Default::default()
                },
            )
            .await
            .expect("repaired probe");

        let finished = coalescer
            .put(user_id, item_id, 96_000, None)
            .await
            .expect("crossing beat");
        assert!(finished.committed);
        assert!(finished.watch.watched);
    }

    #[tokio::test(start_paused = true)]
    async fn graceful_drain_commits_the_latest_pending_beat_immediately() {
        let (store, user_id, item_id) = fixture().await;
        let coalescer = ProgressCoalescer::with_window(Arc::clone(&store), COMMIT_WINDOW);
        coalescer
            .put(user_id, item_id, 1_000, Some(100_000))
            .await
            .expect("leading beat");
        coalescer
            .put(user_id, item_id, 7_000, Some(100_000))
            .await
            .expect("pending beat");

        assert_eq!(coalescer.drain().await.expect("drain"), 1);
        assert_eq!(
            store
                .watch_state(user_id, item_id)
                .await
                .expect("watch state")
                .expect("watch row")
                .position_ms,
            7_000
        );
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

    #[tokio::test(start_paused = true)]
    async fn active_streams_beyond_the_cap_write_through_without_growing_the_map() {
        let (store, user_id, item_id) = fixture().await;
        let second_user = store
            .create_user("progress-cap-2", "hash", false)
            .await
            .expect("second user");
        let coalescer = ProgressCoalescer::with_limits(Arc::clone(&store), COMMIT_WINDOW, 1);

        coalescer
            .put(user_id, item_id, 1_000, Some(100_000))
            .await
            .expect("first stream");
        let overflow = coalescer
            .put(second_user.id, item_id, 2_000, Some(100_000))
            .await
            .expect("overflow stream");

        assert!(overflow.committed);
        assert_eq!(coalescer.entries.lock().await.len(), 1);
    }
}
