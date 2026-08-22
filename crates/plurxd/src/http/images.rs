//! Serve and reconcile node-local artwork.
//!
//! Item rows replicate through Raft, but artwork bytes deliberately do not.
//! Every voter therefore pulls missing files from another live voter and
//! atomically materializes them. A short-lived HMAC proves that the caller is
//! an admitted node; reusable user tokens never cross between peers.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path as FsPath;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{stream, StreamExt};
use plurx_core::cluster::membership::{ArtworkPeerAuth, MembershipManager};
use plurx_core::error::StoreError;
use plurx_core::store::ArtworkInventoryItem;

use super::error::ApiError;
use super::extract::AuthUser;
use crate::state::AppState;
use tokio::sync::{Mutex, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};

const NODE_ID_HEADER: &str = "x-plurx-node-id";
const TIMESTAMP_HEADER: &str = "x-plurx-artwork-time";
const SIGNATURE_HEADER: &str = "x-plurx-artwork-signature";
const MAX_ARTWORK_BYTES: u64 = 15 * 1024 * 1024;
const MATERIALIZE_CONCURRENCY: usize = 8;
const SOURCE_REPAIRS_PER_PASS: usize = 4;
const SOURCE_REPAIR_BACKOFF: Duration = Duration::from_secs(6 * 60 * 60);
const SOURCE_REPAIR_LEASE: Duration = Duration::from_secs(5 * 60);
const MATERIALIZE_INTERVAL: Duration = Duration::from_secs(60);
const SOURCE_REPAIR_GRACE: Duration = Duration::from_secs(2 * 60);
const ARTWORK_FETCH_TOTAL_TIMEOUT: Duration = Duration::from_secs(3);
const PEER_RACE_CONCURRENCY: usize = 3;

/// Shared process-wide bounds for on-demand and background peer fetches.
///
/// The keyed mutex collapses simultaneous misses for one filename; the global
/// semaphore bounds distinct misses so an authenticated card-grid burst cannot
/// allocate one 15 MiB response buffer per request without limit.
#[derive(Debug)]
pub(crate) struct ArtworkCoordinator {
    client: Result<reqwest::Client, String>,
    permits: Arc<Semaphore>,
    filenames: Mutex<HashMap<String, Weak<Mutex<()>>>>,
}

impl ArtworkCoordinator {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            client: artwork_client().map_err(|error| error.to_string()),
            permits: Arc::new(Semaphore::new(MATERIALIZE_CONCURRENCY)),
            filenames: Mutex::new(HashMap::new()),
        })
    }

    async fn filename(&self, filename: &str) -> OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.filenames.lock().await;
            // Weak entries exist only to let overlapping requests find each
            // other. Prune completed keys on every miss so arbitrary valid
            // filenames cannot turn the singleflight index into an unbounded
            // process-lifetime cache.
            locks.retain(|_, lock| lock.strong_count() > 0);
            if let Some(lock) = locks.get(filename).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(Mutex::new(()));
                locks.insert(filename.to_owned(), Arc::downgrade(&lock));
                lock
            }
        };
        lock.lock_owned().await
    }

    async fn permit(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.permits).acquire_owned().await.ok()
    }

    fn client(&self) -> Option<reqwest::Client> {
        self.client.as_ref().ok().cloned()
    }
}

/// GET /api/v1/images/:filename
pub async fn serve(
    _user: AuthUser,
    State(state): State<AppState>,
    Path(filename): Path<String>,
) -> Result<Response, ApiError> {
    serve_cluster_artwork(&state, &filename).await
}

/// GET /api/v1/cluster/artwork/:filename
///
/// This route is intentionally not user-authenticated. Its credential is a
/// one-minute, filename-bound cluster proof, and verification also confirms
/// that the signing node is still a reachable non-tombstoned member.
pub async fn serve_peer(
    State(state): State<AppState>,
    Path(filename): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let safe_name = safe_artwork_name(&filename)?;
    let auth = peer_auth_from_headers(&headers).ok_or(ApiError::Unauthorized)?;
    let verified = state
        .membership
        .verify_artwork_peer_auth(safe_name, &auth)
        .await
        .map_err(|error| {
            tracing::warn!(code = error.code(), "cannot verify artwork peer proof");
            ApiError::Unauthorized
        })?;
    if !verified {
        return Err(ApiError::Unauthorized);
    }
    serve_local_artwork(&state.artwork_dir, safe_name).await
}

