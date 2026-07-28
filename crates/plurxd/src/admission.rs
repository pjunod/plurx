//! How many hardware transcodes this node will run at once, and what happens
//! to the one that asks next.
//!
//! An iGPU has one video-processing block. Two 4K sessions on it do not run at
//! half speed each — they contend, and both can fall under realtime, which
//! turns one person's stream into two people's stutter. OPERATIONS has warned
//! about this since Phase 2 with no mechanism behind the warning.
//!
//! The cap is a count with an atomic acquire, not a scan under the sessions
//! lock. The property that matters is that two starts racing cannot both see
//! the same free slot, and a compare-and-swap gives exactly that without
//! introducing a second lock to order against the first. Leaking is the real
//! risk with a counter, so the slot is a guard: whoever holds it releases it by
//! being dropped, and a session owns its slot for its whole life — killed,
//! reaped, superseded or finished, the slot comes back the same way.
//!
//! What happens when the cap is full is the interesting half, and the answer is
//! *not* "run it in software anyway". Software-decoding 4K HDR is sub-realtime
//! no matter how small the output, so admitting one produces a session that
//! cannot keep up — a viewer watching a stall counter climb, which is worse
//! than a clear refusal. Admission is therefore decided on measured speed for
//! that class of work, never on output height (PERF-PLAN §5, review R6).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use plurx_core::domain::MediaFile;

/// Concurrent hardware sessions, unless the admin says otherwise. Two is the
/// number OPERATIONS has always warned about, now enforced.
pub const DEFAULT_MAX_HW_SESSIONS: usize = 2;

/// How long a start waits for a slot before giving up on hardware. Short on
/// purpose: this is a person who has pressed play, and a queue longer than the
/// start they were promised is not a queue, it is a hang.
pub const QUEUE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Speed a class of work must have been measured at before a session of that
/// class is admitted to software. Above realtime with margin, because a
/// session at exactly 1.0x never builds the reserve that absorbs a hiccup.
const SOFTWARE_SAFE_SPEED: f64 = 1.2;

/// The encoder name admission asks about. A constant rather than a literal
/// because the recording side and the asking side must agree exactly, and a
/// typo in either would silently mean "nothing has ever been measured".
pub const SOFTWARE: &str = "software";

/// A held hardware slot. Releases on drop — which is what keeps the count
/// honest across every way a session can end, including the ones nobody
/// remembers to write a branch for.
#[derive(Debug)]
pub struct HwSlot {
    held: Arc<AtomicUsize>,
}

impl Drop for HwSlot {
    fn drop(&mut self) {
        self.held.fetch_sub(1, Ordering::AcqRel);
    }
}

/// What a start should do, having asked to run on hardware.
#[derive(Debug, PartialEq)]
pub enum Admission {
    /// Run on hardware; hold this until the session ends.
    Hardware(HwSlot),
    /// Hardware is full, but this class of work has been measured running
    /// comfortably above realtime in software. Run it there.
    Software,
    /// Hardware is full and software would stall. Say so.
    Refused(String),
}

impl PartialEq for HwSlot {
    /// Slots are interchangeable; only their existence is meaningful.
    fn eq(&self, _: &HwSlot) -> bool {
        true
    }
}

/// The node's hardware budget, and what it has learned about how fast things
/// actually run here.
#[derive(Debug)]
pub struct Admissions {
    held: Arc<AtomicUsize>,
    /// Recent speed, by class of work — see [`class_of`]. Learned from real
    /// sessions rather than configured, because the answer depends on the box
    /// and no default could be right for both a NUC and a Xeon.
    measured: Mutex<HashMap<String, f64>>,
}

impl Default for Admissions {
    fn default() -> Self {
        Admissions::new()
    }
}

impl Admissions {
    pub fn new() -> Admissions {
        Admissions {
            held: Arc::new(AtomicUsize::new(0)),
            measured: Mutex::new(HashMap::new()),
        }
    }

    pub fn in_use(&self) -> usize {
        self.held.load(Ordering::Acquire)
    }

