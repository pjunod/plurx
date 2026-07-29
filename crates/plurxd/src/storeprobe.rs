//! What this node's storage can actually deliver.
//!
//! Every other part of the pipeline has been measured. The encoder is probed
//! at boot (`encoder.rs`), the tone-map graph is raced against a CPU reference
//! (`pipeprobe.rs`), the output bitrate is bounded and checked (PERF-PLAN
//! §4.6), and the client reports what it received (M0 beacons). The *input*
//! has never been measured at all — and it is the one part of the chain that
//! is usually not local, not fast, and not constant.
//!
//! That gap has a cost. A remux copies the source untouched, so a 69 Mb/s file
//! needs 8.6 MB/s off storage forever just to break even, and `-readrate 4`
//! asks for 34 MB/s. Whether either is available is a question nobody was
//! asking: when the answer was no, it surfaced as a *client* stall, which sent
//! everyone to look at the encoder and the browser. Two numbers per mount —
//! how fast it reads, and how long a cold seek takes — separate "the link
//! can't carry it" from "the box can't make it" before anyone reads a log.
//!
//! ## Measuring honestly
//!
//! The trap is the page cache. A probe that writes its own fixture and reads
//! it back measures RAM and reports a NAS at 6 GB/s. So this reads a *real
//! media file already in the library*, at offsets chosen deep inside it, and:
//!
//! 1. asks the kernel to drop that range first (`posix_fadvise(DONTNEED)`),
//!    which is decisive on local storage and harmless elsewhere;
//! 2. samples twice, at unrelated offsets, and keeps the **lower** result —
//!    one warm region cannot flatter the answer;
//! 3. flags anything faster than any real disk as cache-warm rather than
//!    quietly reporting it, because a number that good is a measurement
//!    failure and should read as one.
//!
//! It never writes to the library, never reads more than [`SAMPLE_BYTES`] per
//! sample, and gives up rather than walking a huge tree looking for something
//! to read. A diagnostic that costs real I/O on a spinning array is one people
//! turn off.
//!
//! ## Per mount, not per library
//!
//! Two libraries on the same mount share one set of spindles and one link, so
//! measuring both would take twice the I/O to produce the same number twice.
//! Roots are grouped by device id and sampled once each.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;

/// Bytes per throughput sample. Big enough to leave the readahead window and
/// any small cache behind, small enough that probing a cold array is a blip
/// rather than an event.
const SAMPLE_BYTES: u64 = 64 * 1024 * 1024;
/// Read size within a sample.
const CHUNK_BYTES: usize = 4 * 1024 * 1024;
/// Samples per mount, at unrelated offsets. The lowest one is the answer.
const SAMPLES: usize = 2;
/// Only files at least this big are worth sampling — the offsets have to be
/// deep enough that nothing playback touched is still resident.
const MIN_FILE_BYTES: u64 = 512 * 1024 * 1024;
/// Stop hunting for a file to sample after this long...
const FIND_BUDGET: Duration = Duration::from_secs(4);
/// ...or after this many directory entries, whichever comes first.
const FIND_ENTRIES: usize = 6000;
/// How deep to descend looking for one. Movie libraries are `root/Title/file`;
/// shows are `root/Show/Season/file`. Four covers both with room to spare.
const FIND_DEPTH: usize = 4;
/// Cold-seek probes, and how much to read at each. Odd so the median is a
/// measurement rather than an average of two.
const SEEK_PROBES: usize = 9;
const SEEK_READ_BYTES: usize = 64 * 1024;
/// Above this, a single-threaded read is timing memcpy rather than a device.
///
/// Set high on purpose. The two ranges genuinely overlap — a fast NVMe and a
/// page-cache hit are not far apart through this code path — and a threshold
/// low enough to catch every cached read would permanently label an ordinary
/// SATA SSD (550 MB/s ≈ 4.4 Gb/s) as "not a measurement", which is a worse lie
/// in the other direction. 64 Gb/s is past any single-stream device read and
/// squarely in RAM territory, so what it catches is the egregious case and
/// nothing else.
const CACHE_SUSPECT_BPS: f64 = 64e9;

/// What the storage under each library root can do. Absent numbers are normal
/// and mean "could not measure", never "zero".
#[derive(Clone, Debug, Default, Serialize)]
pub struct StorageReport {
    /// False before the probe has run — which is its state for the first few
    /// seconds of every boot, and forever if it was never asked to.
    pub ran: bool,
    pub mounts: Vec<MountSample>,
    /// Unix seconds. A NAS reachable over a contended link is not the same
    /// storage minute to minute, so a number with no timestamp is a claim the
    /// probe cannot support.
    pub measured_at: Option<i64>,
}

