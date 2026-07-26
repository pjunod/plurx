//! Scoped API keys — the machine credential.

use async_trait::async_trait;
use rusqlite::{params, OptionalExtension};

use super::SqliteStore;
use crate::domain::ApiKey;
use crate::error::StoreError;
use crate::store::ApiKeyStore;

const KEY_COLS: &str = "id, name, key_hash, scopes, created_at, last_used_at, disabled";

fn key_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApiKey> {
    let scopes: String = row.get(3)?;
    Ok(ApiKey {
        id: row.get(0)?,
        name: row.get(1)?,
        key_hash: row.get(2)?,
        // A key whose scope list somehow failed to parse authorizes
        // NOTHING. The alternative — treating unreadable as unrestricted —
        // turns one corrupt row into a credential that can do anything.
        scopes: serde_json::from_str(&scopes).unwrap_or_default(),
        created_at: row.get(4)?,
        last_used_at: row.get(5)?,
        disabled: row.get::<_, i64>(6)? != 0,
    })
}

#[async_trait]
impl ApiKeyStore for SqliteStore {
    async fn create_api_key(
        &self,
        name: &str,
        key_hash: &str,
        scopes: &[String],
    ) -> Result<ApiKey, StoreError> {
        let (name, key_hash) = (name.to_owned(), key_hash.to_owned());
        let scopes = serde_json::to_string(scopes).unwrap_or_else(|_| "[]".to_owned());
        self.with_conn(move |conn| {
            let key = conn.query_row(
                &format!(
                    "INSERT INTO api_keys (name, key_hash, scopes)
                     VALUES (?1, ?2, ?3) RETURNING {KEY_COLS}"
                ),
                params![name, key_hash, scopes],
                key_from_row,
            )?;
            Ok(key)
        })
        .await
    }

    async fn list_api_keys(&self) -> Result<Vec<ApiKey>, StoreError> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {KEY_COLS} FROM api_keys ORDER BY created_at"
            ))?;
            let rows = stmt.query_map([], key_from_row)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
    }

    async fn api_key_for_hash(&self, key_hash: &str) -> Result<Option<ApiKey>, StoreError> {
        let key_hash = key_hash.to_owned();
        self.with_conn(move |conn| {
            Ok(conn
                .query_row(
                    &format!("SELECT {KEY_COLS} FROM api_keys WHERE key_hash = ?1"),
                    params![key_hash],
                    key_from_row,
                )
                .optional()?)
        })
        .await
    }

    async fn touch_api_key(&self, id: i64) -> Result<(), StoreError> {
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE api_keys SET last_used_at = unixepoch() WHERE id = ?1",
                params![id],
            )?;
            Ok(())
        })
        .await
    }

    async fn delete_api_key(&self, id: i64) -> Result<bool, StoreError> {
        self.with_conn(move |conn| {
            Ok(conn.execute("DELETE FROM api_keys WHERE id = ?1", params![id])? > 0)
        })
        .await
    }

    async fn set_api_key_disabled(&self, id: i64, disabled: bool) -> Result<bool, StoreError> {
        self.with_conn(move |conn| {
            Ok(conn.execute(
                "UPDATE api_keys SET disabled = ?2 WHERE id = ?1",
                params![id, i64::from(disabled)],
            )? > 0)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use crate::auth;
    use crate::domain::scopes;
    use crate::store::{ApiKeyStore, SqliteStore};

    fn store() -> SqliteStore {
        SqliteStore::open_in_memory().expect("store")
    }

    #[tokio::test]
    async fn a_key_round_trips_with_its_scopes_and_never_its_secret() {
        let s = store();
        let secret = auth::generate_api_key().expect("key");
        assert!(secret.starts_with("plx_"), "secret = {secret}");

        let created = s
            .create_api_key(
                "monarr",
                &auth::hash_token(&secret),
                &[scopes::SCAN_TRIGGER.into()],
            )
            .await
            .expect("create");
        assert_eq!(created.scopes, vec![scopes::SCAN_TRIGGER.to_owned()]);
        assert!(!created.disabled);

        // Lookup is by hash: the plaintext exists nowhere in the database,
        // so a database leak is not a credential leak.
        let listed = s.list_api_keys().await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_ne!(listed[0].key_hash, secret);
        let found = s
            .api_key_for_hash(&auth::hash_token(&secret))
            .await
            .expect("lookup")
            .expect("some");
        assert_eq!(found.id, created.id);
        assert!(s
            .api_key_for_hash(&auth::hash_token("plx_wrong"))
            .await
            .expect("lookup")
            .is_none());
    }

    #[tokio::test]
    async fn a_key_only_allows_the_scopes_it_was_given() {
        let s = store();
        let key = s
            .create_api_key("monarr", "h", &[scopes::SCAN_TRIGGER.into()])
            .await
            .expect("create");
        assert!(key.allows(scopes::SCAN_TRIGGER));
        assert!(
            !key.allows(scopes::STATUS_READ),
            "a key must not hold a scope it was not created with"
        );
        assert!(
            !key.allows("settings:read"),
            "and certainly not one that does not exist"
        );
    }

    // Revocation has to be enforced in one place. A disabled key that still
    // passes because a call site forgot to check is not revoked.
    #[tokio::test]
    async fn disabling_a_key_takes_every_scope_away_at_once() {
        let s = store();
        let key = s
            .create_api_key(
                "monarr",
                "h",
                &[scopes::SCAN_TRIGGER.into(), scopes::STATUS_READ.into()],
            )
            .await
            .expect("create");
        assert!(s.set_api_key_disabled(key.id, true).await.expect("disable"));

        let key = s
            .api_key_for_hash("h")
            .await
            .expect("lookup")
            .expect("some");
        assert!(key.disabled);
        for scope in scopes::ALL {
            assert!(!key.allows(scope), "disabled key still allows {scope}");
        }

        assert!(s.delete_api_key(key.id).await.expect("delete"));
        assert!(s.api_key_for_hash("h").await.expect("lookup").is_none());
    }

    #[tokio::test]
    async fn last_used_is_recorded_so_a_forgotten_key_is_visible() {
        let s = store();
        let key = s
            .create_api_key("monarr", "h", &[scopes::SCAN_TRIGGER.into()])
            .await
            .expect("create");
        assert!(key.last_used_at.is_none(), "never used yet");
        s.touch_api_key(key.id).await.expect("touch");
        let key = s
            .api_key_for_hash("h")
            .await
            .expect("lookup")
            .expect("some");
        assert!(
            key.last_used_at.is_some(),
            "a key nobody can tell is unused is a key nobody revokes"
        );
    }
}
