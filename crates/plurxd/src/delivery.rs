//! Who is watching what, right now, by which route.
//!
//! Three of the four ways plurx delivers video already keep a record of
//! themselves, and the activity page simply never asked two of them:
//! an HLS transcode and an HLS copy-remux are both real sessions in
//! [`crate::transcode::TranscodeManager`], and a progressive `/stream.mp4`
//! remux is a [`crate::progressive::Stream`]. Each of those has an exact
//! lifetime — a session reap, a `StreamGuard` drop — and this module does not
//! try to improve on it. Listing them is a read, not a registry.
//!
//! Direct play is the one with nothing at all: `serve_file_range` opens a
//! file, writes bytes and forgets. So [`DirectPlays`] holds exactly the
//! deliveries that have no machinery of their own — which today is exactly
//! that one, hence the name — and the seam that fills it
//! ([`crate::playstart::note_playback_started`]) is told the
//! method of every delivery — including the three it deliberately does not
//! record — so that the choice is visible at each call site instead of being
//! implied by which handler you happen to be reading.
//!
//! **A request is not a session.** Direct play is a storm of ranged 206s: a
//! browser that seeks three times issues dozens of short requests against one
//! file, and one-entry-per-request would put a dozen phantom viewers on the
//! activity page for one person on one sofa. Entries are therefore keyed by
//! the *player*, not the request — the client's own playback id where it sends
//! one, and otherwise the file, which is stable across the whole storm — and
//! they are kept alive by a heartbeat rather than closed by an event, because
//! there is no event: a viewer who closes the tab mid-film sends nothing at
//! all.
//!
//! **The lock is a `std::sync::Mutex`, and it has to be.** The neighbouring
//! registries disagree on purpose: `TranscodeManager.sessions` is a
//! `tokio::sync::Mutex` (its holders await across the critical section),
//! while `progressive::Streams` is a `std::sync::Mutex` because a `Drop` impl
//! deregisters and a `Drop` cannot await. Unifying the three into one map
//! would have forced one of those two answers onto the other, and forcing the
//! async one would have made `StreamGuard::drop` impossible to write. The
//! unification is therefore at the *read* — `activity_detail` is async, joins
//! all three sources, and each keeps the lock discipline its own lifetime
//! needs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often a player reports its position (`POST /items/{id}/progress`),
/// taken from the slowest client that ships: Android beats every 10s
/// (`PlayerScreen.kt`, `delay(10_000)`) and Apple every 10s of advanced
/// position (`PlayerController.swift`, `currentMs - lastReportedMs >= 10_000`).
/// The web player is twice as chatty at 5s (`index.html`, the 5000ms player
/// timer). The slowest one is the one an idle rule has to survive.
pub const BEACON_INTERVAL: Duration = Duration::from_secs(10);

/// How many consecutive silent beats mean the player is gone rather than
/// stuttering. One missed beacon is a GC pause, a Wi-Fi roam or a request that
/// lost a race; three in a row is nobody there. Fewer than two would evict
/// live viewers on a single hiccup, which reads to an admin as playback that
/// keeps dropping out.
pub const MISSED_BEACONS: u32 = 3;

/// Idle timeout for a delivery with no session of its own: 30s, derived from
/// the beacon cadence above rather than picked.
pub const IDLE_TIMEOUT: Duration = idle_timeout(BEACON_INTERVAL, MISSED_BEACONS);

/// Above this many live direct plays for one user the oldest is dropped.
/// A person has one player per device; reaching this means a client is
/// rotating playback ids, and an unbounded map is how that becomes a leak.
const MAX_PER_USER: usize = 8;

/// The timeout a beacon cadence implies. A `const fn` so the number above is
/// a derivation anyone can check, not a constant somebody once chose.
pub const fn idle_timeout(beacon: Duration, missed: u32) -> Duration {
    let ms = beacon.as_secs() * 1_000 + beacon.subsec_millis() as u64;
    Duration::from_millis(ms * missed as u64)
}

/// Is a delivery last heard from `idle` ago still on somebody's screen?
///
/// The one rule, in one place, used by both the read and the prune that rides
/// along with it. Two copies of it would eventually disagree, and the two
/// failure shapes are opposite: a reader stricter than the pruner hides live
/// viewers, a pruner stricter than the reader deletes rows mid-page.
pub fn is_live(idle: Duration, timeout: Duration) -> bool {
    idle < timeout
}

