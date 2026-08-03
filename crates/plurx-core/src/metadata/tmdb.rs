//! TMDB (The Movie Database) provider client.
//!
//! Primary source for movies and TV (REQ-META-1). Uses the v3 REST API with an
//! API key. All HTTP lives here; the pure match-ranking logic is factored out
//! for unit testing without a network.

use serde_json::Value;

use crate::error::MetadataError;

const API_BASE: &str = "https://api.themoviedb.org/3";
const IMAGE_BASE: &str = "https://image.tmdb.org/t/p";

/// How many times one request is sent before giving up, first try included.
/// Three because the failure being ridden out is a burst limit measured in
/// seconds; a longer outage is the retry *sweep's* problem, not this loop's,
/// and holding a scan open for it helps nobody.
const MAX_ATTEMPTS: u32 = 3;
/// First backoff step, doubling thereafter. Short enough that a scan does not
/// visibly stall on one blip.
const RETRY_BASE_DELAY: std::time::Duration = std::time::Duration::from_millis(500);
/// Ceiling on an honoured `Retry-After`. A server asking for an hour is
/// telling us to come back later, not to keep a scan open for one — the
/// artwork retry job is what comes back later.
const RETRY_AFTER_CAP: std::time::Duration = std::time::Duration::from_secs(30);

/// A resolved provider match for a movie or show.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Match {
    pub tmdb_id: i64,
    pub title: String,
    pub year: Option<i32>,
    pub overview: Option<String>,
    pub imdb_id: Option<String>,
    pub runtime_ms: Option<i64>,
    pub air_date: Option<String>,
    /// TMDB-relative image paths (e.g. `/abc.jpg`); resolve with [`image_url`].
    pub poster_path: Option<String>,
    pub backdrop_path: Option<String>,
    /// Genre names, in TMDB's own order. Empty means "this response carried
    /// none" — not "TMDB says this title has none". Nothing downstream needs
    /// that distinction and nothing here pretends to make it.
    pub genres: Vec<String>,
}

/// Which of TMDB's two genre vocabularies an id belongs to. They are separate
/// lists with overlapping numbering and different names ("Action & Adventure"
/// is TV-only, id 10759 is "Action & Adventure" on TV and nothing on film), so
/// resolving against the wrong one mislabels a library instead of failing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GenreKind {
    Movie,
    Tv,
}

impl GenreKind {
    fn path(self) -> &'static str {
        match self {
            GenreKind::Movie => "/genre/movie/list",
            GenreKind::Tv => "/genre/tv/list",
        }
    }
}

/// One season's metadata: its own presentation plus its episodes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SeasonMeta {
    pub poster_path: Option<String>,
    pub overview: Option<String>,
    pub air_date: Option<String>,
    pub episodes: Vec<EpisodeMeta>,
}

/// One episode's metadata within a season.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EpisodeMeta {
    pub episode_number: i32,
    pub title: Option<String>,
    pub overview: Option<String>,
    pub air_date: Option<String>,
    pub runtime_ms: Option<i64>,
    pub still_path: Option<String>,
}

pub struct TmdbClient {
    api_key: String,
    http: reqwest::Client,
    /// API base (defaults to [`API_BASE`]); overridable for a self-hosted
    /// proxy or a mock server in tests.
    base: String,
    /// Image CDN base (defaults to [`IMAGE_BASE`]).
    image_base: String,
    /// TMDB's two genre id→name dictionaries, fetched at most ONCE each per
    /// client and therefore at most once per enrichment run.
    ///
    /// This is the whole reason shows cost one extra request instead of one
    /// per title. A TV *search* result carries `genre_ids` and nothing else,
    /// so the names have to come from somewhere; the somewhere must not be a
    /// second round-trip per show, or a 2,000-episode library's refresh
    /// doubles its request count to learn eighteen strings.
    ///
    /// A fetch that fails caches the *empty* map rather than staying unset.
    /// Leaving it unset would retry per title, which is precisely the shape
    /// this exists to prevent — and the failure mode of an unreachable
    /// endpoint would then be "hammer it once per show".
    genres: std::collections::HashMap<GenreKind, tokio::sync::OnceCell<GenreIndex>>,
    /// Set when a vocabulary fetch failed, so the caller can say so once in
    /// its report instead of every title saying nothing. Without this the
    /// failure is invisible from outside: each show still matches, still gets
    /// its poster, and just quietly has no genres.
    genre_index_failed: std::sync::atomic::AtomicBool,
}

