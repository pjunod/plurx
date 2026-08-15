//! How many hardware transcodes this node will run at once, and what happens
//! to the one that asks next.
//!
//! An iGPU has one video-processing block. Two 4K sessions on it do not run at
//! half speed each — they contend, and both can fall under realtime, which
//! turns one person's stream into two people's stutter. OPERATIONS has warned
//! about this since Phase 2 with no mechanism behind the warning.
//!
//! The cap and its live/background ownership are one small state transition,
//! not a scan under the sessions lock. The property that matters is that two
//! starts racing cannot both see the same free slot, and that background
//! acquisition cannot cross a live waiter's registration. Leaking is the real
//! risk with counters, so every slot is a guard: whoever holds it releases it
//! by being dropped, and a session owns its slot for its whole life — killed,
//! reaped, superseded or finished, the slot comes back the same way.
//!
//! What happens when the cap is full is the interesting half, and the answer is
//! *not* "run it in software anyway". Software-decoding 4K HDR is sub-realtime
//! no matter how small the output, so admitting one produces a session that
//! cannot keep up — a viewer watching a stall counter climb, which is worse
//! than a clear refusal. Admission is therefore decided on measured speed for
//! that class of work, never on output height (PERF-PLAN §5, review R6).
//!
//! Background work — the pre-transcode producer — shares the same slots at
//! strictly lower priority, and yields them by **terminating**, never by
//! suspending. A SIGSTOPped ffmpeg still holds its hardware codec session, its
//! GPU context and its mapped buffers: on an iGPU, where the *session* is the
//! scarce thing, a suspended producer blocks the live viewer exactly as if it
//! were running. (§4.2's live-session SIGSTOP is a *disk* mechanism on a
//! session whose viewer already owns its slot. The two uses share a syscall and
//! nothing else.)

use std::collections::HashMap;
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

/// Who is asking, and therefore who yields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// Somebody pressed play and is looking at a spinner.
    Live,
    /// The pre-transcode producer. Takes what is spare, gives it back the
    /// moment a live start wants it, and does not take it again until every
    /// live encoder permit has been released.
    Background,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermitOwner {
    Live,
    Background,
}

impl From<Priority> for PermitOwner {
    fn from(priority: Priority) -> Self {
        match priority {
            Priority::Live => PermitOwner::Live,
            Priority::Background => PermitOwner::Background,
        }
    }
}

/// Ownership is one state machine, not several counters sampled in sequence.
///
/// The distinction is what makes the priority policy durable. A live waiter
/// blocks new background permits, an existing background permit blocks the
/// live encoder until that worker has actually terminated, and a live permit
/// keeps background work parked after the waiter itself has gone away. Keeping
/// both hardware and software ownership under this small synchronous mutex also
/// closes the registration race: no caller can observe half of a transition
/// between the two pools.
#[derive(Debug, Default)]
struct PermitState {
    live_waiting: usize,
    hardware_live: usize,
    hardware_background: usize,
    software_live_permits: usize,
    software_background_permits: usize,
    software_live_used: usize,
    software_background_used: usize,
}

impl PermitState {
    fn live_active(&self) -> bool {
        self.hardware_live > 0 || self.software_live_permits > 0
    }

    fn background_active(&self) -> bool {
        self.hardware_background > 0 || self.software_background_permits > 0
    }

    fn hardware_used(&self) -> usize {
        self.hardware_live + self.hardware_background
    }

    fn software_used(&self) -> usize {
        self.software_live_used + self.software_background_used
    }
}

/// A held hardware slot. Releases on drop — which is what keeps the count
/// honest across every way a session can end, including the ones nobody
/// remembers to write a branch for.
#[derive(Debug)]
pub struct HwSlot {
    permits: Arc<Mutex<PermitState>>,
    owner: PermitOwner,
}

impl Drop for HwSlot {
    fn drop(&mut self) {
        let mut permits = self
            .permits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match self.owner {
            PermitOwner::Live => {
                debug_assert!(permits.hardware_live > 0);
                permits.hardware_live = permits.hardware_live.saturating_sub(1);
            }
            PermitOwner::Background => {
                debug_assert!(permits.hardware_background > 0);
                permits.hardware_background = permits.hardware_background.saturating_sub(1);
            }
        }
    }
}

