//! Metadata enrichment.
//!
//! Matches scanned items against a provider (TMDB in Phase 1; AniDB/AniList
//! join in Phase 2) and writes titles, overviews, IDs, and cached artwork
//! back through the store. Provider responses and artwork are cached locally
//! so a library keeps working offline once enriched (REQ-META-4).

pub mod anilist;
pub mod book;
pub mod genres;
pub mod local;
pub mod tmdb;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use anilist::AniListClient;
pub use tmdb::TmdbClient;

use crate::domain::{ArtworkAttempt, ItemKind, MetadataPatch};
use crate::store::{ArtworkRepairFence, Store};

/// Poster width bucket — small enough to be snappy in a grid, sharp on TV.
const POSTER_SIZE: &str = "w500";
// Backdrops and episode stills become full-width television heroes. The old
// w1280/w300 cache buckets were visibly upscaled there, especially the w300
// episode art. These files are downloaded once and then served locally, so
// keeping TMDB's source resolution is the right tradeoff for large screens.
const BACKDROP_SIZE: &str = "original";
const STILL_SIZE: &str = "original";

/// Cap on the problem lines one pass records, mirroring `ScanReport`'s: past
/// it only a trailing summary is added. A refresh of a library whose provider
/// is down would otherwise put one line per title into an HTTP response.
const MAX_PROBLEMS: usize = 40;

/// Removes an unpublished artwork file even when the writing future is
/// cancelled. The synchronous unlink is intentional: `Drop` cannot await,
/// and leaving a partial file is worse than a tiny best-effort filesystem
/// call during cancellation.
struct UnpublishedArtwork(Option<PathBuf>);

impl UnpublishedArtwork {
    fn new(path: PathBuf) -> Self {
        Self(Some(path))
    }

    fn path(&self) -> &Path {
        self.0.as_deref().expect("unpublished artwork path")
    }

    fn published(&mut self) {
        self.0 = None;
    }
}

impl Drop for UnpublishedArtwork {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[derive(Default)]
struct ArtworkPublicationState {
    cancelled: bool,
    finished: bool,
}

/// Marks an in-flight blocking publication cancelled. The blocking worker
/// owns the temporary-file guard and checks this state while holding the same
/// lock used for rename, so cancellation and publication have a defined
/// order: either cancellation wins and the temp is removed, or the complete
/// rename wins before cancellation returns.
struct CancelArtworkPublication(Arc<std::sync::Mutex<ArtworkPublicationState>>);

impl Drop for CancelArtworkPublication {
    fn drop(&mut self) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if !state.finished {
            state.cancelled = true;
        }
    }
}

static ARTWORK_PUBLICATIONS: std::sync::OnceLock<std::sync::Mutex<HashSet<PathBuf>>> =
    std::sync::OnceLock::new();

struct ArtworkPublicationSlot(PathBuf);

impl ArtworkPublicationSlot {
    fn claim(target: &Path) -> std::io::Result<Self> {
        let target = target.to_path_buf();
        let mut active = ARTWORK_PUBLICATIONS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !active.insert(target.clone()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "artwork publication is already in flight",
            ));
        }
        Ok(Self(target))
    }
}

impl Drop for ArtworkPublicationSlot {
    fn drop(&mut self) {
        ARTWORK_PUBLICATIONS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.0);
    }
}

/// Exclusive process-local right to publish one final artwork path. It can be
/// held across the replicated metadata write so an older materializer cannot
/// win the path between catalogue publication and byte publication.
#[doc(hidden)]
pub struct ArtworkPublicationReservation {
    target: PathBuf,
    _slot: ArtworkPublicationSlot,
}

#[doc(hidden)]
pub fn reserve_artwork_publication(
    target: PathBuf,
) -> std::io::Result<ArtworkPublicationReservation> {
    let slot = ArtworkPublicationSlot::claim(&target)?;
    Ok(ArtworkPublicationReservation {
        target,
        _slot: slot,
    })
}

async fn publish_artwork_with_reservation<F>(
    reservation: ArtworkPublicationReservation,
    writer: F,
) -> std::io::Result<()>
where
    F: FnOnce(&Path) -> std::io::Result<()> + Send + 'static,
{
    let state = Arc::new(std::sync::Mutex::new(ArtworkPublicationState::default()));
    let cancellation = CancelArtworkPublication(Arc::clone(&state));
    let result = tokio::task::spawn_blocking(move || {
        let ArtworkPublicationReservation {
            target,
            _slot: slot,
        } = reservation;
        let _slot = slot;
        let parent = target.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "artwork path has no parent directory",
            )
        })?;
        let filename = target.file_name().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "artwork path has no filename",
            )
        })?;
        let temporary = parent.join(format!(
            ".{}.{}.tmp",
            filename.to_string_lossy(),
            uuid::Uuid::new_v4().simple()
        ));
        let mut unpublished = UnpublishedArtwork::new(temporary);
        writer(unpublished.path())?;

        let mut publication = state.lock().unwrap_or_else(|error| error.into_inner());
        if publication.cancelled {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "artwork publication was cancelled",
            ));
        }
        std::fs::rename(unpublished.path(), &target)?;
        unpublished.published();
        publication.finished = true;
        Ok(())
    })
    .await;
    drop(cancellation);
    match result {
        Ok(result) => result,
        Err(error) => Err(std::io::Error::other(format!(
            "artwork publication worker failed: {error}"
        ))),
    }
}

async fn publish_artwork_with<F>(target: PathBuf, writer: F) -> std::io::Result<()>
where
    F: FnOnce(&Path) -> std::io::Result<()> + Send + 'static,
{
    let reservation = reserve_artwork_publication(target)?;
    publish_artwork_with_reservation(reservation, writer).await
}

/// Publish artwork through a same-directory temporary file so readers see
/// either the old complete image or the new complete image, never a partial
/// provider response. This is shared by TMDB and Books artwork.
#[doc(hidden)]
pub async fn write_artwork_atomically(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let bytes = bytes.to_vec();
    publish_artwork_with(target.to_path_buf(), move |temporary| {
        std::fs::write(temporary, bytes)
    })
    .await
}

#[doc(hidden)]
pub async fn write_artwork_atomically_reserved(
    reservation: ArtworkPublicationReservation,
    bytes: &[u8],
) -> std::io::Result<()> {
    let bytes = bytes.to_vec();
    publish_artwork_with_reservation(reservation, move |temporary| {
        std::fs::write(temporary, bytes)
    })
    .await
}