/// Serve local artwork, or retrieve it from a reachable voter and materialize
/// it locally. The peer-only route reads local bytes and never recurses, so a
/// filename absent everywhere remains a bounded 404 instead of cycling.
pub(crate) async fn serve_cluster_artwork(
    state: &AppState,
    filename: &str,
) -> Result<Response, ApiError> {
    let safe_name = safe_artwork_name(filename)?;
    if let Ok(response) = serve_local_artwork(&state.artwork_dir, safe_name).await {
        return Ok(response);
    }

    let membership = state.membership.clone();
    let Some(bytes) = fetch_and_materialize(
        &state.artwork_fetch,
        &state.artwork_dir,
        safe_name,
        move |client| async move {
            let peers = match membership.reachable_peer_http_urls().await {
                Ok(peers) => peers,
                Err(error) => {
                    tracing::warn!(code = error.code(), "cannot read artwork peer roster");
                    return None;
                }
            };
            fetch_peer_artwork(&client, &membership, &peers, safe_name).await
        },
    )
    .await
    else {
        return Err(ApiError::NotFound("image"));
    };
    Ok(artwork_response(&state.artwork_dir.join(safe_name), bytes))
}

async fn fetch_and_materialize<F, Fut>(
    coordinator: &ArtworkCoordinator,
    artwork_dir: &FsPath,
    filename: &str,
    fetch: F,
) -> Option<Vec<u8>>
where
    F: FnOnce(reqwest::Client) -> Fut,
    Fut: std::future::Future<Output = Option<Vec<u8>>>,
{
    tokio::time::timeout(
        ARTWORK_FETCH_TOTAL_TIMEOUT,
        fetch_and_materialize_inner(coordinator, artwork_dir, filename, fetch),
    )
    .await
    .ok()
    .flatten()
}

async fn fetch_and_materialize_inner<F, Fut>(
    coordinator: &ArtworkCoordinator,
    artwork_dir: &FsPath,
    filename: &str,
    fetch: F,
) -> Option<Vec<u8>>
where
    F: FnOnce(reqwest::Client) -> Fut,
    Fut: std::future::Future<Output = Option<Vec<u8>>>,
{
    let _filename = coordinator.filename(filename).await;
    if let Ok(bytes) = tokio::fs::read(artwork_dir.join(filename)).await {
        if !bytes.is_empty() {
            return Some(bytes);
        }
    }
    let _permit = coordinator.permit().await?;
    let client = coordinator.client()?;
    let bytes = fetch(client).await?;
    if let Err(error) = install_artwork(artwork_dir, filename, &bytes).await {
        // The caller can still use the complete peer response. Log the failed
        // materialization so a read-only or full data directory is visible,
        // but do not turn working peer failover into another broken image.
        tracing::warn!(filename, %error, "cannot materialize peer artwork");
    }
    Some(bytes)
}

async fn serve_local_artwork(artwork_dir: &FsPath, filename: &str) -> Result<Response, ApiError> {
    let path = artwork_dir.join(filename);
    match tokio::fs::read(&path).await {
        Ok(bytes) if !bytes.is_empty() => Ok(artwork_response(&path, bytes)),
        _ => Err(ApiError::NotFound("image")),
    }
}

fn safe_artwork_name(filename: &str) -> Result<&str, ApiError> {
    // Only a bare filename is allowed — no directories, no traversal.
    FsPath::new(filename)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| *name == filename && !name.is_empty())
        .ok_or_else(|| ApiError::BadRequest("invalid image name".into()))
}

fn artwork_response(path: &FsPath, bytes: Vec<u8>) -> Response {
    let mime = mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string();
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "public, max-age=604800".to_owned()),
        ],
        bytes,
    )
        .into_response()
}

fn artwork_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(750))
        .timeout(Duration::from_secs(3))
        // A redirect could send a valid node proof to a host the replicated
        // roster never authorized.
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

async fn fetch_peer_artwork(
    client: &reqwest::Client,
    membership: &MembershipManager,
    peers: &[String],
    filename: &str,
) -> Option<Vec<u8>> {
    let auth = membership.artwork_peer_auth(filename).ok()?;
    fetch_peer_artwork_with_auth(client, peers, filename, &auth).await
}

