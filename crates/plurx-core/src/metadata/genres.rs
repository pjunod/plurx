//! The one-off genre backfill.
//!
//! Genres arrived in migration v13, and nothing in the database can produce
//! them for a library enriched before that. `tmdb::Match` carries nine fields
//! and none of them is a genre, so unlike `backfill_hdr_format` — which
//! recomputes an HDR label from `probe_json` that was already on disk — this
//! one has nothing to recompute *from*. It must ask the provider again, once
//! per title.
//!
//! That single fact shapes everything here:
//!
//!   * **Opt-in.** An upgrade must never start it. v9's comment records the
//!     failure being avoided: every library in the world re-fetching itself
//!     the first time it starts on a new version, one HTTP call per item,
//!     into a rate limit.
//!   * **Resumable, not restartable.** Progress is stamped in settings after
//!     every title. A reboot, a crash or a 429 costs the titles still ahead
//!     of the cursor and not one behind it.
//!   * **Paced.** A backfill is the only workload here that is deliberately
//!     hundreds of requests in a row with nobody waiting on it, which makes
//!     it both the most likely thing to trip a rate limit and the thing with
//!     the least excuse for doing so.
//!   * **Loud.** A title that fails keeps its empty genres and gets a line in
//!     the report. A backfill that half-completes in silence is the failure
//!     mode this is built against: the counts would say "done" and a third of
//!     the catalogue would have no genre facets with nothing to say why.
//!
//! Items already enriched are invisible to `items_needing_metadata` (they are
//! `metadata_at`-stamped), which is why this cannot simply reuse the ordinary
//! enrichment queue and why [`Store::items_missing_genres`] exists.

use std::collections::HashMap;
use std::time::Duration;

use super::{genre_patch, AniListClient, TmdbClient, MAX_PROBLEMS};
use crate::domain::{Item, ItemKind, MetadataPatch};
use crate::error::MetadataError;
use crate::store::{keys, PublicationStore, Store};

/// Titles one pass will process before returning.
///
/// Bounded so a pass ends and the caller decides whether to start another —
/// the alternative is a single task holding the provider open for hours with
/// no natural point at which anything can look at it, stop it, or notice it
/// has stopped. At [`PACE`] this is under a minute of work, which fits inside
/// the scheduler's own tick.
pub const BATCH: usize = 200;

/// Gap between titles.
///
/// Four requests a second, an order of magnitude under TMDB's public ceiling.
/// Not tuned for speed on purpose: this is background repair competing with
/// real scans and real playback for the same key, and the cost of being slow
/// is that a catalogue finishes overnight instead of over lunch. The cost of
/// being fast is a 429 that stops everything else too.
pub const PACE: Duration = Duration::from_millis(250);

/// What one backfill pass did.
///
/// The three counts are disjoint and together account for every title the
/// pass touched: `backfilled` got genres, `skipped` was asked and had none to
/// give (or had no provider to ask), `failed` errored and still has none.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct GenreBackfillReport {
    /// Titles that now have genres they did not have before.
    pub backfilled: usize,
    /// Titles whose provider call errored. Their genres are still empty and
    /// every one of them is named in `problems`.
    pub failed: usize,
    /// Titles the provider answered for with no genres at all, plus any whose
    /// library has no configured provider. Not an error, and not silent
    /// either — they are the difference between `backfilled + failed` and the
    /// number of titles that remain empty afterwards.
    pub skipped: usize,
    /// Did this pass reach the end of the catalogue? `false` means there is
    /// more to do and another pass will pick it up from `cursor`.
    pub complete: bool,
    /// The cursor this pass started from and the one it left behind. Reported
    /// rather than merely stored so "it resumed" is something an operator can
    /// see rather than infer.
    pub resumed_from: i64,
    pub cursor: i64,
    /// Per-title failures and anything else worth an operator's attention,
    /// capped like every other report's.
    pub problems: Vec<String>,
}

