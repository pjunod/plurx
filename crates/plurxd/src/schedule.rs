//! When the background jobs are due.
//!
//! The deciding is pure — [`due_jobs`] takes a clock reading and some rows and
//! returns what should run — so the awkward cases (both jobs due at once, a
//! clock that jumped, a library whose interval was just turned off) are testable
//! without a running server, a real store, or waiting an hour. The loop that
//! calls it lives in [`crate::state::JobManager::schedule_loop`] and does
//! nothing but ask, dispatch, and stamp.
//!
//! Intervals are minutes and `0` means off, everywhere. Off is the default for
//! every job, so upgrading a server changes nothing about what it does at 3am
//! until someone asks for it — with one deliberate exception, the artwork
//! retry, whose default and reasons live with the setting itself
//! (`plurx_core::store::keys::ARTWORK_RETRY_DEFAULT_MINS`).

use plurx_core::domain::Library;

/// A job the scheduler wants run now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DueJob {
    /// Incremental scan + the library's normal enrichment pass.
    Scan(i64),
    /// The heavy job: rescan *and* re-fetch metadata for already-matched items.
    Refresh(i64),
    /// Re-probe files whose media details were never read (server-wide).
    RetryProbes,
    /// Re-fetch artwork for enriched items that still have no poster
    /// (server-wide). The sibling of [`RetryProbes`](DueJob::RetryProbes): a
    /// download that failed leaves nothing on disk for a rescan to notice, so
    /// without a job that goes looking, nothing ever tries again.
    RetryArtwork,
    /// Delete transcode working directories no live session owns.
    CleanupTranscode,
    /// Pre-transcode what somebody is likely to play next (PERF-PLAN §6.2).
    ProduceCache,
}

/// Server-wide job intervals and their last-run stamps, in minutes and unix
/// seconds. `None` for a stamp means "never run", which counts as due.
#[derive(Debug, Clone, Copy, Default)]
pub struct GlobalSchedule {
    pub probe_retry_mins: i64,
    pub last_probe_retry: Option<i64>,
    pub artwork_retry_mins: i64,
    pub last_artwork_retry: Option<i64>,
    pub transcode_cleanup_mins: i64,
    pub last_transcode_cleanup: Option<i64>,
    pub cache_produce_mins: i64,
    pub last_cache_produce: Option<i64>,
}

/// Is a job with this interval due, given when it last ran?
///
/// A `last` in the future — a clock that was wrong and got corrected, a restore
/// from a backup taken on another machine — would otherwise park the job for as
/// long as the jump was large. Treating it as due costs one extra scan and
/// unsticks the schedule, which is the better failure.
fn due(now: i64, last: Option<i64>, interval_mins: i64) -> bool {
    if interval_mins <= 0 {
        return false;
    }
    match last {
        None => true,
        Some(last) if last > now => true,
        Some(last) => now - last >= interval_mins * 60,
    }
}

/// Everything that should start right now. Libraries currently scanning are the
/// caller's problem to filter — `trigger_scan` already refuses a second run, and
/// deciding that here would mean threading live state into a pure function.
pub fn due_jobs(now: i64, libraries: &[Library], global: GlobalSchedule) -> Vec<DueJob> {
    let mut jobs = Vec::new();
    for lib in libraries {
        // A refresh does everything a scan does and more, so when both are due
        // the refresh wins and the scan is not also queued. Running both would
        // walk the whole library twice for one due moment.
        if due(now, lib.last_refresh_at, lib.refresh_interval_mins) {
            jobs.push(DueJob::Refresh(lib.id));
        } else if due(now, lib.last_scan_at, lib.scan_interval_mins) {
            jobs.push(DueJob::Scan(lib.id));
        }
    }
    if due(now, global.last_probe_retry, global.probe_retry_mins) {
        jobs.push(DueJob::RetryProbes);
    }
    if due(now, global.last_artwork_retry, global.artwork_retry_mins) {
        jobs.push(DueJob::RetryArtwork);
    }
    if due(
        now,
        global.last_transcode_cleanup,
        global.transcode_cleanup_mins,
    ) {
        jobs.push(DueJob::CleanupTranscode);
    }
    // Last, and that is deliberate rather than incidental. This is the only
    // job that competes with live playback for the hardware, so it goes behind
    // everything else a due moment asked for.
    if due(now, global.last_cache_produce, global.cache_produce_mins) {
        jobs.push(DueJob::ProduceCache);
    }
    jobs
}

#[cfg(test)]
mod tests {
    use super::*;
    use plurx_core::domain::LibraryKind;

    fn lib(
        id: i64,
        scan: i64,
        last_scan: Option<i64>,
        refresh: i64,
        last_refresh: Option<i64>,
    ) -> Library {
        Library {
            id,
            name: format!("lib{id}"),
            kind: LibraryKind::Movies,
            paths: vec![],
            anime: false,
            created_at: 0,
            scan_interval_mins: scan,
            refresh_interval_mins: refresh,
            last_scan_at: last_scan,
            last_refresh_at: last_refresh,
        }
    }

    const HOUR: i64 = 3600;
    const NOW: i64 = 1_700_000_000;

