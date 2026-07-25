//! Hand edits of item metadata — home libraries only.
//!
//! Movie and show items are owned by their provider agent: a hand edit there
//! would be silently clobbered by the next refresh, so this endpoint refuses
//! them until the fix-match UI (REQ-META-5) exists. Home libraries have no
//! agent, so the DB is the only truth there and editing is safe by
//! construction (docs/HOMEVIDEO-PLAN.md §2, §8.2).

use axum::extract::{Path, State};
use axum::Json;
use plurx_core::domain::{ItemEdit, LibraryKind};
use serde::Deserialize;

use super::dto::ItemDto;
use super::error::ApiError;
use super::extract::AdminUser;
use crate::state::AppState;

/// A PATCH body. Every field is doubly optional on the wire: absent means
/// "leave it alone", `null` means "clear it".
#[derive(Deserialize)]
pub struct ItemPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    pub overview: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub recorded_at: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub year: Option<Option<i32>>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

/// serde folds `null` and "absent" into the same `None` unless the field is
/// deserialized through a wrapper like this — and telling them apart is the
/// whole point of a PATCH.
fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    serde::Deserialize::deserialize(deserializer).map(Some)
}

/// A capture date we're willing to store: `YYYY-MM-DD`, optionally with a
/// `THH:MM:SS`. Anything else would break lexicographic sorting, which is the
/// only thing this column is for.
fn valid_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    let date_ok = bytes.len() >= 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[..10]
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit());
    if !date_ok {
        return false;
    }
    match bytes.len() {
        10 => true,
        19 => bytes[10] == b'T' && bytes[13] == b':' && bytes[16] == b':',
        _ => false,
    }
}

/// PATCH /api/v1/items/:id — edit one item's metadata (admin).
pub async fn edit(
    AdminUser(_admin): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(patch): Json<ItemPatch>,
) -> Result<Json<ItemDto>, ApiError> {
    let item = state
        .store
        .get_item(id)
        .await?
        .ok_or(ApiError::NotFound("item"))?;
    let library = state
        .store
        .get_library(item.library_id)
        .await?
        .ok_or(ApiError::NotFound("library"))?;
    if library.kind != LibraryKind::Home {
        return Err(ApiError::BadRequest(
            "metadata editing is available for home libraries only — movies and shows are \
             owned by their metadata agent, and a hand edit would be overwritten by the \
             next refresh"
                .into(),
        ));
    }

    let title = match patch.title {
        Some(title) => {
            let trimmed = title.trim().to_owned();
            if trimmed.is_empty() {
                return Err(ApiError::BadRequest("title cannot be empty".into()));
            }
            Some(trimmed)
        }
        None => None,
    };
    // `null` clears the date; a value has to be one we can sort on.
    if let Some(Some(date)) = patch.recorded_at.as_ref() {
        if !valid_date(date.trim()) {
            return Err(ApiError::BadRequest(
                "recorded_at must be YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS".into(),
            ));
        }
    }
    let recorded_at = patch
        .recorded_at
        .map(|v| v.map(|d| d.trim().to_owned()).filter(|d| !d.is_empty()));
    let tags = patch.tags.map(|tags| {
        let mut cleaned: Vec<String> = Vec::new();
        for tag in tags {
            let tag = tag.trim().to_owned();
            if !tag.is_empty() && !cleaned.iter().any(|t| t.eq_ignore_ascii_case(&tag)) {
                cleaned.push(tag);
            }
        }
        cleaned
    });

    let edit = ItemEdit {
        title,
        overview: patch
            .overview
            .map(|v| v.map(|o| o.trim().to_owned()).filter(|o| !o.is_empty())),
        recorded_at,
        year: patch.year,
        tags,
    };
    if edit.is_empty() {
        return Err(ApiError::BadRequest("no fields to update".into()));
    }

    let updated = state
        .store
        .update_item_fields(id, &edit)
        .await?
        .ok_or(ApiError::NotFound("item"))?;
    Ok(Json(ItemDto::from(updated)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_validated_for_sortability() {
        assert!(valid_date("2019-06-14"));
        assert!(valid_date("2019-06-14T18:22:03"));
        assert!(!valid_date("14/06/2019"));
        assert!(!valid_date("2019-06"));
        assert!(!valid_date("2019-06-14 18:22:03"));
        assert!(!valid_date("last summer"));
        assert!(!valid_date(""));
    }
}