/// TMDB genre ids to names, for one vocabulary.
type GenreIndex = std::collections::HashMap<i64, String>;

impl TmdbClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        TmdbClient {
            api_key: api_key.into(),
            http: reqwest::Client::builder()
                .user_agent(concat!("plurx/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default(),
            base: API_BASE.to_owned(),
            image_base: IMAGE_BASE.to_owned(),
            genres: [
                (GenreKind::Movie, tokio::sync::OnceCell::new()),
                (GenreKind::Tv, tokio::sync::OnceCell::new()),
            ]
            .into_iter()
            .collect(),
            genre_index_failed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Point the API and image bases elsewhere (self-hosted TMDB proxy, tests).
    /// Trailing slashes are trimmed so `{base}{path}` composes cleanly.
    pub fn with_base(mut self, base: impl Into<String>, image_base: impl Into<String>) -> Self {
        self.base = base.into().trim_end_matches('/').to_owned();
        self.image_base = image_base.into().trim_end_matches('/').to_owned();
        self
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, MetadataError> {
        let url = format!("{}{path}", self.base);
        let resp = self
            .send_with_retry(|| {
                let mut req = self
                    .http
                    .get(&url)
                    .query(&[("api_key", self.api_key.as_str())]);
                for (k, v) in query {
                    req = req.query(&[(*k, v.as_str())]);
                }
                req
            })
            .await?;
        resp.json()
            .await
            .map_err(|e| MetadataError::Parse(e.to_string()))
    }

    /// Send a request, retrying the failures that are about TMDB's mood
    /// rather than about the request.
    ///
    /// A scan is hundreds of calls in a row, so it is exactly the workload
    /// that trips a rate limit — and before this, a single 429 partway
    /// through silently cost every remaining item its artwork, with the run
    /// reporting success. A 404 is the opposite: TMDB has answered, the
    /// answer is "no", and asking three times is three times the load for the
    /// same word. So only 429 and 5xx come back here.
    async fn send_with_retry(
        &self,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, MetadataError> {
        let mut delay = RETRY_BASE_DELAY;
        for attempt in 1..=MAX_ATTEMPTS {
            let resp = build().send().await;
            let last = attempt == MAX_ATTEMPTS;
            match resp {
                Ok(resp) if resp.status().is_success() => return Ok(resp),
                Ok(resp) => {
                    let status = resp.status();
                    if last || !retryable(status.as_u16()) {
                        return Err(MetadataError::Status(status.as_u16()));
                    }
                    // TMDB says when it will talk again; guessing over the top
                    // of that is how a client earns a longer ban.
                    let wait = retry_after(resp.headers()).unwrap_or(delay);
                    tracing::debug!(
                        status = status.as_u16(),
                        attempt,
                        wait_ms = wait.as_millis() as u64,
                        "tmdb retrying"
                    );
                    tokio::time::sleep(wait).await;
                }
                // A connection that never completed is the same kind of
                // transient as a 503, and the request was never served, so
                // repeating it cannot double anything.
                Err(e) if last => return Err(MetadataError::Http(e.to_string())),
                Err(e) => {
                    tracing::debug!(error = %e, attempt, "tmdb request failed; retrying");
                    tokio::time::sleep(delay).await;
                }
            }
            delay *= 2;
        }
        // Unreachable: the loop returns on its last attempt either way.
        Err(MetadataError::Http("retries exhausted".to_owned()))
    }

    /// One TMDB genre vocabulary as id→name, fetched at most once per client.
    ///
    /// Costs one request the first time either kind is asked for and zero
    /// thereafter, including across every title in a run. A failure is cached
    /// as an empty index on purpose: see [`TmdbClient::genres`] — the
    /// alternative retries per title, which is the cost this method exists to
    /// avoid, paid at its worst moment.
    async fn genre_index(&self, kind: GenreKind) -> &GenreIndex {
        // The map is built with both keys in `new`, so the fallback is
        // unreachable; a `unwrap` here would be a panic in a metadata path
        // for a bug that cannot happen, which is a bad trade.
        static EMPTY: std::sync::OnceLock<GenreIndex> = std::sync::OnceLock::new();
        let Some(cell) = self.genres.get(&kind) else {
            return EMPTY.get_or_init(GenreIndex::new);
        };
        cell.get_or_init(|| async {
            match self.get(kind.path(), &[]).await {
                Ok(body) => {
                    let index = parse_genre_index(&body);
                    tracing::debug!(?kind, count = index.len(), "fetched tmdb genre list");
                    index
                }
                Err(e) => {
                    tracing::error!(?kind, error = %e, "tmdb genre list unavailable; \
                         titles matched by search will be stored without genres");
                    self.genre_index_failed
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    GenreIndex::new()
                }
            }
        })
        .await
    }

    /// Did a genre-vocabulary fetch fail during this client's life?
    ///
    /// One flag for the whole run, because that is the shape of the failure:
    /// the list is fetched once, so it fails once, and everything matched by
    /// search afterwards is affected equally. A caller reports it once.
    pub fn genre_index_failed(&self) -> bool {
        self.genre_index_failed
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Search movies and return the best match (title + year aware).
    pub async fn find_movie(
        &self,
        title: &str,
        year: Option<i32>,
    ) -> Result<Option<Match>, MetadataError> {
        let mut query = vec![("query", title.to_owned())];
        if let Some(y) = year {
            query.push(("year", y.to_string()));
        }
        let body = self.get("/search/movie", &query).await?;
        let results = body.get("results").and_then(|v| v.as_array());
        let Some(best) = results.and_then(|r| pick_best(title, year, r, "release_date")) else {
            return Ok(None);
        };
        let id = best.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        // Details call fills runtime + imdb_id, absent from search results —
        // and genres, which have been in this response all along and were
        // read straight past. A movie's genres therefore cost zero requests:
        // there is no branch here to add, only a field to stop discarding.
        let details = self.get(&format!("/movie/{id}"), &[]).await?;
        Ok(Some(movie_match(id, best, &details)))
    }

    /// Search shows and return the best match.
    pub async fn find_show(
        &self,
        title: &str,
        year: Option<i32>,
    ) -> Result<Option<Match>, MetadataError> {
        let mut query = vec![("query", title.to_owned())];
        if let Some(y) = year {
            query.push(("first_air_date_year", y.to_string()));
        }
        let body = self.get("/search/tv", &query).await?;
        let results = body.get("results").and_then(|v| v.as_array());
        let Some(best) = results.and_then(|r| pick_best(title, year, r, "first_air_date")) else {
            return Ok(None);
        };
        let id = best.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        let mut m = show_match(id, best);
        // A TV search result names genres by id only, so this is the one
        // enrichment path that cannot get them free. Resolved against the
        // once-per-run cached vocabulary rather than a `/tv/{id}` details
        // call: one request for the whole run instead of one per series.
        if m.genres.is_empty() {
            let ids = genre_ids(best);
            if !ids.is_empty() {
                let index = self.genre_index(GenreKind::Tv).await;
                m.genres = ids.iter().filter_map(|id| index.get(id).cloned()).collect();
            }
        }
        Ok(Some(m))
    }

    /// Search movies and return the candidates in TMDB's own relevance order,
    /// capped at `limit`. Unlike [`find_movie`](Self::find_movie) this makes no
    /// per-result detail call — a picker needs a title, a year and a poster,
    /// and paying an HTTP round-trip per candidate for runtime/imdb that nobody
    /// reads would be wasteful. The chosen one gets its details on apply.
    pub async fn search_movies(
        &self,
        title: &str,
        year: Option<i32>,
        limit: usize,
    ) -> Result<Vec<Match>, MetadataError> {
        let mut query = vec![("query", title.to_owned())];
        if let Some(y) = year {
            query.push(("year", y.to_string()));
        }
        let body = self.get("/search/movie", &query).await?;
        let mut found = candidates(&body, limit, |id, v| movie_match(id, v, &Value::Null));
        // The film vocabulary's one caller. Enrichment never needs it — a
        // movie's details response carries genre names outright — but the
        // picker builds its candidates from search results, which carry ids.
        // One request for the whole picker session, cached like the TV one;
        // a human choosing between two films with the same title is exactly
        // who genres help.
        if found.iter().any(|m| m.genres.is_empty()) {
            let ids = candidate_genre_ids(&body, limit);
            if ids.iter().any(|g| !g.is_empty()) {
                let index = self.genre_index(GenreKind::Movie).await;
                for (m, ids) in found.iter_mut().zip(ids) {
                    if m.genres.is_empty() {
                        m.genres = ids.iter().filter_map(|id| index.get(id).cloned()).collect();
                    }
                }
            }
        }
        Ok(found)
    }

    /// Search shows and return the candidates, as
    /// [`search_movies`](Self::search_movies) does.
    pub async fn search_shows(
        &self,
        title: &str,
        year: Option<i32>,
        limit: usize,
    ) -> Result<Vec<Match>, MetadataError> {
        let mut query = vec![("query", title.to_owned())];
        if let Some(y) = year {
            query.push(("first_air_date_year", y.to_string()));
        }
        let body = self.get("/search/tv", &query).await?;
        let mut found = candidates(&body, limit, show_match);
        if found.iter().any(|m| m.genres.is_empty()) {
            let ids = candidate_genre_ids(&body, limit);
            if ids.iter().any(|g| !g.is_empty()) {
                let index = self.genre_index(GenreKind::Tv).await;
                for (m, ids) in found.iter_mut().zip(ids) {
                    if m.genres.is_empty() {
                        m.genres = ids.iter().filter_map(|id| index.get(id).cloned()).collect();
                    }
                }
            }
        }
        Ok(found)
    }

    /// Fetch one movie by TMDB id. This is the authoritative path — a manual
    /// match names an id outright, and a refresh of an already-matched item
    /// re-reads that same id instead of guessing from the title again.
    pub async fn movie_by_id(&self, tmdb_id: i64) -> Result<Match, MetadataError> {
        let details = self.get(&format!("/movie/{tmdb_id}"), &[]).await?;
        // A detail response carries the search fields too, so it can play both
        // parts of `movie_match`.
        Ok(movie_match(tmdb_id, &details, &details))
    }

    /// Fetch one show by TMDB id.
    pub async fn show_by_id(&self, tmdb_id: i64) -> Result<Match, MetadataError> {
        let details = self.get(&format!("/tv/{tmdb_id}"), &[]).await?;
        Ok(show_match(tmdb_id, &details))
    }

    /// The TMDB id behind an IMDb id, or `None` if TMDB does not know it.
    ///
    /// An item can reach plurx carrying only an IMDb id — monarr's movie side
    /// tracks IMDb, and a hand-written NFO usually does too. Spending one
    /// lookup to turn it into a TMDB id is worth it: the alternative is a
    /// title search, and a title search is the thing that gets it wrong.
    pub async fn id_for_imdb(
        &self,
        imdb_id: &str,
        show: bool,
    ) -> Result<Option<i64>, MetadataError> {
        let body = self
            .get(
                &format!("/find/{imdb_id}"),
                &[("external_source", "imdb_id".to_owned())],
            )
            .await?;
        let field = if show { "tv_results" } else { "movie_results" };
        Ok(body
            .get(field)
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_i64()))
    }

    /// Full image URL for a TMDB-relative path, honouring *this* client's image
    /// base so a self-hosted proxy — or a test mock — is respected. The free
    /// [`image_url`] always points at the public CDN.
    pub fn image_url(&self, tmdb_path: &str, size: &str) -> String {
        format!("{}/{size}{tmdb_path}", self.image_base)
    }

    /// Fetch one season: its own artwork/overview plus all episodes.
    pub async fn season_detail(
        &self,
        show_tmdb_id: i64,
        season_number: i32,
    ) -> Result<SeasonMeta, MetadataError> {
        let body = self
            .get(&format!("/tv/{show_tmdb_id}/season/{season_number}"), &[])
            .await?;
        let episodes = body
            .get("episodes")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().map(episode_meta).collect())
            .unwrap_or_default();
        Ok(SeasonMeta {
            poster_path: str_opt(&body, "poster_path"),
            overview: str_opt(&body, "overview"),
            air_date: str_opt(&body, "air_date"),
            episodes,
        })
    }

    /// Download an image by TMDB-relative path at the given size, returning the
    /// raw bytes. `size` is a TMDB bucket like `w500` or `original`.
    pub async fn download_image(
        &self,
        tmdb_path: &str,
        size: &str,
    ) -> Result<Vec<u8>, MetadataError> {
        let url = format!("{}/{size}{tmdb_path}", self.image_base);
        // Same retry as the API calls, and needed more: the image CDN is hit
        // once per poster and once per backdrop, so it absorbs the bulk of a
        // scan's requests and is where a rate limit lands first.
        let resp = self.send_with_retry(|| self.http.get(&url)).await?;
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| MetadataError::Http(e.to_string()))?;
        Ok(bytes.to_vec())
    }
}

/// Full image URL for a TMDB-relative path.
pub fn image_url(tmdb_path: &str, size: &str) -> String {
    format!("{IMAGE_BASE}/{size}{tmdb_path}")
}

/// Is this status worth sending the same request again?
///
/// 429 (rate limited) and 5xx (TMDB is having a moment) yes; everything else
/// no. 404 in particular must stay a fast permanent failure — an item TMDB
/// does not have is not an item TMDB will have in 500ms, and a library full
/// of them would triple every scan's request count to learn nothing.
fn retryable(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

/// `Retry-After` as a duration, capped. Only the delta-seconds form is read:
/// the HTTP-date form is legal but TMDB does not send it, and a date needs a
/// clock comparison to mean anything.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<std::time::Duration> {
    let secs: u64 = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(std::time::Duration::from_secs(secs).min(RETRY_AFTER_CAP))
}

fn year_of(value: &Value, date_field: &str) -> Option<i32> {
    value
        .get(date_field)
        .and_then(|v| v.as_str())
        .filter(|s| s.len() >= 4)
        .and_then(|s| s[..4].parse().ok())
}

/// Rank search candidates: exact (case-insensitive) title AND matching year
/// wins; then exact title; then matching year; then TMDB's own order (first).
/// Returns a reference into `results`.
fn pick_best<'a>(
    query_title: &str,
    query_year: Option<i32>,
    results: &'a [Value],
    date_field: &str,
) -> Option<&'a Value> {
    if results.is_empty() {
        return None;
    }
    let want = query_title.to_lowercase();
    let score = |c: &Value| -> i32 {
        let title = c
            .get("title")
            .or_else(|| c.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_lowercase();
        let year = year_of(c, date_field);
        let title_exact = title == want;
        let year_match = matches!((query_year, year), (Some(a), Some(b)) if a == b);
        match (title_exact, year_match) {
            (true, true) => 3,
            (true, false) => 2,
            (false, true) => 1,
            (false, false) => 0,
        }
    };
    // Stable pick: highest score, ties broken by original order.
    results
        .iter()
        .enumerate()
        .max_by_key(|(i, c)| (score(c), -(*i as i32)))
        .map(|(_, c)| c)
}

/// Map a search response's `results` array into matches, keeping TMDB's own
/// relevance order and stopping at `limit`. Unlike [`pick_best`] this ranks
/// nothing: a human is about to look at the list and decide for themselves.
fn candidates(body: &Value, limit: usize, map: impl Fn(i64, &Value) -> Match) -> Vec<Match> {
    let Some(results) = body.get("results").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    results
        .iter()
        .filter_map(|v| v.get("id").and_then(|x| x.as_i64()).map(|id| map(id, v)))
        .take(limit)
        .collect()
}

fn str_opt(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

/// Genre NAMES from a response that carries the full objects — every
/// `/movie/{id}`, `/tv/{id}` and `/genre/*/list` body does. Search results do
/// not; they carry [`genre_ids`].
fn genre_names(v: &Value) -> Vec<String> {
    v.get("genres")
        .and_then(|g| g.as_array())
        .map(|arr| arr.iter().filter_map(|g| str_opt(g, "name")).collect())
        .unwrap_or_default()
}

/// Genre IDS from a search result. Needs [`TmdbClient::genre_index`] to become
/// names; on its own it is a list of integers no client can render.
fn genre_ids(v: &Value) -> Vec<i64> {
    v.get("genre_ids")
        .and_then(|g| g.as_array())
        .map(|arr| arr.iter().filter_map(|g| g.as_i64()).collect())
        .unwrap_or_default()
}

/// Per-candidate genre ids from a search response, aligned with what
/// [`candidates`] produces from the same body — same filter, same order, same
/// cap. Zipping two lists built by different rules is how a picker ends up
/// showing one film's genres under another's title.
fn candidate_genre_ids(body: &Value, limit: usize) -> Vec<Vec<i64>> {
    let Some(results) = body.get("results").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    results
        .iter()
        .filter(|v| v.get("id").and_then(|x| x.as_i64()).is_some())
        .take(limit)
        .map(genre_ids)
        .collect()
}

/// `/genre/{kind}/list` into id→name.
fn parse_genre_index(body: &Value) -> GenreIndex {
    body.get("genres")
        .and_then(|g| g.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|g| Some((g.get("id")?.as_i64()?, str_opt(g, "name")?)))
                .collect()
        })
        .unwrap_or_default()
}