    #[test]
    fn zero_means_off_and_off_is_the_default() {
        let libs = [lib(1, 0, None, 0, None)];
        assert!(due_jobs(NOW, &libs, GlobalSchedule::default()).is_empty());
        // Even a library that has never been scanned stays alone when its
        // interval is off — "never scanned" is not by itself a reason to scan.
        assert!(due_jobs(NOW, &[lib(1, 0, None, 0, None)], GlobalSchedule::default()).is_empty());
    }

    #[test]
    fn a_scheduled_library_runs_when_the_interval_has_elapsed() {
        // 60-minute interval, scanned 59 minutes ago → not yet.
        let libs = [lib(1, 60, Some(NOW - 59 * 60), 0, None)];
        assert!(due_jobs(NOW, &libs, GlobalSchedule::default()).is_empty());
        // Exactly on the hour → due. (`>=`, so a tick landing precisely on the
        // boundary doesn't wait another whole cycle.)
        let libs = [lib(1, 60, Some(NOW - HOUR), 0, None)];
        assert_eq!(
            due_jobs(NOW, &libs, GlobalSchedule::default()),
            [DueJob::Scan(1)]
        );
        // Never scanned, interval set → run now.
        let libs = [lib(1, 60, None, 0, None)];
        assert_eq!(
            due_jobs(NOW, &libs, GlobalSchedule::default()),
            [DueJob::Scan(1)]
        );
    }

    #[test]
    fn refresh_wins_when_both_are_due() {
        let libs = [lib(
            1,
            60,
            Some(NOW - 2 * HOUR),
            1440,
            Some(NOW - 48 * HOUR),
        )];
        assert_eq!(
            due_jobs(NOW, &libs, GlobalSchedule::default()),
            [DueJob::Refresh(1)],
            "a refresh already walks the library; queuing the scan too walks it twice"
        );
    }

    #[test]
    fn a_clock_that_jumped_backwards_does_not_park_the_schedule() {
        // Stamped an hour into the future (clock corrected, or a restored DB).
        let libs = [lib(1, 60, Some(NOW + HOUR), 0, None)];
        assert_eq!(
            due_jobs(NOW, &libs, GlobalSchedule::default()),
            [DueJob::Scan(1)]
        );
    }

    #[test]
    fn global_jobs_have_their_own_clocks() {
        let global = GlobalSchedule {
            probe_retry_mins: 1440,
            last_probe_retry: Some(NOW - 23 * HOUR),
            transcode_cleanup_mins: 360,
            last_transcode_cleanup: Some(NOW - 7 * HOUR),
            ..Default::default()
        };
        assert_eq!(due_jobs(NOW, &[], global), [DueJob::CleanupTranscode]);

        let global = GlobalSchedule {
            probe_retry_mins: 1440,
            last_probe_retry: None,
            transcode_cleanup_mins: 0,
            last_transcode_cleanup: None,
            ..Default::default()
        };
        assert_eq!(due_jobs(NOW, &[], global), [DueJob::RetryProbes]);
    }

    /// The producer competes with live playback for the hardware, so when a
    /// tick has other work it goes behind that work. It is also off by default
    /// like every other job — an upgraded server must not start encoding
    /// overnight because somebody installed a new build.
    #[test]
    fn the_producer_is_off_by_default_and_goes_last_when_it_is_not() {
        assert!(
            !due_jobs(NOW, &[], GlobalSchedule::default()).contains(&DueJob::ProduceCache),
            "an unconfigured server does not pre-transcode"
        );
        let global = GlobalSchedule {
            probe_retry_mins: 1440,
            last_probe_retry: None,
            cache_produce_mins: 360,
            last_cache_produce: None,
            ..Default::default()
        };
        assert_eq!(
            due_jobs(NOW, &[lib(1, 60, None, 0, None)], global),
            [DueJob::Scan(1), DueJob::RetryProbes, DueJob::ProduceCache]
        );
    }

    #[test]
    fn several_libraries_are_independent() {
        let libs = [
            lib(1, 60, Some(NOW - 2 * HOUR), 0, None),
            lib(2, 60, Some(NOW - 5 * 60), 0, None),
            lib(3, 0, None, 0, None),
        ];
        assert_eq!(
            due_jobs(NOW, &libs, GlobalSchedule::default()),
            [DueJob::Scan(1)]
        );
    }

    /// The artwork retry runs on the default interval — the point of it being
    /// the one job that is on by default is that a server nobody has
    /// configured drains its own backlog of blank posters.
    #[test]
    fn the_artwork_retry_runs_on_its_default_interval() {
        let global = GlobalSchedule {
            artwork_retry_mins: plurx_core::store::keys::ARTWORK_RETRY_DEFAULT_MINS,
            last_artwork_retry: None,
            ..GlobalSchedule::default()
        };
        assert_eq!(due_jobs(NOW, &[], global), [DueJob::RetryArtwork]);

        // Half an hour is the interval, so 29 minutes ago is not yet.
        let global = GlobalSchedule {
            artwork_retry_mins: 30,
            last_artwork_retry: Some(NOW - 29 * 60),
            ..GlobalSchedule::default()
        };
        assert!(due_jobs(NOW, &[], global).is_empty());

        // And an admin who turns it off keeps it off.
        let global = GlobalSchedule {
            artwork_retry_mins: 0,
            last_artwork_retry: None,
            ..GlobalSchedule::default()
        };
        assert!(due_jobs(NOW, &[], global).is_empty());
    }
}
