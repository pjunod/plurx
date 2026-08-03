//! Two small caches that keep the click-to-first-frame path off the NAS and
//! out of side effects.
//!
//! Both exist for the same reason: `/decision` is asked one question — how
//! should this file be delivered — and it was answering it by doing remote
//! I/O and announcing to a third party that someone had started watching.
//! Neither belongs on a path a person is waiting behind, and the second is not
//! even true at the time it fires: a decision is not playback, and a stream
//! that fails to start had still told Trakt it was running.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::delivery::{Key, Method};

/// How long a successful stat is trusted.
///
/// Short on purpose. This is not a claim that the file still exists — only
/// that it did a moment ago, which is all `/decision` ever knew either. The
/// authoritative check is the open that follows, and it is the one that
/// reports a share that went away mid-film.
const AVAILABILITY_TTL: Duration = Duration::from_secs(60);

/// Remembers which media files were readable, so repeated plays of the same
/// title don't each pay a cold NAS attribute lookup between the click and the
/// answer.
#[derive(Default)]
pub struct AvailabilityCache {
    seen: Mutex<HashMap<i64, Instant>>,
}

impl AvailabilityCache {
    pub fn new() -> AvailabilityCache {
        AvailabilityCache::default()
    }

    /// True when the file is on disk. Cached briefly on success and never on
    /// failure — a missing file is the answer that changes a player into an
    /// error message, so it is worth re-asking every time.
    pub async fn is_present(&self, file_id: i64, path: &Path) -> bool {
        if let Ok(seen) = self.seen.lock() {
            if seen
                .get(&file_id)
                .is_some_and(|at| at.elapsed() < AVAILABILITY_TTL)
            {
                return true;
            }
        }
        if tokio::fs::metadata(path).await.is_err() {
            self.forget(file_id);
            return false;
        }
        if let Ok(mut seen) = self.seen.lock() {
            // Bound the map: entries are tiny, but a library-wide sweep should
            // not leave one per file forever.
            if seen.len() > 4096 {
                seen.retain(|_, at| at.elapsed() < AVAILABILITY_TTL);
            }
            seen.insert(file_id, Instant::now());
        }
        true
    }

    /// Drop a file's cached presence — called when an open actually fails, so
    /// an unmounted share stops being remembered as fine.
    pub fn forget(&self, file_id: i64) {
        if let Ok(mut seen) = self.seen.lock() {
            seen.remove(&file_id);
        }
    }
}

/// How long one playback of one item counts as already-announced.
///
/// Long enough that the several media requests a single play makes (a decision
/// followed by a session, or a range request followed by a seek) announce once
/// between them; short enough that genuinely re-watching something announces
/// again.
const START_DEDUP: Duration = Duration::from_secs(300);

/// Announces "watching now" to Trakt when playback actually begins.
///
/// This used to fire from `/decision`, which is wrong twice over: a decision
/// is not playback — a stream that never started had still reported itself
/// running — and it put a third-party call on the click path. It now fires
/// from the points where media is really being delivered, in a detached task,
/// so no viewer ever waits on Trakt being slow.
#[derive(Default)]
pub struct StartNotifier {
    announced: Mutex<HashMap<(i64, i64), Instant>>,
}

impl StartNotifier {
    pub fn new() -> StartNotifier {
        StartNotifier::default()
    }

    /// Claim the right to announce this (viewer, item) pair. False when
    /// another request already did, recently.
    fn claim(&self, user_id: i64, item_id: i64) -> bool {
        let Ok(mut announced) = self.announced.lock() else {
            return false; // a poisoned lock must not double-announce
        };
        let key = (user_id, item_id);
        if announced
            .get(&key)
            .is_some_and(|at| at.elapsed() < START_DEDUP)
        {
            return false;
        }
        if announced.len() > 1024 {
            announced.retain(|_, at| at.elapsed() < START_DEDUP);
        }
        announced.insert(key, Instant::now());
        true
    }
}

/// Note that a viewer has actually started receiving media for `file_id`.
///
/// Fire-and-forget: the work (resolving the item, reading resume position,
/// calling Trakt) happens in a detached task, because the caller is in the
/// middle of serving a stream and must not wait for any of it.
/// `method` is taken from every caller, not only from the one that gets
/// recorded: which routes need a registry entry of their own is a decision
/// [`crate::delivery`] makes once, and passing the method here is what stops a
/// fifth delivery route from being added without answering it.
///
/// `playback_id` is the client's own id for its player, where it sends one. It
/// is not a capability and is trusted for nothing but grouping — the worst a
/// guessed id does is merge two of the guesser's own rows.
pub fn note_playback_started(
    state: &crate::state::AppState,
    user_id: i64,
    user_name: &str,
    file_id: i64,
    method: Method,
    playback_id: Option<&str>,
) {
    let registry_key = method
        .needs_registry()
        .then(|| Key::new(user_id, file_id, playback_id));
    let user_name = user_name.to_owned();
    let state = state.clone();
    tokio::spawn(async move {
        let Ok(Some(file)) = state.store.get_file(file_id).await else {
            return;
        };
        // Ahead of the dedup claim, not behind it. The claim exists to
        // announce one start per play; the registry wants to hear about every
        // request, because that repetition is the heartbeat keeping a live
        // viewer on the activity page. Recorded behind the claim, an entry
        // would be written once and then expire under somebody still watching.
        if let Some(key) = registry_key {
            state
                .direct_plays
                .record(key, &user_name, file_id, file.item_id);
        }
        if !state.starts.claim(user_id, file.item_id) {
            return;
        }
        // Resume position as a percentage, which is what a scrobble start
        // wants — Trakt shows "watching, 34% in" rather than restarting it.
        let percent = state
            .store
            .watch_state(user_id, file.item_id)
            .await
            .ok()
            .flatten()
            .and_then(|w| {
                w.duration_ms
                    .filter(|d| *d > 0)
                    .map(|d| (w.position_ms as f64 / d as f64 * 100.0).clamp(0.0, 100.0))
            })
            .unwrap_or(0.0);
        state.trakt.on_start(user_id, file.item_id, percent);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn availability_caches_presence_but_never_absence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let present = dir.path().join("here.mkv");
        tokio::fs::write(&present, b"x").await.expect("write");
        let missing = dir.path().join("gone.mkv");

        let cache = AvailabilityCache::new();
        assert!(cache.is_present(1, &present).await);
        assert!(!cache.is_present(2, &missing).await);

        // A file that vanishes after being cached still reads as present for
        // the TTL — the cache's honest limit, and why the open that follows is
        // the authority.
        tokio::fs::remove_file(&present).await.expect("remove");
        assert!(cache.is_present(1, &present).await);
        // Until something reports the failure.
        cache.forget(1);
        assert!(!cache.is_present(1, &present).await);

        // Absence is never cached: the second ask re-checks, so a share that
        // comes back is playable immediately rather than a minute later.
        tokio::fs::write(&missing, b"x").await.expect("write");
        assert!(cache.is_present(2, &missing).await);
    }

    #[test]
    fn one_playback_announces_once() {
        let notifier = StartNotifier::new();
        assert!(notifier.claim(1, 10), "first request announces");
        assert!(!notifier.claim(1, 10), "the rest of the same play do not");
        assert!(
            notifier.claim(1, 11),
            "a different item is a different play"
        );
        assert!(notifier.claim(2, 10), "so is a different viewer");
    }
}
