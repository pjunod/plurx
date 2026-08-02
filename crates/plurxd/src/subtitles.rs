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

use plurx_core::domain::MediaFile;

use crate::ffmpeg::ffmpeg_bin;

/// Entries the subtitle cache may hold before the oldest are trimmed, and how
/// far a trim takes it. WebVTT files are normally only kilobytes.
const MAX_ENTRIES: usize = 256;
const TRIM_TO: usize = 224;

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

/// Return a cached WebVTT sidecar, extracting it atomically on a miss.
pub async fn ensure_vtt(dir: &Path, file: &MediaFile, index: i64) -> Result<PathBuf, String> {
    ensure_vtt_with(dir, file, index, |tmp, file, index| async move {
        extract_vtt(&tmp, &file, index).await
    })
    .await
}

/// One extraction per cache key. The extraction itself lives in an owned
/// task, rather than in the HTTP request awaiting it: if AVPlayer times out or
/// a client cancels, ffmpeg still publishes the completed sidecar and every
/// concurrent waiter observes that same result.
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
    let cached = vtt_path(dir, file, index);
    if tokio::fs::metadata(&cached).await.is_ok() {
        return Ok(cached);
    }

    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| format!("creating subtitle cache: {e}"))?;
    let (flight, owner) = {
        let mut active = extractions().lock().await;
        if let Some(flight) = active.get(&cached) {
            (Arc::clone(flight), false)
        } else {
            let flight = Arc::new(Extraction {
                result: tokio::sync::Mutex::new(None),
                ready: tokio::sync::Notify::new(),
            });
            active.insert(cached.clone(), Arc::clone(&flight));
            (flight, true)
        }
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
                publish_extraction(&dir, &cached_for_task, &tmp, &file, index, extract).await;
            if result.is_ok() {
                tracing::info!(
                    file_id = file.id,
                    index,
                    elapsed_ms = started.elapsed().as_millis(),
                    "text subtitle sidecar cached"
                );
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
    extract: F,
) -> Result<PathBuf, String>
where
    F: FnOnce(PathBuf, MediaFile, i64) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    if let Err(e) = extract(tmp.to_owned(), file.clone(), index).await {
        let _ = tokio::fs::remove_file(tmp).await;
        return Err(e);
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
}