impl GenreBackfillReport {
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

/// Is the backfill armed? Anything other than exactly `"1"` — including the
/// `"done"` the pass writes itself, and the absent default — is off.
pub async fn is_armed(store: &dyn Store) -> bool {
    matches!(
        store.get_setting(keys::GENRE_BACKFILL).await,
        Ok(Some(v)) if v.trim() == "1"
    )
}

/// Run one pass of the genre backfill, if it is armed.
///
/// Returns `None` when it is not — the caller can then say nothing rather
/// than reporting a pass of zero, which would be indistinguishable from a
/// pass that found no work.
///
/// `tmdb` is `None` when no API key is configured; anime libraries still run,
/// since AniList needs no key.
pub async fn backfill_pass(
    store: &dyn Store,
    tmdb: Option<&TmdbClient>,
    anilist: &AniListClient,
    pace: Duration,
) -> Option<GenreBackfillReport> {
    backfill_pass_with_publication(&PublicationStore::unfenced(store), tmdb, anilist, pace).await
}

pub async fn backfill_pass_with_publication(
    store: &PublicationStore<'_>,
    tmdb: Option<&TmdbClient>,
    anilist: &AniListClient,
    pace: Duration,
) -> Option<GenreBackfillReport> {
    if !is_armed(store.raw()).await {
        return None;
    }
    let mut report = GenreBackfillReport {
        resumed_from: read_cursor(store.raw()).await,
        ..Default::default()
    };
    report.cursor = report.resumed_from;

    let items = match store
        .items_missing_genres(report.cursor, BATCH as i64)
        .await
    {
        Ok(items) => items,
        Err(e) => {
            tracing::error!(error = %e, "genre backfill: cannot list items");
            report.note(format!("cannot list items missing genres: {e}"));
            return Some(report);
        }
    };

    if items.is_empty() {
        // Nothing left ahead of the cursor: the catalogue is done. Rewind the
        // cursor FIRST and disarm second — a crash between the two leaves the
        // job armed at zero, which costs one extra pass over whatever is
        // still empty and then finishes again. The other order would leave it
        // disarmed at the end of the catalogue, so re-arming it later would
        // complete instantly having done nothing, which looks like success.
        stamp_cursor(store, 0, &mut report).await;
        if let Err(e) = store.put_setting(keys::GENRE_BACKFILL, "done").await {
            tracing::error!(error = %e, "genre backfill: cannot record completion");
            report.note(format!("cannot record backfill completion: {e}"));
        }
        report.complete = true;
        report.cursor = 0;
        tracing::info!("genre backfill complete; disarmed");
        return Some(report);
    }

    // One library lookup per library, not per title.
    let mut libraries: HashMap<i64, Option<crate::domain::Library>> = HashMap::new();
    let mut first = true;
    for item in items {
        // Paced *between* titles, so the first one is immediate and a pass
        // that finds a single item does not sit on a sleep for no reason.
        if !first {
            tokio::time::sleep(pace).await;
        }
        first = false;

        let library = match libraries.entry(item.library_id) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(store.get_library(item.library_id).await.unwrap_or(None))
            }
        };
        let Some(library) = library.as_ref() else {
            // The library went away mid-pass. Not an error worth stopping
            // for, but the cursor must still advance or the pass reads the
            // same row forever.
            report.skipped += 1;
            stamp_cursor(store, item.id, &mut report).await;
            continue;
        };

        let anime = library.anime;
        let genres = if anime {
            anilist_genres(anilist, &item).await
        } else {
            match tmdb {
                Some(tmdb) => tmdb_genres(tmdb, &item).await,
                None => Ok(Vec::new()),
            }
        };