    /// Take a slot if one is free. The CAS loop is what makes two racing
    /// starts unable to both succeed on the last slot.
    pub fn try_acquire(&self, max: usize) -> Option<HwSlot> {
        let mut held = self.held.load(Ordering::Acquire);
        loop {
            if held >= max {
                return None;
            }
            match self.held.compare_exchange_weak(
                held,
                held + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(HwSlot {
                        held: Arc::clone(&self.held),
                    })
                }
                Err(actual) => held = actual,
            }
        }
    }

    /// Record what a running session actually achieved, so the next admission
    /// decision is about this hardware rather than about an assumption.
    ///
    /// Weighted toward history: one slow stretch on a variable-bitrate film
    /// should bend the number, not decide it.
    pub fn record(&self, class: &str, speed: f64) {
        if !speed.is_finite() || speed <= 0.0 {
            return;
        }
        let mut m = self.measured.lock().expect("admissions mutex");
        let next = match m.get(class) {
            Some(prev) => prev * 0.7 + speed * 0.3,
            None => speed,
        };
        m.insert(class.to_owned(), next);
    }

    fn measured(&self, class: &str) -> Option<f64> {
        self.measured
            .lock()
            .expect("admissions mutex")
            .get(class)
            .copied()
    }

    /// Decide what a start that wanted hardware actually gets.
    pub fn admit(&self, max: usize, work: Workload<'_>) -> Admission {
        if let Some(slot) = self.try_acquire(max) {
            return Admission::Hardware(slot);
        }
        match self.measured(&work.software_class()) {
            Some(speed) if speed >= SOFTWARE_SAFE_SPEED => Admission::Software,
            Some(speed) => Admission::Refused(format!(
                "all {max} hardware transcode slots are in use, and this server has \
                 measured this kind of stream at {speed:.2}x in software — it would \
                 stall. Try again in a moment."
            )),
            // Nothing measured yet. Guess from the shape, and guess the way
            // that doesn't strand somebody on a gray screen: §2.9 measured 4K
            // and HDR sub-realtime in software on this class of hardware, and
            // an optimistic guess there costs a viewer their whole session.
            None if work.hopeless_in_software() => Admission::Refused(format!(
                "all {max} hardware transcode slots are in use, and software cannot \
                 keep up with this stream. Try again in a moment."
            )),
            None => Admission::Software,
        }
    }
}

/// The shape of one transcode, for admission purposes.
///
/// Three fields of the source and one of the output, because that is the whole
/// of what decides whether software can keep up — and taking them explicitly
/// rather than a whole `MediaFile` keeps the decision testable without
/// inventing a file.
#[derive(Debug, Clone, Copy)]
pub struct Workload<'a> {
    pub source_height: i64,
    pub codec: &'a str,
    pub hdr: Option<&'a str>,
    pub target_height: i64,
}

