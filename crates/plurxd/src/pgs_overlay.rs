//! Persistent, atomic cache for the versioned PGS application-overlay API.
//!
//! The HTTP layer never interprets PGS. A cold request registers one detached
//! preparation, returns immediately, and later requests observe either a
//! complete generation directory or nothing. Raw SUP and partial PNGs live in
//! a private staging directory that is renamed only after validation.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use flate2::write::ZlibEncoder;
use flate2::Compression;
use plurx_core::domain::MediaFile;
use plurx_pgs::{NormalizedTrack, ParserLimits};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ffmpeg::ffmpeg_bin;

pub const SCHEMA: u8 = 1;
pub const PROTOCOL: &str = "pgs-v1";
const EXTRACTOR_VERSION: &str = "plurx-pgs-overlay-v3-stable-input-pts-wrap";
const PREPARE_TIMEOUT: Duration = Duration::from_secs(600);
const NEGATIVE_TTL: Duration = Duration::from_secs(120);
const MAX_NEGATIVE_ENTRIES: usize = 128;
const MAX_TRACK_BYTES: u64 = 256 * 1024 * 1024;
const MAX_CACHE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_CACHE_TRACKS: usize = 128;

#[derive(Debug, Clone)]
pub enum OverlayError {
    Malformed(String),
    Limit(String),
    SourceChanged,
    Unavailable(String),
    Internal(String),
}

impl std::fmt::Display for OverlayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(why) => write!(f, "malformed PGS: {why}"),
            Self::Limit(why) => write!(f, "PGS safety limit exceeded: {why}"),
            Self::SourceChanged => f.write_str("source changed while overlay was preparing"),
            Self::Unavailable(why) => f.write_str(why),
            Self::Internal(why) => f.write_str(why),
        }
    }
}

impl From<plurx_pgs::AdapterError> for OverlayError {
    fn from(error: plurx_pgs::AdapterError) -> Self {
        match error {
            plurx_pgs::AdapterError::Malformed(why) => Self::Malformed(why),
            plurx_pgs::AdapterError::Limit(why) => Self::Limit(why),
            plurx_pgs::AdapterError::Io(error) => Self::Unavailable(error.to_string()),
            plurx_pgs::AdapterError::Parser(error) => Self::Malformed(error.to_string()),
            plurx_pgs::AdapterError::Cancelled => {
                Self::Unavailable("PGS overlay preparation was cancelled".into())
            }
        }
    }
}

#[derive(Debug)]
pub enum PrepareState {
    Ready(PathBuf),
    Preparing,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OverlayManifest {
    pub schema: u8,
    pub generation: String,
    pub file_id: i64,
    pub track_index: i64,
    pub kind: String,
    pub timebase: String,
    pub duration_ms: i64,
    pub cues: Vec<OverlayCue>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OverlayCue {
    pub id: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub canvas_width: u16,
    pub canvas_height: u16,
    pub objects: Vec<OverlayObject>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OverlayObject {
    pub image: String,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

struct FailureMemo {
    error: OverlayError,
    expires_at: tokio::time::Instant,
}

type Active = tokio::sync::Mutex<HashSet<PathBuf>>;
type Failures = tokio::sync::Mutex<HashMap<PathBuf, FailureMemo>>;

fn active() -> &'static Active {
    static ACTIVE: OnceLock<Active> = OnceLock::new();
    ACTIVE.get_or_init(|| tokio::sync::Mutex::new(HashSet::new()))
}

fn failures() -> &'static Failures {
    static FAILURES: OnceLock<Failures> = OnceLock::new();
    FAILURES.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

fn preparation_capacity() -> &'static Arc<tokio::sync::Semaphore> {
    static CAPACITY: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    CAPACITY.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(2)))
}

fn try_capacity(
    semaphore: Arc<tokio::sync::Semaphore>,
) -> Result<tokio::sync::OwnedSemaphorePermit, OverlayError> {
    semaphore
        .try_acquire_owned()
        .map_err(|_| OverlayError::Unavailable("PGS overlay preparation capacity is full".into()))
}

type PrepareFuture = Pin<Box<dyn Future<Output = Result<(), OverlayError>> + Send>>;
type PrepareRunner =
    Arc<dyn Fn(PathBuf, PathBuf, MediaFile, i64, String) -> PrepareFuture + Send + Sync + 'static>;

/// Stable source fingerprint and protocol version. It contains no path data,
/// so it is safe in a URL and invalidates naturally on replacement/re-mux.
pub fn generation(file: &MediaFile, index: i64) -> String {
    let mut hash = Sha256::new();
    hash.update(b"plurx-pgs-overlay-generation-v1");
    hash.update(EXTRACTOR_VERSION.as_bytes());
    hash.update(plurx_pgs::REVIEWED_LIBPGS_VERSION.as_bytes());
    hash.update([SCHEMA]);
    hash.update(file.id.to_be_bytes());
    hash.update(index.to_be_bytes());
    hash.update(file.size.to_be_bytes());
    hash.update(file.mtime.to_be_bytes());
    hex::encode(hash.finalize())
}

fn pgs_root(subs_dir: &Path) -> PathBuf {
    subs_dir.join("pgs")
}

pub fn generation_dir(subs_dir: &Path, file: &MediaFile, index: i64) -> PathBuf {
    pgs_root(subs_dir).join(generation(file, index))
}

pub fn manifest_path(subs_dir: &Path, file: &MediaFile, index: i64) -> PathBuf {
    generation_dir(subs_dir, file, index).join("manifest.json")
}

pub fn object_path(
    subs_dir: &Path,
    file: &MediaFile,
    index: i64,
    object_hash: &str,
) -> Option<PathBuf> {
    is_sha256(object_hash).then(|| {
        generation_dir(subs_dir, file, index)
            .join("objects")
            .join(format!("{object_hash}.png"))
    })
}

pub fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Best-effort LRU marker for a complete generation. Serving must not fail
/// because the cache filesystem refused this bookkeeping write.
pub async fn record_access(generation_dir: &Path) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string();
    let _ = tokio::fs::write(generation_dir.join(".access"), now).await;
}

