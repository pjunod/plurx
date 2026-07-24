//! Metadata enrichment.
//!
//! Matches scanned items against a provider (TMDB in Phase 1; AniDB/AniList
//! join in Phase 2) and writes titles, overviews, IDs, and cached artwork
//! back through the store. Provider responses and artwork are cached locally
//! so a library keeps working offline once enriched (REQ-META-4).

pub mod anilist;
pub mod tmdb;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub use anilist::AniListClient;
pub use tmdb::TmdbClient;

use crate::domain::{ItemKind, MetadataPatch};
use crate::store::Store;

/// Poster width bucket — small enough to be snappy in a grid, sharp on TV.
const POSTER_SIZE: &str = "w500";
const BACKDROP_SIZE: &str = "w1280";
const STILL_SIZE: &str = "w300";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct EnrichReport {
    pub matched: usize,
    pub unmatched: usize,
    pub episodes_matched: usize,
    pub errors: usize,
}

/// Enrich every movie/show still lacking a TMDB id in the given library (or
/// all libraries when `None`). Artwork is written under `artwork_dir`; the
/// stored paths are relative filenames the API serves from that directory.
pub async fn enrich_library(
    store: &dyn Store,
    tmdb: &TmdbClient,
    artwork_dir: &Path,
    library_id: Option<i64>,
    force: bool,
) -> EnrichReport {
    let mut report = EnrichReport::default();
    if let Err(e) = tokio::fs::create_dir_all(artwork_dir).await {
        tracing::error!(dir = %artwork_dir.display(), error = %e, "cannot create artwork dir");
        report.errors += 1;
        return report;
    }

    let items = match store.items_needing_metadata(library_id, force).await {
        Ok(items) => items,
        Err(e) => {
            tracing::error!(error = %e, "listing items needing metadata");
            report.errors += 1;
            return report;
        }
    };

    for item in items {
        match item.kind {
            ItemKind::Movie => match tmdb.find_movie(&item.title, item.year).await {
                Ok(Some(m)) => {
                    let poster = cache_image(
                        tmdb,
                        artwork_dir,
                        item.id,
                        "poster",
                        m.poster_path.as_deref(),
                        POSTER_SIZE,
                    )
                    .await;
                    let backdrop = cache_image(
                        tmdb,
                        artwork_dir,
                        item.id,
                        "backdrop",
                        m.backdrop_path.as_deref(),
                        BACKDROP_SIZE,
                    )
                    .await;
                    let patch = MetadataPatch {
                        title: Some(m.title),
                        year: m.year,
                        overview: m.overview,
                        tmdb_id: Some(m.tmdb_id),
                        imdb_id: m.imdb_id,
                        air_date: m.air_date,
                        runtime_ms: m.runtime_ms,
                        poster_path: poster,
                        backdrop_path: backdrop,
                    };
                    if apply(store, item.id, patch, &mut report).await {
                        report.matched += 1;
                    }
                }
                Ok(None) => report.unmatched += 1,
                Err(e) => {
                    tracing::warn!(title = %item.title, error = %e, "movie lookup failed");
                    report.errors += 1;
                }
            },
            ItemKind::Show => match tmdb.find_show(&item.title, item.year).await {
                Ok(Some(m)) => {
                    let show_tmdb_id = m.tmdb_id;
                    let poster = cache_image(
                        tmdb,
                        artwork_dir,
                        item.id,
                        "poster",
                        m.poster_path.as_deref(),
                        POSTER_SIZE,
                    )
                    .await;
                    let backdrop = cache_image(
                        tmdb,
                        artwork_dir,
                        item.id,
                        "backdrop",
                        m.backdrop_path.as_deref(),
                        BACKDROP_SIZE,
                    )
                    .await;
                    let patch = MetadataPatch {
                        title: Some(m.title),
                        year: m.year,
                        overview: m.overview,
                        tmdb_id: Some(m.tmdb_id),
                        air_date: m.air_date,
                        poster_path: poster,
                        backdrop_path: backdrop,
                        ..Default::default()
                    };
                    if apply(store, item.id, patch, &mut report).await {
                        report.matched += 1;
                    }
                    enrich_episodes(store, tmdb, artwork_dir, item.id, show_tmdb_id, &mut report)
                        .await;
                }
                Ok(None) => report.unmatched += 1,
                Err(e) => {
                    tracing::warn!(title = %item.title, error = %e, "show lookup failed");
                    report.errors += 1;
                }
            },
            _ => {}
        }
    }

    tracing::info!(
        matched = report.matched,
        unmatched = report.unmatched,
        episodes = report.episodes_matched,
        errors = report.errors,
        "metadata enrichment complete"
    );
    report
}

