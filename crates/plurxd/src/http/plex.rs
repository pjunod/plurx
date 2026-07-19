//! Plex Media Server compatibility endpoints (Tier 1, docs/CLIENTS.md §3).
//!
//! A translation façade over plurx-core services for direct-connect clients
//! (Kodi Composite/PKC, python-plexapi, Home Assistant). Responses are XML
//! `MediaContainer` documents — the Plex default these clients use. Auth is by
//! `X-Plex-Token` (a plurx token); a tokenless request falls back to the admin
//! user so "unclaimed server" LAN clients work. Never contacts plex.tv.

use axum::extract::{FromRequestParts, Path, Query, RawQuery, State};
use axum::http::header::{ACCEPT, CONTENT_TYPE};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use plurx_compat_plex as plex;
use plurx_compat_plex::map::{self, View};
use plurx_core::auth;
use plurx_core::domain::{Item, ItemKind, User};
use std::collections::HashMap;

use super::error::ApiError;
use crate::state::AppState;

/// The requesting Plex user: a valid `X-Plex-Token` maps to its plurx user;
/// no token falls back to the admin (unclaimed-LAN convenience).
pub struct PlexUser(pub User);

fn plex_token(parts: &Parts) -> Option<String> {
    if let Some(v) = parts.headers.get("x-plex-token") {
        if let Ok(s) = v.to_str() {
            if !s.is_empty() {
                return Some(s.to_owned());
            }
        }
    }
    parts.uri.query().and_then(|q| {
        q.split('&').find_map(|kv| {
            let (k, val) = kv.split_once('=')?;
            (k == "X-Plex-Token" || k == "x-plex-token").then(|| val.to_owned())
        })
    })
}

impl FromRequestParts<AppState> for PlexUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if let Some(token) = plex_token(parts) {
            let hash = auth::hash_token(&token);
            if let Some(user) = state.store.user_for_token(&hash).await? {
                return Ok(PlexUser(user));
            }
            return Err(ApiError::Unauthorized);
        }
        // No token: fall back to the admin user (unclaimed LAN server).
        let users = state.store.list_users().await?;
        users
            .into_iter()
            .find(|u| u.is_admin)
            .map(PlexUser)
            .ok_or(ApiError::Unauthorized)
    }
}

/// XML `MediaContainer` response with the Plex content type.
fn xml(el: plex::Element) -> Response {
    (
        StatusCode::OK,
        [(CONTENT_TYPE, "application/xml;charset=utf-8")],
        el.to_document(),
    )
        .into_response()
}

/// Whether a request to `/` came from a Plex client (vs a browser).
pub fn looks_like_plex(headers: &HeaderMap) -> bool {
    headers.contains_key("x-plex-token")
        || headers.contains_key("x-plex-client-identifier")
        || headers.contains_key("x-plex-product")
        || headers
            .get(ACCEPT)
            .and_then(|v| v.to_str().ok())
            .map(|a| a.contains("xml") && !a.contains("html"))
            .unwrap_or(false)
}

/// GET /identity — unauthenticated identity probe.
pub async fn identity(State(state): State<AppState>) -> Result<Response, ApiError> {
    let id = state.store.instance_id().await?;
    Ok(xml(plex::identity_container(&id, version())))
}

/// GET / for Plex clients — server capabilities.
pub async fn root(State(state): State<AppState>) -> Result<Response, ApiError> {
    let id = state.store.instance_id().await?;
    Ok(xml(plex::root_container(
        &id,
        &state.server_name,
        version(),
    )))
}

/// GET /library — the library root container clients load before sections.
pub async fn library_root(_user: PlexUser) -> Response {
    let container = plex::Element::new("MediaContainer")
        .attr_i("size", 2)
        .attr("identifier", "com.plexapp.plugins.library")
        .attr("mediaTagPrefix", "/system/bundle/media/flags/")
        .attr("title1", "plurx")
        .child(
            plex::Element::new("Directory")
                .attr("key", "sections")
                .attr("title", "Library Sections"),
        )
        .child(
            plex::Element::new("Directory")
                .attr("key", "recentlyAdded")
                .attr("title", "Recently Added"),
        );
    xml(container)
}

/// GET /library/sections
pub async fn sections(
    _user: PlexUser,
    State(state): State<AppState>,
) -> Result<Response, ApiError> {
    let libs = state.store.list_libraries().await?;
    let dirs = libs.iter().map(map::section_directory).collect();
    Ok(xml(plex::container(dirs)))
}