/// A held share of the software CPU pool, denominated in encoder threads.
/// Releases its weight on drop, for the same reason [`HwSlot`] does.
#[derive(Debug)]
pub struct SwPermit {
    permits: Arc<Mutex<PermitState>>,
    owner: PermitOwner,
    weight: usize,
}

impl SwPermit {
    /// The x264 thread budget this permit reserved — what the session is
    /// allowed to spend is exactly what it reserved, one number on purpose.
    pub fn threads(&self) -> usize {
        self.weight
    }
}

impl Drop for SwPermit {
    fn drop(&mut self) {
        let mut permits = self
            .permits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match self.owner {
            PermitOwner::Live => {
                debug_assert!(permits.software_live_permits > 0);
                debug_assert!(permits.software_live_used >= self.weight);
                permits.software_live_permits = permits.software_live_permits.saturating_sub(1);
                permits.software_live_used = permits.software_live_used.saturating_sub(self.weight);
            }
            PermitOwner::Background => {
                debug_assert!(permits.software_background_permits > 0);
                debug_assert!(permits.software_background_used >= self.weight);
                permits.software_background_permits =
                    permits.software_background_permits.saturating_sub(1);
                permits.software_background_used =
                    permits.software_background_used.saturating_sub(self.weight);
            }
        }
    }
}

/// A cloneable handle on the software CPU pool's permits, so a task that has
/// no reach back to [`Admissions`] — the stall watchdog's one-step downgrade
/// runs in a detached task — can still take the permit its fallback owes.
#[derive(Debug, Clone)]
pub struct SwPool {
    permits: Arc<Mutex<PermitState>>,
}

impl SwPool {
    /// Take `weight` threads if they fit — with one deliberate exception: an
    /// empty pool always grants, whatever the weight. On a 2-core box every
    /// session is over budget, and refusing all of them would turn the budget
    /// into a ban; one saturating session is the best that box can do.
    fn try_take(&self, budget: usize, weight: usize, priority: Priority) -> Option<SwPermit> {
        let mut permits = self
            .permits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match priority {
            Priority::Live if permits.background_active() => return None,
            Priority::Background if permits.live_waiting > 0 || permits.live_active() => {
                return None;
            }
            _ => {}
        }
        let used = permits.software_used();
        if used > 0 && used + weight > budget {
            return None;
        }
        match priority {
            Priority::Live => {
                permits.software_live_permits += 1;
                permits.software_live_used += weight;
            }
            Priority::Background => {
                permits.software_background_permits += 1;
                permits.software_background_used += weight;
            }
        }
        drop(permits);
        Some(SwPermit {
            permits: Arc::clone(&self.permits),
            owner: priority.into(),
            weight,
        })
    }

    /// Take `weight` threads unconditionally. For the hardware→software
    /// fallback mid-session: the viewer is already watching, and holding
    /// their film hostage to the budget would turn an accounting rule into a
    /// stall. Overcommit is recorded (the pool goes over budget, and every
    /// later `try_take` sees it) rather than hidden.
    pub fn take_forced(&self, weight: usize) -> SwPermit {
        let mut permits = self
            .permits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        permits.software_live_permits += 1;
        permits.software_live_used += weight;
        drop(permits);
        SwPermit {
            permits: Arc::clone(&self.permits),
            owner: PermitOwner::Live,
            weight,
        }
    }
}

/// A live start queuing for a slot.
///
/// Its *existence* is the yield signal, which is why it is a guard and not a
/// flag somebody sets and clears. A flag left set by a start that failed
/// between setting and clearing would park the producer for the life of the
/// process — silently, since a producer that never runs looks exactly like a
/// producer with nothing to do.
#[derive(Debug)]
pub struct LiveWait {
    permits: Arc<Mutex<PermitState>>,
}