        match genres {
            Ok(genres) if genres.is_empty() => {
                report.skipped += 1;
                stamp_cursor(store, item.id, &mut report).await;
            }
            Ok(genres) => {
                let patch = MetadataPatch {
                    genres: genre_patch(genres),
                    ..Default::default()
                };
                match store.apply_metadata(item.id, &patch).await {
                    Ok(()) => report.backfilled += 1,
                    Err(e) => {
                        tracing::error!(item = item.id, error = %e, "genre backfill: store failed");
                        report.failed += 1;
                        report.note(format!("`{}`: storing genres failed: {e}", item.title));
                    }
                }
                stamp_cursor(store, item.id, &mut report).await;
            }
            // A rate limit is a fact about the whole run, not about this
            // title, and the client has already retried it. Stopping here and
            // leaving the cursor where it is means the next pass resumes at
            // this exact item — the alternative, marking it failed and
            // carrying on, would burn the rest of the catalogue's worth of
            // 429s and record every title as broken.
            Err(MetadataError::Status(429)) => {
                tracing::warn!(
                    item = item.id,
                    "genre backfill: rate limited; pausing until the next pass"
                );
                report.note(format!(
                    "TMDB rate-limited the backfill at `{}` — it will resume from \
                     there, nothing before it is repeated",
                    item.title
                ));
                return Some(report);
            }
            Err(e) => {
                tracing::error!(item = item.id, error = %e, "genre backfill: lookup failed");
                report.failed += 1;
                report.note(format!("`{}`: genre lookup failed: {e}", item.title));
                // The cursor advances past a failure on purpose. Holding it
                // would make one permanently-404 title block the whole
                // catalogue forever; the item keeps its empty genres, is
                // named in `problems`, and is picked up again if the backfill
                // is re-armed.
                stamp_cursor(store, item.id, &mut report).await;
            }
        }
    }

    tracing::info!(
        backfilled = report.backfilled,
        failed = report.failed,
        skipped = report.skipped,
        resumed_from = report.resumed_from,
        cursor = report.cursor,
        "genre backfill pass finished"
    );
    Some(report)
}

/// Genres for one already-matched title from TMDB.
///
/// One request, and only one: a `/movie/{id}` or `/tv/{id}` details response
/// carries the genre objects outright. The by-title fallbacks exist for the
/// item whose id never landed (an anime-flagged library switched back, a hand
/// edit) and are the only shape here that can cost two.
async fn tmdb_genres(tmdb: &TmdbClient, item: &Item) -> Result<Vec<String>, MetadataError> {
    match (item.kind, item.tmdb_id) {
        (ItemKind::Movie, Some(id)) => Ok(tmdb.movie_by_id(id).await?.genres),
        (ItemKind::Show, Some(id)) => Ok(tmdb.show_by_id(id).await?.genres),
        (ItemKind::Movie, None) => Ok(tmdb
            .find_movie(&item.title, item.year)
            .await?
            .map(|m| m.genres)
            .unwrap_or_default()),
        (ItemKind::Show, None) => Ok(tmdb
            .find_show(&item.title, item.year)
            .await?
            .map(|m| m.genres)
            .unwrap_or_default()),
        _ => Ok(Vec::new()),
    }
}

/// Genres for one anime title. One request; AniList has no id stored to short
/// -circuit with, and its search *is* the detail call.
async fn anilist_genres(client: &AniListClient, item: &Item) -> Result<Vec<String>, MetadataError> {
    if item.kind != ItemKind::Show {
        return Ok(Vec::new());
    }
    Ok(client
        .find_anime(&item.title)
        .await?
        .map(|m| m.genres)
        .unwrap_or_default())
}

async fn read_cursor(store: &dyn Store) -> i64 {
    store
        .get_setting(keys::GENRE_BACKFILL_CURSOR)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(0)
        .max(0)
}

