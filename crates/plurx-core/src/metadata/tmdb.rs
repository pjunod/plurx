//! TMDB (The Movie Database) provider client.
//!
//! Primary source for movies and TV (REQ-META-1). Uses the v3 REST API with an
//! API key. All HTTP lives here; the pure match-ranking logic is factored out
//! for unit testing without a network.

use serde_json::Value;

use crate::error::MetadataError;

const API_BASE: &str = "https://api.themoviedb.org/3";
const IMAGE_BASE: &str = "https://image.tmdb.org/t/p";

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
}

impl TmdbClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        TmdbClient {
            api_key: api_key.into(),
            http: reqwest::Client::builder()
                .user_agent(concat!("plurx/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default(),
        }
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, MetadataError> {
        let mut req = self
            .http
            .get(format!("{API_BASE}{path}"))
            .query(&[("api_key", self.api_key.as_str())]);
        for (k, v) in query {
            req = req.query(&[(*k, v.as_str())]);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| MetadataError::Http(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(MetadataError::Status(resp.status().as_u16()));
        }
        resp.json()
            .await
            .map_err(|e| MetadataError::Parse(e.to_string()))
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
        // Details call fills runtime + imdb_id, absent from search results.
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
        Ok(Some(show_match(id, best)))
    }

    /// Fetch all episodes of one season.
    pub async fn season_episodes(
        &self,
        show_tmdb_id: i64,
        season_number: i32,
    ) -> Result<Vec<EpisodeMeta>, MetadataError> {
        let body = self
            .get(&format!("/tv/{show_tmdb_id}/season/{season_number}"), &[])
            .await?;
        let episodes = body
            .get("episodes")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().map(episode_meta).collect())
            .unwrap_or_default();
        Ok(episodes)
    }

    /// Download an image by TMDB-relative path at the given size, returning the
    /// raw bytes. `size` is a TMDB bucket like `w500` or `original`.
    pub async fn download_image(
        &self,
        tmdb_path: &str,
        size: &str,
    ) -> Result<Vec<u8>, MetadataError> {
        let url = image_url(tmdb_path, size);
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| MetadataError::Http(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(MetadataError::Status(resp.status().as_u16()));
        }
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

fn str_opt(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
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
}