impl Drop for LiveWait {
    fn drop(&mut self) {
        let mut permits = self
            .permits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        debug_assert!(permits.live_waiting > 0);
        permits.live_waiting = permits.live_waiting.saturating_sub(1);
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
    /// A background encoder still owns either pool. The live start must wait
    /// for the producer to checkpoint and terminate even when its preferred
    /// pool has nominal headroom; starting first would recreate the physical
    /// GPU/CPU contention this priority boundary exists to prevent.
    WaitingForBackground,
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
    permits: Arc<Mutex<PermitState>>,
    /// Encoder threads the software pool has handed out. Software transcodes
    /// used to bypass admission entirely — each x264 process picked its own
    /// thread count, and several of them could oversubscribe every core on
    /// the box, dragging every session (and the live viewers') under
    /// realtime together (review §2.4).
    software: SwPool,
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
        let permits = Arc::new(Mutex::new(PermitState::default()));
        Admissions {
            software: SwPool {
                permits: Arc::clone(&permits),
            },
            permits,
            measured: Mutex::new(HashMap::new()),
        }
    }

    pub fn in_use(&self) -> usize {
        self.permits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .hardware_used()
    }

    /// Encoder threads currently reserved from the software pool.
    pub fn software_in_use(&self) -> usize {
        self.permits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .software_used()
    }

    /// A handle a detached task can carry — see [`SwPool`].
    pub fn software_pool(&self) -> SwPool {
        self.software.clone()
    }

    /// Take a software permit if the pool has room, against `budget` threads.
    ///
    /// Background callers are refused while a live start is queuing or a live
    /// encoder is active, exactly as they are for hardware slots. Live callers
    /// likewise wait for existing background ownership to terminate before
    /// crossing pools.
    pub fn try_admit_software(
        &self,
        budget: usize,
        weight: usize,
        priority: Priority,
    ) -> Option<SwPermit> {
        self.software.try_take(budget, weight, priority)
    }

    /// Announce that a live start is queuing. Hold the guard for as long as the
    /// wait lasts; drop it the moment the start has a slot or has given up.
    pub fn wait_for_slot(&self) -> LiveWait {
        let mut permits = self
            .permits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        permits.live_waiting += 1;
        drop(permits);
        LiveWait {
            permits: Arc::clone(&self.permits),
        }
    }

    /// Is a live start queuing right now?
    ///
    /// Background work asks this on a short interval and, when the answer is
    /// yes, checkpoints and terminates. It is the termination signal; durable
    /// ownership after the handoff lives on the returned encoder permit.
    pub fn live_is_waiting(&self) -> bool {
        self.permits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .live_waiting
            > 0
    }

    /// Does background work still own an encoder permit?
    ///
    /// Live admission uses this to distinguish a producer that has not yet
    /// honored preemption from ordinary configured-cap pressure. The former
    /// must not be bypassed by falling across to the other pool.
    pub fn background_is_active(&self) -> bool {
        self.permits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .background_active()
    }

