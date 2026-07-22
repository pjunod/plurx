//! Browsing: library grids, item detail, home hubs, and search. Every item is
//! annotated with the requesting user's watch state.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::Json;
use plurx_core::domain::{Item, ItemKind, ItemSort, WatchState};
use serde::{Deserialize, Serialize};

use super::dto::{in_progress_dto, recent_dto, FileDto, ItemDto};
use super::error::ApiError;
use super::extract::AuthUser;
use crate::state::AppState;

const DEFAULT_LIMIT: i64 = 60;
const MAX_LIMIT: i64 = 200;

fn clamp_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

/// Fetch this user's watch state for a set of items as a lookup map.
async fn watch_lookup(
    state: &AppState,
    user_id: i64,
    items: &[Item],
) -> Result<HashMap<i64, WatchState>, ApiError> {
    let ids: Vec<i64> = items.iter().map(|i| i.id).collect();
    let map = state.store.watch_map(user_id, &ids).await?;
    Ok(map.into_iter().collect())
}

fn annotate(items: Vec<Item>, watch: &HashMap<i64, WatchState>) -> Vec<ItemDto> {
    items
        .into_iter()
        .map(|item| {
            let w = watch.get(&item.id).copied();
            ItemDto::from(item).with_watch(w)
        })
        .collect()
}

#[derive(Deserialize)]
pub struct ListQuery {
    pub sort: Option<String>,
    pub offset: Option<i64>,
    pub limit: Option<i64>,
}

#[derive(Serialize)]
pub struct ItemListResponse {
    pub items: Vec<ItemDto>,
    pub total: i64,
    pub offset: i64,
    pub limit: i64,
}

/// GET /api/v1/libraries/:id/items
pub async fn list_items(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(library_id): Path<i64>,
    Query(q): Query<ListQuery>,
) -> Result<Json<ItemListResponse>, ApiError> {
    if state.store.get_library(library_id).await?.is_none() {
        return Err(ApiError::NotFound("library"));
    }
    let sort = q
        .sort
        .as_deref()
        .and_then(ItemSort::parse)
        .unwrap_or_default();
    let offset = q.offset.unwrap_or(0).max(0);
    let limit = clamp_limit(q.limit);

    let page = state
        .store
        .list_top_items(library_id, sort, offset, limit)
        .await?;
    let watch = watch_lookup(&state, user.id, &page.items).await?;
    Ok(Json(ItemListResponse {
        items: annotate(page.items, &watch),
        total: page.total,
        offset,
        limit,
    }))
}

#[derive(Serialize)]
pub struct ItemDetail {
    pub item: ItemDto,
    /// Parent chain, outermost first (show, then season) — the breadcrumb.
    pub ancestors: Vec<ItemDto>,
    pub children: Vec<ItemDto>,
    pub files: Vec<FileDto>,
}

/// GET /api/v1/items/:id — item plus its ancestors (for breadcrumbs),
/// children (seasons/episodes), and files.
pub async fn item_detail(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<ItemDetail>, ApiError> {
    let item = state
        .store
        .get_item(id)
        .await?
        .ok_or(ApiError::NotFound("item"))?;

    // Walk up the parent chain (episode → season → show; ≤2 hops today, the
    // guard is against a data cycle ever looping this forever).
    let mut ancestors = Vec::new();
    let mut cursor = item.parent_id;
    while let Some(parent_id) = cursor {
        match state.store.get_item(parent_id).await? {
            Some(parent) => {
                cursor = parent.parent_id;
                ancestors.push(parent);
                if ancestors.len() >= 8 {
                    break;
                }
            }
            None => break,
        }
    }
    ancestors.reverse();

    let children = state.store.get_item_children(id).await?;
    let files = match item.kind {
        ItemKind::Movie | ItemKind::Episode => state.store.files_for_item(id).await?,
        _ => Vec::new(),
    };

    // Annotate the item and its children with watch state in one lookup.
    let mut all = children.clone();
    all.push(item.clone());
    let watch = watch_lookup(&state, user.id, &all).await?;

    let item_dto = ItemDto::from(item).with_watch(watch.get(&id).copied());
    Ok(Json(ItemDetail {
        item: item_dto,
        ancestors: ancestors.into_iter().map(Into::into).collect(),
        children: annotate(children, &watch),
        files: files.into_iter().map(Into::into).collect(),
    }))
}

#[derive(Deserialize)]
pub struct HubsQuery {
    pub library_id: Option<i64>,
}

#[derive(Serialize)]
pub struct Hubs {
    pub continue_watching: Vec<ItemDto>,
    pub next_up: Vec<ItemDto>,
    pub recently_added: Vec<ItemDto>,
}

/// GET /api/v1/hubs — the home screen rows.
pub async fn hubs(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Query(q): Query<HubsQuery>,
) -> Result<Json<Hubs>, ApiError> {
    let in_progress = state.store.continue_watching(user.id, 20).await?;
    let continue_watching = in_progress.into_iter().map(in_progress_dto).collect();

    // Next-up episodes (unwatched tracks per show); no per-item watch state.
    let next = state.store.next_up(user.id, 20).await?;
    let next_up = next.into_iter().map(|r| recent_dto(r, None)).collect();

    let recent = state.store.recently_added(q.library_id, 20).await?;
    let recent_items: Vec<Item> = recent.iter().map(|r| r.item.clone()).collect();
    let watch = watch_lookup(&state, user.id, &recent_items).await?;
    let recently_added = recent
        .into_iter()
        .map(|r| {
            let w = watch.get(&r.item.id).copied();
            recent_dto(r, w)
        })
        .collect();

    Ok(Json(Hubs {
        continue_watching,
        next_up,
        recently_added,
    }))
}

#[derive(Deserialize)]
pub struct SearchQuery {
    pub q: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Serialize)]
pub struct SearchResponse {
    pub results: Vec<ItemDto>,
}

/// GET /api/v1/search?q=
pub async fn search(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<SearchResponse>, ApiError> {
    let query = q.q.unwrap_or_default();
    let limit = clamp_limit(q.limit);
    let hits = state.store.search_items(&query, limit).await?;
    let items: Vec<Item> = hits.iter().map(|r| r.item.clone()).collect();
    let watch = watch_lookup(&state, user.id, &items).await?;
    let results = hits
        .into_iter()
        .map(|r| {
            let w = watch.get(&r.item.id).copied();
            recent_dto(r, w)
        })
        .collect();
    Ok(Json(SearchResponse { results }))
}