/// Remove an unreferenced final file while retaining its publication slot in
/// the blocking worker. If the sweep is cancelled, a later publisher still
/// cannot race an already-dispatched unlink.
#[doc(hidden)]
pub async fn remove_artwork_reserved(
    reservation: ArtworkPublicationReservation,
) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || {
        let ArtworkPublicationReservation {
            target,
            _slot: slot,
        } = reservation;
        let _slot = slot;
        std::fs::remove_file(target)
    })
    .await
    .map_err(|error| std::io::Error::other(format!("artwork removal worker failed: {error}")))?
}

/// Copy an existing local image through the same cancellation-safe atomic
/// publication path used for provider bytes.
pub(crate) async fn copy_artwork_atomically(source: &Path, target: &Path) -> std::io::Result<()> {
    let source = source.to_path_buf();
    publish_artwork_with(target.to_path_buf(), move |temporary| {
        std::fs::copy(source, temporary).map(|_| ())
    })
    .await
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct EnrichReport {
    pub matched: usize,
    pub unmatched: usize,
    pub episodes_matched: usize,
    pub errors: usize,
    /// Human-readable problems, under the same contract
    /// `ScanReport::problems` carries: an `errors` count with nothing to click
    /// on is a dead end for whoever has to fix the library.
    ///
    /// It also carries failures that belong to no single title — chiefly
    /// "TMDB's genre vocabulary could not be fetched", which leaves every show
    /// in the pass without genres while every other field lands perfectly.
    /// Absent a line here that is invisible: the counts all read as success
    /// and the genre facets are simply, silently, empty.
    pub problems: Vec<String>,
}

impl EnrichReport {
    /// Record a problem, capped. Use this for anything that also bumps
    /// `errors` — never push directly, or the cap stops holding.
    fn note(&mut self, problem: String) {
        match self.problems.len().cmp(&MAX_PROBLEMS) {
            std::cmp::Ordering::Less => self.problems.push(problem),
            std::cmp::Ordering::Equal => self.problems.push(
                "…and more not listed — see the server log (Settings → System) \
                 for every one"
                    .to_owned(),
            ),
            std::cmp::Ordering::Greater => {}
        }
    }
}

/// A provider's genres as a patch field: `None` when it named none.
///
/// `Some(vec![])` is a *clear* to `apply_metadata`, and a provider that simply
/// has no genres for a title must not wipe a list an earlier, better-informed
/// pass stored. Same add-or-replace rule every other patch field follows.
fn genre_patch(genres: Vec<String>) -> Option<Vec<String>> {
    (!genres.is_empty()).then_some(genres)
}

/// Enrich every movie/show still lacking a TMDB id in the given library (or
/// all libraries when `None`). Artwork is written under `artwork_dir`; the
/// stored paths are relative filenames the API serves from that directory.
///
/// `only` restricts the pass to specific item ids — a targeted scan enriching
/// what it just placed, or the retry sweep re-fetching one poster. `None` is
/// the whole library, exactly as before.
pub async fn enrich_library(
    store: &dyn Store,
    tmdb: &TmdbClient,
    artwork_dir: &Path,
    library_id: Option<i64>,
    force: bool,
    only: Option<&[i64]>,
) -> EnrichReport {
    enrich_library_for_targets(store, tmdb, artwork_dir, library_id, force, only, only).await
}

/// Enrich a targeted TMDB tree while keeping provider-routing ancestors
/// separate from the rows the caller actually asked to repair.
///
/// `routes` is handed to the store so a season or episode can enter TMDB
/// through its show. `repairs` is the original requested set. A show present
/// only in `routes` contributes its stable TMDB id and is otherwise left
/// untouched: routing one blank episode must not rewrite a healthy show's
/// title or download its poster and backdrop again.
pub async fn enrich_library_for_targets(
    store: &dyn Store,
    tmdb: &TmdbClient,
    artwork_dir: &Path,
    library_id: Option<i64>,
    force: bool,
    routes: Option<&[i64]>,
    repairs: Option<&[i64]>,
) -> EnrichReport {
    enrich_library_for_targets_with_fence(
        store,
        tmdb,
        artwork_dir,
        library_id,
        force,
        routes,
        repairs,
        None,
    )
    .await
}

/// Fenced form used only by leader-arbitrated provider artwork repair.
pub async fn enrich_library_for_targets_with_fence(
    store: &dyn Store,
    tmdb: &TmdbClient,
    artwork_dir: &Path,
    library_id: Option<i64>,
    force: bool,
    routes: Option<&[i64]>,
    repairs: Option<&[i64]>,
    repair_fence: Option<&ArtworkRepairFence>,
) -> EnrichReport {
    let mut report = EnrichReport::default();
    if let Err(e) = tokio::fs::create_dir_all(artwork_dir).await {
        tracing::error!(dir = %artwork_dir.display(), error = %e, "cannot create artwork dir");
        report.errors += 1;
        report.note(format!("cannot create `{}`: {e}", artwork_dir.display()));
        return report;
    }

    let items = match store
        .items_needing_metadata(library_id, force, routes)
        .await
    {
        Ok(items) => items,
        Err(e) => {
            tracing::error!(error = %e, "listing items needing metadata");
            report.errors += 1;
            report.note(format!("cannot list items needing metadata: {e}"));
            return report;
        }
    };

    for item in items {
        let route_only = repairs.is_some_and(|ids| !ids.contains(&item.id));
        if route_only {
            if item.kind == ItemKind::Show {
                // An enriched parent already has the provider identity needed
                // to reach its seasons. Avoid even the show-details request:
                // the ancestor is a route, not a refresh target.
                let show_tmdb_id = if let Some(id) = item.tmdb_id {
                    Some(id)
                } else {
                    let known = known_id(tmdb, &item).await;
                    match show_lookup(tmdb, &item, known).await {
                        Ok(Some(m)) => {
                            // The ancestor is route-only, but the identity we
                            // just resolved is durable routing state. Keeping
                            // it avoids paying for (and risking) the same title
                            // search on every child retry without refreshing
                            // any of the healthy show metadata.
                            let tmdb_id = m.tmdb_id;
                            if repair_fence.is_none() {
                                apply(
                                    store,
                                    item.id,
                                    MetadataPatch {
                                        tmdb_id: Some(tmdb_id),
                                        ..Default::default()
                                    },
                                    &mut report,
                                    None,
                                )
                                .await;
                            }
                            Some(tmdb_id)
                        }
                        Ok(None) => {
                            report.unmatched += 1;
                            None
                        }
                        Err(e) => {
                            tracing::error!(title = %item.title, error = %e, "show route lookup failed");
                            report.errors += 1;
                            report.note(format!("`{}`: show route lookup failed: {e}", item.title));
                            None
                        }
                    }
                };
                if let Some(show_tmdb_id) = show_tmdb_id {
                    enrich_episodes(
                        store,
                        tmdb,
                        artwork_dir,
                        item.id,
                        show_tmdb_id,
                        repairs,
                        &mut report,
                        repair_fence,
                    )
                    .await;
                }
            }
            continue;
        }

        // An id, if one is already known, before any search. This is the
        // whole point: `"Heat (1995) Directors Cut Remux"` is a title a
        // search can get wrong, and a wrong match does not stay local — it
        // propagates into Trakt sync, which matches on TMDB id. When the
        // caller told us the id, guessing is strictly worse than obeying.
        let known = known_id(tmdb, &item).await;
        match item.kind {
            ItemKind::Movie => match movie_lookup(tmdb, &item, known).await {
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
                        // `enriched: true` is still right — the provider DID
                        // answer, and the title and ids it gave are worth
                        // keeping. What used to be wrong was that this was the
                        // only thing recorded: a poster that failed to download
                        // left no trace at all, so the item read as finished.
                        // `artwork` is that trace.
                        artwork: poster.attempt.clone(),
                        poster_path: poster.file,
                        backdrop_path: backdrop.file,
                        // Free: the details call this lookup already made
                        // carries them. No branch, no second request.
                        genres: genre_patch(m.genres),
                        enriched: true,
                        ..Default::default()
                    };
                    if apply(store, item.id, patch, &mut report, repair_fence).await {
                        report.matched += 1;
                    }
                }
                Ok(None) => report.unmatched += 1,
                Err(e) => {
                    tracing::error!(title = %item.title, error = %e, "movie lookup failed");
                    report.errors += 1;
                    report.note(format!("`{}`: movie lookup failed: {e}", item.title));
                }
            },
            ItemKind::Show => match show_lookup(tmdb, &item, known).await {
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
                        artwork: poster.attempt.clone(),
                        poster_path: poster.file,
                        backdrop_path: backdrop.file,
                        // A show matched by id got these from its details
                        // body; one matched by search got them from the
                        // once-per-run genre vocabulary. Either way this is
                        // the only place either lands.
                        genres: genre_patch(m.genres),
                        enriched: true,
                        ..Default::default()
                    };
                    if apply(store, item.id, patch, &mut report, repair_fence).await {
                        report.matched += 1;
                    }
                    enrich_episodes(
                        store,
                        tmdb,
                        artwork_dir,
                        item.id,
                        show_tmdb_id,
                        repairs,
                        &mut report,
                        repair_fence,
                    )
                    .await;
                }
                Ok(None) => report.unmatched += 1,
                Err(e) => {
                    tracing::error!(title = %item.title, error = %e, "show lookup failed");
                    report.errors += 1;
                    report.note(format!("`{}`: show lookup failed: {e}", item.title));
                }
            },
            _ => {}
        }
    }

    // Once, at the end, not once per show. The vocabulary is fetched a single
    // time, so it fails a single time, and every title matched by search
    // afterwards is affected identically — a line per title would be forty
    // copies of one fact. Counted as an error too: the pass really did fail
    // to record something it was asked to record, and a green report with
    // empty genre facets is exactly the invisible failure this forbids.
    if tmdb.genre_index_failed() {
        report.errors += 1;
        report.note(
            "TMDB's genre list could not be fetched, so shows matched by title \
             in this pass were stored without genres — re-run the refresh once \
             TMDB is reachable"
                .to_owned(),
        );
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

/// The TMDB id already attached to this item, directly or via its IMDb id.
///
/// `None` means nobody has told us what this is, so a title search is the
/// only thing left to try. An IMDb lookup that *fails* also lands here — a
/// dead network is not a reason to leave the item unenriched forever, and
/// the search path is exactly the fallback it should get.
async fn known_id(tmdb: &TmdbClient, item: &crate::domain::Item) -> Option<i64> {
    if let Some(id) = item.tmdb_id {
        return Some(id);
    }
    let imdb = item.imdb_id.as_deref()?;
    match tmdb.id_for_imdb(imdb, item.kind == ItemKind::Show).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(imdb, error = %e, "imdb → tmdb lookup failed; falling back to search");
            None
        }
    }
}

