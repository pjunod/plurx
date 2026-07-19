//! Metadata enrichment.
//!
//! Matches scanned items against a provider (TMDB in Phase 1; AniDB/AniList
//! join in Phase 2) and writes titles, overviews, IDs, and cached artwork
//! back through the store. Provider responses and artwork are cached locally
//! so a library keeps working offline once enriched (REQ-META-4).

pub mod tmdb;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
) -> EnrichReport {
    let mut report = EnrichReport::default();
    if let Err(e) = tokio::fs::create_dir_all(artwork_dir).await {
        tracing::error!(dir = %artwork_dir.display(), error = %e, "cannot create artwork dir");
        report.errors += 1;
        return report;
    }

    let items = match store.items_needing_metadata(library_id).await {
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

    for (season_number, locals) in by_season {
        let remote = match tmdb.season_episodes(show_tmdb_id, season_number).await {
            Ok(list) => list,
            Err(e) => {
                tracing::warn!(season = season_number, error = %e, "season fetch failed");
                report.errors += 1;
                continue;
            }
        };
        for ep in locals {
            let Some(meta) = remote
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
