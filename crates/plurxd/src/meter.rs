//! How fast bytes are actually reaching a viewer, measured where they leave.
//!
//! The stats overlay used to source its bandwidth line from hls.js's own
//! estimator, which meant the number existed only when hls.js happened to be
//! the transport. It is not: Safari plays HEVC through native HLS, and Chrome
//! plays a remux as a progressive fMP4 straight off `video.src`. On both of
//! those the line simply vanished — and those are exactly the sessions whose
//! delivery anyone wants to see, because they are the big ones.
//!
//! The server is the one place every path has in common. It knows what it
//! handed over and when, so it can answer for all of them with one number that
//! means the same thing everywhere.

use std::sync::atomic::{AtomicI64, Ordering::Relaxed};
use std::time::Instant;

/// A rate is only meaningful over a window; shorter than this and one segment
/// arriving reads as an enormous link, because a 40 MB fetch that took 300ms
/// off a warm page cache says nothing about the next one.
const WINDOW_MIN_MS: i64 = 1_500;
/// Re-baseline past this. A viewer whose buffer is full stops fetching, and
/// dividing zero bytes by a minute of idling would report a dead link — when
/// the truth is that nothing needed delivering.
const WINDOW_MAX_MS: i64 = 12_000;

/// Bytes delivered to one playback, and the recent rate they arrived at.
///
/// Sampling is deliberately window-based rather than exponential: delivery is
/// bursty by construction. A player fetches a segment flat-out, then waits.
/// Averaging *bursts* answers the wrong question ("how fast is one fetch") —
/// what matters is bytes per second of elapsed viewing, across the gaps.
#[derive(Debug)]
pub struct Meter {
    started: Instant,
    /// Everything handed over since this meter began.
    total: AtomicI64,
    /// Start of the open window: wall ms since `started`, and the byte count
    /// at that moment.
    window_at_ms: AtomicI64,
    window_bytes: AtomicI64,
    /// Bytes per second over the last closed window; `-1` until one closes.
    recent_bps: AtomicI64,
}

impl Default for Meter {
    fn default() -> Self {
        Meter::new()
    }
}

impl Meter {
    pub fn new() -> Meter {
        Meter {
            started: Instant::now(),
            total: AtomicI64::new(0),
            window_at_ms: AtomicI64::new(0),
            window_bytes: AtomicI64::new(0),
            recent_bps: AtomicI64::new(-1),
        }
    }

    /// Record bytes on their way out. Cheap enough to call per chunk.
    pub fn note(&self, bytes: u64) {
        let total = self.total.fetch_add(bytes as i64, Relaxed) + bytes as i64;
        let now = self.started.elapsed().as_millis() as i64;
        let opened = self.window_at_ms.load(Relaxed);
        if now - opened < WINDOW_MIN_MS {
            return;
        }
        // Close the window and open the next one from here. Racing writers can
        // both pass the check; the loser's window is simply a hair short, which
        // is not worth a lock on a per-chunk path.
        let base = self.window_bytes.swap(total, Relaxed);
        self.window_at_ms.store(now, Relaxed);
        let span = now - opened;
        if span <= WINDOW_MAX_MS {
            self.recent_bps
                .store(((total - base).max(0) * 1000) / span, Relaxed);
        }
    }

    pub fn total_bytes(&self) -> i64 {
        self.total.load(Relaxed)
    }

    /// The recent delivery rate in bytes per second, once a window has closed.
    ///
    /// Goes stale rather than wrong: a viewer who has stopped fetching keeps
    /// the last real rate instead of decaying toward zero, because zero would
    /// read as a broken link when it means a full buffer. [`Meter::idle_for_ms`]
    /// is what tells the two apart.
    pub fn recent_bps(&self) -> Option<i64> {
        Some(self.recent_bps.load(Relaxed)).filter(|v| *v >= 0)
    }

    /// Wall time since the open window began — i.e. how long since the rate was
    /// last recomputed. A caller that wants to hide a stale number can.
    pub fn idle_for_ms(&self) -> i64 {
        (self.started.elapsed().as_millis() as i64 - self.window_at_ms.load(Relaxed)).max(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A meter with a synthetic clock: `started` is pushed into the past so
    /// `elapsed()` reports whatever the test wants, without sleeping.
    fn at(ms: u64) -> Meter {
        Meter {
            started: Instant::now() - std::time::Duration::from_millis(ms),
            ..Meter::new()
        }
    }

    #[test]
    fn a_rate_needs_a_window_to_measure_over() {
        let m = at(0);
        m.note(1_000_000);
        assert_eq!(m.total_bytes(), 1_000_000);
        // One fetch is not a rate. Reporting 1 MB "per" the instant it landed
        // is how you get a bandwidth readout of several gigabits.
        assert_eq!(m.recent_bps(), None);
    }

    #[test]
    fn the_rate_is_bytes_over_elapsed_including_the_gaps() {
        // 8 MB handed over across a two-second window → 4 MB/s. The point is
        // that the idle time between segment fetches is *in* the denominator:
        // this is delivery to a viewer, not the speed of one download.
        let m = at(2_000);
        m.note(8_000_000);
        assert_eq!(m.recent_bps(), Some(4_000_000));
    }

    #[test]
    fn an_idle_stretch_re_baselines_instead_of_reporting_a_dead_link() {
        let m = at(2_000);
        m.note(8_000_000);
        assert_eq!(m.recent_bps(), Some(4_000_000));

        // The viewer's buffer fills and fetching stops for half a minute. That
        // is a healthy session, and the last real rate is the honest thing to
        // keep showing — recomputing over the idle stretch would print a link
        // speed of nearly nothing for a stream that is doing fine.
        let idle = Meter {
            started: m.started - std::time::Duration::from_millis(30_000),
            total: AtomicI64::new(m.total_bytes()),
            window_at_ms: AtomicI64::new(m.window_at_ms.load(Relaxed)),
            window_bytes: AtomicI64::new(m.window_bytes.load(Relaxed)),
            recent_bps: AtomicI64::new(m.recent_bps.load(Relaxed)),
        };
        idle.note(4_000);
        assert_eq!(
            idle.recent_bps(),
            Some(4_000_000),
            "kept the last real rate"
        );
        assert_eq!(idle.idle_for_ms(), 0, "but the window restarted here");
    }
}
