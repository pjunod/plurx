//! Telemetry for the progressive remux — the `/stream.mp4` path.
//!
//! Every other way plurx delivers video is a session with a status endpoint,
//! so the player can ask how the server side is doing. The progressive remux
//! was the exception: `video.src = url`, one ffmpeg on the end of one HTTP
//! response, and nothing to ask. That is Chrome's whole remux path, which made
//! the stats overlay's Server section disappear on the browser most people use
//! — on precisely the high-bitrate files where "is the encoder keeping up" is
//! the question.
//!
//! This is deliberately not the HLS session manager. That machinery exists to
//! manage a *segment directory*: publishing, retention, an ahead-window, a
//! process to suspend when the client is far enough ahead. A progressive
//! stream has none of those — it has one pipe, paced by TCP backpressure, and
//! two numbers worth reporting (how fast ffmpeg is producing, how fast bytes
//! are leaving). Modelling it as a session would have meant a session kind
//! where most of the fields are `None`.
//!
//! The id is the client's, not a capability: the player already has a stable
//! playback id, and passes it on the stream URL. Lookups are scoped to the
//! owning user so the id being guessable buys nothing — the worst a guess
//! gets you is your own stream's byte count.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::meter::Meter;
use crate::transcode::Progress;

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Above this many live streams for one user, the oldest registration is
/// dropped rather than accumulating. Reaching it means something is leaking
/// registrations, since a browser holds one stream per open player.
const MAX_PER_USER: usize = 8;

/// One in-flight progressive remux.
#[derive(Debug)]
pub struct Stream {
    pub user_id: i64,
    pub file_id: i64,
    /// The pace this remux was started at, in multiples of realtime (`0` =
    /// unpaced). Without it a reader cannot tell a server sitting at its speed
    /// limit from one that is barely coping: both report the same number.
    pub readrate: f64,
    /// ffmpeg's `-progress` telemetry: encode speed and produced content.
    pub progress: Arc<Progress>,
    /// Bytes actually written to the HTTP response.
    pub delivery: Meter,
    /// Unix seconds, for the same "started at" the HLS sessions report.
    started_unix: i64,
    /// Registration order, so the per-user cap can evict the oldest.
    seq: i64,
}

/// What the player asks for while its stats overlay is open.
#[derive(Debug, Serialize)]
pub struct StreamInfo {
    pub file_id: i64,
    pub started_unix: i64,
    /// Cumulative encode rate as a multiple of realtime, as ffmpeg reports it.
    pub speed: Option<f64>,
    /// Rate over the last few seconds — the one that says whether the server
    /// is keeping up *now*.
    pub recent_speed: Option<f64>,
    /// Content produced so far, ms from this stream's start offset.
    pub out_time_ms: Option<i64>,
    pub delivered_bytes: i64,
    /// Recent delivery rate in bits per second, or `None` before a window has
    /// closed. Bits because that is the unit a viewer's link is sold in and
    /// the unit the overlay already prints bitrates in.
    pub delivered_bps: Option<i64>,
    /// Age of that rate in ms — see `SessionInfo::delivered_idle_ms`.
    pub delivered_idle_ms: i64,
    /// The configured pace, so a client can say "at its limit" rather than
    /// implying the machine is the constraint.
    pub readrate: f64,
}

impl Stream {
    fn info(&self) -> StreamInfo {
        StreamInfo {
            file_id: self.file_id,
            started_unix: self.started_unix,
            speed: self.progress.speed(),
            recent_speed: self.progress.recent_speed(),
            out_time_ms: self.progress.out_time_ms(),
            delivered_bytes: self.delivery.total_bytes(),
            delivered_bps: self.delivery.recent_bps().map(|b| b * 8),
            delivered_idle_ms: self.delivery.idle_for_ms(),
            readrate: self.readrate,
        }
    }
}

/// The live registry. Entries are removed by [`StreamGuard`] when the response
/// body is dropped, which is the same moment `kill_on_drop` takes the ffmpeg
/// down — so an entry outliving its stream would have to outlive the process
/// producing it.
#[derive(Debug, Default)]
pub struct Streams {
    live: Mutex<HashMap<String, Arc<Stream>>>,
    seq: AtomicI64,
}

impl Streams {
    pub fn new() -> Arc<Streams> {
        Arc::new(Streams::default())
    }