async fn fetch_peer_artwork_with_auth(
    client: &reqwest::Client,
    peers: &[String],
    filename: &str,
    auth: &ArtworkPeerAuth,
) -> Option<Vec<u8>> {
    // Build an owned synchronous request list before constructing any future.
    // Returning an async block from a borrowed iterator adapter makes the
    // composed handler lifetime-specific even when the block clones its input.
    let mut requests = Vec::with_capacity(peers.len());
    for peer in peers {
        match peer_artwork_url(peer, filename) {
            Some(url) => requests.push((peer.clone(), url)),
            None => tracing::warn!(peer, "ignoring invalid artwork peer URL"),
        }
    }
    stream::iter(requests.into_iter().map(|(peer, url)| {
        let client = client.clone();
        let auth = auth.clone();
        async move {
            let response = match client
                .get(url)
                .header(NODE_ID_HEADER, &auth.node_id)
                .header(TIMESTAMP_HEADER, auth.timestamp_ms)
                .header(SIGNATURE_HEADER, &auth.signature)
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => response,
                Ok(_) => return None,
                Err(error) => {
                    tracing::debug!(peer, %error, "artwork peer request failed");
                    return None;
                }
            };
            if response
                .content_length()
                .is_some_and(|length| length > MAX_ARTWORK_BYTES)
            {
                return None;
            }
            let is_image = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.to_ascii_lowercase().starts_with("image/"));
            if !is_image {
                return None;
            }

            let mut body = response.bytes_stream();
            let mut bytes = Vec::new();
            let mut failed = false;
            while let Some(chunk) = body.next().await {
                match chunk {
                    Ok(chunk) if bytes.len() as u64 + chunk.len() as u64 <= MAX_ARTWORK_BYTES => {
                        bytes.extend_from_slice(&chunk);
                    }
                    _ => {
                        failed = true;
                        break;
                    }
                }
            }
            if !failed && !bytes.is_empty() {
                return Some(bytes);
            }
            None
        }
    }))
    .buffer_unordered(PEER_RACE_CONCURRENCY)
    .filter_map(futures_util::future::ready)
    .next()
    .await
}

fn peer_auth_from_headers(headers: &HeaderMap) -> Option<ArtworkPeerAuth> {
    Some(ArtworkPeerAuth {
        node_id: headers.get(NODE_ID_HEADER)?.to_str().ok()?.to_owned(),
        timestamp_ms: headers.get(TIMESTAMP_HEADER)?.to_str().ok()?.parse().ok()?,
        signature: headers.get(SIGNATURE_HEADER)?.to_str().ok()?.to_owned(),
    })
}

fn peer_artwork_url(peer: &str, filename: &str) -> Option<reqwest::Url> {
    let mut url = reqwest::Url::parse(peer).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.set_query(None);
    url.set_fragment(None);
    let mut segments = url.path_segments_mut().ok()?;
    segments.pop_if_empty();
    segments.extend(["api", "v1", "cluster", "artwork", filename]);
    drop(segments);
    Some(url)
}

async fn install_artwork(
    artwork_dir: &FsPath,
    filename: &str,
    bytes: &[u8],
) -> Result<(), std::io::Error> {
    tokio::fs::create_dir_all(artwork_dir).await?;
    let destination = artwork_dir.join(filename);
    let temporary = artwork_dir.join(format!(
        ".{filename}.{}.part",
        uuid::Uuid::new_v4().simple()
    ));
    if let Err(error) = tokio::fs::write(&temporary, bytes).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error);
    }
    match tokio::fs::rename(&temporary, &destination).await {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            Err(error)
        }
    }
}

#[derive(Clone, Debug)]
struct ArtworkReference {
    item_id: i64,
    filename: String,
}

