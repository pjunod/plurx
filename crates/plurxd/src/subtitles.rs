//! Persistent extraction cache for embedded text subtitles.
//!
//! ffmpeg/libass must read an embedded text track to EOF before it can render
//! the first frame. On a large MKV over a NAS that can exceed a player's
//! preparation timeout. Extract once to a small WebVTT sidecar: the web
//! subtitle endpoint always uses it, and burned transcodes reuse it for simple
//! text codecs whose authored styling is not lost by WebVTT conversion.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use plurx_core::domain::MediaFile;

use crate::ffmpeg::ffmpeg_bin;

/// Entries the subtitle cache may hold before the oldest are trimmed, and how
/// far a trim takes it. WebVTT files are normally only kilobytes.
const MAX_ENTRIES: usize = 256;
const TRIM_TO: usize = 224;

/// How long one extraction may run before it is abandoned and its ffmpeg
/// killed. A cold extraction is a full-source read: on a large MKV over a
/// congested NAS that has legitimately taken ~180 s, so the bound is set well
/// clear of it — 10 minutes is over three times the worst honest case, which
/// means nothing that would have succeeded is cut off, while a wedged child
/// (a stalled mount that never returns bytes and never errors) can no longer
/// park every waiter forever.
const EXTRACTION_TIMEOUT: Duration = Duration::from_secs(600);

/// How long a failure is remembered. AVPlayer asks for a VTT segment roughly
/// every segment duration (~6 s), and before this memo each of those requests
/// relaunched a full-source read of a file that had just failed. Two minutes
/// turns that storm into one attempt per window — about a twentieth of the
/// scans — while still being short enough that fixing the cause (remounting
/// the NAS, replacing the file) is picked up inside a viewer's patience
/// rather than needing a restart.
const NEGATIVE_TTL: Duration = Duration::from_secs(120);

/// Failures remembered at once. A memo is a path, a deadline and a short
/// message, so the cap is about refusing unbounded growth rather than saving
/// bytes: a library full of broken tracks must not turn the memo into a leak.
const MAX_NEGATIVE_ENTRIES: usize = 128;

/// The largest sidecar publish will accept. Real WebVTT is kilobytes — a
/// dense SDH track for a three-hour film lands near 200 KB — and this file is
/// re-read whole for every subtitle segment request, so the cap is chosen to
/// stay cheap to re-read while leaving two orders of magnitude of headroom
/// above anything legitimate. Above it the track is pathological (a
/// mislabelled stream, a runaway conversion) and is refused rather than
/// allowed to fill the cache directory.
const MAX_SIDECAR_BYTES: u64 = 8 * 1024 * 1024;

/// The bounds an extraction runs under. Held in one struct so tests can shrink
/// all three to something a suite can prove in milliseconds, while production
/// keeps the constants above.
#[derive(Clone, Copy)]
struct ExtractionLimits {
    timeout: Duration,
    negative_ttl: Duration,
    max_sidecar_bytes: u64,
}

impl Default for ExtractionLimits {
    fn default() -> Self {
        Self {
            timeout: EXTRACTION_TIMEOUT,
            negative_ttl: NEGATIVE_TTL,
            max_sidecar_bytes: MAX_SIDECAR_BYTES,
        }
    }
}

/// The stable path for one source fingerprint and subtitle stream.
pub fn vtt_path(dir: &Path, file: &MediaFile, index: i64) -> PathBuf {
    dir.join(vtt_name(file.id, index, file.size, file.mtime))
}

fn vtt_name(file_id: i64, index: i64, size: i64, mtime: i64) -> String {
    format!("f{file_id}-s{index}-{size}-{mtime}.vtt")
}

struct Extraction {
    result: tokio::sync::Mutex<Option<Result<PathBuf, String>>>,
    ready: tokio::sync::Notify,
}

type Extractions = tokio::sync::Mutex<HashMap<PathBuf, Arc<Extraction>>>;

fn extractions() -> &'static Extractions {
    static EXTRACTIONS: OnceLock<Extractions> = OnceLock::new();
    EXTRACTIONS.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

/// Why a cache key failed, and when that answer stops being reused.
struct NegativeMemo {
    why: String,
    expires_at: tokio::time::Instant,
}

type NegativeMemos = tokio::sync::Mutex<HashMap<PathBuf, NegativeMemo>>;