/// How a file is reaching a viewer. The activity array labels every entry with
/// one of these; the strings are the wire contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// The file itself, ranged, byte for byte.
    Direct,
    /// `/stream.mp4` — one ffmpeg repackaging into fragmented MP4 on one pipe.
    Remux,
    /// An HLS session that copies the video and only repackages it.
    HlsCopy,
    /// An HLS session that is re-encoding the picture.
    Transcode,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Direct => "direct",
            Method::Remux => "remux",
            Method::HlsCopy => "hls-copy",
            Method::Transcode => "transcode",
        }
    }

    /// True when nothing else in the process is keeping this delivery's
    /// lifetime, so [`DirectPlays`] has to. Written as a match rather than
    /// `== Direct` so a fifth delivery method cannot be added without
    /// answering the question.
    pub fn needs_registry(self) -> bool {
        match self {
            Method::Direct => true,
            Method::Remux | Method::HlsCopy | Method::Transcode => false,
        }
    }
}

/// One player, not one request — see the module doc on the 206 storm.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    user_id: i64,
    /// The client's playback id when it sent one, else `file:{id}`.
    id: String,
}

impl Key {
    /// A client that sends its playback id gets one entry per player, so two
    /// tabs on one account are two viewers. One that does not — an old build,
    /// an AirPlay target, `curl` — falls back to the file, which is still
    /// stable across the range storm and merely merges two simultaneous plays
    /// of the same file by the same person. Undercounting is the safe error
    /// here; the phantom-viewer direction is the one that makes the page a lie.
    pub fn new(user_id: i64, file_id: i64, playback_id: Option<&str>) -> Key {
        let id = match playback_id.map(str::trim).filter(|s| !s.is_empty()) {
            Some(pb) => pb.to_owned(),
            None => format!("file:{file_id}"),
        };
        Key { user_id, id }
    }
}

#[derive(Debug)]
struct Entry {
    user_name: String,
    file_id: i64,
    item_id: i64,
    started_unix: i64,
    last_seen: Instant,
    /// Registration order, so the per-user cap can evict the oldest.
    seq: u64,
}

/// One live delivery that has no session of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Live {
    pub user_name: String,
    pub file_id: i64,
    pub item_id: i64,
    pub started_unix: i64,
    pub idle_seconds: u64,
}

/// The registry. See the module doc for why the lock is synchronous.
#[derive(Debug, Default)]
pub struct DirectPlays {
    live: Mutex<HashMap<Key, Entry>>,
    seq: std::sync::atomic::AtomicU64,
}

impl DirectPlays {
    pub fn new() -> Arc<DirectPlays> {
        Arc::new(DirectPlays::default())
    }

    /// Record a delivery, or refresh one already known.
    ///
    /// Idempotent on purpose: every range request in the storm arrives here,
    /// and a version of this that inserted afresh each time would restart the
    /// "started" clock on every seek — an hour into a film the page would say
    /// it began four seconds ago.
    pub fn record(&self, key: Key, user_name: &str, file_id: i64, item_id: i64) {
        self.record_at(key, user_name, file_id, item_id, Instant::now());
    }

    fn record_at(&self, key: Key, user_name: &str, file_id: i64, item_id: i64, now: Instant) {
        let Ok(mut live) = self.live.lock() else {
            return; // a poisoned lock must not take the stream down with it
        };
        if let Some(entry) = live.get_mut(&key) {
            entry.last_seen = now;
            return;
        }
        let seq = self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let user_id = key.user_id;
        live.insert(
            key,
            Entry {
                user_name: user_name.to_owned(),
                file_id,
                item_id,
                started_unix: now_unix(),
                last_seen: now,
                seq,
            },
        );
        // Drop anything already expired before judging the cap, so a person
        // who watched eight things today is not evicted by their own history.
        live.retain(|_, e| is_live(now.saturating_duration_since(e.last_seen), IDLE_TIMEOUT));
        let mut mine: Vec<(Key, u64)> = live
            .iter()
            .filter(|(k, _)| k.user_id == user_id)
            .map(|(k, e)| (k.clone(), e.seq))
            .collect();
        if mine.len() > MAX_PER_USER {
            mine.sort_by_key(|(_, seq)| *seq);
            for (key, _) in mine.iter().take(mine.len() - MAX_PER_USER) {
                live.remove(key);
                tracing::warn!(
                    playback = %key.id, user = key.user_id,
                    "evicted a direct-play registration over the cap"
                );
            }
        }
    }