/// Enrich anime shows in a library from AniList (REQ-META-3). Only show items
/// are matched (episodes keep their absolute numbering); artwork is downloaded
/// from AniList's absolute image URLs. All failures are non-fatal.
pub async fn enrich_anime_library(
    store: &dyn Store,
    client: &AniListClient,
    artwork_dir: &Path,
    library_id: i64,
    force: bool,
) -> EnrichReport {
    let mut report = EnrichReport::default();
    if let Err(e) = tokio::fs::create_dir_all(artwork_dir).await {
        tracing::error!(dir = %artwork_dir.display(), error = %e, "cannot create artwork dir");
        report.errors += 1;
        return report;
    }
    let items = match store.items_needing_metadata(Some(library_id), force).await {
        Ok(items) => items,
        Err(e) => {
            tracing::error!(error = %e, "listing anime needing metadata");
            report.errors += 1;
            return report;
        }
    };

    for item in items {
        if item.kind != ItemKind::Show {
            continue;
        }
        match client.find_anime(&item.title).await {
            Ok(Some(m)) => {
                let poster = download_url(
                    client,
                    artwork_dir,
                    item.id,
                    "poster",
                    m.cover_url.as_deref(),
                )
                .await;
                let backdrop = download_url(
                    client,
                    artwork_dir,
                    item.id,
                    "backdrop",
                    m.banner_url.as_deref(),
                )
                .await;
                let patch = MetadataPatch {
                    title: Some(m.title),
                    year: m.year,
                    overview: m.overview,
                    poster_path: poster,
                    backdrop_path: backdrop,
                    ..Default::default()
                };
                if apply(store, item.id, patch, &mut report).await {
                    report.matched += 1;
                }
            }
            Ok(None) => report.unmatched += 1,
            Err(e) => {
                tracing::warn!(title = %item.title, error = %e, "anilist lookup failed");
                report.errors += 1;
            }
        }
    }
    tracing::info!(
        matched = report.matched,
        unmatched = report.unmatched,
        errors = report.errors,
        "anime enrichment complete"
    );
    report
}

/// Download an image from an absolute URL (AniList) into the artwork cache.
async fn download_url(
    client: &AniListClient,
    artwork_dir: &Path,
    item_id: i64,
    kind: &str,
    url: Option<&str>,
) -> Option<String> {
    let url = url?;
    let bytes = match client.download_image(url).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(item_id, kind, error = %e, "anime artwork download failed");
            return None;
        }
    };
    let filename = format!("{item_id}-{kind}.jpg");
    let dest = artwork_dir.join(&filename);
    if let Err(e) = tokio::fs::write(&dest, &bytes).await {
        tracing::warn!(path = %dest.display(), error = %e, "writing anime artwork");
        return None;
    }
    Some(filename)
}