fn negative_memos() -> &'static NegativeMemos {
    static MEMOS: OnceLock<NegativeMemos> = OnceLock::new();
    MEMOS.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

/// The remembered failure for a key, if one is still inside its window. The
/// clock is tokio's, so a paused-clock test moves it without waiting.
async fn remembered_failure(cached: &Path) -> Option<String> {
    let memos = negative_memos().lock().await;
    let memo = memos.get(cached)?;
    if memo.expires_at > tokio::time::Instant::now() {
        Some(memo.why.clone())
    } else {
        None
    }
}

/// Remember a failure, evicting to stay under the cap: expired entries first,
/// since they are already dead weight, and only then the memo closest to
/// expiry — the one whose loss costs the fewest avoided rescans.
async fn remember_failure(cached: &Path, why: &str, ttl: Duration) {
    let mut memos = negative_memos().lock().await;
    let now = tokio::time::Instant::now();
    if memos.len() >= MAX_NEGATIVE_ENTRIES {
        memos.retain(|_, memo| memo.expires_at > now);
    }
    while memos.len() >= MAX_NEGATIVE_ENTRIES {
        let Some(soonest) = memos
            .iter()
            .min_by_key(|(_, memo)| memo.expires_at)
            .map(|(path, _)| path.clone())
        else {
            break;
        };
        memos.remove(&soonest);
    }
    memos.insert(
        cached.to_owned(),
        NegativeMemo {
            why: why.to_owned(),
            expires_at: now + ttl,
        },
    );
}

/// A published sidecar is the truth about a key; any memory of it failing is
/// stale the moment that happens.
async fn forget_failure(cached: &Path) {
    negative_memos().lock().await.remove(cached);
}

/// What a request should do about one cache key, decided under the registry
/// lock. See [`enlist`] for why that lock is the only place it can be decided.
enum Flight {
    /// The sidecar is already on disk. Nothing to run, nothing to wait for.
    Published,
    /// Someone else is extracting this key; wait on their result.
    Join(Arc<Extraction>),
    /// This request registered the key and owes everyone an extraction.
    Own(Arc<Extraction>),
    /// A recent failure for this key is still inside its window.
    Remembered(String),
}

/// Decide, atomically, whether a cold request joins an extraction or starts
/// one.
///
/// The unlocked cache read that precedes this is only a snapshot, and an
/// extraction that finishes between that read and this call leaves *nothing
/// to join*: the owner renames its sidecar into place and then drops its
/// registry entry, so a request landing in that window sees an absent file
/// and an absent flight, and would start a second run of work already done —
/// which is precisely the extraction a deduplicated request must never
/// perform.
///
/// The registry lock is the only ordering point that closes it. Because the
/// rename happens-before the entry is removed, and the removal happens-before
/// this lock is acquired, a key with no flight registered here is a key whose
/// sidecar is either visible on disk right now or genuinely absent. So the
/// cache is read again, under the lock, before anyone is allowed to own the
/// work. (A failed extraction leaves neither, and the next request correctly
/// retries it.)
async fn enlist(cached: &Path) -> Flight {
    let mut active = extractions().lock().await;
    if let Some(flight) = active.get(cached) {
        return Flight::Join(Arc::clone(flight));
    }
    if tokio::fs::metadata(cached).await.is_ok() {
        return Flight::Published;
    }
    // Disk beats the memo: a published sidecar is the truth about a key, and
    // `forget_failure` may not have run yet.
    if let Some(why) = remembered_failure(cached).await {
        return Flight::Remembered(why);
    }
    let flight = Arc::new(Extraction {
        result: tokio::sync::Mutex::new(None),
        ready: tokio::sync::Notify::new(),
    });
    active.insert(cached.to_owned(), Arc::clone(&flight));
    Flight::Own(flight)
}

/// Return a cached WebVTT sidecar, extracting it atomically on a miss.
pub async fn ensure_vtt(dir: &Path, file: &MediaFile, index: i64) -> Result<PathBuf, String> {
    ensure_vtt_with(dir, file, index, |tmp, file, index| async move {
        extract_vtt(&tmp, &file, index).await
    })
    .await
}

/// The production seam: default bounds, injected extractor.
async fn ensure_vtt_with<F, Fut>(
    dir: &Path,
    file: &MediaFile,
    index: i64,
    extract: F,
) -> Result<PathBuf, String>
where
    F: FnOnce(PathBuf, MediaFile, i64) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), String>> + Send + 'static,
{
    ensure_vtt_bounded(dir, file, index, ExtractionLimits::default(), extract).await
}