/// Resolve a movie: by id when one is known, by title otherwise.
///
/// A by-id fetch never returns `Ok(None)` — an id either resolves or errors,
/// and "TMDB does not have id 999999" is an error worth seeing rather than a
/// silent unmatched.
async fn movie_lookup(
    tmdb: &TmdbClient,
    item: &crate::domain::Item,
    known: Option<i64>,
) -> Result<Option<tmdb::Match>, crate::error::MetadataError> {
    match known {
        Some(id) => tmdb.movie_by_id(id).await.map(Some),
        None => tmdb.find_movie(&item.title, item.year).await,
    }
}

/// Resolve a show, as [`movie_lookup`] does for movies.
async fn show_lookup(
    tmdb: &TmdbClient,
    item: &crate::domain::Item,
    known: Option<i64>,
) -> Result<Option<tmdb::Match>, crate::error::MetadataError> {
    match known {
        Some(id) => tmdb.show_by_id(id).await.map(Some),
        None => tmdb.find_show(&item.title, item.year).await,
    }
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
    only: Option<&[i64]>,
) -> EnrichReport {
    enrich_anime_library_with_fence(store, client, artwork_dir, library_id, force, only, None).await
}

/// Fenced form used only by leader-arbitrated provider artwork repair.
pub async fn enrich_anime_library_with_fence(
    store: &dyn Store,
    client: &AniListClient,
    artwork_dir: &Path,
    library_id: i64,
    force: bool,
    only: Option<&[i64]>,
    repair_fence: Option<&ArtworkRepairFence>,
) -> EnrichReport {
    let mut report = EnrichReport::default();
    if let Err(e) = tokio::fs::create_dir_all(artwork_dir).await {
        tracing::error!(dir = %artwork_dir.display(), error = %e, "cannot create artwork dir");
        report.errors += 1;
        report.note(format!("cannot create `{}`: {e}", artwork_dir.display()));
        return report;
    }
    let items = match store
        .items_needing_metadata(Some(library_id), force, only)
        .await
    {
        Ok(items) => items,
        Err(e) => {
            tracing::error!(error = %e, "listing anime needing metadata");
            report.errors += 1;
            report.note(format!("cannot list anime needing metadata: {e}"));
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
                    artwork: poster.attempt.clone(),
                    poster_path: poster.file,
                    backdrop_path: backdrop.file,
                    // AniList's own vocabulary, and free — `genres` is one
                    // more field on the search query that already runs.
                    genres: genre_patch(m.genres),
                    // AniList never yields a TMDB id, so before `metadata_at`
                    // existed an anime show was re-matched on every single
                    // scan — the id-is-the-marker rule had no id to read.
                    enriched: true,
                    ..Default::default()
                };
                if apply(store, item.id, patch, &mut report, repair_fence).await {
                    report.matched += 1;
                }
            }
            Ok(None) => report.unmatched += 1,
            Err(e) => {
                tracing::error!(title = %item.title, error = %e, "anilist lookup failed");
                report.errors += 1;
                report.note(format!("`{}`: anilist lookup failed: {e}", item.title));
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
) -> Artwork {
    let Some(url) = url else {
        return Artwork::unavailable();
    };
    let bytes = match client.download_image(url).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(item_id, kind, error = %e, "anime artwork download failed");
            return Artwork::failed(format!("download: {e}"));
        }
    };
    write_artwork(artwork_dir, item_id, kind, &bytes).await
}