impl<'a> Workload<'a> {
    pub fn of(file: &'a MediaFile, target_height: i64) -> Workload<'a> {
        Workload {
            source_height: file.height.unwrap_or(0),
            codec: file.video_codec.as_deref().unwrap_or("?"),
            hdr: file.hdr.as_deref(),
            target_height,
        }
    }

    /// The bucket this session's speed is remembered under, on `encoder`.
    ///
    /// Two things are deliberately in the key.
    ///
    /// The *source's* resolution and HDR, not the output's: the decode and the
    /// tone-map both happen at source resolution however small the output is,
    /// so bucketing by target height alone is what makes a 4K HDR source look
    /// admissible as long as you ask for 480p — and then stall.
    ///
    /// And the encoder, because the question admission asks is specifically
    /// "how fast is this work *in software*". A QSV session at 6x says nothing
    /// about that. Sharing a bucket would have the hardware vouch for the
    /// software, which is precisely the stall this exists to prevent.
    pub fn class(&self, encoder: &str) -> String {
        let src = match self.source_height {
            h if h >= 2160 => "4k",
            h if h >= 1080 => "1080",
            h if h >= 720 => "720",
            _ => "sd",
        };
        let hdr = self.hdr.unwrap_or("sdr");
        format!(
            "{src}:{}:{hdr}->{}@{encoder}",
            self.codec, self.target_height
        )
    }

    /// The bucket that answers the admission question.
    pub fn software_class(&self) -> String {
        self.class(SOFTWARE)
    }

    /// Has software any realistic chance here, on a node that has not measured
    /// itself yet?
    fn hopeless_in_software(&self) -> bool {
        let heavy_codec = matches!(self.codec, "hevc" | "h265" | "hevc10" | "av1" | "vvc");
        // 4K of anything, or HDR in a codec that costs to decode. §2.9's
        // corpus put both under realtime on exactly this class of hardware.
        self.source_height >= 2160 || (heavy_codec && self.hdr.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work(
        source_height: i64,
        codec: &'static str,
        hdr: Option<&'static str>,
        target_height: i64,
    ) -> Workload<'static> {
        Workload {
            source_height,
            codec,
            hdr,
            target_height,
        }
    }

    /// The property the cap exists for: two starts racing for the last slot,
    /// and only one of them getting it. A count read then written would let
    /// both through, which is precisely the case that stalls an iGPU.
    #[test]
    fn the_last_slot_goes_to_exactly_one_caller() {
        let a = Arc::new(Admissions::new());
        let winners = Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::new();
        for _ in 0..16 {
            let a = Arc::clone(&a);
            let winners = Arc::clone(&winners);
            threads.push(std::thread::spawn(move || {
                if let Some(slot) = a.try_acquire(2) {
                    winners.fetch_add(1, Ordering::AcqRel);
                    // Hold it: a slot released immediately would let the next
                    // thread in and prove nothing.
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    drop(slot);
                }
            }));
        }
        for t in threads {
            t.join().expect("thread");
        }
        assert_eq!(winners.load(Ordering::Acquire), 2, "the cap is the cap");
        assert_eq!(a.in_use(), 0, "every slot came back");
    }

    /// A slot returns however its session ended. This is the whole reason it
    /// is a guard rather than a decrement somewhere: the paths a session can
    /// die down are many and the ones nobody writes a branch for are the ones
    /// that leak a slot until restart.
    #[test]
    fn a_slot_comes_back_when_its_holder_does_not() {
        let a = Admissions::new();
        {
            let _slot = a.try_acquire(1).expect("first");
            assert_eq!(a.in_use(), 1);
            assert!(a.try_acquire(1).is_none(), "full means full");
        }
        assert_eq!(a.in_use(), 0);
        assert!(a.try_acquire(1).is_some(), "and free again");
    }

    /// Admission is about measured speed, never about output height. A 4K HDR
    /// source is sub-realtime in software however small you ask the output to
    /// be, because the decode and the tone-map happen at source resolution.
    #[test]
    fn a_small_output_does_not_make_a_big_source_cheap() {
        let a = Admissions::new();
        let _held = a.try_acquire(1).expect("fill the node");

        for target in [2160, 1080, 720, 480] {
            match a.admit(1, work(2160, "hevc", Some("hdr10"), target)) {
                Admission::Refused(why) => assert!(why.contains("software"), "{why}"),
                other => panic!("4K HDR admitted to software at {target}p: {other:?}"),
            }
        }

        // …and something software genuinely handles is admitted.
        assert_eq!(
            a.admit(1, work(720, "h264", None, 480)),
            Admission::Software
        );
    }

    /// Once the box has measured itself, the measurement wins over the guess —
    /// in both directions. A fast machine should not be refused forever, and a
    /// slow one should not be admitted because its file looked easy.
    #[test]
    fn measurement_overrides_the_guess_either_way() {
        let a = Admissions::new();
        let _held = a.try_acquire(1).expect("fill the node");
        let uhd = work(2160, "hevc", Some("hdr10"), 1080);
        let easy = work(1080, "h264", None, 720);

        // A machine that really does clear realtime on 4K HDR in software.
        a.record(&uhd.software_class(), 2.4);
        assert_eq!(a.admit(1, uhd), Admission::Software);

        // And one that does not, on something that looked easy.
        a.record(&easy.software_class(), 0.6);
        match a.admit(1, easy) {
            Admission::Refused(why) => assert!(why.contains("0.60x"), "{why}"),
            other => panic!("a measured-slow class was admitted: {other:?}"),
        }
    }

    /// The record is smoothed, so one bad stretch of a variable-bitrate film
    /// does not lock a capable machine out of software for the rest of its uptime.
    #[test]
    fn one_slow_stretch_bends_the_record_rather_than_deciding_it() {
        let a = Admissions::new();
        let class = "4k:hevc:hdr10->1080";
        a.record(class, 3.0);
        a.record(class, 0.5);
        let after = a.measured(class).expect("recorded");
        assert!(after > 2.0, "one sample should not halve it: {after}");
        assert!(after < 3.0, "but it should move: {after}");

        // Nonsense is ignored rather than averaged in — ffmpeg reports N/A as
        // nothing, and a zero would drag a healthy class under the bar.
        a.record(class, 0.0);
        a.record(class, f64::NAN);
        assert_eq!(a.measured(class), Some(after));
    }

    /// Classes are about the work, so two sessions that cost the same share a
    /// record and two that don't, don't.
    #[test]
    fn the_class_key_separates_work_that_costs_differently() {
        let uhd_hdr = work(2160, "hevc", Some("hdr10"), 1080);
        let uhd_sdr = work(2160, "hevc", None, 1080);
        let hd = work(1080, "h264", None, 1080);
        assert_ne!(
            uhd_hdr.software_class(),
            uhd_sdr.software_class(),
            "the tone-map is the expensive part; it cannot share a bucket"
        );
        assert_ne!(uhd_sdr.software_class(), hd.software_class());
        assert_ne!(
            uhd_hdr.software_class(),
            work(2160, "hevc", Some("hdr10"), 480).software_class(),
            "the encode size still matters, just less than the source"
        );
        // Two files of the same shape are the same work.
        assert_eq!(
            work(2160, "hevc", Some("hdr10"), 1080).software_class(),
            uhd_hdr.software_class()
        );

        // And a hardware run never vouches for a software one. This is the
        // whole reason the encoder is in the key: a QSV session at 6x has
        // measured nothing about what x264 would do with the same file.
        assert_ne!(
            uhd_hdr.class("Intel QuickSync"),
            uhd_hdr.software_class(),
            "hardware and software cannot share a bucket"
        );
    }
}