/// One extraction per cache key. The extraction itself lives in an owned
/// task, rather than in the HTTP request awaiting it: if AVPlayer times out or
/// a client cancels, ffmpeg still publishes the completed sidecar and every
/// concurrent waiter observes that same result.
async fn ensure_vtt_bounded<F, Fut>(
    dir: &Path,
    file: &MediaFile,
    index: i64,
    limits: ExtractionLimits,
    extract: F,
) -> Result<PathBuf, String>
where
    F: FnOnce(PathBuf, MediaFile, i64) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), String>> + Send + 'static,
{
    let cached = vtt_path(dir, file, index);
    if tokio::fs::metadata(&cached).await.is_ok() {
        return Ok(cached);
    }

    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| format!("creating subtitle cache: {e}"))?;
    let (flight, owner) = match enlist(&cached).await {
        Flight::Published => return Ok(cached),
        // An in-flight extraction outranks the memo; a remembered failure
        // outranks starting the scan over again.
        Flight::Remembered(why) => return Err(why),
        Flight::Join(flight) => (flight, false),
        Flight::Own(flight) => (flight, true),
    };

    if owner {
        let tmp = dir.join(format!(".tmp-{}.vtt", uuid::Uuid::new_v4()));
        let cached_for_task = cached.clone();
        let flight_for_task = Arc::clone(&flight);
        let file = file.clone();
        let dir = dir.to_owned();
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            tracing::info!(
                file_id = file.id,
                index,
                "extracting embedded text subtitle to the sidecar cache"
            );
            let result =
                publish_extraction(&dir, &cached_for_task, &tmp, &file, index, limits, extract)
                    .await;
            match &result {
                Ok(_) => {
                    tracing::info!(
                        file_id = file.id,
                        index,
                        elapsed_ms = started.elapsed().as_millis(),
                        "text subtitle sidecar cached"
                    );
                    forget_failure(&cached_for_task).await;
                }
                Err(why) => {
                    tracing::warn!(
                        file_id = file.id,
                        index,
                        elapsed_ms = started.elapsed().as_millis(),
                        why,
                        "text subtitle extraction failed; suppressing retries for now"
                    );
                    remember_failure(&cached_for_task, why, limits.negative_ttl).await;
                }
            }
            *flight_for_task.result.lock().await = Some(result);
            extractions().lock().await.remove(&cached_for_task);
            flight_for_task.ready.notify_waiters();
        });
    }

    loop {
        // Register before inspecting the result so a publish between the two
        // operations cannot become a lost notification.
        let notified = flight.ready.notified();
        if let Some(result) = flight.result.lock().await.clone() {
            return result;
        }
        notified.await;
    }
}

async fn extract_vtt(tmp: &Path, file: &MediaFile, index: i64) -> Result<(), String> {
    let out = tokio::process::Command::new(ffmpeg_bin())
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(&file.path)
        .args(["-map", &format!("0:s:{index}"), "-f", "webvtt"])
        .arg(tmp)
        .stdin(std::process::Stdio::null())
        // The timeout in `publish_extraction` bounds this by dropping the
        // future; without `kill_on_drop` that drop would merely stop waiting
        // and leave the wedged ffmpeg holding the stalled mount open. This is
        // what makes the bound an actual kill.
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("spawning subtitle extraction: {e}"))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        return Err(format!("subtitle extraction failed: {}", why.trim()));
    }
    Ok(())
}