/// Return current readiness, registering one detached preparation on a miss.
pub async fn prepare(
    subs_dir: &Path,
    file: &MediaFile,
    index: i64,
) -> Result<PrepareState, OverlayError> {
    let runner: PrepareRunner = Arc::new(|root, final_dir, file, index, generation| {
        Box::pin(prepare_once(root, final_dir, file, index, generation))
    });
    prepare_with(subs_dir, file, index, runner, PREPARE_TIMEOUT, true).await
}

async fn prepare_with(
    subs_dir: &Path,
    file: &MediaFile,
    index: i64,
    runner: PrepareRunner,
    timeout: Duration,
    enforce_capacity: bool,
) -> Result<PrepareState, OverlayError> {
    let root = pgs_root(subs_dir);
    let generation = generation(file, index);
    let final_dir = root.join(&generation);
    let manifest = final_dir.join("manifest.json");
    if published_generation_is_valid(&final_dir, file, index, &generation).await {
        return Ok(PrepareState::Ready(manifest.clone()));
    }

    if let Some(error) = remembered_failure(&final_dir).await {
        return Err(error);
    }

    let mut active_guard = active().lock().await;
    if published_generation_is_valid(&final_dir, file, index, &generation).await {
        return Ok(PrepareState::Ready(manifest));
    }
    if active_guard.contains(&final_dir) {
        return Ok(PrepareState::Preparing);
    }
    let capacity = if enforce_capacity {
        Some(try_capacity(Arc::clone(preparation_capacity()))?)
    } else {
        None
    };
    active_guard.insert(final_dir.clone());
    drop(active_guard);

    let file = file.clone();
    tokio::spawn(async move {
        let _capacity = capacity;
        let started = std::time::Instant::now();
        tracing::info!(
            file_id = file.id,
            index,
            generation,
            "pgs_overlay_prepare_started"
        );
        let result = match tokio::time::timeout(
            timeout,
            runner(
                root.clone(),
                final_dir.clone(),
                file.clone(),
                index,
                generation.clone(),
            ),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(OverlayError::Unavailable(format!(
                "PGS overlay preparation timed out after {:.3}s",
                timeout.as_secs_f64()
            ))),
        };
        match result {
            Ok(()) => {
                failures().lock().await.remove(&final_dir);
                active().lock().await.remove(&final_dir);
                tracing::info!(
                    file_id = file.id,
                    index,
                    generation,
                    elapsed_ms = started.elapsed().as_millis(),
                    "pgs_overlay_prepare_completed"
                );
            }
            Err(error) => {
                // Publish the negative memo before dropping the active marker.
                // Otherwise a request in the gap could own a duplicate run of
                // the work that just failed.
                remember_failure(final_dir.clone(), error.clone()).await;
                active().lock().await.remove(&final_dir);
                tracing::warn!(
                    file_id = file.id,
                    index,
                    generation,
                    elapsed_ms = started.elapsed().as_millis(),
                    error = %error,
                    "pgs_overlay_prepare_failed"
                );
            }
        }
        // Completion, failure, and every future exit route share one bounded
        // cleanup point; cache growth cannot depend on which result arrived.
        prune(&root).await;
    });

    Ok(PrepareState::Preparing)
}

async fn published_generation_is_valid(
    final_dir: &Path,
    file: &MediaFile,
    index: i64,
    generation: &str,
) -> bool {
    if tokio::fs::metadata(final_dir.join("manifest.json"))
        .await
        .is_err()
    {
        return false;
    }
    match validate_generation(
        final_dir,
        file,
        index,
        generation,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                path = %final_dir.display(),
                error = %error,
                "discarding invalid published PGS generation"
            );
            invalidate_generation(final_dir).await;
            false
        }
    }
}

pub async fn invalidate_generation(final_dir: &Path) {
    let _ = tokio::fs::remove_dir_all(final_dir).await;
    if let Some(root) = final_dir.parent() {
        let root = root.to_owned();
        let _ = tokio::task::spawn_blocking(move || sync_directory(&root)).await;
    }
}

async fn remembered_failure(key: &Path) -> Option<OverlayError> {
    let now = tokio::time::Instant::now();
    let mut failures = failures().lock().await;
    failures.retain(|_, memo| memo.expires_at > now);
    failures.get(key).map(|memo| memo.error.clone())
}

async fn remember_failure(key: PathBuf, error: OverlayError) {
    let now = tokio::time::Instant::now();
    let mut failures = failures().lock().await;
    failures.retain(|_, memo| memo.expires_at > now);
    while failures.len() >= MAX_NEGATIVE_ENTRIES {
        let Some(key) = failures
            .iter()
            .min_by_key(|(_, memo)| memo.expires_at)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        failures.remove(&key);
    }
    failures.insert(
        key,
        FailureMemo {
            error,
            expires_at: now + NEGATIVE_TTL,
        },
    );
}

async fn prepare_once(
    root: PathBuf,
    final_dir: PathBuf,
    file: MediaFile,
    index: i64,
    generation: String,
) -> Result<(), OverlayError> {
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(|error| OverlayError::Internal(format!("creating PGS cache: {error}")))?;
    let stage = root.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir(&stage).await.map_err(|error| {
        OverlayError::Internal(format!("creating PGS staging directory: {error}"))
    })?;
    let mut staging = StagingDir::new(stage);

    prepare_stage(staging.path(), &file, index, &generation).await?;

    match tokio::fs::rename(staging.path(), &final_dir).await {
        Ok(()) => {
            staging.disarm();
            let root_for_sync = root.clone();
            tokio::task::spawn_blocking(move || sync_directory(&root_for_sync))
                .await
                .map_err(|error| {
                    OverlayError::Internal(format!("PGS directory sync task failed: {error}"))
                })?
                .map_err(|error| {
                    OverlayError::Internal(format!("syncing PGS cache directory: {error}"))
                })?;
            Ok(())
        }
        Err(_)
            if tokio::fs::metadata(final_dir.join("manifest.json"))
                .await
                .is_ok() =>
        {
            Ok(())
        }
        Err(error) => Err(OverlayError::Internal(format!(
            "publishing PGS overlay generation: {error}"
        ))),
    }
}

