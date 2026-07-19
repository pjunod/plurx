//! Library CRUD.

use std::path::PathBuf;

use async_trait::async_trait;
use rusqlite::{params, OptionalExtension, Row};

use super::{conversion_err, SqliteStore};
use crate::domain::{Library, LibraryKind, NewLibrary};
use crate::error::StoreError;
use crate::store::LibraryStore;

const LIB_COLS: &str = "id, name, kind, paths, anime, created_at";

fn library_from_row(row: &Row<'_>) -> rusqlite::Result<Library> {
    let kind_raw: String = row.get(2)?;
    let kind = LibraryKind::parse(&kind_raw)
        .ok_or_else(|| conversion_err(2, format!("unknown library kind `{kind_raw}`")))?;
    let paths_json: String = row.get(3)?;
    let paths: Vec<PathBuf> = serde_json::from_str(&paths_json)
        .map_err(|e| conversion_err(3, format!("library paths: {e}")))?;
    Ok(Library {
        id: row.get(0)?,
        name: row.get(1)?,
        kind,
        paths,
        anime: row.get::<_, i64>(4)? != 0,
        created_at: row.get(5)?,
    })
}

fn paths_json(paths: &[PathBuf]) -> Result<String, StoreError> {
    serde_json::to_string(paths).map_err(|e| StoreError::Database(e.to_string()))
}

#[async_trait]
impl LibraryStore for SqliteStore {
    async fn create_library(&self, library: &NewLibrary) -> Result<Library, StoreError> {
        let name = library.name.clone();
        let kind = library.kind.as_str();
        let paths = paths_json(&library.paths)?;
        let anime = library.anime as i64;
        self.with_conn(move |conn| {
            Ok(conn.query_row(
                &format!(
                    "INSERT INTO libraries (name, kind, paths, anime)
                     VALUES (?1, ?2, ?3, ?4) RETURNING {LIB_COLS}"
                ),
                params![name, kind, paths, anime],
                library_from_row,
            )?)
        })
        .await
    }

    async fn update_library(
        &self,
        id: i64,
        library: &NewLibrary,
    ) -> Result<Option<Library>, StoreError> {
        let name = library.name.clone();
        let kind = library.kind.as_str();
        let paths = paths_json(&library.paths)?;
        let anime = library.anime as i64;
        self.with_conn(move |conn| {
            Ok(conn
                .query_row(
                    &format!(
                        "UPDATE libraries SET name = ?2, kind = ?3, paths = ?4, anime = ?5
                         WHERE id = ?1 RETURNING {LIB_COLS}"
                    ),
                    params![id, name, kind, paths, anime],
                    library_from_row,
                )
                .optional()?)
        })
        .await
    }

    async fn delete_library(&self, id: i64) -> Result<bool, StoreError> {
        self.with_conn(move |conn| {
            Ok(conn.execute("DELETE FROM libraries WHERE id = ?1", params![id])? > 0)
        })
        .await
    }

    async fn get_library(&self, id: i64) -> Result<Option<Library>, StoreError> {
        self.with_conn(move |conn| {
            Ok(conn
                .query_row(
                    &format!("SELECT {LIB_COLS} FROM libraries WHERE id = ?1"),
                    params![id],
                    library_from_row,
                )
                .optional()?)
        })
        .await
    }

    async fn list_libraries(&self) -> Result<Vec<Library>, StoreError> {
        self.with_conn(|conn| {
            let mut stmt =
                conn.prepare(&format!("SELECT {LIB_COLS} FROM libraries ORDER BY name"))?;
            let libraries = stmt
                .query_map([], library_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(libraries)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::domain::{LibraryKind, NewLibrary};
    use crate::store::{LibraryStore, SqliteStore};

    #[tokio::test]
    async fn library_crud_roundtrip() {
        let store = SqliteStore::open_in_memory().expect("open");
        let lib = store
            .create_library(&NewLibrary {
                name: "Movies".into(),
                kind: LibraryKind::Movies,
                paths: vec![PathBuf::from("/media/movies")],
                anime: false,
            })
            .await
            .expect("create");
        assert_eq!(lib.kind, LibraryKind::Movies);
        assert_eq!(lib.paths, vec![PathBuf::from("/media/movies")]);

        let updated = store
            .update_library(
                lib.id,
                &NewLibrary {
                    name: "Films".into(),
                    kind: LibraryKind::Movies,
                    paths: vec![PathBuf::from("/media/movies"), PathBuf::from("/mnt/more")],
                    anime: false,
                },
            )
            .await
            .expect("update")
            .expect("present");
        assert_eq!(updated.name, "Films");
        assert_eq!(updated.paths.len(), 2);

        assert_eq!(store.list_libraries().await.expect("list").len(), 1);
        assert!(store.delete_library(lib.id).await.expect("delete"));
        assert!(store.get_library(lib.id).await.expect("get").is_none());
    }
}