async fn publish_extraction<F, Fut>(
    dir: &Path,
    cached: &Path,
    tmp: &Path,
    file: &MediaFile,
    index: i64,
    limits: ExtractionLimits,
    extract: F,
) -> Result<PathBuf, String>
where
    F: FnOnce(PathBuf, MediaFile, i64) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    // A timeout here rather than around the whole task: expiring drops the
    // extractor future (killing its child) while this frame still owns the
    // temp file and can delete it, so a wedge leaves the cache dir as clean as
    // an ordinary failure does.
    let extracted =
        match tokio::time::timeout(limits.timeout, extract(tmp.to_owned(), file.clone(), index))
            .await
        {
            Ok(extracted) => extracted,
            Err(_) => Err(format!(
                "subtitle extraction timed out after {}s",
                limits.timeout.as_secs()
            )),
        };
    if let Err(e) = extracted {
        let _ = tokio::fs::remove_file(tmp).await;
        return Err(e);
    }
    // Checked before the rename, so an oversized sidecar is never visible
    // under its cache name: publishing it would commit the daemon to re-reading
    // it for every segment request until the cache is trimmed.
    let size = tokio::fs::metadata(tmp).await.map(|m| m.len()).unwrap_or(0);
    if size > limits.max_sidecar_bytes {
        let _ = tokio::fs::remove_file(tmp).await;
        return Err(format!(
            "subtitle sidecar is {size} bytes, above the {} byte cap",
            limits.max_sidecar_bytes
        ));
    }
    match tokio::fs::rename(&tmp, &cached).await {
        Ok(()) => {}
        // Two racing misses produce identical bytes. If the peer published
        // first, its file is the answer and this temp is disposable.
        Err(_) if tokio::fs::metadata(&cached).await.is_ok() => {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(format!("publishing subtitle cache: {e}"));
        }
    }
    prune(dir).await;
    Ok(cached.to_owned())
}

