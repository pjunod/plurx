//! Revision-bound EPUB locator state.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use plurx_core::domain::{ItemKind, MediaFile, ReadingStateWrite};
use serde::Deserialize;
use serde_json::Value;

use super::dto::{ReadingDto, RevisionDto};
use super::error::ApiError;
use super::extract::AuthUser;
use crate::state::AppState;

const MAX_LOCATOR_BYTES: usize = 32 * 1024;
const MAX_HREF_BYTES: usize = 2 * 1024;

#[derive(Deserialize)]
pub struct ReadingQuery {
    pub file_id: i64,
}

#[derive(Deserialize)]
pub struct PutReadingRequest {
    pub file_id: i64,
    pub revision: RevisionDto,
    pub locator: Value,
    pub progression: f64,
    pub completed: bool,
    #[serde(default)]
    pub recorded_at: Option<i64>,
}

#[derive(serde::Serialize)]
pub struct ReadingResponse {
    pub state: Option<ReadingDto>,
    pub stale: bool,
}

async fn book_file(state: &AppState, item_id: i64, file_id: i64) -> Result<MediaFile, ApiError> {
    let item = state
        .store
        .get_item(item_id)
        .await?
        .ok_or(ApiError::NotFound("item"))?;
    if item.kind != ItemKind::Book {
        return Err(ApiError::BadRequest(
            "reading state is only available for text books".to_owned(),
        ));
    }
    let file = state
        .store
        .get_file(file_id)
        .await?
        .filter(|file| file.item_id == item_id)
        .ok_or(ApiError::NotFound("file"))?;
    Ok(file)
}

fn validate_href(href: &str) -> Result<(), ApiError> {
    if href.is_empty() || href.len() > MAX_HREF_BYTES || href.trim() != href {
        return Err(ApiError::BadRequest(
            "locator href must be a non-empty relative publication path".to_owned(),
        ));
    }
    if href.chars().any(char::is_control) || href.contains('\\') || href.starts_with('/') {
        return Err(ApiError::BadRequest(
            "locator href must be a normalized relative publication path".to_owned(),
        ));
    }
    let path = href.split(['?', '#']).next().unwrap_or_default();
    let lowercase = path.to_ascii_lowercase();
    if path.is_empty()
        || path.contains(':')
        || lowercase.contains("%2e")
        || lowercase.contains("%2f")
        || lowercase.contains("%5c")
        || path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(ApiError::BadRequest(
            "locator href must be a normalized relative publication path".to_owned(),
        ));
    }
    Ok(())
}

fn validate_locator(locator: &Value) -> Result<String, ApiError> {
    let mut locator = locator.clone();
    let object = locator
        .as_object_mut()
        .ok_or_else(|| ApiError::BadRequest("locator must be a JSON object".to_owned()))?;
    if object.get("version").and_then(Value::as_u64) != Some(1) {
        return Err(ApiError::BadRequest("locator.version must be 1".to_owned()));
    }
    let href = object
        .get("href")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::BadRequest("locator.href must be a string".to_owned()))?;
    validate_href(href)?;

    if let Some(locations) = object.get("locations") {
        let locations = locations.as_object().ok_or_else(|| {
            ApiError::BadRequest("locator.locations must be a JSON object".to_owned())
        })?;
        if locations.contains_key("totalProgression") && locations.contains_key("total_progression")
        {
            return Err(ApiError::BadRequest(
                "locator.locations must not contain both totalProgression and total_progression"
                    .to_owned(),
            ));
        }
        for key in ["progression", "totalProgression"] {
            let value = locations.get(key).or_else(|| {
                (key == "totalProgression")
                    .then(|| locations.get("total_progression"))
                    .flatten()
            });
            if let Some(value) = value {
                let value = value
                    .as_f64()
                    .filter(|value| value.is_finite())
                    .ok_or_else(|| {
                        ApiError::BadRequest(format!("locator.locations.{key} must be a number"))
                    })?;
                if !(0.0..=1.0).contains(&value) {
                    return Err(ApiError::BadRequest(format!(
                        "locator.locations.{key} must be between 0 and 1"
                    )));
                }
            }
        }
    }

    // Apple requests use the API-wide snake-case encoder. Accept that wire
    // spelling, then persist the Readium locator's canonical camel-case key.
    if let Some(locations) = object.get_mut("locations").and_then(Value::as_object_mut) {
        if let Some(total_progression) = locations.remove("total_progression") {
            locations.insert("totalProgression".to_owned(), total_progression);
        }
    }

    let json = serde_json::to_string(&locator)?;
    if json.len() > MAX_LOCATOR_BYTES {
        return Err(ApiError::typed(
            StatusCode::PAYLOAD_TOO_LARGE,
            "locator_too_large",
            format!("locator exceeds the {MAX_LOCATOR_BYTES}-byte limit"),
        ));
    }
    Ok(json)
}