    /// The progress beacon: every direct play this viewer has of this item is
    /// still on screen.
    ///
    /// This is what keeps a *paused* player listed. A viewer who has buffered
    /// the rest of the film makes no further range requests at all, so without
    /// the beacon they would drop off the page while still sitting in front of
    /// it — and the beacon is the only signal that arrives from a player that
    /// has stopped fetching.
    pub fn touch_item(&self, user_id: i64, item_id: i64) {
        let Ok(mut live) = self.live.lock() else {
            return;
        };
        let now = Instant::now();
        for entry in live
            .iter_mut()
            .filter(|(k, e)| k.user_id == user_id && e.item_id == item_id)
            .map(|(_, e)| e)
        {
            entry.last_seen = now;
        }
    }

    /// Everything still live, newest first, pruning what is not as it goes.
    ///
    /// The read *is* the sweep. A background reaper would be a second clock to
    /// keep in step with this one, and the only thing it would buy is
    /// forgetting sooner on a server nobody is looking at — where the map is
    /// already bounded by the per-user cap.
    pub fn list(&self) -> Vec<Live> {
        self.list_at(Instant::now())
    }

    fn list_at(&self, now: Instant) -> Vec<Live> {
        let Ok(mut live) = self.live.lock() else {
            return Vec::new();
        };
        live.retain(|_, e| is_live(now.saturating_duration_since(e.last_seen), IDLE_TIMEOUT));
        let mut out: Vec<Live> = live
            .values()
            .map(|e| Live {
                user_name: e.user_name.clone(),
                file_id: e.file_id,
                item_id: e.item_id,
                started_unix: e.started_unix,
                idle_seconds: now.saturating_duration_since(e.last_seen).as_secs(),
            })
            .collect();
        // A HashMap hands entries back in whatever order it likes, and an
        // activity page that reshuffles every two-second poll is unreadable.
        out.sort_by(|a, b| {
            b.started_unix
                .cmp(&a.started_unix)
                .then(a.file_id.cmp(&b.file_id))
        });
        out
    }

