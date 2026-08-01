//! Persistent extraction cache for embedded text subtitles.
//!
//! ffmpeg/libass must read an embedded text track to EOF before it can render
//! the first frame. On a large MKV over a NAS that can exceed a player's
//! preparation timeout. Extract once to a small WebVTT sidecar: the web
//! subtitle endpoint always uses it, and burned transcodes reuse it for simple
//! text codecs whose authored styling is not lost by WebVTT conversion.

use std::path::{Path, PathBuf};

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

/// Return a cached WebVTT sidecar, extracting it atomically on a miss.
pub async fn ensure_vtt(dir: &Path, file: &MediaFile, index: i64) -> Result<PathBuf, String> {
    let cached = vtt_path(dir, file, index);
    if tokio::fs::metadata(&cached).await.is_ok() {
        return Ok(cached);
    }

    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| format!("creating subtitle cache: {e}"))?;
    let tmp = dir.join(format!(".tmp-{}.vtt", uuid::Uuid::new_v4()));
    let started = std::time::Instant::now();
    tracing::info!(
        file_id = file.id,
        index,
        "extracting embedded text subtitle to the sidecar cache"
    );
    let out = tokio::process::Command::new(ffmpeg_bin())
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(&file.path)
        .args(["-map", &format!("0:s:{index}"), "-f", "webvtt"])
        .arg(&tmp)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| format!("spawning subtitle extraction: {e}"))?;
    if !out.status.success() {
        let _ = tokio::fs::remove_file(&tmp).await;
        let why = String::from_utf8_lossy(&out.stderr);
        return Err(format!("subtitle extraction failed: {}", why.trim()));
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
    tracing::info!(
        file_id = file.id,
        index,
        elapsed_ms = started.elapsed().as_millis(),
        "text subtitle sidecar cached"
    );
    Ok(cached)
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

    #[test]
    fn cache_key_changes_with_source_identity_and_track() {
        let first = vtt_name(42, 2, 1_000, 200);
        assert_eq!(first, "f42-s2-1000-200.vtt");
        assert_ne!(first, vtt_name(42, 3, 1_000, 200));
        assert_ne!(first, vtt_name(42, 2, 1_000, 201));
    }
}