impl StorageReport {
    /// The mount `path` lives on, by longest matching root.
    ///
    /// Longest and not first: a library at `/20t` and another at `/20t/4k` are
    /// different mounts often enough that picking either arbitrarily would
    /// attribute a file to the wrong storage — and the whole value of the
    /// number is that it belongs to the device this file is actually on.
    pub fn for_path(&self, path: &Path) -> Option<&MountSample> {
        self.mounts
            .iter()
            .filter_map(|m| {
                m.roots
                    .iter()
                    .filter(|r| path.starts_with(r))
                    .map(|r| r.len())
                    .max()
                    .map(|len| (len, m))
            })
            .max_by_key(|(len, _)| *len)
            .map(|(_, m)| m)
    }
}

/// One mount's numbers.
#[derive(Clone, Debug, Serialize)]
pub struct MountSample {
    /// Every library root that resolves to this device.
    pub roots: Vec<String>,
    /// Sustained sequential read, bits per second — the same unit as a source
    /// bitrate, so the two can be compared without arithmetic.
    pub read_bps: Option<f64>,
    /// Median time to the first bytes of a read at an untouched offset. This
    /// is what a seek costs before ffmpeg can emit anything, and on a remote
    /// mount it is frequently the number that explains a slow scrub.
    pub seek_ms: Option<f64>,
    /// Which file the numbers came from, and how much of it was read.
    pub sampled: Option<String>,
    pub sampled_bytes: u64,
    /// The throughput was too high to be storage. Kept and shown rather than
    /// discarded: "this file was already in RAM" is itself worth knowing.
    pub cache_suspect: bool,
    /// Why a number is missing.
    pub note: Option<String>,
}

impl MountSample {
    /// How many times realtime this mount can feed a source of `bitrate_bps`.
    ///
    /// The whole point of the measurement: `1.0` means a direct play or remux
    /// of that file has no margin at all, and anything under it means the
    /// source cannot be read as fast as it must be played — which no amount of
    /// client buffering, encoder tuning or pacing can fix.
    pub fn realtime_multiple(&self, bitrate_bps: f64) -> Option<f64> {
        let read = self.read_bps?;
        if bitrate_bps <= 0.0 {
            return None;
        }
        Some(read / bitrate_bps)
    }
}

/// Measure every distinct device under `roots`.
///
/// Blocking work, so it runs on the blocking pool: a probe of a stalled NFS
/// mount can sit in `read` for a long time and must not be holding a runtime
/// worker while it does.
pub async fn probe(roots: Vec<PathBuf>) -> StorageReport {
    tokio::task::spawn_blocking(move || probe_blocking(&roots))
        .await
        .unwrap_or_default()
}

fn probe_blocking(roots: &[PathBuf]) -> StorageReport {
    let mut by_device: BTreeMap<u64, Vec<PathBuf>> = BTreeMap::new();
    let mut unreadable: Vec<PathBuf> = Vec::new();
    for root in roots {
        match std::fs::metadata(root) {
            Ok(md) => by_device.entry(md.dev()).or_default().push(root.clone()),
            Err(_) => unreadable.push(root.clone()),
        }
    }

    let mut mounts: Vec<MountSample> = by_device
        .into_values()
        .map(|group| sample_device(&group))
        .collect();
    for root in unreadable {
        mounts.push(MountSample {
            roots: vec![root.display().to_string()],
            read_bps: None,
            seek_ms: None,
            sampled: None,
            sampled_bytes: 0,
            cache_suspect: false,
            note: Some("path does not resolve — unmounted, or not visible to plurx".to_owned()),
        });
    }

    StorageReport {
        ran: true,
        mounts,
        measured_at: Some(now_secs()),
    }
}

fn sample_device(roots: &[PathBuf]) -> MountSample {
    let names: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
    let Some((path, size)) = find_sample_file(roots) else {
        return MountSample {
            roots: names,
            read_bps: None,
            seek_ms: None,
            sampled: None,
            sampled_bytes: 0,
            cache_suspect: false,
            note: Some(format!(
                "no file of at least {} MB found to read — nothing to measure",
                MIN_FILE_BYTES / (1024 * 1024)
            )),
        };
    };

    let mut best: Option<f64> = None;
    let mut read_total = 0u64;
    let mut err: Option<String> = None;
    for i in 0..SAMPLES {
        match throughput_at(&path, size, i) {
            Ok((bps, bytes)) => {
                read_total += bytes;
                // The LOWER of the samples, not the higher and not the mean:
                // the question is what this mount can be relied on for, and a
                // region that happened to be resident answers a different one.
                best = Some(best.map_or(bps, |b: f64| b.min(bps)));
            }
            Err(e) => err = Some(e),
        }
    }
    let seek_ms = seek_latency(&path, size).ok();

    let cache_suspect = best.is_some_and(|b| b > CACHE_SUSPECT_BPS);
    MountSample {
        roots: names,
        read_bps: best,
        seek_ms,
        sampled: Some(path.display().to_string()),
        sampled_bytes: read_total,
        cache_suspect,
        note: if cache_suspect {
            Some("faster than any real device — this range was already in RAM, so treat it as a floor, not a measurement".to_owned())
        } else {
            best.is_none()
                .then(|| err.unwrap_or_else(|| "read failed".to_owned()))
        },
    }
}