    /// Take a slot if one is free. Capacity and owner change under the same
    /// mutex, which makes two racing starts unable to both succeed on the last
    /// slot and closes the waiter-registration/background-acquisition race.
    ///
    /// Background callers are refused while a live start is queuing or any
    /// live permit remains. Live callers wait while background owns either
    /// pool, even if this hardware counter has numerical headroom.
    pub fn try_acquire(&self, max: usize, priority: Priority) -> Option<HwSlot> {
        let mut permits = self
            .permits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match priority {
            Priority::Live if permits.background_active() => return None,
            Priority::Background if permits.live_waiting > 0 || permits.live_active() => {
                return None;
            }
            _ => {}
        }
        if permits.hardware_used() >= max {
            return None;
        }
        match priority {
            Priority::Live => permits.hardware_live += 1,
            Priority::Background => permits.hardware_background += 1,
        }
        drop(permits);
        Some(HwSlot {
            permits: Arc::clone(&self.permits),
            owner: priority.into(),
        })
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

    /// Decide what a live start that wanted hardware actually gets.
    pub fn admit(&self, max: usize, work: Workload<'_>) -> Admission {
        if let Some(slot) = self.try_acquire(max, Priority::Live) {
            return Admission::Hardware(slot);
        }
        // `try_acquire` performs the authoritative owner check together with
        // its capacity transition. Re-observe only to classify the refusal;
        // in production the caller's LiveWait prevents a producer from
        // appearing in this interval, while a producer that just released is
        // safe to follow into the ordinary software decision.
        if self.background_is_active() {
            return Admission::WaitingForBackground;
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

    /// The x264 thread budget for this shape of work — and, identically, its
    /// weight in the software pool. One number on purpose: what a session is
    /// allowed to spend is exactly what it reserves, so the pool's arithmetic
    /// and the encoder's `-threads` cannot drift apart.
    ///
    /// The encode scales with the *output*, which sets the base; the decode
    /// (and any tone-map) happen at *source* resolution however small the
    /// output is, which is the 4K surcharge — the same asymmetry
    /// [`Workload::class`] encodes for admission speed.
    pub fn software_threads(&self) -> usize {
        let base = match self.target_height {
            h if h <= 480 => 2,
            h if h <= 720 => 3,
            h if h <= 1080 => 4,
            _ => 6,
        };
        if self.source_height >= 2160 {
            base + 2
        } else {
            base
        }
    }
}

/// Threads the software pool may hand out at once on this machine: every
/// core but one. The reserved core is everything that is not a video
/// encoder — the daemon itself, audio transcodes, the copy segmenter, the
/// OS. Floor of two, because a budget of one on a tiny box would refuse work
/// the empty-pool exception exists to admit anyway.
///
/// Passed into [`Admissions::try_admit_software`] by the caller rather than
/// read here, exactly like the hardware cap: the tests hand in fixed numbers
/// so the suite is not an assertion about the build machine's core count.
pub fn software_budget() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .saturating_sub(1)
        .max(2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
                if let Some(slot) = a.try_acquire(2, Priority::Live) {
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
            let _slot = a.try_acquire(1, Priority::Live).expect("first");
            assert_eq!(a.in_use(), 1);
            assert!(
                a.try_acquire(1, Priority::Live).is_none(),
                "full means full"
            );
        }
        assert_eq!(a.in_use(), 0);
        assert!(a.try_acquire(1, Priority::Live).is_some(), "and free again");
    }

    /// Admission is about measured speed, never about output height. A 4K HDR
    /// source is sub-realtime in software however small you ask the output to
    /// be, because the decode and the tone-map happen at source resolution.
    #[test]
    fn a_small_output_does_not_make_a_big_source_cheap() {
        let a = Admissions::new();
        let _held = a.try_acquire(1, Priority::Live).expect("fill the node");

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
        let _held = a.try_acquire(1, Priority::Live).expect("fill the node");
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

    /// A nominally spare hardware slot is not permission to overlap a live
    /// start with background work on the same physical codec block. The
    /// foreground waits for termination first, then acquires; its permit keeps
    /// the producer out after the short-lived waiter is gone.
    #[test]
    fn hardware_background_yields_before_a_real_live_handoff_with_spare_capacity() {
        let a = Admissions::new();
        let producer = a.try_acquire(2, Priority::Background).expect("spare");
        assert_eq!(a.in_use(), 1);
        let live = {
            let _queued = a.wait_for_slot();
            assert_eq!(
                a.admit(2, work(1080, "h264", None, 720)),
                Admission::WaitingForBackground,
                "a numerically free slot must not start beside the producer"
            );
            // `run_part` observes the waiter, checkpoints, terminates ffmpeg,
            // and only then drops this permit.
            drop(producer);
            match a.admit(2, work(1080, "h264", None, 720)) {
                Admission::Hardware(slot) => slot,
                other => panic!("the foreground did not win the handoff: {other:?}"),
            }
        }; // the real start has returned; no waiter is being held by the test
        assert!(!a.live_is_waiting());
        assert!(
            a.try_acquire(2, Priority::Background).is_none(),
            "durable live ownership must keep the producer parked"
        );
        assert!(
            a.try_admit_software(12, 1, Priority::Background).is_none(),
            "background work cannot escape to software while hardware is live"
        );
        drop(live);
        assert!(a.try_acquire(2, Priority::Background).is_some());
    }

    /// Software is the same priority boundary, not a fallback loophole. A
    /// background x264 process must terminate before a live x264 process can
    /// start even when both weights fit the configured pool.
    #[test]
    fn software_background_yields_before_live_and_stays_out_until_release() {
        let a = Admissions::new();
        let producer = a
            .try_admit_software(12, 4, Priority::Background)
            .expect("background fits");
        let live = {
            let _queued = a.wait_for_slot();
            assert!(
                a.try_admit_software(12, 4, Priority::Live).is_none(),
                "spare threads do not bypass background preemption"
            );
            drop(producer);
            a.try_admit_software(12, 4, Priority::Live)
                .expect("live wins after producer termination")
        };
        assert!(!a.live_is_waiting());
        assert!(
            a.try_admit_software(12, 1, Priority::Background).is_none(),
            "the producer must remain parked for the live session's lifetime"
        );
        assert!(
            a.try_acquire(4, Priority::Background).is_none(),
            "live software ownership also parks background hardware work"
        );
        drop(live);
        assert!(a.try_admit_software(12, 1, Priority::Background).is_some());
    }

    /// Registration and background acquisition share one state transition.
    /// Whichever wins a race is visible to the other: either the background
    /// attempt is refused, or the live start waits for and then receives its
    /// released permit. They can never both leave admission holding permits.
    #[test]
    fn a_background_acquisition_racing_waiter_registration_cannot_cross_the_handoff() {
        let a = Arc::new(Admissions::new());
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let background = {
            let a = Arc::clone(&a);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                a.try_acquire(2, Priority::Background)
            })
        };
        barrier.wait();
        let _queued = a.wait_for_slot();
        let raced = background.join().expect("background racer");
        if raced.is_some() {
            assert_eq!(
                a.admit(2, work(1080, "h264", None, 720)),
                Admission::WaitingForBackground
            );
        }
        drop(raced);
        let live = match a.admit(2, work(1080, "h264", None, 720)) {
            Admission::Hardware(slot) => slot,
            other => panic!("live did not acquire after the race settled: {other:?}"),
        };
        assert!(a.try_acquire(2, Priority::Background).is_none());
        drop(live);
    }

    /// The preemption protocol, both halves. A queuing live start is the signal
    /// background work stands down for — and, just as importantly, the signal
    /// that stops it taking the slot straight back. Without the second half a
    /// producer that had just yielded could win the race to its own slot and
    /// the start it yielded to would go on waiting.
    #[test]
    fn a_producer_yields_to_a_queuing_start_and_cannot_take_the_slot_back() {
        let a = Admissions::new();
        let producer = a
            .try_acquire(1, Priority::Background)
            .expect("the node is idle");
        assert!(!a.live_is_waiting(), "nobody has pressed play");

        // Somebody presses play. The node is full, so the start queues.
        let queued = a.wait_for_slot();
        assert!(a.live_is_waiting(), "the producer's cue to stand down");
        assert!(
            a.try_acquire(1, Priority::Background).is_none(),
            "background work must not queue for a slot ahead of a viewer"
        );

        // The producer checkpoints and terminates, which drops its slot…
        drop(producer);
        assert_eq!(a.in_use(), 0);
        // …and cannot immediately re-take it, even though it is free.
        assert!(
            a.try_acquire(1, Priority::Background).is_none(),
            "the producer took back the slot it had just yielded"
        );
        // The waiter gets it.
        let live = a.try_acquire(1, Priority::Live).expect("the viewer wins");
        drop(queued);

        // And while the viewer holds it, background work simply finds it full.
        assert!(a.try_acquire(1, Priority::Background).is_none());
        drop(live);
        assert!(
            a.try_acquire(1, Priority::Background).is_some(),
            "once the viewer is gone the producer may resume"
        );
    }

    /// A start that gives up, fails, or is cancelled must not park background
    /// work forever. The signal is a guard rather than a flag precisely because
    /// a flag left set is invisible — a producer that never runs looks exactly
    /// like a producer with nothing to do.
    #[test]
    fn a_start_that_never_gets_its_slot_still_releases_the_producer() {
        let a = Admissions::new();
        let _producer = a.try_acquire(1, Priority::Background).expect("idle node");
        {
            let _queued = a.wait_for_slot();
            assert!(a.live_is_waiting());
        } // the start times out and returns an error
        assert!(!a.live_is_waiting(), "the queue emptied with the waiter");
    }

    /// Two viewers queuing, one served: the producer stays down until the last
    /// of them is out of the queue. A count rather than a flag is what makes
    /// that true without anybody tracking whose wait is whose.
    #[test]
    fn the_producer_waits_out_every_queuing_start_not_just_the_first() {
        let a = Admissions::new();
        let first = a.wait_for_slot();
        let second = a.wait_for_slot();
        drop(first);
        assert!(a.live_is_waiting(), "one is still queuing");
        assert!(a.try_acquire(4, Priority::Background).is_none());
        drop(second);
        assert!(a.try_acquire(4, Priority::Background).is_some());
    }

    // ---- the software CPU pool (review §2.4) --------------------------------

    /// The pool bounds *concurrent* software encoders in threads, and the
    /// weight comes back however a permit's holder ends — same guard shape,
    /// same reason, as the hardware slots.
    #[test]
    fn the_software_pool_is_a_bound_and_permits_come_back() {
        let a = Admissions::new();
        let first = a
            .try_admit_software(6, 4, Priority::Live)
            .expect("an empty pool admits");
        assert_eq!(a.software_in_use(), 4);
        assert!(
            a.try_admit_software(6, 4, Priority::Live).is_none(),
            "4 + 4 exceeds a budget of 6"
        );
        let second = a
            .try_admit_software(6, 2, Priority::Live)
            .expect("4 + 2 fits exactly");
        assert_eq!(a.software_in_use(), 6);
        drop(first);
        assert_eq!(a.software_in_use(), 2, "weight returns on drop");
        assert!(a.try_admit_software(6, 4, Priority::Live).is_some());
        drop(second);
    }

    /// An empty pool always admits, whatever the weight: on a 2-core box
    /// every session is over budget, and refusing all of them would turn the
    /// budget into a ban. One saturating session is the best that box can do.
    #[test]
    fn an_empty_pool_admits_even_over_budget() {
        let a = Admissions::new();
        let big = a
            .try_admit_software(2, 8, Priority::Live)
            .expect("the only session may saturate the box");
        assert_eq!(a.software_in_use(), 8);
        assert!(
            a.try_admit_software(2, 2, Priority::Live).is_none(),
            "but nothing joins it while it does"
        );
        drop(big);
    }

    /// The mid-film hardware→software fallback takes its threads
    /// unconditionally — a viewer already watching is not held hostage to
    /// the budget — but the overcommit is *recorded*, so later admissions
    /// see a full pool rather than an accounting hole.
    #[test]
    fn a_forced_permit_overcommits_visibly_and_still_returns() {
        let a = Admissions::new();
        let pool = a.software_pool();
        let live = a
            .try_admit_software(6, 4, Priority::Live)
            .expect("a live session");
        let forced = pool.take_forced(6);
        assert_eq!(forced.threads(), 6);
        assert_eq!(a.software_in_use(), 10, "the overcommit is on the books");
        assert!(
            a.try_admit_software(6, 2, Priority::Live).is_none(),
            "and later admissions respect it"
        );
        drop(forced);
        drop(live);
        assert_eq!(a.software_in_use(), 0);
    }

    /// The producer must not spend the cores a viewer is waiting on — the
    /// same preemption rule as hardware, now covering the pool software
    /// starts used to bypass.
    #[test]
    fn background_software_work_yields_to_a_queuing_start() {
        let a = Admissions::new();
        assert!(
            a.try_admit_software(6, 3, Priority::Background).is_some(),
            "spare capacity is anyone's"
        );
        let queued = a.wait_for_slot();
        assert!(
            a.try_admit_software(6, 1, Priority::Background).is_none(),
            "a queuing viewer parks background work"
        );
        assert!(
            a.try_admit_software(6, 1, Priority::Live).is_some(),
            "and never the viewer"
        );
        drop(queued);
    }

    /// The thread budget scales with the encode and pays the 4K decode
    /// surcharge — the same source-vs-output asymmetry as the class key.
    #[test]
    fn software_threads_follow_the_work() {
        assert_eq!(work(1080, "h264", None, 480).software_threads(), 2);
        assert_eq!(work(1080, "h264", None, 720).software_threads(), 3);
        assert_eq!(work(1080, "h264", None, 1080).software_threads(), 4);
        assert_eq!(
            work(2160, "hevc", Some("hdr10"), 1080).software_threads(),
            6,
            "the decode and tone-map happen at source resolution"
        );
        assert!(software_budget() >= 2, "the floor holds on any machine");
    }
}
