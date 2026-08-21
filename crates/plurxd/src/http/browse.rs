//! Browsing: library grids, item detail, home hubs, and search. Every item is
//! annotated with the requesting user's watch state.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::Path as FsPath;

use axum::extract::{Path, Query, State};
use axum::Json;
use plurx_core::domain::{Item, ItemKind, ItemSort, WatchState};
use plurx_core::mediafacts::MediaFacts;
use serde::{Deserialize, Serialize};

use super::dto::{
    chapters_from_probe_json, in_progress_dto, recent_dto, FileDto, ItemDto, ReadingDto,
};
use super::error::ApiError;
use super::extract::AuthUser;
use crate::state::AppState;

const DEFAULT_LIMIT: i64 = 60;
const MAX_LIMIT: i64 = 200;

/// Compare paths the way a listener reads numbered parts: Part 2 precedes
/// Part 10 even though lexical ordering puts `10` first. Non-numeric runs are
/// compared case-insensitively, with the original spelling as a stable tie.
fn natural_path_cmp(left: &FsPath, right: &FsPath) -> Ordering {
    fn chunks(value: &str) -> Vec<(&str, bool)> {
        let mut out = Vec::new();
        let mut start = 0;
        let mut digit = value.as_bytes().first().is_some_and(u8::is_ascii_digit);
        for (index, byte) in value.bytes().enumerate().skip(1) {
            let next = byte.is_ascii_digit();
            if next != digit {
                out.push((&value[start..index], digit));
                start = index;
                digit = next;
            }
        }
        if start < value.len() {
            out.push((&value[start..], digit));
        }
        out
    }

    let left_text = left.to_string_lossy();
    let right_text = right.to_string_lossy();
    let left_chunks = chunks(&left_text);
    let right_chunks = chunks(&right_text);
    for ((left, left_digit), (right, right_digit)) in left_chunks.iter().zip(&right_chunks) {
        let order = if *left_digit && *right_digit {
            let left_trimmed = left.trim_start_matches('0');
            let right_trimmed = right.trim_start_matches('0');
            left_trimmed
                .len()
                .cmp(&right_trimmed.len())
                .then_with(|| left_trimmed.cmp(right_trimmed))
                .then_with(|| left.len().cmp(&right.len()))
        } else {
            left.to_ascii_lowercase().cmp(&right.to_ascii_lowercase())
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    left_chunks
        .len()
        .cmp(&right_chunks.len())
        .then_with(|| left_text.cmp(&right_text))
}

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

/// Map items to DTOs with per-user watch state, and (for folders) how many
/// children each holds.
fn annotate_with_counts(
    items: Vec<Item>,
    watch: &HashMap<i64, WatchState>,
    counts: &HashMap<i64, i64>,
    media: &mut HashMap<i64, MediaFacts>,
) -> Vec<ItemDto> {
    items
        .into_iter()
        .map(|item| {
            let w = watch.get(&item.id).copied();
            let count = counts.get(&item.id).copied();
            let facts = media.remove(&item.id);
            let resolution = facts.as_ref().and_then(|f| f.height);
            ItemDto::from(item)
                .with_watch(w)
                .with_resolution(resolution)
                .with_media(facts)
                .with_child_count(count)
        })
        .collect()
}

#[derive(Deserialize)]
pub struct ListQuery {
    pub sort: Option<String>,
    pub offset: Option<i64>,
    pub limit: Option<i64>,
    /// `1` adds the aggregated `media` block to each playable item. Opt-in
    /// because it costs one more (still page-wide) query and a few hundred
    /// bytes a row: the clients that want spec columns ask for them, and
    /// everyone else keeps the response they already parse, byte for byte.
    pub facts: Option<u8>,
    /// Narrow the grid to one genre. Absent (the default) is the whole
    /// library, byte for byte what this endpoint returned before the
    /// parameter existed. Matched case-insensitively against the item's
    /// stored genres; an unknown genre is an empty page and a `total` of 0,
    /// not an error — the client asked a well-formed question and the answer
    /// is "nothing".
    pub genre: Option<String>,
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

    // Trimmed and emptied-to-None so `?genre=` and `?genre=%20` mean the same
    // thing as omitting it, rather than "the genre whose name is one space".
    let genre = q.genre.as_deref().map(str::trim).filter(|g| !g.is_empty());
    let page = state
        .store
        .list_top_items_in_genre(library_id, sort, offset, limit, genre)
        .await?;
    let watch = watch_lookup(&state, user.id, &page.items).await?;
    // Per-item resolution so the grid can badge/section it. Home videos carry
    // a badge for the same reason movies do — phone footage ranges from 480p
    // to 4K in the same folder.
    let badged: Vec<i64> = page
        .items
        .iter()
        .filter(|i| matches!(i.kind, ItemKind::Movie | ItemKind::Video))
        .map(|i| i.id)
        .collect();
    let heights = state.store.item_max_heights(&badged).await?;
    // Codec/HDR/audio/size for the same set of items, and only when asked.
    // One query for the page, never one per item: `badged` is already the
    // page's playable ids, so this is a second constant-cost lookup, not a
    // fan-out. Photos and folders are not in `badged` — a photo has no codec
    // to name and a folder has no files of its own.
    let file_backed: Vec<i64> = page
        .items
        .iter()
        .filter(|i| {
            matches!(
                i.kind,
                ItemKind::Movie | ItemKind::Video | ItemKind::Book | ItemKind::Audiobook
            )
        })
        .map(|i| i.id)
        .collect();
    let mut facts = if q.facts == Some(1) {
        state.store.item_media_facts(&file_backed).await?
    } else {
        HashMap::new()
    };
    // Folder cards say how much is inside them.
    let folder_ids: Vec<i64> = page
        .items
        .iter()
        .filter(|i| i.kind == ItemKind::Folder)
        .map(|i| i.id)
        .collect();
    let counts = state.store.child_counts(&folder_ids).await?;
    // Containers carry no watch row of their own, so a grid filtering by
    // "Watched"/"In progress" has nothing to filter a show on — the state
    // lives on its episodes, which aren't in this response. One batched
    // rollup query per page answers it for every container at once; the
    // per-card version would be an N+1 over a recursive walk. Gated by kind
    // exactly as `item_detail` gates its single rollup; leaves keep `watch`
    // only, which already tells the whole truth about them.
    let container_ids: Vec<i64> = page
        .items
        .iter()
        .filter(|i| matches!(i.kind, ItemKind::Show | ItemKind::Season | ItemKind::Folder))
        .map(|i| i.id)
        .collect();
    let rollups = state.store.watch_rollups(user.id, &container_ids).await?;
    let items = page
        .items
        .into_iter()
        .map(|item| {
            let rollup = rollups.get(&item.id).copied();
            let w = watch.get(&item.id).copied();
            let res = heights.get(&item.id).copied();
            let count = counts.get(&item.id).copied();
            let media = facts.remove(&item.id);
            ItemDto::from(item)
                .with_watch(w)
                .with_resolution(res)
                .with_media(media)
                .with_child_count(count)
                .with_rollup(rollup)
        })
        .collect();
    Ok(Json(ItemListResponse {
        items,
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
    /// Other text/audio editions sharing a proven work id. Empty when no
    /// explicit relation exists; title + author never populate this list.
    pub editions: Vec<ItemDto>,
    /// Current revision-bound locator for text books. `None` means unread or
    /// that the saved locator belongs to a replaced file revision.
    pub reading: Option<ReadingDto>,
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

    // Walk up the parent chain (episode → season → show; home libraries mirror
    // whatever folder depth is on disk, which is legitimately deep). The guard
    // is only an anti-cycle backstop.
    let mut ancestors = Vec::new();
    let mut cursor = item.parent_id;
    while let Some(parent_id) = cursor {
        match state.store.get_item(parent_id).await? {
            Some(parent) => {
                cursor = parent.parent_id;
                ancestors.push(parent);
                if ancestors.len() >= 16 {
                    break;
                }
            }
            None => break,
        }
    }
    ancestors.reverse();

    let children = state.store.get_item_children(id).await?;
    let child_counts = state
        .store
        .child_counts(
            &children
                .iter()
                .filter(|c| c.kind == ItemKind::Folder)
                .map(|c| c.id)
                .collect::<Vec<_>>(),
        )
        .await?;
    // A season page is a media listing just as surely as a library grid is.
    // Fetch one best-file summary for all episode children in a single query;
    // asking for every episode detail independently would turn a 24-episode
    // season into 24 extra requests and queries. Keep other item-detail child
    // responses unchanged: home folders, for example, did not previously
    // attach list-style media badges.
    let mut child_media = if item.kind == ItemKind::Season {
        let episode_ids: Vec<i64> = children
            .iter()
            .filter(|child| child.kind == ItemKind::Episode)
            .map(|child| child.id)
            .collect();
        state.store.item_media_facts(&episode_ids).await?
    } else {
        HashMap::new()
    };
    let mut files = match item.kind {
        // Home videos and photos have files exactly like movies do; folders
        // (and shows/seasons) have children instead.
        ItemKind::Movie
        | ItemKind::Episode
        | ItemKind::Book
        | ItemKind::Audiobook
        | ItemKind::Video
        | ItemKind::Photo => state.store.files_for_item(id).await?,
        _ => Vec::new(),
    };
    if item.kind == ItemKind::Audiobook {
        // Generic media versions sort best-quality first. Audiobook files are
        // sequential parts, not alternate versions, so their path order is
        // the playback order.
        files.sort_by(|left, right| natural_path_cmp(&left.path, &right.path));
    }

    // Check each file actually resolves on disk right now, so the client can
    // refuse to "play" a file that's missing (unmounted share, moved file,
    // wrong container mount) instead of opening a dead player. One stat per
    // file — cheap for the handful a movie/episode has. Admins also get the
    // full path back so they can see what to fix.
    //
    // Track defaults are independent of that availability check: they use the
    // stored stream rows plus one settings snapshot, never a playback decision
    // or a media probe. Missing/unmounted files can therefore still explain
    // which tracks they contain and which policy choice would apply.
    let playback_prefs = state.transcode.lang_prefs().await;
    let mut file_dtos: Vec<FileDto> = Vec::with_capacity(files.len());
    let mut part_offset_ms = 0_i64;
    for f in files {
        let path = f.path.clone();
        let available = tokio::fs::metadata(&path).await.is_ok();
        let raw_probe = state.store.get_file_probe_json(f.id).await?;
        let duration_ms = f.duration_ms.unwrap_or(0).max(0);
        let mut dto = FileDto::from_media_file(f, &playback_prefs);
        dto.available = available;
        dto.part_offset_ms = part_offset_ms;
        dto.chapters = chapters_from_probe_json(raw_probe.as_deref());
        if item.kind == ItemKind::Audiobook {
            part_offset_ms = part_offset_ms.saturating_add(duration_ms);
        }
        if !available && user.is_admin {
            dto.missing_path = Some(path.to_string_lossy().into_owned());
        }
        file_dtos.push(dto);
    }

    // Annotate the item and its children with watch state in one lookup.
    let mut all = children.clone();
    all.push(item.clone());
    let watch = watch_lookup(&state, user.id, &all).await?;

    // Containers have no watch row of their own, so the client would have no
    // way to know a series is finished — its seasons carry nothing, and their
    // episodes aren't in this response at all. One rollup query answers it.
    let rollup = match item.kind {
        ItemKind::Show | ItemKind::Season | ItemKind::Folder => {
            Some(state.store.watch_rollup(user.id, id).await?)
        }
        _ => None,
    };

    let reading = if item.kind == ItemKind::Book {
        state
            .store
            .current_reading_state(user.id, id)
            .await?
            .map(ReadingDto::try_from)
            .transpose()?
    } else {
        None
    };
    let editions = if matches!(item.kind, ItemKind::Book | ItemKind::Audiobook) {
        match item.book_work_id.as_deref() {
            Some(work_id) => state.store.related_book_editions(item.id, work_id).await?,
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };
    let item_dto = ItemDto::from(item)
        .with_watch(watch.get(&id).copied())
        .with_rollup(rollup);
    Ok(Json(ItemDetail {
        item: item_dto,
        ancestors: ancestors.into_iter().map(Into::into).collect(),
        children: annotate_with_counts(children, &watch, &child_counts, &mut child_media),
        files: file_dtos,
        editions: editions.into_iter().map(Into::into).collect(),
        reading,
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
    let mut continue_watching: Vec<ItemDto> =
        in_progress.into_iter().map(in_progress_dto).collect();

    // Next-up episodes (unwatched tracks per show); no per-item watch state.
    let next = state.store.next_up(user.id, 20).await?;
    let mut next_up: Vec<ItemDto> = next.into_iter().map(|r| recent_dto(r, None)).collect();

    let recent = state.store.recently_added(q.library_id, 20).await?;
    let recent_items: Vec<Item> = recent.iter().map(|r| r.item.clone()).collect();
    let watch = watch_lookup(&state, user.id, &recent_items).await?;
    // Folder cards say "12 items" here too, not just on the library grid.
    let folder_ids: Vec<i64> = recent_items
        .iter()
        .filter(|i| i.kind == ItemKind::Folder)
        .map(|i| i.id)
        .collect();
    let counts = state.store.child_counts(&folder_ids).await?;
    let mut recently_added: Vec<ItemDto> = recent
        .into_iter()
        .map(|r| {
            let w = watch.get(&r.item.id).copied();
            let count = counts.get(&r.item.id).copied();
            recent_dto(r, w).with_child_count(count)
        })
        .collect();

    // Resolution badges, same as the library grid gets. Home is where most
    // people actually browse, so a movie card that carries a 4K chip in the
    // library should carry it here too — one lookup covers all three rows.
    let badged: Vec<i64> = continue_watching
        .iter()
        .chain(next_up.iter())
        .chain(recently_added.iter())
        .filter(|d| matches!(d.kind, ItemKind::Movie | ItemKind::Video))
        .map(|d| d.id)
        .collect();
    if !badged.is_empty() {
        let heights = state.store.item_max_heights(&badged).await?;
        for d in continue_watching
            .iter_mut()
            .chain(next_up.iter_mut())
            .chain(recently_added.iter_mut())
        {
            if matches!(d.kind, ItemKind::Movie | ItemKind::Video) {
                d.resolution = heights.get(&d.id).copied();
            }
        }
    }

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