/// One artwork slot after an attempt: the file to store, and the attempt to
/// record. They are separate because the interesting case has a file of
/// `None` and an attempt of `Failed` — which is precisely the state that used
/// to be unrepresentable, and so went unrepaired.
#[derive(Debug, Clone, Default)]
struct Artwork {
    file: Option<String>,
    attempt: Option<ArtworkAttempt>,
}

impl Artwork {
    /// We chose not to fetch (the item already has this image). Records
    /// nothing: the attempt columns describe fetches, and a deliberate skip
    /// would otherwise overwrite the record of the last real one.
    fn skipped() -> Self {
        Artwork::default()
    }

    /// The provider answered and named no image at all.
    ///
    /// Recorded as a failure, not as silence, and the distinction earns its
    /// keep in the retry sweep: an item with no poster and no attempt stamp
    /// is eligible on *every* cycle, so a film TMDB simply has no art for
    /// would be re-fetched forever. Stamping it puts it on the same daily
    /// backoff as everything else.
    fn unavailable() -> Self {
        Artwork::failed("the provider has no image for this".to_owned())
    }

    fn stored(filename: String) -> Self {
        Artwork {
            file: Some(filename),
            attempt: Some(ArtworkAttempt::Stored),
        }
    }

    fn failed(why: String) -> Self {
        Artwork {
            file: None,
            attempt: Some(ArtworkAttempt::Failed(why)),
        }
    }
}

/// Write downloaded bytes into the artwork cache under the conventional name.
async fn write_artwork(artwork_dir: &Path, item_id: i64, kind: &str, bytes: &[u8]) -> Artwork {
    let filename = format!("{item_id}-{kind}.jpg");
    let dest: PathBuf = artwork_dir.join(&filename);
    if let Err(e) = write_artwork_atomically(&dest, bytes).await {
        tracing::warn!(path = %dest.display(), error = %e, "writing artwork");
        // A full or read-only artwork directory is as much a reason to come
        // back as a failed download, and from the item's side it is the same
        // failure: the provider had an image and we do not.
        return Artwork::failed(format!("write: {e}"));
    }
    Artwork::stored(filename)
}