/// Fetch each season once and patch this show's episodes by episode number.
async fn enrich_episodes(
    store: &dyn Store,
    tmdb: &TmdbClient,
    artwork_dir: &Path,
    show_id: i64,
    show_tmdb_id: i64,
    report: &mut EnrichReport,
) {
    let episodes = match store.episodes_for_show(show_id).await {
        Ok(eps) => eps,
        Err(e) => {
            tracing::warn!(error = %e, "listing episodes");
            report.errors += 1;
            return;
        }
    };
    // Group local episodes by season so each season is fetched exactly once.
    let mut by_season: BTreeMap<i32, Vec<crate::domain::Item>> = BTreeMap::new();
    for ep in episodes {
        by_season
            .entry(ep.season_number.unwrap_or(0))
            .or_default()
            .push(ep);
    }
    // The season items themselves get the season's own poster + overview —
    // a seasons grid of blank cards is what this prevents.
    let season_items: std::collections::HashMap<i32, crate::domain::Item> = store
        .get_item_children(show_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.kind == ItemKind::Season)
        .filter_map(|s| s.season_number.map(|n| (n, s)))
        .collect();

    for (season_number, locals) in by_season {
        let remote = match tmdb.season_detail(show_tmdb_id, season_number).await {
            Ok(detail) => detail,
            Err(e) => {
                tracing::warn!(season = season_number, error = %e, "season fetch failed");
                report.errors += 1;
                continue;
            }
        };
        if let Some(season_item) = season_items.get(&season_number) {
            // Skip the artwork download when the season is already enriched.
            let needs_art = season_item.poster_path.is_none() && remote.poster_path.is_some();
            let poster = if needs_art {
                cache_image(
                    tmdb,
                    artwork_dir,
                    season_item.id,
                    "poster",
                    remote.poster_path.as_deref(),
                    POSTER_SIZE,
                )
                .await
            } else {
                None
            };
            let patch = MetadataPatch {
                overview: remote
                    .overview
                    .clone()
                    .filter(|_| season_item.overview.is_none()),
                air_date: remote.air_date.clone(),
                poster_path: poster,
                ..Default::default()
            };
            if !patch.is_empty() {
                apply(store, season_item.id, patch, report).await;
            }
        }
        for ep in locals {
            let Some(meta) = remote
                .episodes
                .iter()
                .find(|r| Some(r.episode_number) == ep.episode_number)
            else {
                continue;
            };
            let still = cache_image(
                tmdb,
                artwork_dir,
                ep.id,
                "poster",
                meta.still_path.as_deref(),
                STILL_SIZE,
            )
            .await;
            let patch = MetadataPatch {
                title: meta.title.clone(),
                overview: meta.overview.clone(),
                air_date: meta.air_date.clone(),
                runtime_ms: meta.runtime_ms,
                poster_path: still,
                ..Default::default()
            };
            if apply(store, ep.id, patch, report).await {
                report.episodes_matched += 1;
            }
        }
    }
}

async fn apply(
    store: &dyn Store,
    item_id: i64,
    patch: MetadataPatch,
    report: &mut EnrichReport,
) -> bool {
    match store.apply_metadata(item_id, &patch).await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(item_id, error = %e, "applying metadata");
            report.errors += 1;
            false
        }
    }
}