/// GET /api/v1/items/:id/reading-state?file_id=:file
pub async fn get_state(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(item_id): Path<i64>,
    Query(query): Query<ReadingQuery>,
) -> Result<Json<ReadingResponse>, ApiError> {
    let file = book_file(&state, item_id, query.file_id).await?;
    let stored = state
        .store
        .reading_state(user.id, item_id, query.file_id)
        .await?;
    let stale = stored
        .as_ref()
        .is_some_and(|saved| saved.file_size != file.size || saved.file_mtime != file.mtime);
    let state = if stale {
        None
    } else {
        stored.map(ReadingDto::try_from).transpose()?
    };
    Ok(Json(ReadingResponse { state, stale }))
}

/// PUT /api/v1/items/:id/reading-state
pub async fn put_state(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(item_id): Path<i64>,
    Json(request): Json<PutReadingRequest>,
) -> Result<Json<ReadingDto>, ApiError> {
    let file = book_file(&state, item_id, request.file_id).await?;
    if request.revision.size != file.size || request.revision.mtime != file.mtime {
        return Err(ApiError::Conflict(
            "the book file changed; reload it before saving reading state".to_owned(),
        ));
    }
    if !request.progression.is_finite() || !(0.0..=1.0).contains(&request.progression) {
        return Err(ApiError::BadRequest(
            "progression must be a finite number between 0 and 1".to_owned(),
        ));
    }
    let locator_json = validate_locator(&request.locator)?;
    let saved = state
        .store
        .put_reading_state(
            user.id,
            item_id,
            &ReadingStateWrite {
                file_id: file.id,
                file_size: file.size,
                file_mtime: file.mtime,
                locator_json,
                progression_millis: (request.progression * 1_000_000.0).round() as i64,
                completed: request.completed,
                recorded_at: request.recorded_at,
            },
        )
        .await?;
    Ok(Json(ReadingDto::try_from(saved)?))
}

/// DELETE /api/v1/items/:id/reading-state?file_id=:file
pub async fn delete_state(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(item_id): Path<i64>,
    Query(query): Query<ReadingQuery>,
) -> Result<StatusCode, ApiError> {
    book_file(&state, item_id, query.file_id).await?;
    state
        .store
        .delete_reading_state(user.id, item_id, query.file_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn href_must_stay_inside_the_publication() {
        for valid in ["chapter.xhtml", "Text/chapter-2.xhtml#section"] {
            validate_href(valid).unwrap_or_else(|error| panic!("{valid}: {error:?}"));
        }
        for invalid in [
            "../secret",
            "/absolute",
            "https://example.com/chapter",
            "Text//chapter.xhtml",
            "Text/%2e%2e/secret",
            "Text\\chapter.xhtml",
        ] {
            assert!(validate_href(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn apple_snake_case_total_progression_is_stored_canonically() {
        let locator = serde_json::json!({
            "version": 1,
            "href": "chapter.xhtml",
            "locations": {"progression": 0.25, "total_progression": 0.5}
        });
        let canonical = validate_locator(&locator).expect("snake-case locator should be accepted");
        let stored: Value =
            serde_json::from_str(&canonical).expect("validated locator should remain JSON");
        assert_eq!(stored["locations"]["totalProgression"], 0.5);
        assert!(stored["locations"].get("total_progression").is_none());
    }
}