/// Look up per-user view state for a set of items.
async fn views(
    state: &AppState,
    user_id: i64,
    items: &[Item],
) -> Result<HashMap<i64, View>, ApiError> {
    let ids: Vec<i64> = items.iter().map(|i| i.id).collect();
    let map = state.store.watch_map(user_id, &ids).await?;
    let lookup: HashMap<i64, _> = map.into_iter().collect();
    Ok(items
        .iter()
        .map(|i| (i.id, View::from(lookup.get(&i.id).copied())))
        .collect())
}

/// Build the right element for an item (Video with media, or Directory).
async fn element_for(state: &AppState, item: &Item, view: View) -> Result<plex::Element, ApiError> {
    match item.kind {
        ItemKind::Movie | ItemKind::Episode => {
            let files = state.store.files_for_item(item.id).await?;
            Ok(map::video_element(item, &files, view))
        }
        ItemKind::Show | ItemKind::Season => {
            let children = state.store.get_item_children(item.id).await?;
            Ok(map::directory_element(
                item,
                Some(children.len() as i64),
                view,
            ))
        }
    }
}

/// GET /library/sections/:id/all
pub async fn section_all(
    PlexUser(user): PlexUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    if state.store.get_library(id).await?.is_none() {
        return Err(ApiError::NotFound("section"));
    }
    let page = state
        .store
        .list_top_items(id, Default::default(), 0, 5000)
        .await?;
    let views = views(&state, user.id, &page.items).await?;
    let mut elements = Vec::with_capacity(page.items.len());
    for item in &page.items {
        let view = views.get(&item.id).copied().unwrap_or_default();
        elements.push(element_for(&state, item, view).await?);
    }
    Ok(xml(plex::container(elements)))
}

/// GET /library/metadata/:key
pub async fn metadata(
    PlexUser(user): PlexUser,
    State(state): State<AppState>,
    Path(key): Path<i64>,
) -> Result<Response, ApiError> {
    let item = state
        .store
        .get_item(key)
        .await?
        .ok_or(ApiError::NotFound("metadata"))?;
    let view = View::from(state.store.watch_state(user.id, key).await?);
    Ok(xml(plex::container(vec![
        element_for(&state, &item, view).await?,
    ])))
}

/// GET /library/metadata/:key/children
pub async fn children(
    PlexUser(user): PlexUser,
    State(state): State<AppState>,
    Path(key): Path<i64>,
) -> Result<Response, ApiError> {
    if state.store.get_item(key).await?.is_none() {
        return Err(ApiError::NotFound("metadata"));
    }
    let kids = state.store.get_item_children(key).await?;
    let views = views(&state, user.id, &kids).await?;
    let mut elements = Vec::with_capacity(kids.len());
    for item in &kids {
        let view = views.get(&item.id).copied().unwrap_or_default();
        elements.push(element_for(&state, item, view).await?);
    }
    Ok(xml(plex::container(elements)))
}