/// Fetch each season once and patch this show's episodes by episode number.
async fn enrich_episodes(
    store: &dyn Store,
    tmdb: &TmdbClient,
    artwork_dir: &Path,
    show_id: i64,
    show_tmdb_id: i64,
    only: Option<&[i64]>,
    report: &mut EnrichReport,
    repair_fence: Option<&ArtworkRepairFence>,
) {
    let episodes = match store.episodes_for_show(show_id).await {
        Ok(eps) => eps,
        Err(e) => {
            tracing::error!(error = %e, "listing episodes");
            report.errors += 1;
            report.note(format!("cannot list episodes of item {show_id}: {e}"));
            return;
        }
    };
    // The season items themselves get the season's own poster + overview --
    // a seasons grid of blank cards is what this prevents. Build this map
    // before filtering episodes because a retry may explicitly target a
    // season whose episode stills are already healthy.
    let season_items: std::collections::HashMap<i32, crate::domain::Item> = store
        .get_item_children(show_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.kind == ItemKind::Season)
        .filter_map(|s| s.season_number.map(|n| (n, s)))
        .collect();
    // Group local episodes by season so each season is fetched exactly once.
    let mut by_season: BTreeMap<i32, Vec<crate::domain::Item>> = BTreeMap::new();
    for ep in episodes {
        // A targeted import walks through its already-enriched show to reach
        // new children. Keep that forced ancestor from turning one episode
        // notification into a re-download of every still in every season.
        if only.is_some_and(|ids| !ids.contains(&ep.id)) {
            continue;
        }
        // Unknown is not season zero. Routing a malformed episode to TMDB's
        // Specials endpoint can overwrite it with an unrelated special that
        // happens to share the episode number.
        let Some(season_number) = ep.season_number else {
            tracing::warn!(
                item_id = ep.id,
                "episode has no season number; skipping TMDB artwork"
            );
            continue;
        };
        by_season.entry(season_number).or_default().push(ep);
    }
    // A season can be the missing card even when every episode still exists.
    // Keep an empty bucket for an explicitly requested season so its TMDB
    // detail (and poster) is fetched without needlessly re-downloading a child
    // still just to reach the endpoint.
    if let Some(ids) = only {
        for (season_number, season) in &season_items {
            if ids.contains(&season.id) {
                by_season.entry(*season_number).or_default();
            }
        }
    }

    for (season_number, locals) in by_season {
        let remote = match tmdb.season_detail(show_tmdb_id, season_number).await {
            Ok(detail) => detail,
            Err(e) => {
                tracing::error!(season = season_number, error = %e, "season fetch failed");
                report.errors += 1;
                report.note(format!("season {season_number} fetch failed: {e}"));
                continue;
            }
        };
        if let Some(season_item) = season_items
            .get(&season_number)
            .filter(|season| repair_fence.is_none_or(|fence| fence.item_id == season.id))
        {
            // Skip the artwork download when the season is already enriched.
            // When TMDB offers no poster, `cache_image(None)` deliberately
            // stamps that outcome so this season observes the daily backoff
            // instead of being selected by every half-hour sweep forever.
            let needs_art = season_item.poster_path.is_none();
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
                Artwork::skipped()
            };
            let patch = MetadataPatch {
                overview: remote
                    .overview
                    .clone()
                    .filter(|_| season_item.overview.is_none()),
                air_date: remote.air_date.clone(),
                artwork: poster.attempt.clone(),
                poster_path: poster.file,
                ..Default::default()
            };
            if !patch.is_empty() {
                apply(store, season_item.id, patch, report, repair_fence).await;
            }
        }
        for ep in locals {
            let Some(meta) = remote
                .episodes
                .iter()
                .find(|r| Some(r.episode_number) == ep.episode_number)
            else {
                // The season request succeeded, so this is a real provider
                // outcome rather than a network/key/store failure. Record it
                // here, where that distinction is known, so the retry sweep
                // can back it off without fabricating an artwork attempt for
                // paths that never reached TMDB.
                if ep.poster_path.is_none() {
                    apply(
                        store,
                        ep.id,
                        MetadataPatch {
                            artwork: Some(ArtworkAttempt::Failed(
                                "the provider has no matching episode".to_owned(),
                            )),
                            ..Default::default()
                        },
                        report,
                        repair_fence,
                    )
                    .await;
                }
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
                artwork: still.attempt.clone(),
                poster_path: still.file,
                ..Default::default()
            };
            if apply(store, ep.id, patch, report, repair_fence).await {
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
    repair_fence: Option<&ArtworkRepairFence>,
) -> bool {
    if repair_fence.is_some_and(|fence| fence.item_id != item_id) {
        tracing::error!(
            item_id,
            fence_item_id = repair_fence.map(|fence| fence.item_id),
            "refusing metadata write under another item's artwork fence"
        );
        report.errors += 1;
        report.note(format!(
            "cannot store metadata for item {item_id}: artwork fence names another item"
        ));
        return false;
    }
    let result = if let Some(fence) = repair_fence {
        store
            .apply_metadata_if_artwork_repair_current(item_id, &patch, fence)
            .await
    } else {
        store.apply_metadata(item_id, &patch).await.map(|()| true)
    };
    match result {
        Ok(applied) => applied,
        Err(e) => {
            tracing::error!(item_id, error = %e, "applying metadata");
            report.errors += 1;
            report.note(format!("cannot store metadata for item {item_id}: {e}"));
            false
        }
    }
}

/// Download and cache one image, reporting both the file and the attempt.
/// Failures stay non-fatal — they are now merely recorded rather than lost.
async fn cache_image(
    tmdb: &TmdbClient,
    artwork_dir: &Path,
    item_id: i64,
    kind: &str,
    tmdb_path: Option<&str>,
    size: &str,
) -> Artwork {
    let Some(tmdb_path) = tmdb_path else {
        return Artwork::unavailable();
    };
    let bytes = match tmdb.download_image(tmdb_path, size).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(item_id, kind, error = %e, "artwork download failed");
            return Artwork::failed(format!("download: {e}"));
        }
    };
    write_artwork(artwork_dir, item_id, kind, &bytes).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ItemKind, LibraryKind, MetadataPatch, NewItem, NewLibrary};
    use crate::store::{LibraryStore, MediaStore, SqliteStore};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn television_hero_art_keeps_source_resolution() {
        assert_eq!(BACKDROP_SIZE, "original");
        assert_eq!(STILL_SIZE, "original");
    }

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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_during_blocking_publication_leaves_no_output() {
        let artwork = tempfile::tempdir().expect("artwork");
        let target = artwork.path().join("42-poster.jpg");
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let release = Arc::new(std::sync::Barrier::new(2));
        let task_release = Arc::clone(&release);
        let task_target = target.clone();
        let task = tokio::spawn(async move {
            publish_artwork_with(task_target, move |temporary| {
                std::fs::write(temporary, b"partial")?;
                started_tx.send(()).expect("signal start");
                task_release.wait();
                Ok(())
            })
            .await
        });

        tokio::task::spawn_blocking(move || started_rx.recv())
            .await
            .expect("start waiter")
            .expect("publication started");
        task.abort();
        let _ = task.await;
        let competing = write_artwork_atomically(&target, b"newer").await;
        let competing_kind = competing.as_ref().err().map(std::io::Error::kind);
        release.wait();
        for _ in 0..100 {
            if std::fs::read_dir(artwork.path())
                .expect("read artwork")
                .next()
                .is_none()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert_eq!(competing_kind, Some(std::io::ErrorKind::WouldBlock));

        assert!(!target.exists(), "a partial image must never be published");
        let entries = std::fs::read_dir(artwork.path())
            .expect("read artwork")
            .collect::<Result<Vec<_>, _>>()
            .expect("entries");
        assert!(entries.is_empty(), "temporary output must be reaped");
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

        let report = enrich_library(&store, &tmdb, art.path(), Some(lib.id), false, None).await;
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
        let again = enrich_library(&store, &tmdb, art.path(), Some(lib.id), false, None).await;
        assert_eq!(again.matched, 0);
    }

    /// A healthy show can be only the route to one blank season. If that show
    /// was enriched from local data and has no provider id yet, the one title
    /// search needed to route this pass must persist its result without
    /// refreshing the show's artwork or descriptive metadata.
    #[tokio::test]
    async fn route_only_show_persists_a_resolved_tmdb_id() {
        let store = SqliteStore::open_in_memory().expect("open");
        let lib = store
            .create_library(&NewLibrary {
                name: "TV".into(),
                kind: LibraryKind::Shows,
                paths: vec![],
                anime: false,
            })
            .await
            .expect("lib");
        let show = store
            .insert_item(&NewItem {
                library_id: lib.id,
                kind: ItemKind::Show,
                parent_id: None,
                title: "Severance".into(),
                year: Some(2022),
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
                title: "Season 1".into(),
                year: None,
                season_number: Some(1),
                episode_number: None,
            })
            .await
            .expect("season");
        store
            .apply_metadata(
                show,
                &MetadataPatch {
                    poster_path: Some("local-show.jpg".into()),
                    backdrop_path: Some("local-backdrop.jpg".into()),
                    overview: Some("Keep this copy".into()),
                    enriched: true,
                    artwork: Some(ArtworkAttempt::Stored),
                    ..Default::default()
                },
            )
            .await
            .expect("seed show");

        let base = serve(tmdb_mock()).await;
        let tmdb = TmdbClient::new("k").with_base(&base, &base);
        let art = tempfile::tempdir().expect("tmp");
        let report = enrich_library_for_targets(
            &store,
            &tmdb,
            art.path(),
            Some(lib.id),
            true,
            Some(&[show, season]),
            Some(&[season]),
        )
        .await;
        assert_eq!(report.errors, 0);
        let show = store.get_item(show).await.expect("get").expect("show");
        assert_eq!(show.tmdb_id, Some(42));
        assert_eq!(show.poster_path.as_deref(), Some("local-show.jpg"));
        assert_eq!(show.backdrop_path.as_deref(), Some("local-backdrop.jpg"));
        assert_eq!(show.overview.as_deref(), Some("Keep this copy"));
    }

    /// A movie library with one movie in it, titled however you like.
    async fn one_movie(title: &str) -> (SqliteStore, i64, i64) {
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
                title: title.into(),
                year: None,
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");
        (store, lib.id, movie)
    }

    /// TMDB with `/movie/949` answering as Heat, and both search endpoints
    /// counting their callers so a test can assert they were never used.
    fn counting_mock(searches: Arc<AtomicUsize>) -> axum::Router {
        use axum::routing::get;
        use axum::Json;
        let movie_hits = searches.clone();
        let tv_hits = searches.clone();
        axum::Router::new()
            .route(
                "/movie/949",
                get(|| async {
                    Json(json!({
                        "id": 949, "title": "Heat", "release_date": "1995-12-15",
                        "overview": "A crew of professionals.", "imdb_id": "tt0113277",
                        "runtime": 170, "poster_path": "/heat.jpg"
                    }))
                }),
            )
            .route(
                "/find/tt0113277",
                get(|| async { Json(json!({ "movie_results": [{ "id": 949 }] })) }),
            )
            .route(
                "/search/movie",
                get(move || {
                    let hits = movie_hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(json!({ "results": [] }))
                    }
                }),
            )
            .route(
                "/search/tv",
                get(move || {
                    let hits = tv_hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(json!({ "results": [] }))
                    }
                }),
            )
            .fallback(get(|| async { vec![0u8, 1, 2, 3] }))
    }

    /// TMDB with genres on both sides of the split: a movie's details body
    /// carries them outright, a TV *search* result carries only ids. Every
    /// endpoint counts its callers, because the cost is the whole point of
    /// the design and an assertion is the only thing that keeps it true.
    #[allow(clippy::type_complexity)]
    fn genre_counting_mock() -> (axum::Router, Vec<(&'static str, Arc<AtomicUsize>)>) {
        use axum::routing::get;
        use axum::Json;
        let counters: Vec<(&'static str, Arc<AtomicUsize>)> = [
            "search/movie",
            "movie/details",
            "search/tv",
            "genre/tv/list",
            "genre/movie/list",
        ]
        .into_iter()
        .map(|k| (k, Arc::new(AtomicUsize::new(0))))
        .collect();
        let c = |name: &str| -> Arc<AtomicUsize> {
            counters
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.clone())
                .expect("counter")
        };
        let (search_movie, movie_details) = (c("search/movie"), c("movie/details"));
        let (search_tv, tv_list, movie_list) =
            (c("search/tv"), c("genre/tv/list"), c("genre/movie/list"));
        let router = axum::Router::new()
            .route(
                "/search/movie",
                get(move || {
                    let hits = search_movie.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        // Search results name genres by id only — for movies
                        // this is never read, because the details call below
                        // has the names and is made anyway.
                        Json(json!({ "results": [
                            { "id": 603, "title": "The Matrix", "release_date": "1999-03-30",
                              "genre_ids": [28, 878], "poster_path": "/mp.jpg" }
                        ]}))
                    }
                }),
            )
            .route(
                "/movie/603",
                get(move || {
                    let hits = movie_details.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(json!({
                            "runtime": 136, "imdb_id": "tt0133093",
                            "genres": [{ "id": 28, "name": "Action" },
                                       { "id": 878, "name": "Science Fiction" }]
                        }))
                    }
                }),
            )
            .route(
                "/search/tv",
                get(move || {
                    let hits = search_tv.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(json!({ "results": [
                            { "id": 42, "name": "Severance", "first_air_date": "2022-02-18",
                              "genre_ids": [18, 9648], "poster_path": "/sp.jpg" }
                        ]}))
                    }
                }),
            )
            .route(
                "/genre/tv/list",
                get(move || {
                    let hits = tv_list.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(json!({ "genres": [{ "id": 18, "name": "Drama" },
                                                { "id": 9648, "name": "Mystery" }]}))
                    }
                }),
            )
            .route(
                "/genre/movie/list",
                get(move || {
                    let hits = movie_list.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(json!({ "genres": [] }))
                    }
                }),
            )
            .fallback(get(|| async { vec![0u8, 1, 2, 3] }));
        (router, counters)
    }

    /// S3's cost contract, asserted rather than assumed: a movie's genres are
    /// free and a library of shows costs ONE extra request between all of
    /// them.
    ///
    /// The failure this forbids is the obvious implementation — resolving a
    /// show's `genre_ids` with a `/tv/{id}` details call — which is correct,
    /// invisible in any single test, and doubles the request count of every
    /// TV refresh forever.
    #[tokio::test]
    async fn genres_cost_a_movie_nothing_and_a_whole_run_of_shows_one_call() {
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
        let mut movies = Vec::new();
        let mut shows = Vec::new();
        for n in 0..2 {
            movies.push(
                store
                    .insert_item(&NewItem {
                        library_id: lib.id,
                        kind: ItemKind::Movie,
                        parent_id: None,
                        title: format!("matrix {n}"),
                        year: Some(1999),
                        season_number: None,
                        episode_number: None,
                    })
                    .await
                    .expect("movie"),
            );
            shows.push(
                store
                    .insert_item(&NewItem {
                        library_id: lib.id,
                        kind: ItemKind::Show,
                        parent_id: None,
                        title: format!("severance {n}"),
                        year: None,
                        season_number: None,
                        episode_number: None,
                    })
                    .await
                    .expect("show"),
            );
        }

        let (mock, counters) = genre_counting_mock();
        let count = |name: &str| -> usize {
            counters
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.load(Ordering::SeqCst))
                .expect("counter")
        };
        let base = serve(mock).await;
        let tmdb = TmdbClient::new("k").with_base(&base, &base);
        let art = tempfile::tempdir().expect("tmp");

        let report = enrich_library(&store, &tmdb, art.path(), Some(lib.id), false, None).await;
        assert_eq!(report.matched, 4, "two movies and two shows");
        assert_eq!(report.errors, 0, "problems: {:?}", report.problems);

        for id in &movies {
            let m = store.get_item(*id).await.expect("get").expect("movie");
            assert_eq!(
                m.genres,
                vec!["Action".to_owned(), "Science Fiction".to_owned()],
                "a movie's genres come out of the details call it already makes"
            );
        }
        for id in &shows {
            let s = store.get_item(*id).await.expect("get").expect("show");
            assert_eq!(
                s.genres,
                vec!["Drama".to_owned(), "Mystery".to_owned()],
                "a show's genre ids resolve against the cached vocabulary"
            );
        }

        // The cost accounting. Two movies: one search + one details each, and
        // not one request more than before genres existed.
        assert_eq!(count("search/movie"), 2);
        assert_eq!(count("movie/details"), 2);
        assert_eq!(
            count("genre/movie/list"),
            0,
            "a movie must never trigger a vocabulary fetch — its details \
             response already names its genres"
        );
        // Two shows: one search each, and ONE list call shared between them.
        assert_eq!(count("search/tv"), 2);
        assert_eq!(
            count("genre/tv/list"),
            1,
            "the TV vocabulary is fetched once per run, not once per show"
        );
    }

    /// A vocabulary fetch that fails must not be silent, must not be retried
    /// per title, and must not fail the titles themselves. All three, because
    /// each was a plausible way to build it.
    #[tokio::test]
    async fn a_missing_genre_vocabulary_is_reported_once_and_costs_one_request() {
        use axum::routing::get;
        use axum::Json;
        let store = SqliteStore::open_in_memory().expect("open");
        let lib = store
            .create_library(&NewLibrary {
                name: "L".into(),
                kind: LibraryKind::Shows,
                paths: vec![],
                anime: false,
            })
            .await
            .expect("lib");
        for n in 0..3 {
            store
                .insert_item(&NewItem {
                    library_id: lib.id,
                    kind: ItemKind::Show,
                    parent_id: None,
                    title: format!("severance {n}"),
                    year: None,
                    season_number: None,
                    episode_number: None,
                })
                .await
                .expect("show");
        }
        let attempts = Arc::new(AtomicUsize::new(0));
        let list_hits = attempts.clone();
        let app = axum::Router::new()
            .route(
                "/search/tv",
                get(|| async {
                    Json(json!({ "results": [
                        { "id": 42, "name": "Severance", "first_air_date": "2022-02-18",
                          "genre_ids": [18], "poster_path": "/sp.jpg" }
                    ]}))
                }),
            )
            .route(
                "/genre/tv/list",
                get(move || {
                    let hits = list_hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        axum::http::StatusCode::NOT_FOUND
                    }
                }),
            )
            .fallback(get(|| async { vec![0u8, 1, 2, 3] }));
        let base = serve(app).await;
        let tmdb = TmdbClient::new("k").with_base(&base, &base);
        let art = tempfile::tempdir().expect("tmp");

        let report = enrich_library(&store, &tmdb, art.path(), Some(lib.id), false, None).await;
        assert_eq!(
            report.matched, 3,
            "every show still matched and got a poster"
        );
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "a failed vocabulary fetch is cached as empty — three shows must \
             not mean three attempts"
        );
        assert_eq!(
            report.errors, 1,
            "the pass really did fail to record something it was asked to"
        );
        assert_eq!(
            report.problems.len(),
            1,
            "one line for one failure, not one per show: {:?}",
            report.problems
        );
        assert!(
            report.problems[0].contains("genre list"),
            "the problem must say what went wrong: {:?}",
            report.problems
        );
    }

    /// The P4 acceptance case: a title a search would plausibly get wrong,
    /// plus the id the caller already knew. The id must win outright.
    #[tokio::test]
    async fn a_known_tmdb_id_is_used_directly_and_the_search_is_never_called() {
        let (store, lib, movie) = one_movie("Heat (1995) Directors Cut Remux").await;
        // Exactly what a monarr scan request leaves behind: an id, nothing else.
        store
            .apply_metadata(
                movie,
                &MetadataPatch {
                    tmdb_id: Some(949),
                    ..Default::default()
                },
            )
            .await
            .expect("ids");

        let searches = Arc::new(AtomicUsize::new(0));
        let base = serve(counting_mock(searches.clone())).await;
        let tmdb = TmdbClient::new("k").with_base(&base, &base);
        let art = tempfile::tempdir().expect("tmp");

        let report = enrich_library(&store, &tmdb, art.path(), Some(lib), false, None).await;
        assert_eq!(report.matched, 1, "the id-carrying item was enriched");
        assert_eq!(report.errors, 0);
        assert_eq!(
            searches.load(Ordering::SeqCst),
            0,
            "an item that names its own id must never be searched for"
        );

        let m = store.get_item(movie).await.expect("get").expect("item");
        assert_eq!(
            m.title, "Heat",
            "the id's canonical title, not the filename"
        );
        assert_eq!(m.year, Some(1995));
        assert_eq!(m.imdb_id.as_deref(), Some("tt0113277"));
        assert!(m.poster_path.is_some());

        // And now it is done: a second pass has nothing to do.
        let again = enrich_library(&store, &tmdb, art.path(), Some(lib), false, None).await;
        assert_eq!(again.matched, 0);
        assert_eq!(searches.load(Ordering::SeqCst), 0);
    }

    /// IMDb-only is the other half of the same case — monarr's movie side
    /// tracks IMDb ids, and one lookup beats a title search.
    #[tokio::test]
    async fn an_imdb_id_is_resolved_to_a_tmdb_id_rather_than_searched() {
        let (store, lib, movie) = one_movie("Heat.1995.Directors.Cut.2160p").await;
        store
            .apply_metadata(
                movie,
                &MetadataPatch {
                    imdb_id: Some("tt0113277".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("ids");

        let searches = Arc::new(AtomicUsize::new(0));
        let base = serve(counting_mock(searches.clone())).await;
        let tmdb = TmdbClient::new("k").with_base(&base, &base);
        let art = tempfile::tempdir().expect("tmp");

        let report = enrich_library(&store, &tmdb, art.path(), Some(lib), false, None).await;
        assert_eq!(report.matched, 1);
        assert_eq!(searches.load(Ordering::SeqCst), 0);
        let m = store.get_item(movie).await.expect("get").expect("item");
        assert_eq!(m.title, "Heat");
        assert_eq!(m.tmdb_id, Some(949));
    }

    /// A forced refresh re-reads the stored id. It must not fall back to
    /// matching on the title again — `force` means "fetch it again", not
    /// "start over and possibly land somewhere else".
    #[tokio::test]
    async fn a_forced_refresh_refreshes_by_id_not_by_title() {
        let (store, lib, movie) = one_movie("Heat (1995) Directors Cut Remux").await;
        store
            .apply_metadata(
                movie,
                &MetadataPatch {
                    tmdb_id: Some(949),
                    enriched: true,
                    ..Default::default()
                },
            )
            .await
            .expect("ids");

        let searches = Arc::new(AtomicUsize::new(0));
        let base = serve(counting_mock(searches.clone())).await;
        let tmdb = TmdbClient::new("k").with_base(&base, &base);
        let art = tempfile::tempdir().expect("tmp");

        // Not due normally — already enriched.
        assert_eq!(
            enrich_library(&store, &tmdb, art.path(), Some(lib), false, None)
                .await
                .matched,
            0
        );
        // Forced: fetched again, still by id.
        let report = enrich_library(&store, &tmdb, art.path(), Some(lib), true, None).await;
        assert_eq!(report.matched, 1);
        assert_eq!(searches.load(Ordering::SeqCst), 0);
    }

    /// No id anywhere is still the ordinary case: search, as before.
    #[tokio::test]
    async fn an_item_with_no_id_at_all_still_falls_back_to_searching() {
        let (store, lib, movie) = one_movie("matrix").await;
        let searches = Arc::new(AtomicUsize::new(0));
        // The counting mock answers searches with nothing, so this also
        // pins down that an unmatched item is not counted as an error.
        let base = serve(counting_mock(searches.clone())).await;
        let tmdb = TmdbClient::new("k").with_base(&base, &base);
        let art = tempfile::tempdir().expect("tmp");

        let report = enrich_library(&store, &tmdb, art.path(), Some(lib), false, None).await;
        assert_eq!(searches.load(Ordering::SeqCst), 1, "the search ran");
        assert_eq!(report.unmatched, 1);
        assert_eq!(report.errors, 0);
        let m = store.get_item(movie).await.expect("get").expect("item");
        assert_eq!(m.title, "matrix", "left alone when nothing matched");
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

        let report = enrich_anime_library(&store, &client, art.path(), lib.id, false, None).await;
        assert_eq!(report.matched, 1);
        let s = store.get_item(show).await.expect("get").expect("item");
        assert_eq!(s.title, "Frieren");
        assert_eq!(s.year, Some(2023));
    }

    /// TMDB answers the search and then refuses every image, the way a rate
    /// limit does halfway through a scan.
    fn image_refusing_mock(status: axum::http::StatusCode) -> axum::Router {
        use axum::routing::get;
        use axum::Json;
        axum::Router::new()
            .route(
                "/search/movie",
                get(|| async {
                    Json(json!({ "results": [
                        { "id": 603, "title": "The Matrix", "release_date": "1999-03-30",
                          "overview": "Truth.", "poster_path": "/mp.jpg" }
                    ]}))
                }),
            )
            .route(
                "/movie/603",
                get(|| async { Json(json!({ "runtime": 136 })) }),
            )
            .fallback(get(move || async move { status }))
    }

    /// The defect, stated as a test: an item whose poster download failed must
    /// still be *findable*. It used to be indistinguishable from an item TMDB
    /// has no art for — both were a null `poster_path` and a stamped
    /// `metadata_at` — so one transient failure meant a blank card forever.
    #[tokio::test]
    async fn a_failed_poster_download_leaves_the_item_eligible_for_retry() {
        let (store, lib, movie) = one_movie("matrix").await;
        // 404 on the image CDN: not retried (that is `download_image`'s job to
        // decide), so this is one clean failed attempt.
        let base = serve(image_refusing_mock(axum::http::StatusCode::NOT_FOUND)).await;
        let tmdb = TmdbClient::new("k").with_base(&base, &base);
        let art = tempfile::tempdir().expect("tmp");

        let report = enrich_library(&store, &tmdb, art.path(), Some(lib), false, None).await;
        assert_eq!(report.matched, 1, "the metadata itself still landed");

        let m = store.get_item(movie).await.expect("get").expect("item");
        assert_eq!(m.title, "The Matrix");
        assert!(m.poster_path.is_none(), "the image never arrived");
        assert!(
            m.artwork_attempted_at.is_some(),
            "the attempt has to be on the record — without it, 'no poster' \
             cannot be told from 'never tried', which is the whole bug"
        );
        assert!(m
            .artwork_error
            .as_deref()
            .is_some_and(|e| e.contains("404")));

        // And the sweep will therefore come back for it.
        let due = store
            .items_missing_artwork(Some(lib), 0, 100)
            .await
            .expect("sweep");
        assert_eq!(due.iter().map(|i| i.id).collect::<Vec<_>>(), [movie]);
        // But not immediately: the recorded attempt is what makes the backoff
        // possible, and a permanent 404 must not be re-fetched every cycle.
        assert!(store
            .items_missing_artwork(Some(lib), 3600, 100)
            .await
            .expect("backoff")
            .is_empty());
    }

    /// A provider that answers with no image at all is recorded too. Left
    /// unrecorded it would have no attempt stamp, hence no backoff, hence a
    /// re-fetch on every single sweep — a self-healing job turned into a
    /// rate-limit generator.
    #[tokio::test]
    async fn an_item_the_provider_has_no_art_for_is_also_stamped() {
        use axum::routing::get;
        use axum::Json;
        let (store, lib, movie) = one_movie("matrix").await;
        let app = axum::Router::new()
            .route(
                "/search/movie",
                get(|| async {
                    Json(json!({ "results": [
                        { "id": 603, "title": "The Matrix", "release_date": "1999-03-30" }
                    ]}))
                }),
            )
            .route(
                "/movie/603",
                get(|| async { Json(json!({ "runtime": 136 })) }),
            );
        let base = serve(app).await;
        let tmdb = TmdbClient::new("k").with_base(&base, &base);
        let art = tempfile::tempdir().expect("tmp");

        enrich_library(&store, &tmdb, art.path(), Some(lib), false, None).await;
        let m = store.get_item(movie).await.expect("get").expect("item");
        assert!(m.poster_path.is_none());
        assert!(m.artwork_attempted_at.is_some());
        assert!(store
            .items_missing_artwork(Some(lib), 3600, 100)
            .await
            .expect("backoff")
            .is_empty());
    }

    /// `only` is what keeps a targeted scan bounded. The library's other
    /// items must be left exactly as they were.
    #[tokio::test]
    async fn enrich_library_touches_only_the_ids_it_was_given() {
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
        let mut ids = Vec::new();
        for title in ["matrix", "matrix"] {
            ids.push(
                store
                    .insert_item(&NewItem {
                        library_id: lib.id,
                        kind: ItemKind::Movie,
                        parent_id: None,
                        title: title.into(),
                        year: Some(1999),
                        season_number: None,
                        episode_number: None,
                    })
                    .await
                    .expect("movie"),
            );
        }

        let base = serve(tmdb_mock()).await;
        let tmdb = TmdbClient::new("k").with_base(&base, &base);
        let art = tempfile::tempdir().expect("tmp");

        let report = enrich_library(
            &store,
            &tmdb,
            art.path(),
            Some(lib.id),
            false,
            Some(&ids[..1]),
        )
        .await;
        assert_eq!(report.matched, 1);
        let touched = store.get_item(ids[0]).await.expect("get").expect("item");
        let untouched = store.get_item(ids[1]).await.expect("get").expect("item");
        assert!(touched.poster_path.is_some());
        assert!(untouched.poster_path.is_none());
        assert_eq!(untouched.title, "matrix", "left for the next full pass");
    }
}