fn movie_match(id: i64, search: &Value, details: &Value) -> Match {
    Match {
        tmdb_id: id,
        title: str_opt(search, "title").unwrap_or_default(),
        year: year_of(search, "release_date"),
        overview: str_opt(search, "overview"),
        imdb_id: str_opt(details, "imdb_id"),
        runtime_ms: details
            .get("runtime")
            .and_then(|v| v.as_i64())
            .filter(|m| *m > 0)
            .map(|m| m * 60_000),
        air_date: str_opt(search, "release_date"),
        poster_path: str_opt(search, "poster_path"),
        backdrop_path: str_opt(search, "backdrop_path"),
        // From `details`, where the full objects live. `movie_by_id` passes
        // the details body as both arguments, so it is covered too; only the
        // picker's `Value::Null` details leaves this empty, and that path
        // fills it from the id vocabulary afterwards.
        genres: genre_names(details),
    }
}

fn show_match(id: i64, search: &Value) -> Match {
    Match {
        tmdb_id: id,
        title: str_opt(search, "name").unwrap_or_default(),
        year: year_of(search, "first_air_date"),
        overview: str_opt(search, "overview"),
        imdb_id: None,
        runtime_ms: None,
        air_date: str_opt(search, "first_air_date"),
        poster_path: str_opt(search, "poster_path"),
        backdrop_path: str_opt(search, "backdrop_path"),
        // Empty from a search result (it only has `genre_ids`), populated
        // from a `/tv/{id}` details body. `show_by_id` therefore costs
        // nothing extra; `find_show` resolves the ids itself.
        genres: genre_names(search),
    }
}