/// GET /library/parts/:file_id/:mtime/:name — direct play with range support.
pub async fn part(
    _user: PlexUser,
    State(state): State<AppState>,
    Path((file_id, _mtime, _name)): Path<(i64, String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let file = state
        .store
        .get_file(file_id)
        .await?
        .ok_or(ApiError::NotFound("part"))?;
    super::stream::serve_file_range(&file.path, &headers).await
}

/// GET /library/metadata/:key/thumb  and  /art — serve cached artwork.
pub async fn image(
    _user: PlexUser,
    State(state): State<AppState>,
    Path((key, kind)): Path<(i64, String)>,
) -> Result<Response, ApiError> {
    let item = state
        .store
        .get_item(key)
        .await?
        .ok_or(ApiError::NotFound("image"))?;
    let filename = match kind.as_str() {
        "art" => item.backdrop_path,
        _ => item.poster_path,
    };
    let filename = filename.ok_or(ApiError::NotFound("image"))?;
    super::images::serve_artwork(&state.artwork_dir, &filename).await
}

/// GET /photo/:/transcode — Plex image resizer. We proxy the underlying image
/// (the `url` param points at one of our thumb/art paths); resizing is a
/// documented follow-up (clients tolerate the original size).
pub async fn photo_transcode(
    _user: PlexUser,
    State(state): State<AppState>,
    Query(q): Query<PhotoQuery>,
) -> Result<Response, ApiError> {
    // url like "/library/metadata/42/thumb"
    let url = q.url.unwrap_or_default();
    let mut parts = url.trim_start_matches('/').split('/');
    let (Some("library"), Some("metadata")) = (parts.next(), parts.next()) else {
        return Err(ApiError::NotFound("image"));
    };
    let key = parts
        .next()
        .and_then(|k| k.parse::<i64>().ok())
        .ok_or(ApiError::NotFound("image"))?;
    let kind = parts.next().unwrap_or("thumb").to_owned();

    let item = state
        .store
        .get_item(key)
        .await?
        .ok_or(ApiError::NotFound("image"))?;
    let filename = if kind == "art" {
        item.backdrop_path
    } else {
        item.poster_path
    };
    let filename = filename.ok_or(ApiError::NotFound("image"))?;
    super::images::serve_artwork(&state.artwork_dir, &filename).await
}

#[derive(serde::Deserialize)]
pub struct PhotoQuery {
    pub url: Option<String>,
}

/// GET /:/timeline — playback progress. Plex sends key, time, duration, state.
pub async fn timeline(
    PlexUser(user): PlexUser,
    State(state): State<AppState>,
    Query(q): Query<TimelineQuery>,
) -> Result<Response, ApiError> {
    if let Some(rating_key) = q
        .rating_key
        .or_else(|| q.key.as_deref().and_then(parse_key))
    {
        if state.store.get_item(rating_key).await?.is_some() {
            state
                .store
                .put_progress(user.id, rating_key, q.time.unwrap_or(0).max(0), q.duration)
                .await?;
        }
    }
    // Plex clients accept an empty container ack.
    Ok(xml(plex::container(vec![])))
}

#[derive(serde::Deserialize)]
pub struct TimelineQuery {
    #[serde(rename = "ratingKey")]
    pub rating_key: Option<i64>,
    pub key: Option<String>,
    pub time: Option<i64>,
    pub duration: Option<i64>,
    #[allow(dead_code)]
    pub state: Option<String>,
}

/// GET /:/scrobble and /:/unscrobble — mark watched/unwatched.
pub async fn scrobble(
    PlexUser(user): PlexUser,
    State(state): State<AppState>,
    Query(q): Query<ScrobbleQuery>,
) -> Result<Response, ApiError> {
    if let Some(key) = q.key {
        if state.store.get_item(key).await?.is_some() {
            state.store.set_watched(user.id, key, true).await?;
        }
    }
    Ok(xml(plex::container(vec![])))
}

pub async fn unscrobble(
    PlexUser(user): PlexUser,
    State(state): State<AppState>,
    Query(q): Query<ScrobbleQuery>,
) -> Result<Response, ApiError> {
    if let Some(key) = q.key {
        if state.store.get_item(key).await?.is_some() {
            state.store.set_watched(user.id, key, false).await?;
        }
    }
    Ok(xml(plex::container(vec![])))
}

#[derive(serde::Deserialize)]
pub struct ScrobbleQuery {
    pub key: Option<i64>,
    #[allow(dead_code)]
    pub identifier: Option<String>,
}

/// GET /search and /hubs/search — text search.
pub async fn search(
    PlexUser(user): PlexUser,
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
) -> Result<Response, ApiError> {
    let query = raw
        .as_deref()
        .and_then(|q| {
            q.split('&').find_map(|kv| {
                let (k, v) = kv.split_once('=')?;
                (k == "query").then(|| urldecode(v))
            })
        })
        .unwrap_or_default();
    let hits = state.store.search_items(&query, 50).await?;
    let items: Vec<Item> = hits.into_iter().map(|r| r.item).collect();
    let views = views(&state, user.id, &items).await?;
    let mut elements = Vec::with_capacity(items.len());
    for item in &items {
        let view = views.get(&item.id).copied().unwrap_or_default();
        elements.push(element_for(&state, item, view).await?);
    }
    Ok(xml(plex::container(elements)))
}

fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Parse a Plex `key` like `/library/metadata/42` into a rating key.
fn parse_key(key: &str) -> Option<i64> {
    key.trim_start_matches('/')
        .strip_prefix("library/metadata/")
        .and_then(|rest| rest.split('/').next())
        .and_then(|k| k.parse().ok())
}

fn urldecode(s: &str) -> String {
    let bytes = s.replace('+', " ");
    let bytes = bytes.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            ) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plex_key() {
        assert_eq!(parse_key("/library/metadata/42"), Some(42));
        assert_eq!(parse_key("/library/metadata/42/children"), Some(42));
        assert_eq!(parse_key("/other/7"), None);
    }

    #[test]
    fn urldecodes() {
        assert_eq!(urldecode("the%20matrix"), "the matrix");
        assert_eq!(urldecode("a+b"), "a b");
    }
}