/// Record progress durably, and only claim it in the report once the write
/// landed. A cursor the report says was reached but the database never saw is
/// the one lie that turns "resumable" back into "restarts".
async fn stamp_cursor(store: &PublicationStore<'_>, id: i64, report: &mut GenreBackfillReport) {
    match store
        .put_setting(keys::GENRE_BACKFILL_CURSOR, &id.to_string())
        .await
    {
        Ok(()) => report.cursor = id,
        Err(e) => {
            tracing::error!(error = %e, cursor = id, "genre backfill: cannot stamp progress");
            report.note(format!("cannot record backfill progress at {id}: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{LibraryKind, NewItem, NewLibrary};
    use crate::store::{LibraryStore, MediaStore, SettingsStore, SqliteStore};
    use serde_json::json;
    use std::sync::Arc;

    /// A movie whose TMDB id is `1000 + n`. Ids are chosen so a test can say
    /// exactly which titles a pass touched.
    const BASE_ID: i64 = 1000;
    /// `/movie/1002` answers 404 — a title TMDB no longer has.
    const GONE: i64 = BASE_ID + 2;
    /// `/movie/1003` answers with an empty genre list — TMDB has it and files
    /// it under nothing, which is a skip and not a failure.
    const GENRELESS: i64 = BASE_ID + 3;

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

    /// TMDB details for `/movie/{id}`, counting every request so a test can
    /// prove a resumed pass did not re-fetch what an interrupted one already
    /// paid for.
    ///
    /// `quirks` turns on the two answers that are not "here are your genres":
    /// [`GONE`] 404s and [`GENRELESS`] answers with an empty list. Off by
    /// default, so a test about resumption can equate "has genres" with
    /// "was processed" without a 404 quietly breaking the equivalence.
    fn details_mock(hits: Arc<std::sync::Mutex<Vec<i64>>>, quirks: bool) -> axum::Router {
        use axum::extract::Path;
        use axum::response::IntoResponse;
        use axum::routing::get;
        axum::Router::new().route(
            "/movie/{id}",
            get(move |Path(id): Path<i64>| {
                let hits = hits.clone();
                async move {
                    hits.lock().expect("hits").push(id);
                    if quirks && id == GONE {
                        return axum::http::StatusCode::NOT_FOUND.into_response();
                    }
                    let genres = if quirks && id == GENRELESS {
                        json!([])
                    } else {
                        json!([{ "id": 28, "name": "Action" }, { "id": 80, "name": "Crime" }])
                    };
                    axum::Json(json!({ "id": id, "title": "x", "genres": genres })).into_response()
                }
            }),
        )
    }

    /// `count` already-enriched movies with ids but no genres — exactly the
    /// state a library upgraded to v13 is in.
    async fn seed(count: i64) -> (SqliteStore, Vec<i64>) {
        let store = SqliteStore::open_in_memory().expect("open");
        let lib = store
            .create_library(&NewLibrary {
                name: "Movies".into(),
                kind: LibraryKind::Movies,
                paths: vec![],
                anime: false,
            })
            .await
            .expect("lib");
        let mut ids = Vec::new();
        for n in 0..count {
            let id = store
                .insert_item(&NewItem {
                    library_id: lib.id,
                    kind: ItemKind::Movie,
                    parent_id: None,
                    title: format!("film {n}"),
                    year: None,
                    season_number: None,
                    episode_number: None,
                })
                .await
                .expect("item");
            store
                .apply_metadata(
                    id,
                    &MetadataPatch {
                        tmdb_id: Some(BASE_ID + n),
                        // `enriched` is what makes these invisible to the
                        // ordinary enrichment queue, which is the entire
                        // reason a backfill has to exist.
                        enriched: true,
                        ..Default::default()
                    },
                )
                .await
                .expect("enrich stamp");
            ids.push(id);
        }
        store
            .put_setting(keys::GENRE_BACKFILL, "1")
            .await
            .expect("arm");
        (store, ids)
    }

    async fn with_genres(store: &SqliteStore, ids: &[i64]) -> usize {
        let mut n = 0;
        for id in ids {
            let item = store.get_item(*id).await.expect("get").expect("item");
            if !item.genres.is_empty() {
                n += 1;
            }
        }
        n
    }

    /// The acceptance case: kill a pass mid-run and the next one picks up
    /// where it stopped.
    ///
    /// Proved by request accounting rather than by timing — every title is
    /// fetched exactly ONCE across both passes. A backfill that restarted
    /// would still end up correct, and would still look correct in every
    /// assertion about stored genres; the only visible difference is that it
    /// paid TMDB twice for the first half, which on a real catalogue is the
    /// difference between finishing and being rate-limited.
    #[tokio::test]
    async fn an_interrupted_backfill_resumes_instead_of_restarting() {
        const TITLES: i64 = 8;
        let (store, ids) = seed(TITLES).await;
        let hits = Arc::new(std::sync::Mutex::new(Vec::new()));
        let base = serve(details_mock(hits.clone(), false)).await;
        let tmdb = TmdbClient::new("k").with_base(&base, &base);
        let anilist = AniListClient::new();

        // Drop the pass mid-flight. This is a crash: the future is cancelled
        // at whatever await it is on, with no chance to tidy up — which is
        // exactly why progress has to be durable per title rather than
        // written at the end.
        let pace = Duration::from_millis(80);
        let interrupted = tokio::time::timeout(
            Duration::from_millis(250),
            backfill_pass(&store, Some(&tmdb), &anilist, pace),
        )
        .await;
        assert!(interrupted.is_err(), "the pass must have been cut short");

        let cursor: i64 = store
            .get_setting(keys::GENRE_BACKFILL_CURSOR)
            .await
            .expect("setting")
            .expect("a cursor must be stamped before the pass ends")
            .parse()
            .expect("numeric cursor");
        // The cursor is the durable resume point. Cancellation can also land
        // in the small window after the next title's genres are stored but
        // before its cursor stamp lands. In that case the store is one title
        // ahead of the cursor, and the resume query correctly skips that
        // already-patched title instead of fetching it again.
        let done_first = ids
            .iter()
            .position(|id| *id == cursor)
            .expect("the cursor names one of the seeded items")
            + 1;
        assert!(
            done_first >= 1 && (done_first as i64) < TITLES,
            "the interruption must land mid-run, not before or after it: \
             {done_first} of {TITLES} done, cursor {cursor}"
        );
        let stored_first = with_genres(&store, &ids).await;
        assert!(
            (done_first..=done_first + 1).contains(&stored_first),
            "every title behind the cursor has its genres, and at most the \
             one being stamped may be ahead of it: cursor covers {done_first}, \
             store has {stored_first}"
        );

        let before_resume = hits.lock().expect("hits").len();

        // Resume. Same store, same settings, nothing handed over in memory.
        let report = backfill_pass(&store, Some(&tmdb), &anilist, Duration::ZERO)
            .await
            .expect("armed");
        assert_eq!(
            report.resumed_from, cursor,
            "the second pass starts from the stamp the first one left"
        );
        assert_eq!(
            report.backfilled,
            TITLES as usize - stored_first,
            "it does the rest, and only the rest"
        );
        assert_eq!(with_genres(&store, &ids).await, TITLES as usize);

        // The proof. The resumed pass fetched EXACTLY the titles after the
        // cursor — not one before it, not one twice.
        let fetched = hits.lock().expect("hits").clone();
        let resumed: Vec<i64> = fetched[before_resume..].to_vec();
        let expected: Vec<i64> = (stored_first as i64..TITLES).map(|n| BASE_ID + n).collect();
        assert_eq!(
            resumed, expected,
            "a resumed backfill re-fetches nothing behind its cursor; the \
             whole list was {fetched:?}"
        );
        // At most ONE title can appear twice across both passes: the one the
        // cancellation caught in flight, whose response never arrived to be
        // stored. That repeat is inherent to being killed mid-request and is
        // the bound worth stating — anything more is a restart.
        let mut seen = fetched.clone();
        seen.sort_unstable();
        let total = seen.len();
        seen.dedup();
        let duplicates = total - seen.len();
        assert!(
            duplicates <= 1,
            "only the in-flight title may be repeated, got {duplicates}: {fetched:?}"
        );
    }

    /// The three counts, and the rule that a failure is never silent.
    #[tokio::test]
    async fn a_pass_counts_backfilled_failed_and_skipped_and_names_the_failure() {
        let (store, ids) = seed(4).await;
        let hits = Arc::new(std::sync::Mutex::new(Vec::new()));
        let base = serve(details_mock(hits, true)).await;
        let tmdb = TmdbClient::new("k").with_base(&base, &base);
        let anilist = AniListClient::new();

        let report = backfill_pass(&store, Some(&tmdb), &anilist, Duration::ZERO)
            .await
            .expect("armed");
        assert_eq!(report.backfilled, 2, "films 0 and 1");
        assert_eq!(report.failed, 1, "film 2 is gone from TMDB");
        assert_eq!(report.skipped, 1, "film 3 has no genres to give");
        assert!(
            !report.complete,
            "there was work, so this is not the last pass"
        );
        assert_eq!(
            report.problems.len(),
            1,
            "the failure must be named, not merely counted: {:?}",
            report.problems
        );
        assert!(
            report.problems[0].contains("film 2"),
            "and named by title: {:?}",
            report.problems
        );

        // The failed title keeps its empty genres — and the cursor moved past
        // it anyway, or one dead id would block the catalogue forever.
        let gone = store.get_item(ids[2]).await.expect("get").expect("item");
        assert!(gone.genres.is_empty());
        let cursor = store
            .get_setting(keys::GENRE_BACKFILL_CURSOR)
            .await
            .expect("setting");
        assert_eq!(cursor.as_deref(), Some(ids[3].to_string().as_str()));
    }

    /// It stops on its own. A backfill an operator has to remember to turn
    /// off is one that quietly re-runs against TMDB forever.
    #[tokio::test]
    async fn a_finished_backfill_disarms_itself_and_rewinds_the_cursor() {
        let (store, ids) = seed(2).await;
        let hits = Arc::new(std::sync::Mutex::new(Vec::new()));
        let base = serve(details_mock(hits, false)).await;
        let tmdb = TmdbClient::new("k").with_base(&base, &base);
        let anilist = AniListClient::new();

        let first = backfill_pass(&store, Some(&tmdb), &anilist, Duration::ZERO)
            .await
            .expect("armed");
        assert_eq!(first.backfilled, 2);
        assert!(!first.complete);
        assert_eq!(with_genres(&store, &ids).await, 2);

        let second = backfill_pass(&store, Some(&tmdb), &anilist, Duration::ZERO)
            .await
            .expect("still armed");
        assert!(second.complete, "nothing left means done");
        assert_eq!(second.cursor, 0, "rewound, so re-arming starts at the top");
        assert!(
            !is_armed(&store).await,
            "and disarmed, so the scheduler stops calling it"
        );
        assert_eq!(
            backfill_pass(&store, Some(&tmdb), &anilist, Duration::ZERO).await,
            None,
            "a disarmed backfill does nothing at all, quietly"
        );
    }

    #[test]
    fn problems_are_capped_with_a_trailing_line() {
        let mut r = GenreBackfillReport::default();
        for i in 0..(MAX_PROBLEMS + 10) {
            r.note(format!("problem {i}"));
        }
        assert_eq!(r.problems.len(), MAX_PROBLEMS + 1);
        assert!(
            r.problems[MAX_PROBLEMS].contains("and more not listed"),
            "the cap must announce itself, or a truncated list reads as a complete one"
        );
    }

    #[test]
    fn the_pace_stays_under_tmdbs_public_ceiling() {
        // ~50 req/s is TMDB's published guidance; this is 4. A change that
        // makes the backfill "fast" should have to argue with this line.
        assert!(PACE >= Duration::from_millis(100), "pace: {PACE:?}");
    }
}