    /// Register a stream under a client-chosen id, replacing any previous
    /// registration for that id. Returns the shared handle to feed, plus a
    /// guard that deregisters on drop.
    pub fn register(
        self: &Arc<Self>,
        id: &str,
        user_id: i64,
        file_id: i64,
        readrate: f64,
    ) -> (Arc<Stream>, StreamGuard) {
        let stream = Arc::new(Stream {
            user_id,
            file_id,
            readrate,
            progress: Arc::new(Progress::new()),
            delivery: Meter::new(),
            started_unix: now_unix(),
            seq: self.seq.fetch_add(1, Relaxed),
        });
        {
            let mut live = self.live.lock().expect("streams mutex");
            live.insert(id.to_owned(), Arc::clone(&stream));
            // A seek re-opens the stream under the same id, so the common case
            // is a replacement, not growth. The cap is for the case where
            // something stops dropping guards.
            let mine: Vec<(String, i64)> = live
                .iter()
                .filter(|(_, s)| s.user_id == user_id)
                .map(|(k, s)| (k.clone(), s.seq))
                .collect();
            if mine.len() > MAX_PER_USER {
                let mut mine = mine;
                mine.sort_by_key(|(_, seq)| *seq);
                for (key, _) in mine.iter().take(mine.len() - MAX_PER_USER) {
                    live.remove(key);
                    tracing::warn!(stream = %key, user = user_id, "evicted a progressive stream registration over the cap");
                }
            }
        }
        let guard = StreamGuard {
            streams: Arc::clone(self),
            id: id.to_owned(),
            seq: stream.seq,
        };
        (stream, guard)
    }

    /// Status for `id`, but only for the user who owns it.
    pub fn status(&self, id: &str, user_id: i64) -> Option<StreamInfo> {
        let live = self.live.lock().expect("streams mutex");
        live.get(id)
            .filter(|s| s.user_id == user_id)
            .map(|s| s.info())
    }

    /// Remove `id`, but only if it is still the registration `seq` made.
    ///
    /// A seek re-registers under the same id and the superseded guard is
    /// dropped *after* the replacement is in place — without this check it
    /// would delete its successor on the way out, and the overlay would go
    /// blank every time somebody scrubbed.
    fn remove_if_current(&self, id: &str, seq: i64) {
        let mut live = self.live.lock().expect("streams mutex");
        if live.get(id).is_some_and(|s| s.seq == seq) {
            live.remove(id);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.live.lock().expect("streams mutex").len()
    }
}

/// Deregisters its stream when dropped. Held by the response body, so it
/// outlives the request handler and dies with the connection.
pub struct StreamGuard {
    streams: Arc<Streams>,
    id: String,
    seq: i64,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.streams.remove_if_current(&self.id, self.seq);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_is_visible_to_its_owner_and_gone_when_dropped() {
        let streams = Streams::new();
        let (stream, guard) = streams.register("pb-1", 7, 42, 4.0);
        stream.delivery.note(2_000_000);

        let info = streams.status("pb-1", 7).expect("owner sees it");
        assert_eq!(info.file_id, 42);
        assert_eq!(info.delivered_bytes, 2_000_000);

        // The id is the client's own playback id, so it is guessable. Scoping
        // by user is what makes that not matter.
        assert!(
            streams.status("pb-1", 8).is_none(),
            "another user cannot read it"
        );

        drop(guard);
        assert!(streams.status("pb-1", 7).is_none(), "deregistered on drop");
        assert_eq!(streams.len(), 0);
    }

    #[test]
    fn re_opening_the_same_id_replaces_rather_than_accumulates() {
        // A seek tears the stream down and opens another under the same
        // playback id. Two entries would mean the overlay could read the dead
        // one's frozen numbers.
        let streams = Streams::new();
        let (first, g1) = streams.register("pb-1", 7, 42, 4.0);
        first.delivery.note(999);
        let (_second, _g2) = streams.register("pb-1", 7, 42, 4.0);
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams.status("pb-1", 7).expect("live").delivered_bytes,
            0,
            "the new stream's counters, not the old one's"
        );
        drop(g1); // the superseded guard must not take the live entry with it
        assert_eq!(streams.len(), 1);
        assert!(
            streams.status("pb-1", 7).is_some(),
            "the replacement survives"
        );
        drop(_g2);
        assert_eq!(streams.len(), 0);
    }

    #[test]
    fn one_user_cannot_grow_the_registry_without_bound() {
        let streams = Streams::new();
        let mut guards = Vec::new();
        for n in 0..(MAX_PER_USER + 4) {
            let (_s, g) = streams.register(&format!("pb-{n}"), 7, 42, 4.0);
            guards.push(g);
        }
        assert_eq!(streams.len(), MAX_PER_USER);
        assert!(streams.status("pb-0", 7).is_none(), "oldest evicted first");
        assert!(streams.status("pb-11", 7).is_some(), "newest kept");
    }
}