    /// Age every entry by `by`, so a test can watch expiry happen without
    /// sleeping through it.
    #[cfg(test)]
    pub fn backdate(&self, by: Duration) {
        let Ok(mut live) = self.live.lock() else {
            return;
        };
        for entry in live.values_mut() {
            entry.last_seen -= by;
        }
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_idle_timeout_is_derived_from_the_beacon_not_chosen() {
        assert_eq!(
            idle_timeout(Duration::from_secs(10), 3),
            Duration::from_secs(30)
        );
        // Sub-second cadences must not truncate to nothing — the derivation
        // works in ms so a future 500ms beacon still produces a real timeout.
        assert_eq!(
            idle_timeout(Duration::from_millis(500), 3),
            Duration::from_millis(1_500)
        );
        assert_eq!(IDLE_TIMEOUT, Duration::from_secs(30));
        assert!(
            IDLE_TIMEOUT >= BEACON_INTERVAL * 2,
            "a timeout under two beats evicts live viewers on one hiccup"
        );
    }

    #[test]
    fn liveness_is_a_boundary_not_a_feeling() {
        let t = Duration::from_secs(30);
        assert!(is_live(Duration::ZERO, t), "just heard from");
        assert!(is_live(Duration::from_secs(29), t), "two beats missed");
        assert!(
            !is_live(t, t),
            "exactly at the timeout is expired — the third beat did not arrive"
        );
        assert!(!is_live(Duration::from_secs(31), t));
    }

    #[test]
    fn a_beacons_worth_of_silence_never_expires_anyone() {
        // The property that matters: whatever the constants become, one
        // missed beat must not remove a viewer who is still watching.
        assert!(is_live(BEACON_INTERVAL, IDLE_TIMEOUT));
        assert!(is_live(BEACON_INTERVAL * 2, IDLE_TIMEOUT));
        assert!(!is_live(BEACON_INTERVAL * MISSED_BEACONS, IDLE_TIMEOUT));
    }

    #[test]
    fn the_range_storm_is_one_viewer() {
        let pb = DirectPlays::new();
        // Forty range requests for one file, the shape a seeking browser makes.
        for _ in 0..40 {
            pb.record(Key::new(7, 42, None), "paul", 42, 5);
        }
        assert_eq!(pb.list().len(), 1, "one player, not forty");
        // A second player on the same account is a second viewer, and only
        // because it identified itself.
        pb.record(Key::new(7, 42, Some("pb-tab-2")), "paul", 42, 5);
        assert_eq!(pb.list().len(), 2);
        // As is somebody else entirely.
        pb.record(Key::new(8, 42, None), "sam", 42, 5);
        assert_eq!(pb.list().len(), 3);
    }

    #[test]
    fn an_entry_expires_after_the_idle_timeout_and_a_beacon_saves_it() {
        let pb = DirectPlays::new();
        let start = Instant::now();
        pb.record_at(Key::new(7, 42, None), "paul", 42, 5, start);

        // Two beats of silence: still watching, just quiet.
        assert_eq!(pb.list_at(start + BEACON_INTERVAL * 2).len(), 1);
        // The third never comes.
        assert!(pb.list_at(start + IDLE_TIMEOUT).is_empty());

        // A beacon inside the window resets the clock — this is the paused
        // player that has buffered the whole film and fetches nothing.
        pb.record_at(Key::new(7, 42, None), "paul", 42, 5, start);
        pb.touch_item(7, 5);
        assert_eq!(pb.list().len(), 1, "the beacon kept it");
        pb.backdate(IDLE_TIMEOUT);
        assert!(pb.list().is_empty(), "and only the beacon was keeping it");
    }

    #[test]
    fn a_beacon_for_another_item_does_not_keep_this_one_alive() {
        let pb = DirectPlays::new();
        pb.record(Key::new(7, 42, None), "paul", 42, 5);
        pb.backdate(IDLE_TIMEOUT - Duration::from_secs(1));
        pb.touch_item(7, 999); // a different item
        pb.backdate(Duration::from_secs(1));
        assert!(pb.list().is_empty());
        // And a beacon from another viewer of the same item does not either.
        pb.record(Key::new(7, 42, None), "paul", 42, 5);
        pb.backdate(IDLE_TIMEOUT - Duration::from_secs(1));
        pb.touch_item(8, 5);
        pb.backdate(Duration::from_secs(1));
        assert!(pb.list().is_empty());
    }

    #[test]
    fn the_started_clock_survives_a_seek() {
        let pb = DirectPlays::new();
        let start = Instant::now();
        pb.record_at(Key::new(7, 42, None), "paul", 42, 5, start);
        let began = pb.list().first().expect("live").started_unix;
        pb.record_at(
            Key::new(7, 42, None),
            "paul",
            42,
            5,
            start + IDLE_TIMEOUT / 2,
        );
        assert_eq!(
            pb.list().first().expect("live").started_unix,
            began,
            "a seek is the same play, not a new one"
        );
    }

    #[test]
    fn one_user_cannot_grow_the_registry_without_bound() {
        let pb = DirectPlays::new();
        for n in 0..(MAX_PER_USER + 4) {
            pb.record(Key::new(7, 42, Some(&format!("pb-{n}"))), "paul", 42, 5);
        }
        assert_eq!(pb.list().len(), MAX_PER_USER, "the oldest were evicted");
    }

    #[test]
    fn every_method_has_a_distinct_label_and_knows_who_keeps_it() {
        let all = [
            Method::Direct,
            Method::Remux,
            Method::HlsCopy,
            Method::Transcode,
        ];
        let labels: Vec<&str> = all.iter().map(|m| m.as_str()).collect();
        assert_eq!(labels, ["direct", "remux", "hls-copy", "transcode"]);
        assert!(Method::Direct.needs_registry());
        assert!(
            all.iter().filter(|m| m.needs_registry()).count() == 1,
            "only the delivery with no machinery of its own is registered here"
        );
    }
}