/// Download and cache one image; returns the relative filename to store, or
/// `None` if there was no source path or the download failed (non-fatal).
async fn cache_image(
    tmdb: &TmdbClient,
    artwork_dir: &Path,
    item_id: i64,
    kind: &str,
    tmdb_path: Option<&str>,
    size: &str,
) -> Option<String> {
    let tmdb_path = tmdb_path?;
    let bytes = match tmdb.download_image(tmdb_path, size).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(item_id, kind, error = %e, "artwork download failed");
            return None;
        }
    };
    let filename = format!("{item_id}-{kind}.jpg");
    let dest: PathBuf = artwork_dir.join(&filename);
    if let Err(e) = tokio::fs::write(&dest, &bytes).await {
        tracing::warn!(path = %dest.display(), error = %e, "writing artwork");
        return None;
    }
    Some(filename)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ItemKind, LibraryKind, NewItem, NewLibrary};
    use crate::store::{LibraryStore, MediaStore, SqliteStore};
    use serde_json::json;

    async fn serve(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    /// A TMDB mock covering the movie + show + season calls, with any image
    /// path answered by a few bytes so the artwork cache exercises its writes.
    fn tmdb_mock() -> axum::Router {
        use axum::routing::get;
        use axum::Json;
        axum::Router::new()
            .route(
                "/search/movie",
                get(|| async {
                    Json(json!({ "results": [
                        { "id": 603, "title": "The Matrix", "release_date": "1999-03-30",
                          "overview": "Truth.", "poster_path": "/mp.jpg", "backdrop_path": "/mb.jpg" }
                    ]}))
                }),
            )
            .route(
                "/movie/603",
                get(|| async { Json(json!({ "runtime": 136, "imdb_id": "tt0133093" })) }),
            )
            .route(
                "/search/tv",
                get(|| async {
                    Json(json!({ "results": [
                        { "id": 42, "name": "Severance", "first_air_date": "2022-02-18",
                          "poster_path": "/sp.jpg" }
                    ]}))
                }),
            )
            .route(
                "/tv/42/season/1",
                get(|| async {
                    Json(json!({
                        "poster_path": "/season.jpg", "overview": "S1", "air_date": "2022-02-18",
                        "episodes": [
                            { "episode_number": 1, "name": "Good News", "runtime": 57,
                              "still_path": "/e1.jpg" }
                        ]
                    }))
                }),
            )
            .fallback(get(|| async { vec![0u8, 1, 2, 3] }))
    }

    #[tokio::test]
    async fn enrich_library_matches_movie_show_and_episode() {
        let store = SqliteStore::open_in_memory().expect("open");
        let lib = store
            .create_library(&NewLibrary {
                name: "L".into(),
                kind: LibraryKind::Movies,
                paths: vec![],
                anime: false,
            })
            .await
            .expect("lib");
        let movie = store
            .insert_item(&NewItem {
                library_id: lib.id,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "matrix".into(),
                year: Some(1999),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");
        let show = store
            .insert_item(&NewItem {
                library_id: lib.id,
                kind: ItemKind::Show,
                parent_id: None,
                title: "severance".into(),
                year: None,
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("show");
        let season = store
            .insert_item(&NewItem {
                library_id: lib.id,
                kind: ItemKind::Season,
                parent_id: Some(show),
                title: "S1".into(),
                year: None,
                season_number: Some(1),
                episode_number: None,
            })
            .await
            .expect("season");
        store
            .insert_item(&NewItem {
                library_id: lib.id,
                kind: ItemKind::Episode,
                parent_id: Some(season),
                title: "e1".into(),
                year: None,
                season_number: Some(1),
                episode_number: Some(1),
            })
            .await
            .expect("ep");

        let base = serve(tmdb_mock()).await;
        let tmdb = TmdbClient::new("k").with_base(&base, &base);
        let art = tempfile::tempdir().expect("tmp");

        let report = enrich_library(&store, &tmdb, art.path(), Some(lib.id), false).await;
        assert_eq!(report.matched, 2, "movie + show");
        assert_eq!(report.episodes_matched, 1);
        assert_eq!(report.errors, 0);

        // The store actually took the patch.
        let m = store.get_item(movie).await.expect("get").expect("item");
        assert_eq!(m.tmdb_id, Some(603));
        assert_eq!(m.title, "The Matrix");
        assert!(m.poster_path.is_some());
        let s = store.get_item(show).await.expect("get").expect("item");
        assert_eq!(s.tmdb_id, Some(42));

        // A second run has nothing left needing metadata.
        let again = enrich_library(&store, &tmdb, art.path(), Some(lib.id), false).await;
        assert_eq!(again.matched, 0);
    }

    #[tokio::test]
    async fn enrich_anime_library_matches_shows() {
        use axum::routing::post;
        use axum::Json;
        let store = SqliteStore::open_in_memory().expect("open");
        let lib = store
            .create_library(&NewLibrary {
                name: "Anime".into(),
                kind: LibraryKind::Shows,
                paths: vec![],
                anime: true,
            })
            .await
            .expect("lib");
        let show = store
            .insert_item(&NewItem {
                library_id: lib.id,
                kind: ItemKind::Show,
                parent_id: None,
                title: "frieren".into(),
                year: None,
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("show");

        // No cover/banner → artwork download is skipped, match still lands.
        let app = axum::Router::new().route(
            "/",
            post(|| async {
                Json(json!({ "data": { "Media": {
                    "id": 154587, "title": { "english": "Frieren" }, "seasonYear": 2023
                }}}))
            }),
        );
        let base = serve(app).await;
        let client = AniListClient::new().with_base(&base);
        let art = tempfile::tempdir().expect("tmp");

        let report = enrich_anime_library(&store, &client, art.path(), lib.id, false).await;
        assert_eq!(report.matched, 1);
        let s = store.get_item(show).await.expect("get").expect("item");
        assert_eq!(s.title, "Frieren");
        assert_eq!(s.year, Some(2023));
    }
}