/// Cancellation-safe ownership of a private generation directory. The
/// producer future is dropped on timeout, so ordinary async cleanup code would
/// never run; Drop schedules removal in that exact path.
struct StagingDir {
    path: PathBuf,
    armed: bool,
}

struct CancellationFlag {
    flag: Arc<AtomicBool>,
    armed: bool,
}

impl CancellationFlag {
    fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            armed: true,
        }
    }

    fn worker(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.flag)
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancellationFlag {
    fn drop(&mut self) {
        if self.armed {
            self.flag.store(true, Ordering::Release);
        }
    }
}

impl StagingDir {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let path = self.path.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = tokio::fs::remove_dir_all(path).await;
            });
        } else {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

async fn prepare_stage(
    stage: &Path,
    file: &MediaFile,
    index: i64,
    generation: &str,
) -> Result<(), OverlayError> {
    let mut cancellation = CancellationFlag::new();
    source_is_current(file).await?;
    let sup = stage.join("track.sup");
    let maximum_demux_bytes = MAX_TRACK_BYTES.to_string();
    let output = tokio::process::Command::new(ffmpeg_bin())
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(&file.path)
        .args([
            "-map",
            &format!("0:s:{index}"),
            "-c:s",
            "copy",
            "-f",
            "sup",
            "-fs",
            &maximum_demux_bytes,
        ])
        .arg(&sup)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| OverlayError::Unavailable(format!("starting PGS demux: {error}")))?;
    if !output.status.success() {
        return Err(OverlayError::Unavailable(format!(
            "PGS demux failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    let sup_for_worker = sup.clone();
    let stage_for_worker = stage.to_owned();
    let file_for_worker = file.clone();
    let generation_for_worker = generation.to_owned();
    let worker_cancellation = cancellation.worker();
    tokio::task::spawn_blocking(move || {
        let track = plurx_pgs::normalize_sup_cancellable(
            &sup_for_worker,
            &ParserLimits::default(),
            &worker_cancellation,
        )?;
        compile_generation(
            &stage_for_worker,
            &file_for_worker,
            index,
            &generation_for_worker,
            track,
            Some(&worker_cancellation),
        )
    })
    .await
    .map_err(|error| OverlayError::Internal(format!("PGS normalizer task failed: {error}")))??;

    let _ = tokio::fs::remove_file(&sup).await;
    source_is_current(file).await?;
    let result = validate_generation(stage, file, index, generation, cancellation.worker()).await;
    if result.is_ok() {
        let stage_for_sync = stage.to_owned();
        tokio::task::spawn_blocking(move || sync_generation(&stage_for_sync))
            .await
            .map_err(|error| {
                OverlayError::Internal(format!("PGS generation sync task failed: {error}"))
            })??;
        cancellation.disarm();
    }
    result
}

#[cfg(unix)]
fn metadata_mtime(metadata: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    metadata.mtime()
}

#[cfg(not(unix))]
fn metadata_mtime(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|value| value.as_secs() as i64)
        .unwrap_or_default()
}

async fn source_is_current(file: &MediaFile) -> Result<(), OverlayError> {
    let metadata = tokio::fs::metadata(&file.path)
        .await
        .map_err(|error| OverlayError::Unavailable(format!("reading media source: {error}")))?;
    if metadata.len() != file.size.max(0) as u64 || metadata_mtime(&metadata) != file.mtime {
        return Err(OverlayError::SourceChanged);
    }
    Ok(())
}

#[derive(PartialEq, Eq)]
struct Snapshot {
    start_ms: i64,
    canvas_width: u16,
    canvas_height: u16,
    objects: Vec<OverlayObject>,
}

impl Snapshot {
    fn same_composition(&self, other: &Self) -> bool {
        self.canvas_width == other.canvas_width
            && self.canvas_height == other.canvas_height
            && self.objects == other.objects
    }
}

fn compile_generation(
    stage: &Path,
    file: &MediaFile,
    index: i64,
    generation: &str,
    track: NormalizedTrack,
    cancelled: Option<&AtomicBool>,
) -> Result<(), OverlayError> {
    check_worker_cancelled(cancelled)?;
    let duration_ms = file
        .duration_ms
        .filter(|duration| *duration > 0)
        .ok_or_else(|| {
            OverlayError::Unavailable("media duration is required for a PGS manifest".into())
        })?;
    let objects_dir = stage.join("objects");
    std::fs::create_dir(&objects_dir)
        .map_err(|error| OverlayError::Internal(format!("creating PGS object cache: {error}")))?;

    let mut unique_bytes = 0u64;
    let mut published = HashSet::new();
    let mut snapshots: Vec<Snapshot> = Vec::with_capacity(track.compositions.len());
    for composition in track.compositions {
        check_worker_cancelled(cancelled)?;
        let mut objects = Vec::with_capacity(composition.objects.len());
        for object in composition.objects {
            check_worker_cancelled(cancelled)?;
            let png = encode_rgba_png(object.width, object.height, &object.rgba)?;
            let hash = hex::encode(Sha256::digest(&png));
            if published.insert(hash.clone()) {
                unique_bytes = unique_bytes
                    .checked_add(png.len() as u64)
                    .ok_or_else(|| OverlayError::Limit("PNG byte count overflowed".into()))?;
                if unique_bytes > MAX_TRACK_BYTES {
                    return Err(OverlayError::Limit(format!(
                        "overlay objects exceed the {MAX_TRACK_BYTES} byte track cap"
                    )));
                }
                write_durable(&objects_dir.join(format!("{hash}.png")), &png).map_err(|error| {
                    OverlayError::Internal(format!("writing PGS object: {error}"))
                })?;
            }
            objects.push(OverlayObject {
                image: format!("overlay/{generation}/objects/{hash}.png"),
                x: object.x,
                y: object.y,
                width: object.width,
                height: object.height,
            });
        }
        let snapshot = Snapshot {
            start_ms: ((composition.pts_90khz + 45) / 90).min(i64::MAX as u64) as i64,
            canvas_width: composition.canvas_width,
            canvas_height: composition.canvas_height,
            objects,
        };
        if snapshots
            .last()
            .is_some_and(|previous| previous.start_ms == snapshot.start_ms)
        {
            let replacement_index = snapshots.len() - 1;
            snapshots[replacement_index] = snapshot;
            if replacement_index > 0
                && snapshots[replacement_index - 1].same_composition(&snapshots[replacement_index])
            {
                snapshots.pop();
            }
            continue;
        }
        if snapshots
            .last()
            .is_some_and(|previous| previous.same_composition(&snapshot))
        {
            continue;
        }
        snapshots.push(snapshot);
    }

    let mut cues = Vec::new();
    for (position, snapshot) in snapshots.iter().enumerate() {
        check_worker_cancelled(cancelled)?;
        if snapshot.objects.is_empty() || snapshot.start_ms >= duration_ms {
            continue;
        }
        let end_ms = snapshots
            .get(position + 1)
            .map(|next| next.start_ms)
            .unwrap_or(duration_ms)
            .min(duration_ms);
        if end_ms <= snapshot.start_ms {
            continue;
        }
        cues.push(OverlayCue {
            id: format!("c{:08}", cues.len() + 1),
            start_ms: snapshot.start_ms.max(0),
            end_ms,
            canvas_width: snapshot.canvas_width,
            canvas_height: snapshot.canvas_height,
            objects: snapshot.objects.clone(),
        });
    }

    let manifest = OverlayManifest {
        schema: SCHEMA,
        generation: generation.to_owned(),
        file_id: file.id,
        track_index: index,
        kind: "pgs".into(),
        timebase: "source_ms".into(),
        duration_ms,
        cues,
    };
    let bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| OverlayError::Internal(format!("encoding PGS manifest: {error}")))?;
    write_durable(&stage.join("manifest.json"), &bytes)
        .map_err(|error| OverlayError::Internal(format!("writing PGS manifest: {error}")))?;
    let total_bytes = directory_size_sync(stage, cancelled)?;
    if total_bytes > MAX_TRACK_BYTES {
        return Err(OverlayError::Limit(format!(
            "overlay generation is {total_bytes} bytes, above the {MAX_TRACK_BYTES} byte track cap"
        )));
    }
    validate_generation_sync(stage, file, index, generation, cancelled)
}

fn write_durable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn sync_generation(stage: &Path) -> Result<(), OverlayError> {
    sync_directory(&stage.join("objects"))
        .map_err(|error| OverlayError::Internal(format!("syncing PGS objects: {error}")))?;
    sync_directory(stage)
        .map_err(|error| OverlayError::Internal(format!("syncing PGS generation: {error}")))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn check_worker_cancelled(cancelled: Option<&AtomicBool>) -> Result<(), OverlayError> {
    if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
        Err(OverlayError::Unavailable(
            "PGS overlay preparation was cancelled".into(),
        ))
    } else {
        Ok(())
    }
}

fn directory_size_sync(path: &Path, cancelled: Option<&AtomicBool>) -> Result<u64, OverlayError> {
    let mut total = 0u64;
    let mut pending = vec![path.to_owned()];
    while let Some(dir) = pending.pop() {
        check_worker_cancelled(cancelled)?;
        for entry in std::fs::read_dir(dir)
            .map_err(|error| OverlayError::Internal(format!("reading PGS cache size: {error}")))?
        {
            let entry = entry.map_err(|error| {
                OverlayError::Internal(format!("reading PGS cache entry: {error}"))
            })?;
            let metadata = entry.metadata().map_err(|error| {
                OverlayError::Internal(format!("reading PGS cache metadata: {error}"))
            })?;
            if metadata.is_dir() {
                pending.push(entry.path());
            } else {
                total = total
                    .checked_add(metadata.len())
                    .ok_or_else(|| OverlayError::Limit("cache byte count overflowed".into()))?;
            }
        }
    }
    Ok(total)
}

async fn validate_generation(
    dir: &Path,
    file: &MediaFile,
    index: i64,
    generation: &str,
    cancelled: Arc<AtomicBool>,
) -> Result<(), OverlayError> {
    let dir = dir.to_owned();
    let file = file.clone();
    let generation = generation.to_owned();
    tokio::task::spawn_blocking(move || {
        validate_generation_sync(&dir, &file, index, &generation, Some(&cancelled))
    })
    .await
    .map_err(|error| OverlayError::Internal(format!("PGS validation task failed: {error}")))?
}

fn validate_generation_sync(
    dir: &Path,
    file: &MediaFile,
    index: i64,
    generation: &str,
    cancelled: Option<&AtomicBool>,
) -> Result<(), OverlayError> {
    check_worker_cancelled(cancelled)?;
    let bytes = std::fs::read(dir.join("manifest.json"))
        .map_err(|error| OverlayError::Internal(format!("reading PGS manifest: {error}")))?;
    let manifest: OverlayManifest = serde_json::from_slice(&bytes)
        .map_err(|error| OverlayError::Internal(format!("validating PGS manifest: {error}")))?;
    if manifest.schema != SCHEMA
        || manifest.generation != generation
        || manifest.file_id != file.id
        || manifest.track_index != index
        || manifest.duration_ms != file.duration_ms.unwrap_or_default()
    {
        return Err(OverlayError::Internal(
            "published PGS manifest identity does not match its source".into(),
        ));
    }
    let mut last_end = 0;
    let mut verified_objects = HashSet::new();
    for cue in &manifest.cues {
        check_worker_cancelled(cancelled)?;
        if cue.start_ms < last_end
            || cue.start_ms >= cue.end_ms
            || cue.end_ms > manifest.duration_ms
        {
            return Err(OverlayError::Internal(
                "published PGS cues are not sorted, half-open intervals".into(),
            ));
        }
        last_end = cue.end_ms;
        if cue.canvas_width == 0
            || cue.canvas_height == 0
            || cue.canvas_width > 4096
            || cue.canvas_height > 2160
            || cue.objects.len() > 64
        {
            return Err(OverlayError::Internal(
                "published PGS cue exceeds canvas or object limits".into(),
            ));
        }
        for object in &cue.objects {
            let Some(name) = object.image.rsplit('/').next() else {
                return Err(OverlayError::Internal("PGS object URL is invalid".into()));
            };
            let Some(hash) = name.strip_suffix(".png") else {
                return Err(OverlayError::Internal("PGS object URL is invalid".into()));
            };
            if !is_sha256(hash)
                || object.image != format!("overlay/{generation}/objects/{hash}.png")
                || u32::from(object.x) + u32::from(object.width) > u32::from(cue.canvas_width)
                || u32::from(object.y) + u32::from(object.height) > u32::from(cue.canvas_height)
                || object.width == 0
                || object.height == 0
            {
                return Err(OverlayError::Internal(
                    "published PGS object geometry or URL is invalid".into(),
                ));
            }
            if verified_objects.insert(hash.to_owned()) {
                check_worker_cancelled(cancelled)?;
                let png = std::fs::read(dir.join("objects").join(name)).map_err(|_| {
                    OverlayError::Internal("PGS manifest references an unpublished object".into())
                })?;
                if hex::encode(Sha256::digest(&png)) != hash {
                    return Err(OverlayError::Internal(
                        "published PGS object does not match its content hash".into(),
                    ));
                }
            }
        }
    }
    Ok(())
}

fn encode_rgba_png(width: u16, height: u16, rgba: &[u8]) -> Result<Vec<u8>, OverlayError> {
    let expected = width as usize * height as usize * 4;
    if width == 0 || height == 0 || rgba.len() != expected {
        return Err(OverlayError::Malformed(format!(
            "RGBA object {}x{} carries {} bytes instead of {expected}",
            width,
            height,
            rgba.len()
        )));
    }
    let row_bytes = width as usize * 4;
    let mut scanlines = Vec::with_capacity((row_bytes + 1) * height as usize);
    for row in rgba.chunks_exact(row_bytes) {
        scanlines.push(0); // PNG filter: None
        scanlines.extend_from_slice(row);
    }
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(&scanlines)
        .map_err(|error| OverlayError::Internal(format!("compressing PGS PNG: {error}")))?;
    let compressed = encoder
        .finish()
        .map_err(|error| OverlayError::Internal(format!("finishing PGS PNG: {error}")))?;

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // RGBA8, deflate, no interlace
    push_png_chunk(&mut png, b"IHDR", &ihdr);
    push_png_chunk(&mut png, b"IDAT", &compressed);
    push_png_chunk(&mut png, b"IEND", &[]);
    Ok(png)
}

fn push_png_chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend_from_slice(&(data.len() as u32).to_be_bytes());
    png.extend_from_slice(kind);
    png.extend_from_slice(data);
    let mut crc = crc32fast::Hasher::new();
    crc.update(kind);
    crc.update(data);
    png.extend_from_slice(&crc.finalize().to_be_bytes());
}

async fn prune(root: &Path) {
    prune_bounded(root, MAX_CACHE_BYTES, MAX_CACHE_TRACKS).await;
}

async fn prune_bounded(root: &Path, max_bytes: u64, max_tracks: usize) {
    let Ok(mut entries) = tokio::fs::read_dir(root).await else {
        return;
    };
    let mut generations = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(metadata) = entry.metadata().await else {
            continue;
        };
        if name.starts_with(".tmp-") {
            if metadata
                .modified()
                .ok()
                .and_then(|time| time.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(3600))
            {
                let _ = tokio::fs::remove_dir_all(entry.path()).await;
            }
            continue;
        }
        if !metadata.is_dir() {
            continue;
        }
        let modified = match tokio::fs::metadata(entry.path().join(".access")).await {
            Ok(access) => access.modified().unwrap_or(std::time::UNIX_EPOCH),
            Err(_) => metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
        };
        let size = directory_size(&entry.path()).await;
        generations.push((modified, size, entry.path()));
    }
    generations.sort_by_key(|(modified, _, _)| *modified);
    let mut bytes: u64 = generations.iter().map(|(_, size, _)| *size).sum();
    let mut count = generations.len();
    for (_, size, path) in generations {
        if count <= max_tracks && bytes <= max_bytes {
            break;
        }
        if tokio::fs::remove_dir_all(path).await.is_ok() {
            count = count.saturating_sub(1);
            bytes = bytes.saturating_sub(size);
        }
    }
}

