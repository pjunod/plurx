//! AniList metadata provider for anime (REQ-META-3).
//!
//! AniList's public GraphQL API needs no API key, and gives anime-correct
//! titles (romaji/english/native), synopsis, year, and artwork. Per-episode
//! detail is limited on AniList, so episodes keep their absolute numbering and
//! generic titles — the series-level match is what makes an anime library
//! look right.

use serde_json::Value;

use crate::error::MetadataError;

const API: &str = "https://graphql.anilist.co";

const SEARCH_QUERY: &str = r#"
query ($search: String) {
  Media(search: $search, type: ANIME) {
    id
    title { romaji english native }
    description(asHtml: false)
    seasonYear
    coverImage { extraLarge large }
    bannerImage
  }
}"#;

/// A resolved AniList series match.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AniMatch {
    pub anilist_id: i64,
    /// Preferred display title (english, else romaji, else native).
    pub title: String,
    pub year: Option<i32>,
    pub overview: Option<String>,
    /// Absolute image URLs (AniList serves full URLs, unlike TMDB).
    pub cover_url: Option<String>,
    pub banner_url: Option<String>,
}

pub struct AniListClient {
    http: reqwest::Client,
}

impl AniListClient {
    pub fn new() -> Self {
        AniListClient {
            http: reqwest::Client::builder()
                .user_agent(concat!("plurx/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default(),
        }
    }

    /// Search anime by title and return the best (first) match.
    pub async fn find_anime(&self, title: &str) -> Result<Option<AniMatch>, MetadataError> {
        let body = serde_json::json!({
            "query": SEARCH_QUERY,
            "variables": { "search": title },
        });
        let resp = self
            .http
            .post(API)
            .json(&body)
            .send()
            .await
            .map_err(|e| MetadataError::Http(e.to_string()))?;
        // AniList returns 404 with a GraphQL error when nothing matches.
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(MetadataError::Status(resp.status().as_u16()));
        }
        let json: Value = resp
            .json()
            .await
            .map_err(|e| MetadataError::Parse(e.to_string()))?;
        Ok(parse_media(json.get("data").and_then(|d| d.get("Media"))))
    }

    /// Download an image from an absolute URL (AniList serves full URLs).
    pub async fn download_image(&self, url: &str) -> Result<Vec<u8>, MetadataError> {
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| MetadataError::Http(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(MetadataError::Status(resp.status().as_u16()));
        }
        Ok(resp
            .bytes()
            .await
            .map_err(|e| MetadataError::Http(e.to_string()))?
            .to_vec())
    }
}

impl Default for AniListClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse the `Media` object from an AniList response.
fn parse_media(media: Option<&Value>) -> Option<AniMatch> {
    let media = media?;
    if media.is_null() {
        return None;
    }
    let title = media.get("title");
    let display = title
        .and_then(|t| t.get("english"))
        .and_then(|v| v.as_str())
        .or_else(|| title.and_then(|t| t.get("romaji")).and_then(|v| v.as_str()))
        .or_else(|| title.and_then(|t| t.get("native")).and_then(|v| v.as_str()))?
        .to_owned();

    Some(AniMatch {
        anilist_id: media.get("id").and_then(|v| v.as_i64()).unwrap_or(0),
        title: display,
        year: media
            .get("seasonYear")
            .and_then(|v| v.as_i64())
            .map(|y| y as i32),
        overview: media
            .get("description")
            .and_then(|v| v.as_str())
            .map(strip_html)
            .filter(|s| !s.is_empty()),
        cover_url: media
            .get("coverImage")
            .and_then(|c| c.get("extraLarge").or_else(|| c.get("large")))
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        banner_url: media
            .get("bannerImage")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
    })
}

/// AniList descriptions carry light HTML (`<br>`, `<i>`); strip tags to plain
/// text so the overview renders cleanly.
fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            // A closed tag becomes a space so `over<br>but` → `over but`.
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#039;", "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_media_prefers_english_title() {
        let media = json!({
            "id": 154587,
            "title": { "romaji": "Sousou no Frieren", "english": "Frieren: Beyond Journey's End", "native": "葬送のフリーレン" },
            "description": "The <i>adventure</i> is over<br>but life goes on.",
            "seasonYear": 2023,
            "coverImage": { "extraLarge": "https://img/cover.jpg", "large": "https://img/large.jpg" },
            "bannerImage": "https://img/banner.jpg"
        });
        let m = parse_media(Some(&media)).expect("match");
        assert_eq!(m.anilist_id, 154587);
        assert_eq!(m.title, "Frieren: Beyond Journey's End");
        assert_eq!(m.year, Some(2023));
        assert_eq!(
            m.overview.as_deref(),
            Some("The adventure is over but life goes on.")
        );
        assert_eq!(m.cover_url.as_deref(), Some("https://img/cover.jpg"));
        assert_eq!(m.banner_url.as_deref(), Some("https://img/banner.jpg"));
    }

    #[test]
    fn null_media_is_no_match() {
        assert!(parse_media(Some(&json!(null))).is_none());
        assert!(parse_media(None).is_none());
    }

    #[test]
    fn falls_back_to_romaji() {
        let media = json!({
            "id": 1, "title": { "romaji": "Bocchi the Rock!", "english": null, "native": "..." }
        });
        assert_eq!(
            parse_media(Some(&media)).expect("m").title,
            "Bocchi the Rock!"
        );
    }
}