fn episode_meta(v: &Value) -> EpisodeMeta {
    EpisodeMeta {
        episode_number: v
            .get("episode_number")
            .and_then(|x| x.as_i64())
            .unwrap_or(0) as i32,
        title: str_opt(v, "name"),
        overview: str_opt(v, "overview"),
        air_date: str_opt(v, "air_date"),
        runtime_ms: v
            .get("runtime")
            .and_then(|x| x.as_i64())
            .filter(|m| *m > 0)
            .map(|m| m * 60_000),
        still_path: str_opt(v, "still_path"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn image_url_composes() {
        assert_eq!(
            image_url("/abc.jpg", "w500"),
            "https://image.tmdb.org/t/p/w500/abc.jpg"
        );
    }

    #[test]
    fn pick_best_prefers_exact_title_and_year() {
        let results = vec![
            json!({ "title": "The Matrix Reloaded", "release_date": "2003-05-15" }),
            json!({ "title": "The Matrix", "release_date": "1999-03-30" }),
            json!({ "title": "The Matrix", "release_date": "2021-12-22" }),
        ];
        // Exact title + year → the 1999 one.
        let best = pick_best("The Matrix", Some(1999), &results, "release_date").expect("best");
        assert_eq!(best.get("release_date").expect("field"), "1999-03-30");
        // Exact title, no year hint → first exact-title candidate wins.
        let best = pick_best("the matrix", None, &results, "release_date").expect("best");
        assert_eq!(best.get("title").expect("field"), "The Matrix");
    }

    #[test]
    fn pick_best_falls_back_to_first() {
        let results = vec![
            json!({ "title": "Something Else", "release_date": "2000-01-01" }),
            json!({ "title": "Another Thing", "release_date": "2001-01-01" }),
        ];
        let best = pick_best("Nonexistent", Some(1990), &results, "release_date").expect("best");
        assert_eq!(best.get("title").expect("field"), "Something Else");
        assert!(pick_best("X", None, &[], "release_date").is_none());
    }

    #[test]
    fn movie_match_extracts_runtime_and_imdb() {
        let search = json!({
            "title": "Heat", "release_date": "1995-12-15",
            "overview": "A crew...", "poster_path": "/p.jpg", "backdrop_path": "/b.jpg"
        });
        let details = json!({ "runtime": 170, "imdb_id": "tt0113277" });
        let m = movie_match(603, &search, &details);
        assert_eq!(m.tmdb_id, 603);
        assert_eq!(m.year, Some(1995));
        assert_eq!(m.runtime_ms, Some(170 * 60_000));
        assert_eq!(m.imdb_id.as_deref(), Some("tt0113277"));
        assert_eq!(m.poster_path.as_deref(), Some("/p.jpg"));
    }

    #[test]
    fn episode_meta_parses() {
        let e = episode_meta(&json!({
            "episode_number": 3, "name": "In Perpetuity",
            "overview": "...", "air_date": "2022-02-25", "runtime": 48, "still_path": "/s.jpg"
        }));
        assert_eq!(e.episode_number, 3);
        assert_eq!(e.title.as_deref(), Some("In Perpetuity"));
        assert_eq!(e.runtime_ms, Some(48 * 60_000));
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

    #[tokio::test]
    async fn client_walks_search_details_season_and_images() {
        use axum::routing::get;
        use axum::Json;
        let app = axum::Router::new()
            .route(
                "/search/movie",
                get(|| async {
                    Json(json!({ "results": [
                        { "id": 603, "title": "The Matrix", "release_date": "1999-03-30",
                          "overview": "A hacker learns the truth.",
                          "poster_path": "/p.jpg", "backdrop_path": "/b.jpg" }
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
                          "poster_path": "/s.jpg" }
                    ]}))
                }),
            )
            .route(
                "/tv/42/season/1",
                get(|| async {
                    Json(json!({
                        "poster_path": "/sp.jpg", "overview": "Season one", "air_date": "2022-02-18",
                        "episodes": [
                            { "episode_number": 1, "name": "Good News", "runtime": 57,
                              "air_date": "2022-02-18", "still_path": "/e1.jpg" }
                        ]
                    }))
                }),
            )
            .route("/w500/p.jpg", get(|| async { vec![1u8, 2, 3, 4] }))
            .route("/search/movie/empty", get(|| async { Json(json!({})) }));
        let base = serve(app).await;
        let c = TmdbClient::new("k").with_base(&base, &base);

        let m = c
            .find_movie("The Matrix", Some(1999))
            .await
            .expect("ok")
            .expect("match");
        assert_eq!(m.tmdb_id, 603);
        assert_eq!(m.runtime_ms, Some(136 * 60_000));
        assert_eq!(m.imdb_id.as_deref(), Some("tt0133093"));

        let s = c
            .find_show("Severance", None)
            .await
            .expect("ok")
            .expect("m");
        assert_eq!(s.tmdb_id, 42);
        assert_eq!(s.title, "Severance");

        let sd = c.season_detail(42, 1).await.expect("season");
        assert_eq!(sd.episodes.len(), 1);
        assert_eq!(sd.episodes[0].runtime_ms, Some(57 * 60_000));
        assert_eq!(sd.overview.as_deref(), Some("Season one"));

        let bytes = c.download_image("/p.jpg", "w500").await.expect("image");
        assert_eq!(bytes, vec![1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn no_results_is_none_and_bad_status_errors() {
        use axum::http::StatusCode;
        use axum::Json;
        // Empty results → Ok(None) for both movie and show.
        let empty =
            serve(axum::Router::new().fallback(|| async { Json(json!({ "results": [] })) })).await;
        let c = TmdbClient::new("k").with_base(&empty, &empty);
        assert!(c.find_movie("Nope", None).await.expect("ok").is_none());
        assert!(c.find_show("Nope", None).await.expect("ok").is_none());

        // Non-2xx surfaces as a Status error.
        let bad = serve(
            axum::Router::new().fallback(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
        )
        .await;
        let c = TmdbClient::new("k").with_base(&bad, &bad);
        assert!(matches!(
            c.find_movie("x", None).await,
            Err(crate::error::MetadataError::Status(500))
        ));
        assert!(c.download_image("/x.jpg", "w500").await.is_err());
    }

    #[test]
    fn only_rate_limits_and_server_faults_are_worth_repeating() {
        assert!(retryable(429));
        assert!(retryable(500));
        assert!(retryable(503));
        assert!(!retryable(404), "TMDB has answered; the answer is no");
        assert!(!retryable(401), "a bad key is not a bad moment");
        assert!(!retryable(200));
    }

    #[test]
    fn retry_after_is_read_in_seconds_and_capped() {
        use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
        let mut headers = HeaderMap::new();
        assert_eq!(retry_after(&headers), None);
        headers.insert(RETRY_AFTER, HeaderValue::from_static("2"));
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(2)));
        // An hour is a server telling us to come back another day; the sweep
        // is what comes back, not a scan held open for 60 minutes.
        headers.insert(RETRY_AFTER, HeaderValue::from_static("3600"));
        assert_eq!(retry_after(&headers), Some(RETRY_AFTER_CAP));
        // The HTTP-date form is legal and unread — better no wait than a
        // parse that silently yields zero.
        headers.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        assert_eq!(retry_after(&headers), None);
    }

    /// A rate limit partway through a scan used to cost every remaining item
    /// its artwork, with the run reporting success. One retry fixes the whole
    /// tail; a 404 must not pay for it.
    #[tokio::test]
    async fn a_429_is_retried_and_a_404_is_not() {
        use axum::http::StatusCode;
        use axum::routing::get;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let hits = Arc::new(AtomicUsize::new(0));
        let image_hits = Arc::clone(&hits);
        let missing_hits = Arc::new(AtomicUsize::new(0));
        let missing = Arc::clone(&missing_hits);
        let app = axum::Router::new()
            .route(
                "/w500/limited.jpg",
                get(move || {
                    let hits = Arc::clone(&image_hits);
                    async move {
                        // Rate limited once, then fine — a burst limit, which
                        // is what a scan actually trips.
                        if hits.fetch_add(1, Ordering::SeqCst) == 0 {
                            (StatusCode::TOO_MANY_REQUESTS, Vec::new())
                        } else {
                            (StatusCode::OK, vec![7u8, 7, 7])
                        }
                    }
                }),
            )
            .route(
                "/w500/gone.jpg",
                get(move || {
                    let hits = Arc::clone(&missing);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        StatusCode::NOT_FOUND
                    }
                }),
            );
        let base = serve(app).await;
        let c = TmdbClient::new("k").with_base(&base, &base);

        let bytes = c
            .download_image("/limited.jpg", "w500")
            .await
            .expect("the retry recovers the image");
        assert_eq!(bytes, vec![7, 7, 7]);
        assert_eq!(hits.load(Ordering::SeqCst), 2, "one 429, then the retry");

        assert!(matches!(
            c.download_image("/gone.jpg", "w500").await,
            Err(crate::error::MetadataError::Status(404))
        ));
        assert_eq!(
            missing_hits.load(Ordering::SeqCst),
            1,
            "a 404 is permanent; asking again is load for the same word"
        );
    }

    /// The API side gets the same treatment, and a 5xx counts too.
    #[tokio::test]
    async fn a_500_on_the_api_is_retried() {
        use axum::http::StatusCode;
        use axum::routing::get;
        use axum::Json;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let app = axum::Router::new().route(
            "/search/movie",
            get(move || {
                let hits = Arc::clone(&counter);
                async move {
                    if hits.fetch_add(1, Ordering::SeqCst) == 0 {
                        (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(json!({ "status_message": "down" })),
                        )
                    } else {
                        (
                            StatusCode::OK,
                            Json(json!({ "results": [
                                { "id": 1, "title": "X", "release_date": "2000-01-01" }
                            ]})),
                        )
                    }
                }
            }),
        );
        let base = serve(app).await;
        let c = TmdbClient::new("k").with_base(&base, &base);
        // The details call 404s from this router; the search succeeding on the
        // second attempt is what is under test.
        let _ = c.find_movie("X", None).await;
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }
}