/// Breadth-first, bounded three ways, and it stops at the first file big
/// enough. Finding *a* representative file matters; finding the biggest one
/// would mean walking the whole library at every boot.
fn find_sample_file(roots: &[PathBuf]) -> Option<(PathBuf, u64)> {
    let deadline = Instant::now() + FIND_BUDGET;
    let mut seen = 0usize;
    let mut queue: Vec<(PathBuf, usize)> = roots.iter().map(|r| (r.clone(), 0usize)).collect();
    let mut next: Vec<(PathBuf, usize)> = Vec::new();

    while !queue.is_empty() {
        for (dir, depth) in queue.drain(..) {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                seen += 1;
                if seen > FIND_ENTRIES || Instant::now() > deadline {
                    return None;
                }
                let Ok(ft) = entry.file_type() else { continue };
                if ft.is_dir() {
                    if depth < FIND_DEPTH {
                        next.push((entry.path(), depth + 1));
                    }
                } else if ft.is_file() {
                    if let Ok(md) = entry.metadata() {
                        if md.len() >= MIN_FILE_BYTES {
                            return Some((entry.path(), md.len()));
                        }
                    }
                }
            }
        }
        std::mem::swap(&mut queue, &mut next);
    }
    None
}

/// One throughput sample: drop the range, then read it and time it.
///
/// `nth` spreads the samples across the back half of the file. The opening is
/// deliberately avoided — it is the one region a scan, a probe, or a playback
/// start is likely to have left resident.
fn throughput_at(path: &Path, size: u64, nth: usize) -> Result<(f64, u64), String> {
    let span = size.saturating_sub(SAMPLE_BYTES);
    if span == 0 {
        return Err("file is smaller than one sample".to_owned());
    }
    // 45% and 75% in, for two samples: far apart, both well clear of the head.
    let spread = SAMPLES.saturating_sub(1).max(1) as f64;
    let frac = 0.45 + 0.30 * (nth as f64 / spread);
    let offset = ((span as f64) * frac) as u64;

    let mut f = File::open(path).map_err(|e| e.to_string())?;
    drop_cache(&f, offset, SAMPLE_BYTES);
    f.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;

    let mut buf = vec![0u8; CHUNK_BYTES];
    let mut read = 0u64;
    let started = Instant::now();
    while read < SAMPLE_BYTES {
        let want = ((SAMPLE_BYTES - read) as usize).min(CHUNK_BYTES);
        match f.read(&mut buf[..want]) {
            Ok(0) => break,
            Ok(n) => read += n as u64,
            Err(e) => return Err(e.to_string()),
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    if read == 0 || elapsed <= 0.0 {
        return Err("read returned nothing".to_owned());
    }
    Ok(((read * 8) as f64 / elapsed, read))
}

/// Median time to the first bytes at an untouched offset.
///
/// Nine probes rather than one because a single cold seek on a remote mount is
/// as likely to catch a retransmit as a representative round trip, and the
/// outlier would then be the whole answer.
fn seek_latency(path: &Path, size: u64) -> Result<f64, String> {
    let mut f = File::open(path).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; SEEK_READ_BYTES];
    let mut samples: Vec<f64> = Vec::with_capacity(SEEK_PROBES);
    for i in 0..SEEK_PROBES {
        // Spread across the file, and deliberately not in reading order: a
        // forward-walking pattern is exactly what readahead is built to make
        // look fast, which would turn a seek probe into a throughput probe.
        let frac = ((i * 7) % SEEK_PROBES) as f64 / SEEK_PROBES as f64;
        let offset = ((size.saturating_sub(SEEK_READ_BYTES as u64)) as f64 * frac) as u64;
        drop_cache(&f, offset, SEEK_READ_BYTES as u64);
        if f.seek(SeekFrom::Start(offset)).is_err() {
            continue;
        }
        let started = Instant::now();
        match f.read(&mut buf) {
            Ok(n) if n > 0 => samples.push(started.elapsed().as_secs_f64() * 1000.0),
            _ => continue,
        }
    }
    if samples.is_empty() {
        return Err("no seek completed".to_owned());
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Ok(samples[samples.len() / 2])
}

/// Ask the kernel to forget a range so the next read is a real one.
///
/// Advisory by definition: it does what it can on local filesystems and is
/// ignored on some remote ones, which is why the deep offsets and the
/// keep-the-lower-sample rule carry the measurement rather than this.
#[cfg(target_os = "linux")]
fn drop_cache(f: &File, offset: u64, len: u64) {
    use std::os::unix::io::AsRawFd;
    // Safety: a valid fd owned by `f`, and POSIX_FADV_DONTNEED only ever drops
    // clean cache — it cannot lose data or change the file.
    unsafe {
        libc::posix_fadvise(
            f.as_raw_fd(),
            offset as libc::off_t,
            len as libc::off_t,
            libc::POSIX_FADV_DONTNEED,
        );
    }
}

#[cfg(not(target_os = "linux"))]
fn drop_cache(_f: &File, _offset: u64, _len: u64) {}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mount_with_no_number_answers_no_multiple_rather_than_zero() {
        let m = MountSample {
            roots: vec!["/20t".to_owned()],
            read_bps: None,
            seek_ms: None,
            sampled: None,
            sampled_bytes: 0,
            cache_suspect: false,
            note: Some("unmounted".to_owned()),
        };
        // Zero would read as "this mount cannot feed anything", which is a
        // much stronger claim than "nobody measured it".
        assert_eq!(m.realtime_multiple(69e6), None);
    }

    #[test]
    fn realtime_multiple_is_read_over_bitrate() {
        let m = MountSample {
            roots: vec!["/20t".to_owned()],
            read_bps: Some(276e6),
            seek_ms: Some(4.0),
            sampled: None,
            sampled_bytes: 0,
            cache_suspect: false,
            note: None,
        };
        let x = m.realtime_multiple(69e6).expect("has a read rate");
        assert!((x - 4.0).abs() < 1e-6, "{x}");
        // And a source it cannot keep up with reports under 1.0 rather than
        // clamping — the number below the line is the whole finding.
        assert!(m.realtime_multiple(400e6).expect("has a read rate") < 1.0);
    }

    #[test]
    fn a_probe_of_a_directory_with_only_small_files_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("small.mkv"), b"not a movie").expect("write");
        let s = sample_device(&[dir.path().to_path_buf()]);
        assert!(s.read_bps.is_none());
        assert!(
            s.note
                .as_deref()
                .unwrap_or_default()
                .contains("nothing to measure"),
            "{:?}",
            s.note
        );
    }

    #[test]
    fn a_root_that_does_not_resolve_is_reported_not_skipped() {
        // The silent version of this is the bug it exists to prevent: an
        // unmounted share vanishing from the report reads as "no storage
        // problems found".
        let r = probe_blocking(&[PathBuf::from("/definitely/not/here")]);
        assert!(r.ran);
        assert_eq!(r.mounts.len(), 1);
        assert!(r.mounts[0].read_bps.is_none());
        assert!(r.mounts[0]
            .note
            .as_deref()
            .unwrap_or_default()
            .contains("does not resolve"));
    }

    /// The measurement path end to end, against a real file on a real
    /// filesystem. Unit tests of the arithmetic pass just as happily when the
    /// reads never happen, which is the failure worth catching: a probe that
    /// silently measures nothing is indistinguishable in the report from one
    /// that measured fine.
    ///
    /// `#[ignore]` because it writes and reads most of a gigabyte. Run with
    /// `cargo test -p plurxd -- --ignored storeprobe`.
    #[test]
    #[ignore = "writes ~600MB"]
    fn a_real_file_yields_a_real_throughput_and_seek_number() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fixture.mkv");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&path).expect("create");
            // Incompressible-ish and cheap: a filesystem that transparently
            // compresses would otherwise turn this into a test of zeroes.
            let mut block = vec![0u8; 4 * 1024 * 1024];
            for (i, b) in block.iter_mut().enumerate() {
                *b = (i % 251) as u8;
            }
            for i in 0..150u64 {
                block[0] = i as u8;
                f.write_all(&block).expect("write");
            }
            f.sync_all().expect("sync");
        }
        let s = sample_device(&[dir.path().to_path_buf()]);
        assert_eq!(s.sampled.as_deref(), Some(path.to_str().expect("utf8")));
        let bps = s.read_bps.expect("a throughput number");
        assert!(bps > 0.0, "{bps}");
        assert_eq!(s.sampled_bytes, SAMPLE_BYTES * SAMPLES as u64);
        let seek = s.seek_ms.expect("a seek number");
        assert!((0.0..60_000.0).contains(&seek), "{seek}");
    }

    #[test]
    fn roots_on_one_device_are_sampled_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir_all(&a).expect("mkdir");
        std::fs::create_dir_all(&b).expect("mkdir");
        let r = probe_blocking(&[a.clone(), b.clone()]);
        assert_eq!(r.mounts.len(), 1, "same device, one sample");
        assert_eq!(r.mounts[0].roots.len(), 2);
    }
}