fn artwork_references(items: Vec<ArtworkInventoryItem>) -> Vec<ArtworkReference> {
    let mut references = BTreeMap::<String, i64>::new();
    for item in items {
        for filename in [item.poster_path, item.backdrop_path].into_iter().flatten() {
            if safe_artwork_name(&filename).is_ok() {
                references.entry(filename).or_insert(item.id);
            }
        }
    }
    references
        .into_iter()
        .map(|(filename, item_id)| ArtworkReference { item_id, filename })
        .collect()
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct MaterializeReport {
    pub references: usize,
    pub missing: usize,
    pub copied: usize,
    pub source_repairs: usize,
    pub unresolved: usize,
}

/// Reconcile every filename named by replicated item rows onto this voter.
/// Peer copies are preferred; source/provider repair is a bounded fallback for
/// the case where the last node holding a file disappeared before convergence.
async fn materialize_once(
    state: &AppState,
    repair_after: &mut HashMap<i64, Instant>,
) -> Result<MaterializeReport, StoreError> {
    if !state.membership.is_replicated() {
        return Ok(MaterializeReport::default());
    }

    let references = artwork_references(state.store.items_with_artwork().await?);
    let mut report = MaterializeReport {
        references: references.len(),
        ..Default::default()
    };
    let mut missing = Vec::new();
    for reference in references {
        let path = state.artwork_dir.join(&reference.filename);
        if !tokio::fs::metadata(path)
            .await
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
        {
            missing.push(reference);
        }
    }
    report.missing = missing.len();
    if missing.is_empty() {
        repair_after.clear();
        return Ok(report);
    }

    let peers = match state.membership.reachable_peer_http_urls().await {
        Ok(peers) => peers,
        Err(error) => {
            tracing::warn!(code = error.code(), "cannot read artwork peer roster");
            Vec::new()
        }
    };
    let peers = Arc::new(peers);
    let pulls = stream::iter(missing.into_iter().map(|reference| {
        let membership = state.membership.clone();
        let peers = Arc::clone(&peers);
        let artwork_dir = state.artwork_dir.clone();
        let coordinator = Arc::clone(&state.artwork_fetch);
        async move {
            let filename = reference.filename.clone();
            let fetch_filename = filename.clone();
            let copied = fetch_and_materialize(
                &coordinator,
                &artwork_dir,
                &filename,
                move |client| async move {
                    fetch_peer_artwork(&client, &membership, &peers, &fetch_filename).await
                },
            )
            .await
            .is_some();
            (reference, copied)
        }
    }))
    .buffer_unordered(MATERIALIZE_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    let now = Instant::now();
    let mut unresolved_by_item = BTreeMap::<i64, Vec<String>>::new();
    let mut observed_items = BTreeSet::new();
    for (reference, copied) in pulls {
        observed_items.insert(reference.item_id);
        if copied {
            report.copied += 1;
        } else {
            report.unresolved += 1;
            unresolved_by_item
                .entry(reference.item_id)
                .or_default()
                .push(reference.filename);
        }
    }
    for item_id in observed_items {
        if !unresolved_by_item.contains_key(&item_id) {
            repair_after.remove(&item_id);
        }
    }

    let mut repair_items = BTreeMap::<i64, Vec<String>>::new();
    for (item_id, filenames) in unresolved_by_item {
        let retry = repair_after
            .entry(item_id)
            .or_insert(now + SOURCE_REPAIR_GRACE);
        if *retry <= now {
            repair_items.insert(item_id, filenames);
        }
    }

    for (item_id, filenames) in repair_items.into_iter().take(SOURCE_REPAIRS_PER_PASS) {
        match state
            .membership
            .claim_artwork_source_repair(item_id, SOURCE_REPAIR_LEASE)
            .await
        {
            Ok(true) => {
                let outcome = state.jobs.refresh_item_artwork(item_id).await;
                let materialized =
                    futures_util::future::join_all(filenames.into_iter().map(|filename| {
                        let path = state.artwork_dir.join(&filename);
                        async move {
                            tokio::fs::metadata(path)
                                .await
                                .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
                        }
                    }))
                    .await
                    .into_iter()
                    .all(|present| present);
                if outcome.is_ok() && materialized {
                    report.source_repairs += 1;
                    repair_after.insert(item_id, now + SOURCE_REPAIR_BACKOFF);
                } else {
                    if let Err(error) = outcome {
                        tracing::warn!(item_id, %error, "artwork source repair failed");
                    } else {
                        tracing::warn!(item_id, "artwork source repair produced no requested file");
                    }
                    // Keep the failed claimant's lease until its bucket ends.
                    // Releasing early would let the same deterministic voter
                    // reclaim every minute, multiplying writes and starving a
                    // capable peer until rotation.
                    repair_after.insert(item_id, now + SOURCE_REPAIR_LEASE);
                }
            }
            Ok(false) => {
                repair_after.insert(item_id, now + MATERIALIZE_INTERVAL);
            }
            Err(error) => {
                repair_after.insert(item_id, now + MATERIALIZE_INTERVAL);
                tracing::warn!(item_id, code = error.code(), "artwork repair lease failed");
            }
        }
    }
    Ok(report)
}

pub(crate) async fn materialize_loop(state: AppState) {
    if !state.membership.is_replicated() {
        return;
    }
    let mut repair_after = HashMap::new();
    tokio::time::sleep(Duration::from_secs(2)).await;
    loop {
        match state.membership.local_node_is_active_voter().await {
            Ok(false) => match state.membership.local_node_is_committed_voter().await {
                Ok(false) => {
                    tracing::info!("stopping artwork reconciliation on removed voter");
                    break;
                }
                Ok(true) => {
                    // Membership changes fence the row before changing the
                    // Raft set. Wait for either commit or rollback without
                    // performing provider work in that transition window.
                    tokio::time::sleep(MATERIALIZE_INTERVAL).await;
                    continue;
                }
                Err(error) => {
                    tracing::warn!(
                        code = error.code(),
                        "cannot verify artwork voter membership"
                    );
                    tokio::time::sleep(MATERIALIZE_INTERVAL).await;
                    continue;
                }
            },
            Ok(true) => {}
            Err(error) => {
                tracing::warn!(
                    code = error.code(),
                    "cannot verify artwork reconciliation authority"
                );
                tokio::time::sleep(MATERIALIZE_INTERVAL).await;
                continue;
            }
        }
        match materialize_once(&state, &mut repair_after).await {
            Ok(report) if report.missing > 0 => tracing::info!(
                references = report.references,
                missing = report.missing,
                copied = report.copied,
                source_repairs = report.source_repairs,
                unresolved = report.unresolved,
                "reconciled node-local artwork"
            ),
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "artwork reconciliation failed"),
        }
        tokio::time::sleep(MATERIALIZE_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::body::Bytes;
    use axum::extract::Path;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::get;
    use axum::Router;

    use super::*;

    #[test]
    fn artwork_inventory_deduplicates_bare_names_and_rejects_paths() {
        let references = artwork_references(vec![
            ArtworkInventoryItem {
                id: 10,
                poster_path: Some("shared.jpg".to_owned()),
                backdrop_path: Some("nested/rejected.jpg".to_owned()),
            },
            ArtworkInventoryItem {
                id: 20,
                poster_path: Some("shared.jpg".to_owned()),
                backdrop_path: Some("hero.jpg".to_owned()),
            },
        ]);
        assert_eq!(references.len(), 2);
        assert_eq!(references[0].filename, "hero.jpg");
        assert_eq!(references[0].item_id, 20);
        assert_eq!(references[1].filename, "shared.jpg");
        assert_eq!(references[1].item_id, 10);
    }

    #[tokio::test]
    async fn simultaneous_same_filename_misses_fetch_and_install_once() {
        let coordinator = ArtworkCoordinator::new();
        let directory = Arc::new(tempfile::tempdir().expect("artwork directory"));
        let hits = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..32 {
            let coordinator = Arc::clone(&coordinator);
            let directory = Arc::clone(&directory);
            let hits = Arc::clone(&hits);
            tasks.push(tokio::spawn(async move {
                fetch_and_materialize(
                    &coordinator,
                    directory.path(),
                    "singleflight.jpg",
                    move |_| async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        Some(b"one peer response".to_vec())
                    },
                )
                .await
                .expect("materialized bytes")
            }));
        }
        for task in tasks {
            assert_eq!(task.await.expect("request task"), b"one peer response");
        }
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(
            tokio::fs::read(directory.path().join("singleflight.jpg"))
                .await
                .expect("installed artwork"),
            b"one peer response"
        );
    }

    #[tokio::test]
    async fn distinct_filename_fetches_obey_the_global_buffer_bound() {
        let coordinator = ArtworkCoordinator::new();
        let directory = Arc::new(tempfile::tempdir().expect("artwork directory"));
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for ordinal in 0..24 {
            let coordinator = Arc::clone(&coordinator);
            let directory = Arc::clone(&directory);
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            tasks.push(tokio::spawn(async move {
                let filename = format!("bounded-{ordinal}.jpg");
                fetch_and_materialize(&coordinator, directory.path(), &filename, move |_| {
                    let active = Arc::clone(&active);
                    let maximum = Arc::clone(&maximum);
                    async move {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        maximum.fetch_max(now, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        Some(vec![ordinal as u8 + 1])
                    }
                })
                .await
                .expect("bounded fetch")
            }));
        }
        for task in tasks {
            task.await.expect("request task");
        }
        assert_eq!(maximum.load(Ordering::SeqCst), MATERIALIZE_CONCURRENCY);
    }

    #[test]
    fn peer_url_keeps_a_reverse_proxy_prefix_and_escapes_the_filename() {
        let url =
            peer_artwork_url("https://plurx-b.test/cinema/", "poster one.jpg").expect("peer URL");
        assert_eq!(
            url.as_str(),
            "https://plurx-b.test/cinema/api/v1/cluster/artwork/poster%20one.jpg"
        );
        assert!(peer_artwork_url("file:///tmp/plurx", "poster.jpg").is_none());
    }

    #[test]
    fn peer_proof_headers_are_complete_and_typed() {
        let mut headers = HeaderMap::new();
        headers.insert(NODE_ID_HEADER, "node-b".parse().expect("node"));
        headers.insert(TIMESTAMP_HEADER, "12345".parse().expect("time"));
        headers.insert(SIGNATURE_HEADER, "aabb".parse().expect("signature"));
        assert_eq!(
            peer_auth_from_headers(&headers),
            Some(ArtworkPeerAuth {
                node_id: "node-b".to_owned(),
                timestamp_ms: 12_345,
                signature: "aabb".to_owned(),
            })
        );
        headers.remove(SIGNATURE_HEADER);
        assert!(peer_auth_from_headers(&headers).is_none());
    }

    #[tokio::test]
    async fn a_peer_miss_sends_node_proof_and_materializes_complete_bytes() {
        async fn peer(
            Path(filename): Path<String>,
            headers: HeaderMap,
        ) -> (StatusCode, HeaderMap, Bytes) {
            let authorized = headers
                .get(NODE_ID_HEADER)
                .and_then(|value| value.to_str().ok())
                == Some("node-b")
                && headers
                    .get(TIMESTAMP_HEADER)
                    .and_then(|value| value.to_str().ok())
                    == Some("12345")
                && headers
                    .get(SIGNATURE_HEADER)
                    .and_then(|value| value.to_str().ok())
                    == Some("aabb");
            if !authorized || filename != "poster one.jpg" {
                return (StatusCode::UNAUTHORIZED, HeaderMap::new(), Bytes::new());
            }
            let mut response_headers = HeaderMap::new();
            response_headers.insert(header::CONTENT_TYPE, "image/jpeg".parse().expect("mime"));
            (
                StatusCode::OK,
                response_headers,
                Bytes::from_static(b"\xff\xd8\xff peer artwork"),
            )
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/cinema/api/v1/cluster/artwork/{filename}", get(peer)),
            )
            .await
            .expect("peer server");
        });

        let client = artwork_client().expect("client");
        let bytes = fetch_peer_artwork_with_auth(
            &client,
            &["not a URL".to_owned(), format!("http://{address}/cinema")],
            "poster one.jpg",
            &ArtworkPeerAuth {
                node_id: "node-b".to_owned(),
                timestamp_ms: 12_345,
                signature: "aabb".to_owned(),
            },
        )
        .await
        .expect("peer bytes");
        assert_eq!(bytes, b"\xff\xd8\xff peer artwork");

        let directory = tempfile::tempdir().expect("artwork directory");
        install_artwork(directory.path(), "poster one.jpg", &bytes)
            .await
            .expect("materialize");
        assert_eq!(
            tokio::fs::read(directory.path().join("poster one.jpg"))
                .await
                .expect("cached artwork"),
            bytes
        );
        server.abort();
    }
}