/// Drop the oldest cached subtitles once the cache outgrows its cap, and any
/// abandoned temp file a crashed extraction left behind.
async fn prune(dir: &Path) {
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return;
    };
    let mut entries: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    while let Ok(Some(entry)) = rd.next_entry().await {
        let path = entry.path();
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".tmp-") {
            if modified.elapsed().is_ok_and(|age| age.as_secs() > 3600) {
                let _ = tokio::fs::remove_file(&path).await;
            }
            continue;
        }
        entries.push((modified, path));
    }
    if entries.len() <= MAX_ENTRIES {
        return;
    }
    entries.sort_by_key(|(modified, _)| *modified);
    let doomed = entries.len() - TRIM_TO;
    for (_, path) in entries.into_iter().take(doomed) {
        let _ = tokio::fs::remove_file(path).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media_file(path: PathBuf) -> MediaFile {
        MediaFile {
            id: 77,
            item_id: 1,
            path,
            size: 12_345,
            mtime: 678,
            duration_ms: Some(60_000),
            container: Some("mkv".into()),
            video_codec: Some("h264".into()),
            video_profile: None,
            width: Some(1920),
            height: Some(1080),
            bit_depth: Some(8),
            hdr: None,
            hdr_format: None,
            bitrate: Some(8_000_000),
            audio_streams: vec![],
            subtitle_streams: vec![],
            scanned_at: 0,
            audio_offset_ms: 0,
            probed: true,
        }
    }

    #[test]
    fn cache_key_changes_with_source_identity_and_track() {
        let first = vtt_name(42, 2, 1_000, 200);
        assert_eq!(first, "f42-s2-1000-200.vtt");
        assert_ne!(first, vtt_name(42, 3, 1_000, 200));
        assert_ne!(first, vtt_name(42, 2, 1_000, 201));
    }

    #[tokio::test]
    async fn cold_extraction_is_deduplicated_and_survives_waiter_cancellation() {
        let dir = tempfile::tempdir().expect("cache");
        let file = media_file(dir.path().join("source.mkv"));
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();

        let first = {
            let dir = dir.path().to_owned();
            let file = file.clone();
            let runs = Arc::clone(&runs);
            let release = Arc::clone(&release);
            tokio::spawn(async move {
                ensure_vtt_with(&dir, &file, 0, move |tmp, _, _| async move {
                    runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let _ = started_tx.send(());
                    let _permit = release.acquire().await.expect("release");
                    tokio::fs::write(tmp, "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nhello\n")
                        .await
                        .map_err(|e| e.to_string())
                })
                .await
            })
        };
        started_rx.await.expect("extraction started");

        // A second cold request joins the same extraction. Dropping the first
        // simulates AVPlayer timing out while ffmpeg is still reading the MKV.
        let second = {
            let dir = dir.path().to_owned();
            let file = file.clone();
            let runs = Arc::clone(&runs);
            tokio::spawn(async move {
                ensure_vtt_with(&dir, &file, 0, move |_, _, _| async move {
                    runs.fetch_add(100, std::sync::atomic::Ordering::SeqCst);
                    Err("a deduplicated runner must never execute".into())
                })
                .await
            })
        };
        first.abort();
        release.add_permits(1);

        let published = tokio::time::timeout(std::time::Duration::from_secs(2), second)
            .await
            .expect("background extraction publishes promptly")
            .expect("waiter task")
            .expect("published VTT");
        assert_eq!(
            runs.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "only the owner runs extraction"
        );
        assert_eq!(published, vtt_path(dir.path(), &file, 0));
        assert!(tokio::fs::metadata(&published).await.is_ok());

        let mut entries = tokio::fs::read_dir(dir.path()).await.expect("read cache");
        while let Some(entry) = entries.next_entry().await.expect("entry") {
            assert!(
                !entry.file_name().to_string_lossy().starts_with(".tmp-"),
                "a completed extraction must not remain unpublished"
            );
        }
    }

    /// Real time, deliberately: the bounds under test are injected small, so
    /// the suite proves them in milliseconds without a paused clock — which
    /// here would be a trap, since every task in this test parks in file IO
    /// and the runtime would leap straight to the extraction deadline.
    fn wedged_limits() -> ExtractionLimits {
        ExtractionLimits {
            timeout: Duration::from_millis(150),
            negative_ttl: Duration::from_secs(60),
            ..ExtractionLimits::default()
        }
    }

    /// P1-4: a stalled NAS read used to park every waiter forever on
    /// `notified()`, and each following segment request relaunched the same
    /// doomed multi-GB scan. Both waiters must come back with a bounded error,
    /// and the request after them must be answered from the memo.
    #[tokio::test]
    async fn a_wedged_extraction_times_out_and_the_failure_is_memoized() {
        let dir = tempfile::tempdir().expect("cache");
        let file = media_file(dir.path().join("source.mkv"));
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();

        let first = {
            let dir = dir.path().to_owned();
            let file = file.clone();
            let runs = Arc::clone(&runs);
            tokio::spawn(async move {
                ensure_vtt_bounded(&dir, &file, 0, wedged_limits(), move |_, _, _| async move {
                    runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let _ = started_tx.send(());
                    // The wedge: an ffmpeg that never returns and never errors.
                    tokio::time::sleep(Duration::from_secs(3_600)).await;
                    Ok(())
                })
                .await
            })
        };
        started_rx.await.expect("extraction started");

        // Joins the same flight, the way a second segment request would.
        let second = {
            let dir = dir.path().to_owned();
            let file = file.clone();
            let runs = Arc::clone(&runs);
            tokio::spawn(async move {
                ensure_vtt_bounded(&dir, &file, 0, wedged_limits(), move |_, _, _| async move {
                    runs.fetch_add(100, std::sync::atomic::Ordering::SeqCst);
                    Err("a deduplicated runner must never execute".into())
                })
                .await
            })
        };

        for waiter in [first, second] {
            let observed = tokio::time::timeout(Duration::from_secs(10), waiter)
                .await
                .expect("a wedged producer must not park its waiters")
                .expect("waiter task");
            let why = observed.expect_err("a wedged extraction cannot publish");
            assert!(
                why.contains("timed out"),
                "waiters observe the bounded error, got {why}"
            );
        }

        // The memo is written before the flight is dropped, so by the time a
        // waiter has its answer a fresh request is already covered.
        let runs_for_repeat = Arc::clone(&runs);
        let repeat = ensure_vtt_bounded(
            dir.path(),
            &file,
            0,
            wedged_limits(),
            move |_, _, _| async move {
                runs_for_repeat.fetch_add(100, std::sync::atomic::Ordering::SeqCst);
                Err("a memoized failure must not relaunch the scan".into())
            },
        )
        .await;
        assert!(repeat.is_err(), "the memo answers with the same failure");
        assert_eq!(
            runs.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "one scan per memo window, not one per request"
        );
        forget_failure(&vtt_path(dir.path(), &file, 0)).await;
    }

    /// The other side of the TTL comparison: an expired memo must not become a
    /// permanent tomb for a track whose cause has since been fixed, and the
    /// success that follows must clear it outright rather than leave a corpse
    /// in the map.
    #[tokio::test]
    async fn an_expired_memo_relaunches_and_success_clears_it() {
        let dir = tempfile::tempdir().expect("cache");
        let file = media_file(dir.path().join("source.mkv"));
        let key = vtt_path(dir.path(), &file, 0);
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // A zero TTL is a memo that is written and already expired: the
        // deterministic version of "the window has passed", with no wall clock
        // in the test.
        let expired = ExtractionLimits {
            negative_ttl: Duration::ZERO,
            ..ExtractionLimits::default()
        };

        let runs_for_failure = Arc::clone(&runs);
        let failed = ensure_vtt_bounded(dir.path(), &file, 0, expired, move |_, _, _| async move {
            runs_for_failure.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err("no such stream".into())
        })
        .await;
        assert!(failed.is_err(), "the first attempt fails");
        assert!(
            negative_memos().lock().await.contains_key(&key),
            "the failure was memoized, expired or not"
        );

        let runs_for_success = Arc::clone(&runs);
        let published =
            ensure_vtt_bounded(dir.path(), &file, 0, expired, move |tmp, _, _| async move {
                runs_for_success.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::fs::write(tmp, "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nhello\n")
                    .await
                    .map_err(|e| e.to_string())
            })
            .await
            .expect("an expired memo cannot block a working extraction");

        assert_eq!(published, key);
        assert_eq!(
            runs.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the expired memo let the second attempt run"
        );
        assert!(
            !negative_memos().lock().await.contains_key(&key),
            "a published sidecar invalidates the memo"
        );
    }

    /// P2-4: the sidecar is re-read whole for every segment request, so a
    /// pathological track must be refused at publish — and refused the way any
    /// other failure is, leaving neither a cache entry nor a temp file.
    #[tokio::test]
    async fn an_oversized_sidecar_is_rejected_and_leaves_no_file() {
        let dir = tempfile::tempdir().expect("cache");
        let file = media_file(dir.path().join("source.mkv"));
        let limits = ExtractionLimits {
            max_sidecar_bytes: 64,
            negative_ttl: Duration::ZERO,
            ..ExtractionLimits::default()
        };

        let why = ensure_vtt_bounded(dir.path(), &file, 0, limits, |tmp, _, _| async move {
            tokio::fs::write(tmp, "WEBVTT\n".to_string() + &"x".repeat(4_096))
                .await
                .map_err(|e| e.to_string())
        })
        .await
        .expect_err("an oversized sidecar is not publishable");
        assert!(why.contains("cap"), "the error names the cap, got {why}");

        assert!(
            tokio::fs::metadata(vtt_path(dir.path(), &file, 0))
                .await
                .is_err(),
            "a rejected sidecar must never appear under its cache name"
        );
        let mut entries = tokio::fs::read_dir(dir.path()).await.expect("read cache");
        while let Some(entry) = entries.next_entry().await.expect("entry") {
            assert!(
                !entry.file_name().to_string_lossy().starts_with(".tmp-"),
                "and its temp file must be gone too"
            );
        }
    }

    /// The interleaving the test above only hits by luck, stated directly.
    ///
    /// A request reads the cache, misses, and reaches the registry a moment
    /// later — by which time the owner has renamed its sidecar into place and
    /// retired its entry. There is no flight left to join, and the stale miss
    /// says nothing is there. Owning a second extraction at that point is the
    /// bug; [`enlist`] must read the cache again under the lock and hand back
    /// the published answer.
    #[tokio::test]
    async fn a_sidecar_published_after_the_cache_read_is_joined_not_re_extracted() {
        let dir = tempfile::tempdir().expect("cache");
        let file = media_file(dir.path().join("source.mkv"));
        let cached = vtt_path(dir.path(), &file, 0);
        tokio::fs::write(&cached, "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nhello\n")
            .await
            .expect("publish");

        // Exactly the state a request finds in that window: sidecar on disk,
        // registry empty for this key.
        assert!(
            !extractions().lock().await.contains_key(&cached),
            "no flight is registered for a retired extraction"
        );
        assert!(
            matches!(enlist(&cached).await, Flight::Published),
            "a published sidecar must never be re-owned"
        );
        assert!(
            !extractions().lock().await.contains_key(&cached),
            "and deciding must not leave a flight behind"
        );

        // The other two arms still work: an unpublished key is owned once and
        // joined thereafter.
        let missing = vtt_path(dir.path(), &file, 1);
        assert!(matches!(enlist(&missing).await, Flight::Own(_)));
        assert!(matches!(enlist(&missing).await, Flight::Join(_)));
        extractions().lock().await.remove(&missing);
    }
}
