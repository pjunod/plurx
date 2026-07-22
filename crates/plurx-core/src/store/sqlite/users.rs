//! Users and login tokens.

use async_trait::async_trait;
use rusqlite::{params, OptionalExtension};

use super::{user_from_row, SqliteStore, USER_COLS};
use crate::domain::User;
use crate::error::StoreError;
use crate::store::UserStore;

#[async_trait]
impl UserStore for SqliteStore {
    async fn count_users(&self) -> Result<i64, StoreError> {
        self.with_conn(|conn| {
            Ok(conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?)
        })
        .await
    }

    async fn create_user(
        &self,
        username: &str,
        password_hash: &str,
        is_admin: bool,
    ) -> Result<User, StoreError> {
        let username = username.to_owned();
        let password_hash = password_hash.to_owned();
        self.with_conn(move |conn| {
            let user = conn.query_row(
                &format!(
                    "INSERT INTO users (username, password_hash, is_admin)
                     VALUES (?1, ?2, ?3) RETURNING {USER_COLS}"
                ),
                params![username, password_hash, is_admin as i64],
                user_from_row,
            )?;
            Ok(user)
        })
        .await
    }

    async fn get_user(&self, id: i64) -> Result<Option<User>, StoreError> {
        self.with_conn(move |conn| {
            Ok(conn
                .query_row(
                    &format!("SELECT {USER_COLS} FROM users WHERE id = ?1"),
                    params![id],
                    user_from_row,
                )
                .optional()?)
        })
        .await
    }

    async fn get_user_by_username(&self, username: &str) -> Result<Option<User>, StoreError> {
        let username = username.to_owned();
        self.with_conn(move |conn| {
            Ok(conn
                .query_row(
                    &format!("SELECT {USER_COLS} FROM users WHERE username = ?1"),
                    params![username],
                    user_from_row,
                )
                .optional()?)
        })
        .await
    }

    async fn list_users(&self) -> Result<Vec<User>, StoreError> {
        self.with_conn(|conn| {
            let mut stmt =
                conn.prepare(&format!("SELECT {USER_COLS} FROM users ORDER BY username"))?;
            let users = stmt
                .query_map([], user_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(users)
        })
        .await
    }

    async fn delete_user(&self, id: i64) -> Result<bool, StoreError> {
        self.with_conn(move |conn| {
            Ok(conn.execute("DELETE FROM users WHERE id = ?1", params![id])? > 0)
        })
        .await
    }

    async fn count_admins(&self) -> Result<i64, StoreError> {
        self.with_conn(|conn| {
            Ok(
                conn.query_row("SELECT COUNT(*) FROM users WHERE is_admin = 1", [], |row| {
                    row.get(0)
                })?,
            )
        })
        .await
    }

    async fn set_password(&self, id: i64, password_hash: &str) -> Result<bool, StoreError> {
        let password_hash = password_hash.to_owned();
        self.with_conn(move |conn| {
            Ok(conn.execute(
                "UPDATE users SET password_hash = ?2 WHERE id = ?1",
                params![id, password_hash],
            )? > 0)
        })
        .await
    }

    async fn set_admin(&self, id: i64, is_admin: bool) -> Result<bool, StoreError> {
        self.with_conn(move |conn| {
            Ok(conn.execute(
                "UPDATE users SET is_admin = ?2 WHERE id = ?1",
                params![id, is_admin as i64],
            )? > 0)
        })
        .await
    }

    async fn delete_tokens_for_user(&self, user_id: i64) -> Result<u64, StoreError> {
        self.with_conn(move |conn| {
            Ok(conn.execute("DELETE FROM tokens WHERE user_id = ?1", params![user_id])? as u64)
        })
        .await
    }

    async fn create_token(
        &self,
        token_hash: &str,
        user_id: i64,
        device: Option<&str>,
    ) -> Result<(), StoreError> {
        let token_hash = token_hash.to_owned();
        let device = device.map(str::to_owned);
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO tokens (token_hash, user_id, device) VALUES (?1, ?2, ?3)",
                params![token_hash, user_id, device],
            )?;
            Ok(())
        })
        .await
    }

    async fn user_for_token(&self, token_hash: &str) -> Result<Option<User>, StoreError> {
        let token_hash = token_hash.to_owned();
        self.with_conn(move |conn| {
            let user = conn
                .query_row(
                    &format!(
                        "SELECT {cols} FROM users u
                         JOIN tokens t ON t.user_id = u.id
                         WHERE t.token_hash = ?1",
                        cols = "u.id, u.username, u.password_hash, u.is_admin, u.created_at"
                    ),
                    params![token_hash],
                    user_from_row,
                )
                .optional()?;
            if user.is_some() {
                // Touch at most once a minute to keep write volume trivial.
                conn.execute(
                    "UPDATE tokens SET last_seen_at = unixepoch()
                     WHERE token_hash = ?1 AND last_seen_at < unixepoch() - 60",
                    params![token_hash],
                )?;
            }
            Ok(user)
        })
        .await
    }

    async fn delete_token(&self, token_hash: &str) -> Result<bool, StoreError> {
        let token_hash = token_hash.to_owned();
        self.with_conn(move |conn| {
            Ok(conn.execute(
                "DELETE FROM tokens WHERE token_hash = ?1",
                params![token_hash],
            )? > 0)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use crate::store::{SqliteStore, UserStore};

    #[tokio::test]
    async fn user_and_token_lifecycle() {
        let store = SqliteStore::open_in_memory().expect("open");
        assert_eq!(store.count_users().await.expect("count"), 0);

        let user = store
            .create_user("paul", "hash", true)
            .await
            .expect("create");
        assert!(user.is_admin);
        assert_eq!(store.count_users().await.expect("count"), 1);

        // Username lookup is case-insensitive.
        let found = store
            .get_user_by_username("PAUL")
            .await
            .expect("lookup")
            .expect("present");
        assert_eq!(found.id, user.id);

        store
            .create_token("th_abc", user.id, Some("test"))
            .await
            .expect("token");
        let via_token = store
            .user_for_token("th_abc")
            .await
            .expect("resolve")
            .expect("present");
        assert_eq!(via_token.id, user.id);
        assert!(store.delete_token("th_abc").await.expect("del"));
        assert!(store
            .user_for_token("th_abc")
            .await
            .expect("resolve")
            .is_none());

        // Deleting the user cascades to tokens.
        store
            .create_token("th_2", user.id, None)
            .await
            .expect("token");
        assert!(store.delete_user(user.id).await.expect("del user"));
        assert!(store
            .user_for_token("th_2")
            .await
            .expect("resolve")
            .is_none());
    }
}
