//! Serve and reconcile node-local artwork.
//!
//! Item rows replicate through Raft, but artwork bytes deliberately do not.
//! Every voter therefore pulls missing files from another live voter and
//! atomically materializes them. A short-lived HMAC proves that the caller is
//! an admitted node; reusable user tokens never cross between peers.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path as FsPath;
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{stream, StreamExt};
use plurx_core::cluster::membership::{ArtworkPeerAuth, MembershipManager};
use plurx_core::domain::Item;
use plurx_core::error::StoreError;

use super::error::ApiError;
use super::extract::AuthUser;
use crate::state::AppState;

const NODE_ID_HEADER: &str = "x-plurx-node-id";
const TIMESTAMP_HEADER: &str = "x-plurx-artwork-time";
const SIGNATURE_HEADER: &str = "x-plurx-artwork-signature";
const MAX_ARTWORK_BYTES: u64 = 15 * 1024 * 1024;
const MATERIALIZE_CONCURRENCY: usize = 8;
const SOURCE_REPAIRS_PER_PASS: usize = 4;
const SOURCE_REPAIR_BACKOFF: Duration = Duration::from_secs(6 * 60 * 60);
const MATERIALIZE_INTERVAL: Duration = Duration::from_secs(60);

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

    let peers = match state.membership.reachable_peer_http_urls().await {
        Ok(peers) => peers,
        Err(error) => {
            tracing::warn!(code = error.code(), "cannot read artwork peer roster");
            return Err(ApiError::NotFound("image"));
        }
    };
    let Ok(client) = artwork_client() else {
        return Err(ApiError::NotFound("image"));
    };
    let Some(bytes) = fetch_peer_artwork(&client, &state.membership, &peers, safe_name).await
    else {
        return Err(ApiError::NotFound("image"));
    };

    if let Err(error) = install_artwork(&state.artwork_dir, safe_name, &bytes).await {
        // The caller can still use the complete peer response. Log the failed
        // materialization so a read-only or full data directory is visible,
        // but do not turn working peer failover into another broken image.
        tracing::warn!(filename = safe_name, %error, "cannot materialize peer artwork");
    }
    Ok(artwork_response(&state.artwork_dir.join(safe_name), bytes))
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
            (
                header::CACHE_CONTROL,
                "private, max-age=604800, immutable".to_owned(),
            ),
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
    for peer in peers {
        let Some(url) = peer_artwork_url(peer, filename) else {
            tracing::warn!(peer, "ignoring invalid artwork peer URL");
            continue;
        };
        let response = match client
            .get(url)
            .header(NODE_ID_HEADER, &auth.node_id)
            .header(TIMESTAMP_HEADER, auth.timestamp_ms)
            .header(SIGNATURE_HEADER, &auth.signature)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => response,
            Ok(_) => continue,
            Err(error) => {
                tracing::debug!(peer, %error, "artwork peer request failed");
                continue;
            }
        };
        if response
            .content_length()
            .is_some_and(|length| length > MAX_ARTWORK_BYTES)
        {
            continue;
        }
        let is_image = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.to_ascii_lowercase().starts_with("image/"));
        if !is_image {
            continue;
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
    }
    None
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

fn artwork_references(items: Vec<Item>) -> Vec<ArtworkReference> {
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
    let client = artwork_client().map_err(|error| StoreError::Database(error.to_string()))?;
    let pulls = stream::iter(missing.into_iter().map(|reference| {
        let client = client.clone();
        let membership = state.membership.clone();
        let peers = peers.clone();
        let artwork_dir = state.artwork_dir.clone();
        async move {
            let bytes = fetch_peer_artwork(&client, &membership, &peers, &reference.filename).await;
            let copied = if let Some(bytes) = bytes {
                install_artwork(&artwork_dir, &reference.filename, &bytes)
                    .await
                    .is_ok()
            } else {
                false
            };
            (reference, copied)
        }
    }))
    .buffer_unordered(MATERIALIZE_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    let now = Instant::now();
    let mut repair_items = BTreeSet::new();
    for (reference, copied) in pulls {
        if copied {
            report.copied += 1;
            repair_after.remove(&reference.item_id);
        } else {
            report.unresolved += 1;
            if repair_after
                .get(&reference.item_id)
                .is_none_or(|retry| *retry <= now)
            {
                repair_items.insert(reference.item_id);
            }
        }
    }

    for item_id in repair_items.into_iter().take(SOURCE_REPAIRS_PER_PASS) {
        repair_after.insert(item_id, now + SOURCE_REPAIR_BACKOFF);
        match state.jobs.refresh_item_artwork(item_id).await {
            Ok(_) => report.source_repairs += 1,
            Err(error) => tracing::warn!(item_id, %error, "artwork source repair failed"),
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
    use axum::body::Bytes;
    use axum::extract::Path;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::get;
    use axum::Router;

    use super::*;

    #[test]
    fn versioned_artwork_is_private_and_immutable() {
        let response = artwork_response(
            FsPath::new("287-poster.jpg"),
            b"\xff\xd8\xff artwork".to_vec(),
        );
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/jpeg");
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, max-age=604800, immutable"
        );
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