async fn directory_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut pending = vec![path.to_owned()];
    while let Some(dir) = pending.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(metadata) = entry.metadata().await else {
                continue;
            };
            if metadata.is_dir() {
                pending.push(entry.path());
            } else {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use plurx_pgs::{InspectionReport, NormalizedComposition, NormalizedObject};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn file(path: PathBuf) -> MediaFile {
        MediaFile {
            id: 7,
            item_id: 1,
            path,
            size: 4,
            mtime: 5,
            duration_ms: Some(10_000),
            container: Some("mkv".into()),
            video_codec: Some("hevc".into()),
            video_profile: None,
            width: Some(3840),
            height: Some(2160),
            bit_depth: Some(10),
            hdr: Some("dolby_vision".into()),
            hdr_format: Some("Dolby Vision".into()),
            bitrate: None,
            audio_streams: vec![],
            subtitle_streams: vec![],
            scanned_at: 0,
            audio_offset_ms: 0,
            probed: true,
        }
    }

    fn report() -> InspectionReport {
        InspectionReport {
            adapter_profile: "bounded-sup-v1",
            libpgs_version: plurx_pgs::REVIEWED_LIBPGS_VERSION,
            source_bytes: 1,
            parser_bytes_read: 1,
            segments: 1,
            display_sets: 3,
            content_display_sets: 2,
            clear_display_sets: 1,
            duplicate_timestamps: 0,
            pts_wraps: 0,
            epoch_continue_display_sets: 0,
            palette_definitions: 1,
            object_definitions: 1,
            max_canvas_width: 1920,
            max_canvas_height: 1080,
            max_canvas_pixels: 2_073_600,
            max_composition_objects: 1,
            max_object_rgba_bytes: 4,
            max_object_rle_bytes: 1,
            peak_cached_pixel_bytes: 1,
            compositions: vec![],
        }
    }

    fn object() -> NormalizedObject {
        NormalizedObject {
            x: 10,
            y: 20,
            width: 1,
            height: 1,
            rgba: vec![255, 255, 255, 255],
            rgba_sha256: "unused-by-publisher".into(),
        }
    }

    async fn publish_empty_manifest(
        root: &Path,
        final_dir: &Path,
        file: &MediaFile,
        index: i64,
        generation: &str,
    ) -> Result<(), OverlayError> {
        tokio::fs::create_dir_all(root)
            .await
            .map_err(|error| OverlayError::Internal(format!("creating test cache: {error}")))?;
        let stage = root.join(format!(".tmp-test-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir(&stage)
            .await
            .map_err(|error| OverlayError::Internal(format!("creating test stage: {error}")))?;
        let mut staging = StagingDir::new(stage);
        tokio::fs::create_dir(staging.path().join("objects"))
            .await
            .map_err(|error| OverlayError::Internal(format!("creating test objects: {error}")))?;
        let manifest = OverlayManifest {
            schema: SCHEMA,
            generation: generation.to_owned(),
            file_id: file.id,
            track_index: index,
            kind: "pgs".into(),
            timebase: "source_ms".into(),
            duration_ms: file.duration_ms.unwrap_or_default(),
            cues: vec![],
        };
        tokio::fs::write(
            staging.path().join("manifest.json"),
            serde_json::to_vec(&manifest)
                .map_err(|error| OverlayError::Internal(error.to_string()))?,
        )
        .await
        .map_err(|error| OverlayError::Internal(format!("writing test manifest: {error}")))?;
        validate_generation(
            staging.path(),
            file,
            index,
            generation,
            Arc::new(AtomicBool::new(false)),
        )
        .await?;
        tokio::fs::rename(staging.path(), final_dir)
            .await
            .map_err(|error| {
                OverlayError::Internal(format!("publishing test generation: {error}"))
            })?;
        staging.disarm();
        Ok(())
    }

    #[test]
    fn manifest_is_complete_snapshots_with_clear_gaps_and_deduplicated_images() {
        let dir = tempfile::tempdir().expect("cache");
        let file = file(dir.path().join("source.mkv"));
        let generation = generation(&file, 0);
        let track = NormalizedTrack {
            report: report(),
            compositions: vec![
                NormalizedComposition {
                    pts_90khz: 90_000,
                    start_ms: 1000.0,
                    canvas_width: 1920,
                    canvas_height: 1080,
                    objects: vec![object()],
                },
                // Same complete state: coalesced, not a second cue/object.
                NormalizedComposition {
                    pts_90khz: 180_000,
                    start_ms: 2000.0,
                    canvas_width: 1920,
                    canvas_height: 1080,
                    objects: vec![object()],
                },
                NormalizedComposition {
                    pts_90khz: 270_000,
                    start_ms: 3000.0,
                    canvas_width: 1920,
                    canvas_height: 1080,
                    objects: vec![],
                },
            ],
        };
        compile_generation(dir.path(), &file, 0, &generation, track, None).expect("compile");
        let manifest: OverlayManifest = serde_json::from_slice(
            &std::fs::read(dir.path().join("manifest.json")).expect("manifest"),
        )
        .expect("valid manifest");
        assert_eq!(manifest.cues.len(), 1);
        assert_eq!(
            (manifest.cues[0].start_ms, manifest.cues[0].end_ms),
            (1000, 3000)
        );
        assert_eq!(
            (
                manifest.cues[0].canvas_width,
                manifest.cues[0].canvas_height
            ),
            (1920, 1080),
            "a 4K MediaFile retains its authored 1080p PGS canvas"
        );
        assert_eq!(
            std::fs::read_dir(dir.path().join("objects"))
                .expect("objects")
                .count(),
            1
        );
    }

    #[test]
    fn manifest_clamps_a_missing_final_clear_to_media_duration() {
        let dir = tempfile::tempdir().expect("cache");
        let mut file = file(dir.path().join("source.mkv"));
        file.duration_ms = Some(10_000);
        let generation = generation(&file, 0);
        let track = NormalizedTrack {
            report: report(),
            compositions: vec![NormalizedComposition {
                pts_90khz: 90_000,
                start_ms: 1000.0,
                canvas_width: 1920,
                canvas_height: 1080,
                objects: vec![object()],
            }],
        };

        compile_generation(dir.path(), &file, 0, &generation, track, None).expect("compile");
        let manifest: OverlayManifest = serde_json::from_slice(
            &std::fs::read(dir.path().join("manifest.json")).expect("manifest"),
        )
        .expect("valid manifest");
        assert_eq!(manifest.cues.len(), 1);
        assert_eq!(
            (manifest.cues[0].start_ms, manifest.cues[0].end_ms),
            (1000, 10_000)
        );
    }

    #[test]
    fn identical_timestamps_keep_the_last_display_set_in_file_order() {
        let dir = tempfile::tempdir().expect("cache");
        let file = file(dir.path().join("source.mkv"));
        let generation = generation(&file, 0);
        let first = object();
        let mut last = object();
        last.x = 99;
        let track = NormalizedTrack {
            report: report(),
            compositions: vec![
                NormalizedComposition {
                    pts_90khz: 90_000,
                    start_ms: 1000.0,
                    canvas_width: 1920,
                    canvas_height: 1080,
                    objects: vec![first],
                },
                NormalizedComposition {
                    pts_90khz: 90_000,
                    start_ms: 1000.0,
                    canvas_width: 1920,
                    canvas_height: 1080,
                    objects: vec![last],
                },
                NormalizedComposition {
                    pts_90khz: 180_000,
                    start_ms: 2000.0,
                    canvas_width: 1920,
                    canvas_height: 1080,
                    objects: vec![],
                },
            ],
        };

        compile_generation(dir.path(), &file, 0, &generation, track, None).expect("compile");
        let manifest: OverlayManifest = serde_json::from_slice(
            &std::fs::read(dir.path().join("manifest.json")).expect("manifest"),
        )
        .expect("valid manifest");
        assert_eq!(manifest.cues.len(), 1);
        assert_eq!(manifest.cues[0].objects[0].x, 99);
        assert_eq!(
            (manifest.cues[0].start_ms, manifest.cues[0].end_ms),
            (1000, 2000)
        );
    }

    #[test]
    fn same_timestamp_replacement_coalesces_with_the_preceding_state() {
        let dir = tempfile::tempdir().expect("cache");
        let file = file(dir.path().join("source.mkv"));
        let generation = generation(&file, 0);
        let mut transient = object();
        transient.x = 99;
        let track = NormalizedTrack {
            report: report(),
            compositions: vec![
                NormalizedComposition {
                    pts_90khz: 0,
                    start_ms: 0.0,
                    canvas_width: 1920,
                    canvas_height: 1080,
                    objects: vec![object()],
                },
                NormalizedComposition {
                    pts_90khz: 90_000,
                    start_ms: 1000.0,
                    canvas_width: 1920,
                    canvas_height: 1080,
                    objects: vec![transient],
                },
                NormalizedComposition {
                    pts_90khz: 90_000,
                    start_ms: 1000.0,
                    canvas_width: 1920,
                    canvas_height: 1080,
                    objects: vec![object()],
                },
                NormalizedComposition {
                    pts_90khz: 180_000,
                    start_ms: 2000.0,
                    canvas_width: 1920,
                    canvas_height: 1080,
                    objects: vec![],
                },
            ],
        };

        compile_generation(dir.path(), &file, 0, &generation, track, None).expect("compile");
        let manifest: OverlayManifest = serde_json::from_slice(
            &std::fs::read(dir.path().join("manifest.json")).expect("manifest"),
        )
        .expect("valid manifest");
        assert_eq!(manifest.cues.len(), 1);
        assert_eq!(manifest.cues[0].objects[0].x, 10);
        assert_eq!(
            (manifest.cues[0].start_ms, manifest.cues[0].end_ms),
            (0, 2000)
        );
    }

    #[test]
    fn png_encoder_writes_a_valid_rgba_header_and_crc_chunks() {
        let png = encode_rgba_png(1, 1, &[1, 2, 3, 4]).expect("png");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[24..29], &[8, 6, 0, 0, 0]);
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }

    #[tokio::test]
    async fn cold_requests_coalesce_and_publish_only_after_runner_completes() {
        let dir = tempfile::tempdir().expect("cache");
        let file = file(dir.path().join("source.mkv"));
        let runs = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let runner: PrepareRunner = {
            let runs = Arc::clone(&runs);
            let release = Arc::clone(&release);
            Arc::new(move |root, final_dir, file, index, generation| {
                let runs = Arc::clone(&runs);
                let release = Arc::clone(&release);
                Box::pin(async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    let _permit = release.acquire().await.expect("release");
                    publish_empty_manifest(&root, &final_dir, &file, index, &generation).await
                })
            })
        };

        assert!(matches!(
            prepare_with(
                dir.path(),
                &file,
                0,
                Arc::clone(&runner),
                PREPARE_TIMEOUT,
                false,
            )
            .await,
            Ok(PrepareState::Preparing)
        ));
        assert!(matches!(
            prepare_with(dir.path(), &file, 0, runner, PREPARE_TIMEOUT, false).await,
            Ok(PrepareState::Preparing)
        ));
        tokio::task::yield_now().await;
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(tokio::fs::metadata(manifest_path(dir.path(), &file, 0))
            .await
            .is_err());
        release.add_permits(1);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    prepare_with(
                        dir.path(),
                        &file,
                        0,
                        Arc::new(|_, _, _, _, _| Box::pin(async { unreachable!() })),
                        PREPARE_TIMEOUT,
                        false,
                    )
                    .await,
                    Ok(PrepareState::Ready(_))
                ) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("publication");
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_torn_published_generation_is_deleted_and_reprepared() {
        let dir = tempfile::tempdir().expect("cache");
        let file = file(dir.path().join("source.mkv"));
        let final_dir = generation_dir(dir.path(), &file, 0);
        tokio::fs::create_dir_all(&final_dir)
            .await
            .expect("torn generation");
        tokio::fs::write(final_dir.join("manifest.json"), b"")
            .await
            .expect("torn manifest");
        let rejection = validate_generation(
            &final_dir,
            &file,
            0,
            &generation(&file, 0),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .expect_err("the empty manifest must be rejected");
        assert!(
            matches!(
                &rejection,
                OverlayError::Internal(why)
                    if why.starts_with("validating PGS manifest: EOF while parsing a value")
            ),
            "unexpected torn-generation rejection: {rejection}"
        );
        let runs = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let runner: PrepareRunner = {
            let runs = Arc::clone(&runs);
            let release = Arc::clone(&release);
            Arc::new(move |root, final_dir, file, index, generation| {
                let runs = Arc::clone(&runs);
                let release = Arc::clone(&release);
                Box::pin(async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    let _permit = release.acquire().await.expect("release");
                    publish_empty_manifest(&root, &final_dir, &file, index, &generation).await
                })
            })
        };

        assert!(matches!(
            prepare_with(
                dir.path(),
                &file,
                0,
                Arc::clone(&runner),
                PREPARE_TIMEOUT,
                false,
            )
            .await,
            Ok(PrepareState::Preparing)
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            while runs.load(Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("repair started");
        assert!(
            tokio::fs::metadata(&final_dir).await.is_err(),
            "reader-side validation must remove the torn generation"
        );
        for _ in 0..64 {
            assert!(matches!(
                prepare_with(
                    dir.path(),
                    &file,
                    0,
                    Arc::clone(&runner),
                    PREPARE_TIMEOUT,
                    false,
                )
                .await,
                Ok(PrepareState::Preparing)
            ));
        }
        tokio::task::yield_now().await;
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        release.add_permits(1);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    prepare_with(
                        dir.path(),
                        &file,
                        0,
                        Arc::clone(&runner),
                        PREPARE_TIMEOUT,
                        false,
                    )
                    .await,
                    Ok(PrepareState::Ready(_))
                ) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("self-healed publication");
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn timed_out_preparation_is_negatively_memoized() {
        let dir = tempfile::tempdir().expect("cache");
        let file = file(dir.path().join("source.mkv"));
        let runs = Arc::new(AtomicUsize::new(0));
        let runner: PrepareRunner = {
            let runs = Arc::clone(&runs);
            Arc::new(move |_, _, _, _, _| {
                let runs = Arc::clone(&runs);
                Box::pin(async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    Ok(())
                })
            })
        };
        assert!(matches!(
            prepare_with(
                dir.path(),
                &file,
                0,
                Arc::clone(&runner),
                Duration::from_millis(20),
                false,
            )
            .await,
            Ok(PrepareState::Preparing)
        ));
        let key = generation_dir(dir.path(), &file, 0);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if remembered_failure(&key).await.is_some() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("negative memo");
        assert!(matches!(
            prepare_with(
                dir.path(),
                &file,
                0,
                runner,
                Duration::from_millis(20),
                false,
            )
            .await,
            Err(OverlayError::Unavailable(_))
        ));
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn generation_invalidates_on_track_or_source_identity_change() {
        let mut file = file("/media/movie.mkv".into());
        let original = generation(&file, 0);
        assert_ne!(original, generation(&file, 1));
        file.mtime += 1;
        assert_ne!(original, generation(&file, 0));
    }

    #[tokio::test]
    async fn ffmpeg_demux_to_published_manifest_preserves_source_timing() {
        crate::transcode::require_ffmpeg();
        let dir = tempfile::tempdir().expect("fixture directory");
        let sup = dir.path().join("fixture.sup");
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/mkpgs");
        let authored = std::process::Command::new(script)
            .args(["1920", "1080"])
            .arg(&sup)
            .output()
            .expect("run deterministic PGS author");
        assert!(
            authored.status.success(),
            "PGS fixture author failed: {}",
            String::from_utf8_lossy(&authored.stderr)
        );

        let source = dir.path().join("source.mkv");
        let muxed = std::process::Command::new(ffmpeg_bin())
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=320x180:r=1:d=16",
                "-itsoffset",
                "1",
                "-i",
            ])
            .arg(&sup)
            .args([
                "-map", "0:v:0", "-map", "1:s:0", "-c:v", "mpeg4", "-c:s", "copy", "-t", "16",
            ])
            .arg(&source)
            .output()
            .expect("mux deterministic PGS fixture");
        assert!(
            muxed.status.success(),
            "PGS fixture mux failed: {}",
            String::from_utf8_lossy(&muxed.stderr)
        );

        let metadata = std::fs::metadata(&source).expect("source metadata");
        let mut file = file(source);
        file.size = metadata.len() as i64;
        file.mtime = metadata_mtime(&metadata);
        file.duration_ms = Some(16_000);
        let stage = dir.path().join("stage");
        tokio::fs::create_dir(&stage).await.expect("stage");
        let generation = generation(&file, 0);
        prepare_stage(&stage, &file, 0, &generation)
            .await
            .expect("production PGS preparation");

        let manifest: OverlayManifest = serde_json::from_slice(
            &tokio::fs::read(stage.join("manifest.json"))
                .await
                .expect("manifest"),
        )
        .expect("manifest schema");
        assert_eq!(manifest.cues.len(), 2);
        assert_eq!(
            (manifest.cues[0].start_ms, manifest.cues[0].end_ms),
            (1000, 7000)
        );
        assert_eq!(
            (manifest.cues[1].start_ms, manifest.cues[1].end_ms),
            (9000, 15000)
        );
        let object_name = manifest.cues[0].objects[0]
            .image
            .rsplit('/')
            .next()
            .expect("object name");
        let png = tokio::fs::read(stage.join("objects").join(object_name))
            .await
            .expect("PNG object");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert!(tokio::fs::metadata(stage.join("track.sup")).await.is_err());
    }

    #[test]
    fn preparation_capacity_refuses_a_third_cold_generation() {
        let capacity = Arc::new(tokio::sync::Semaphore::new(2));
        let _first = try_capacity(Arc::clone(&capacity)).expect("first producer");
        let _second = try_capacity(Arc::clone(&capacity)).expect("second producer");
        assert!(matches!(
            try_capacity(capacity),
            Err(OverlayError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn cache_prune_removes_the_least_recently_accessed_generation() {
        let root = tempfile::tempdir().expect("cache");
        let older = root.path().join("older");
        let newer = root.path().join("newer");
        tokio::fs::create_dir(&older).await.expect("older");
        tokio::fs::write(older.join(".access"), b"1")
            .await
            .expect("old access");
        tokio::time::sleep(Duration::from_millis(20)).await;
        tokio::fs::create_dir(&newer).await.expect("newer");
        tokio::fs::write(newer.join(".access"), b"2")
            .await
            .expect("new access");

        prune_bounded(root.path(), u64::MAX, 1).await;
        assert!(tokio::fs::metadata(&older).await.is_err());
        assert!(tokio::fs::metadata(&newer).await.is_ok());
    }

    #[tokio::test]
    async fn dropping_staging_ownership_reaps_partial_output() {
        let root = tempfile::tempdir().expect("cache");
        let path = root.path().join(".tmp-cancelled");
        tokio::fs::create_dir(&path).await.expect("stage");
        tokio::fs::write(path.join("partial.png"), b"partial")
            .await
            .expect("partial");
        drop(StagingDir::new(path.clone()));
        tokio::time::timeout(Duration::from_secs(5), async {
            while tokio::fs::metadata(&path).await.is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("staging cleanup");
    }
}
