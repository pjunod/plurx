//! The Trakt manager: link lifecycle, live scrobbling, and the sync engine.
//!
//! One instance in [`AppState`]. Playback handlers call [`on_start`] /
//! [`on_progress`]; a sweep loop turns abandoned sessions into scrobble
//! pauses; the sync loop runs hourly (or on demand) and reconciles both
//! directions via the pure planner in `plurx_core::trakt`. Everything is
//! fire-and-forget from the request path — a Trakt outage never blocks
//! playback.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use plurx_core::domain::{ItemKind, TraktAuth};
use plurx_core::secrets::{CredentialKey, Secret};
use plurx_core::store::{keys, Store};
use plurx_core::trakt::{
    plan_sync, DevicePoll, Ident, ScrobbleAction, TraktClient, TraktError, REFRESH_MARGIN_SECS,
};
use tokio::sync::{Mutex, Notify};

/// A playback session as far as scrobbling cares: what, how far, and when we
/// last heard from the player.
struct ScrobbleSession {
    ident: Ident,
    pct: f64,
    last_beat: Instant,
    /// A stop was already sent (the watch is recorded) — don't send another.
    stopped: bool,
}

/// An in-flight device-code link attempt.
#[derive(Clone)]
pub struct PendingLink {
    pub user_id: i64,
    pub user_code: String,
    pub verification_url: String,
    pub expires_at: i64,
    pub error: Option<String>,
}

/// What the settings page needs to render the Trakt card.
pub struct TraktStatus {
    pub configured: bool,
    pub auth: Option<TraktAuth>,
    pub syncing: bool,
    pub note: Option<String>,
    pub pending: Option<PendingLink>,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Sessions quiet longer than this become scrobble pauses (progress beats
/// arrive every ~5s while the player is open).
const IDLE_PAUSE: Duration = Duration::from_secs(150);
/// Periodic full sync interval.
const SYNC_EVERY: Duration = Duration::from_secs(3600);

pub struct TraktManager {
    store: Arc<dyn Store>,
    /// Node-local key for the stored bearer pair. Sync is the only subsystem
    /// that unwraps a credential, and it unwraps exactly at the point of an
    /// outbound call — never on the way out of the store.
    key: Arc<CredentialKey>,
    base: String,
    sessions: Mutex<HashMap<(i64, i64), ScrobbleSession>>,
    pending: Mutex<Option<PendingLink>>,
    syncing: AtomicBool,
    note: Mutex<Option<String>>,
    kick: Notify,
}

impl TraktManager {
    pub fn new(store: Arc<dyn Store>, key: Arc<CredentialKey>, base: String) -> Self {
        TraktManager {
            store,
            key,
            base,
            sessions: Mutex::new(HashMap::new()),
            pending: Mutex::new(None),
            syncing: AtomicBool::new(false),
            note: Mutex::new(None),
            kick: Notify::new(),
        }
    }

    /// A client built from the admin's app credentials, if configured.
    pub async fn client(&self) -> Option<TraktClient> {
        let id = self
            .store
            .get_setting(keys::TRAKT_CLIENT_ID)
            .await
            .ok()
            .flatten()?;
        let secret = self
            .store
            .get_setting(keys::TRAKT_CLIENT_SECRET)
            .await
            .ok()
            .flatten()?;
        if id.trim().is_empty() || secret.trim().is_empty() {
            return None;
        }
        Some(TraktClient::new(id.trim(), secret.trim(), &self.base))
    }

    /// Unwrap a stored access token for one outbound call.
    ///
    /// A failure here means the row is sealed under a key this node does not
    /// hold, or was never migrated. Both are operator problems, and both are
    /// reported as "not linked" rather than guessed at: there is no cleartext
    /// to fall back to, which is the entire point of storing an envelope.
    async fn reveal_access(&self, auth: &TraktAuth) -> Option<Secret> {
        match auth.reveal_access_token(&self.key) {
            Ok(secret) => Some(secret),
            Err(error) => {
                tracing::warn!(
                    %error,
                    user_id = auth.user_id,
                    "trakt: stored access token cannot be decrypted with this node's credential key"
                );
                None
            }
        }
    }

    /// A live access token for the user, refreshing (and persisting) when
    /// it's close to expiry. `None` = not linked / creds gone / refresh dead.
    async fn access(&self, client: &TraktClient, user_id: i64) -> Option<Secret> {
        let auth = self.store.get_trakt_auth(user_id).await.ok().flatten()?;
        if auth.expires_at - now_unix() > REFRESH_MARGIN_SECS {
            return self.reveal_access(&auth).await;
        }
        let refresh_token = match auth.reveal_refresh_token(&self.key) {
            Ok(secret) => secret,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "trakt: stored refresh token cannot be decrypted with this node's credential key"
                );
                return None;
            }
        };
        match client.refresh(refresh_token.expose()).await {
            Ok(tok) => {
                // Seal before the store call, not after: `update_trakt_tokens`
                // takes envelopes, so there is no signature that could carry
                // the new bearer pair into a durable row in the clear.
                let (sealed_access, sealed_refresh) = match (
                    self.key.seal_trakt(user_id, &tok.access_token),
                    self.key.seal_trakt(user_id, &tok.refresh_token),
                ) {
                    (Ok(access), Ok(refresh)) => (access, refresh),
                    _ => {
                        tracing::warn!("trakt: could not encrypt the rotated tokens for storage");
                        return None;
                    }
                };
                let updated = self
                    .store
                    .update_trakt_tokens(
                        user_id,
                        // The compare-and-set operand is the envelope we read,
                        // so the race is decided on the exact stored bytes.
                        &auth.refresh_token,
                        &sealed_access,
                        &sealed_refresh,
                        tok.expires_at(),
                    )
                    .await
                    .unwrap_or(false);
                if updated {
                    Some(Secret::from_cleartext(tok.access_token))
                } else {
                    // Another refresh using the same rotating token won the
                    // compare-and-set. Use the winner's credential instead of
                    // returning a token that was deliberately not persisted.
                    let current = self.store.get_trakt_auth(user_id).await.ok().flatten()?;
                    self.reveal_access(&current).await
                }
            }
            Err(TraktError::AuthExpired) => {
                match self
                    .store
                    .delete_trakt_auth_if_current(user_id, &auth.refresh_token)
                    .await
                {
                    Ok(true) => {
                        tracing::warn!("trakt: refresh token rejected — unlinking user {user_id}");
                        *self.note.lock().await =
                            Some("Trakt link expired — connect again".to_owned());
                        None
                    }
                    Ok(false) => {
                        let current = self.store.get_trakt_auth(user_id).await.ok().flatten()?;
                        self.reveal_access(&current).await
                    }
                    Err(error) => {
                        tracing::warn!(%error, "trakt: could not classify rejected refresh token");
                        self.reveal_access(&auth).await
                    }
                }
            }
            Err(e) => {
                // Transient (network, 5xx): keep the stale token; a request
                // with it may still succeed, and the next pass retries.
                tracing::warn!("trakt: token refresh failed: {e}");
                self.reveal_access(&auth).await
            }
        }
    }

    /// Trakt identity for an item: movies by their own TMDB id, episodes by
    /// show TMDB id + season/episode (episode → season → show walk).
    async fn ident_for(&self, item_id: i64) -> Option<Ident> {
        let item = self.store.get_item(item_id).await.ok().flatten()?;
        match item.kind {
            ItemKind::Movie => item.tmdb_id.map(|tmdb| Ident::Movie { tmdb }),
            ItemKind::Episode => {
                let season = item.parent_id?;
                let season = self.store.get_item(season).await.ok().flatten()?;
                let show = self
                    .store
                    .get_item(season.parent_id?)
                    .await
                    .ok()
                    .flatten()?;
                Some(Ident::Episode {
                    show_tmdb: show.tmdb_id?,
                    season: item.season_number?,
                    episode: item.episode_number?,
                })
            }
            _ => None,
        }
    }

    // -- scrobbling hooks (fire-and-forget from playback handlers) ----------

    /// Playback decided → tell Trakt "watching now".
    pub fn on_start(self: &Arc<Self>, user_id: i64, item_id: i64, pct: f64) {
        let mgr = Arc::clone(self);
        tokio::spawn(async move {
            let Some(client) = mgr.client().await else {
                return;
            };
            let Some(access) = mgr.access(&client, user_id).await else {
                return;
            };
            let Some(ident) = mgr.ident_for(item_id).await else {
                return;
            };
            mgr.sessions.lock().await.insert(
                (user_id, item_id),
                ScrobbleSession {
                    ident,
                    pct,
                    last_beat: Instant::now(),
                    stopped: false,
                },
            );
            if let Err(e) = client
                .scrobble(access.expose(), ScrobbleAction::Start, ident, pct)
                .await
            {
                tracing::warn!("trakt: scrobble start failed: {e}");
            }
        });
    }

    /// A progress beat landed. Crossing the watched threshold sends the stop
    /// (which is what makes Trakt record the play).
    pub fn on_progress(self: &Arc<Self>, user_id: i64, item_id: i64, pct: f64, watched: bool) {
        let mgr = Arc::clone(self);
        tokio::spawn(async move {
            let mut sessions = mgr.sessions.lock().await;
            let Some(sess) = sessions.get_mut(&(user_id, item_id)) else {
                return; // not linked or start never fired — nothing to do
            };
            sess.pct = pct;
            sess.last_beat = Instant::now();
            if !watched || sess.stopped {
                return;
            }
            sess.stopped = true;
            let ident = sess.ident;
            drop(sessions);
            let Some(client) = mgr.client().await else {
                return;
            };
            let Some(access) = mgr.access(&client, user_id).await else {
                return;
            };
            // ≥80% is what Trakt counts as a watch; we cross at plurx's own
            // 95% threshold so the two agree and the next sync is a no-op.
            let send = pct.max(95.0);
            if let Err(e) = client
                .scrobble(access.expose(), ScrobbleAction::Stop, ident, send)
                .await
            {
                tracing::warn!("trakt: scrobble stop failed: {e}");
            }
        });
    }

    /// Turn abandoned sessions into pauses (player closed, tab gone).
    pub async fn sweep_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let mut idle = Vec::new();
            {
                let mut sessions = self.sessions.lock().await;
                let ids: Vec<_> = sessions
                    .iter()
                    .filter(|(_, s)| s.last_beat.elapsed() > IDLE_PAUSE)
                    .map(|(k, _)| *k)
                    .collect();
                for k in ids {
                    if let Some(s) = sessions.remove(&k) {
                        if !s.stopped {
                            idle.push((k.0, s.ident, s.pct));
                        }
                    }
                }
            }
            if idle.is_empty() {
                continue;
            }
            let Some(client) = self.client().await else {
                continue;
            };
            for (user_id, ident, pct) in idle {
                let Some(access) = self.access(&client, user_id).await else {
                    continue;
                };
                if let Err(e) = client
                    .scrobble(access.expose(), ScrobbleAction::Pause, ident, pct)
                    .await
                {
                    tracing::warn!("trakt: scrobble pause failed: {e}");
                }
            }
        }
    }

    // -- linking -------------------------------------------------------------

    /// Begin a device-code link for a user. Returns the pending state to show;
    /// a background task polls until approval/denial/expiry.
    pub async fn link_start(self: &Arc<Self>, user_id: i64) -> Result<PendingLink, String> {
        let client = self
            .client()
            .await
            .ok_or("add the Trakt client id + secret first")?;
        let code = client.device_code().await.map_err(|e| e.to_string())?;
        let pending = PendingLink {
            user_id,
            user_code: code.user_code.clone(),
            verification_url: code.verification_url.clone(),
            expires_at: now_unix() + code.expires_in,
            error: None,
        };
        *self.pending.lock().await = Some(pending.clone());

        let mgr = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = code.interval.max(1) as u64;
            let deadline = Instant::now() + Duration::from_secs(code.expires_in.max(60) as u64);
            loop {
                tokio::time::sleep(Duration::from_secs(interval)).await;
                if Instant::now() > deadline {
                    mgr.fail_pending("the code expired before it was entered")
                        .await;
                    return;
                }
                match client.poll_device(&code.device_code).await {
                    Ok(DevicePoll::Ready(tok)) => {
                        let username = client.username(&tok.access_token).await.ok();
                        // A freshly linked account is sealed on the way in, so
                        // the credential is never durable in the clear even for
                        // the moment between linking and the first refresh.
                        let sealed =
                            mgr.key
                                .seal_trakt(user_id, &tok.access_token)
                                .and_then(|access| {
                                    mgr.key
                                        .seal_trakt(user_id, &tok.refresh_token)
                                        .map(|refresh| (access, refresh))
                                });
                        let (access_token, refresh_token) = match sealed {
                            Ok(pair) => pair,
                            Err(error) => {
                                mgr.fail_pending(&format!("securing the link failed: {error}"))
                                    .await;
                                return;
                            }
                        };
                        let auth = TraktAuth {
                            user_id,
                            access_token,
                            refresh_token,
                            expires_at: tok.expires_at(),
                            trakt_username: username.clone(),
                            connected_at: now_unix(),
                            last_sync_at: 0,
                            last_activities: None,
                        };
                        if let Err(e) = mgr.store.put_trakt_auth(&auth).await {
                            mgr.fail_pending(&format!("saving the link failed: {e}"))
                                .await;
                            return;
                        }
                        *mgr.pending.lock().await = None;
                        *mgr.note.lock().await = Some(format!(
                            "linked as {} — first sync starting",
                            username.as_deref().unwrap_or("(unknown)")
                        ));
                        tracing::info!(
                            "trakt: linked user {user_id} as {}",
                            username.as_deref().unwrap_or("(unknown)")
                        );
                        mgr.kick.notify_one(); // full-import backfill now
                        return;
                    }
                    Ok(DevicePoll::Pending) => {}
                    Ok(DevicePoll::SlowDown) => interval += 2,
                    Ok(DevicePoll::Denied) => {
                        mgr.fail_pending("the code was denied on trakt.tv").await;
                        return;
                    }
                    Ok(DevicePoll::Expired) => {
                        mgr.fail_pending("the code expired — start again").await;
                        return;
                    }
                    Err(e) => tracing::warn!("trakt: device poll failed: {e}"),
                }
            }
        });
        Ok(pending)
    }

    async fn fail_pending(&self, why: &str) {
        let mut pending = self.pending.lock().await;
        if let Some(p) = pending.as_mut() {
            p.error = Some(why.to_owned());
        }
        tracing::warn!("trakt: link failed: {why}");
    }

    pub async fn unlink(&self, user_id: i64) -> Result<(), String> {
        self.store
            .delete_trakt_auth(user_id)
            .await
            .map_err(|e| e.to_string())?;
        *self.pending.lock().await = None;
        *self.note.lock().await = None;
        self.sessions.lock().await.retain(|(u, _), _| *u != user_id);
        Ok(())
    }

    pub async fn status(&self, user_id: i64) -> TraktStatus {
        let configured = self.client().await.is_some();
        let auth = self.store.get_trakt_auth(user_id).await.ok().flatten();
        let mut pending = self.pending.lock().await.clone();
        if let Some(p) = &pending {
            // A finished/expired attempt with no error clears itself once the
            // auth row exists; keep errored attempts visible until re-tried.
            if p.user_id != user_id || (auth.is_some() && p.error.is_none()) {
                pending = None;
            }
        }
        TraktStatus {
            configured,
            auth,
            syncing: self.syncing.load(Ordering::Relaxed),
            note: self.note.lock().await.clone(),
            pending,
        }
    }

    /// Ask the sync loop to run now (link completion, the Sync button).
    pub fn request_sync(&self) {
        self.kick.notify_one();
    }

    /// For the activity pill/page.
    pub async fn activity(&self) -> Option<(String, Option<String>)> {
        if self.syncing.load(Ordering::Relaxed) {
            return Some(("Syncing Trakt".to_owned(), None));
        }
        let pending = self.pending.lock().await;
        pending.as_ref().filter(|p| p.error.is_none()).map(|p| {
            (
                "Waiting for Trakt link".to_owned(),
                Some(format!("enter {} at {}", p.user_code, p.verification_url)),
            )
        })
    }

    // -- the sync engine -----------------------------------------------------

    /// Hourly + on-demand loop over every linked account.
    pub async fn sync_loop(self: Arc<Self>) {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(SYNC_EVERY) => {}
                _ = self.kick.notified() => {}
            }
            let linked = match self.store.list_trakt_auth().await {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!("trakt: listing linked accounts failed: {e}");
                    continue;
                }
            };
            for auth in linked {
                if let Err(e) = self.sync_user(auth.user_id).await {
                    tracing::warn!("trakt: sync for user {} failed: {e}", auth.user_id);
                    *self.note.lock().await = Some(format!("sync failed: {e}"));
                }
            }
        }
    }

    pub async fn sync_user(&self, user_id: i64) -> Result<(), String> {
        if self.syncing.swap(true, Ordering::SeqCst) {
            return Ok(()); // one at a time; the loop comes back around
        }
        let result = self.sync_user_inner(user_id).await;
        self.syncing.store(false, Ordering::SeqCst);
        result
    }

    async fn sync_user_inner(&self, user_id: i64) -> Result<(), String> {
        let client = self.client().await.ok_or("not configured")?;
        let access = self.access(&client, user_id).await.ok_or("not linked")?;
        let auth = self
            .store
            .get_trakt_auth(user_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("not linked")?;

        let candidates = self
            .store
            .trakt_sync_candidates(user_id)
            .await
            .map_err(|e| e.to_string())?;

        // Change gate: if Trakt reports the same last_activities as the
        // previous run AND nothing local moved since, skip the heavy pulls.
        let activities = client
            .last_activities(access.expose())
            .await
            .map_err(|e| e.to_string())?;
        let local_dirty = candidates.iter().any(|c| {
            c.watch
                .map(|w| w.updated_at > auth.last_sync_at)
                .unwrap_or(false)
        });
        if Some(activities.as_str()) == auth.last_activities.as_deref()
            && !local_dirty
            && auth.last_sync_at > 0
        {
            tracing::debug!("trakt: nothing changed on either side — skipping");
            return Ok(());
        }

        let remote_watched = client
            .watched(access.expose())
            .await
            .map_err(|e| e.to_string())?;
        let remote_playback = client
            .playback(access.expose())
            .await
            .map_err(|e| e.to_string())?;
        let plan = plan_sync(
            &candidates,
            &remote_watched,
            &remote_playback,
            auth.last_sync_at,
        );

        // Pull side: mark watched with the remote timestamp; land resume points.
        for (item_id, watched_at) in &plan.mark_watched {
            let dur = candidates
                .iter()
                .find(|c| c.item_id == *item_id)
                .and_then(|c| c.watch.and_then(|w| w.duration_ms).or(c.file_duration_ms));
            self.store
                .apply_remote_watch(user_id, *item_id, true, dur.unwrap_or(0), dur, *watched_at)
                .await
                .map_err(|e| e.to_string())?;
        }
        for (item_id, pos, dur, at) in &plan.set_progress {
            self.store
                .apply_remote_watch(user_id, *item_id, false, *pos, Some(*dur), *at)
                .await
                .map_err(|e| e.to_string())?;
        }

        // Push side: batched, additions in chunks (Trakt is fine with large
        // bodies but chunking keeps any one failure small).
        for chunk in plan.push_add.chunks(500) {
            client
                .history_add(access.expose(), chunk)
                .await
                .map_err(|e| e.to_string())?;
        }
        if !plan.push_remove.is_empty() {
            client
                .history_remove(access.expose(), &plan.push_remove)
                .await
                .map_err(|e| e.to_string())?;
        }

        // The push just changed remote history — refresh the gate value so the
        // next run doesn't see our own writes as foreign changes.
        let activities = if plan.push_add.is_empty() && plan.push_remove.is_empty() {
            activities
        } else {
            client
                .last_activities(access.expose())
                .await
                .unwrap_or(activities)
        };
        self.store
            .set_trakt_sync(user_id, now_unix(), Some(&activities))
            .await
            .map_err(|e| e.to_string())?;

        let summary = format!(
            "synced with Trakt: {} watched in, {} resume points in, {} pushed, {} removed",
            plan.mark_watched.len(),
            plan.set_progress.len(),
            plan.push_add.len(),
            plan.push_remove.len()
        );
        tracing::info!("trakt: {summary}");
        *self.note.lock().await = Some(summary);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plurx_core::domain::{LibraryKind, MetadataPatch, NewItem, NewLibrary, ProbeResult};
    use plurx_core::store::SqliteStore;
    use serde_json::{json, Value};

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

    fn fixed(status: u16, body: Value) -> axum::Router {
        use axum::http::StatusCode;
        use axum::Json;
        axum::Router::new().fallback(move || {
            let body = body.clone();
            async move { (StatusCode::from_u16(status).expect("status"), Json(body)) }
        })
    }

    async fn store_with_creds() -> (Arc<dyn Store>, i64) {
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let user = store.create_user("u", "h", true).await.expect("user");
        store
            .put_setting(keys::TRAKT_CLIENT_ID, "cid")
            .await
            .expect("id");
        store
            .put_setting(keys::TRAKT_CLIENT_SECRET, "sec")
            .await
            .expect("secret");
        (store, user.id)
    }

    /// The cleartext behind an `access()` result, for assertions.
    fn revealed(secret: Option<Secret>) -> Option<String> {
        secret.map(|s| s.expose().to_owned())
    }

    /// A fixed key so a fixture row and the manager that opens it agree
    /// without either writing key material to disk.
    fn test_key() -> Arc<CredentialKey> {
        Arc::new(CredentialKey::from_bytes([0x5a; 32]))
    }

    fn linked_auth(user_id: i64, expires_at: i64) -> TraktAuth {
        let key = test_key();
        TraktAuth {
            user_id,
            access_token: key.seal_trakt(user_id, "acc").expect("seal access"),
            refresh_token: key.seal_trakt(user_id, "ref").expect("seal refresh"),
            expires_at,
            trakt_username: Some("neo".into()),
            connected_at: now_unix(),
            last_sync_at: 0,
            last_activities: None,
        }
    }

    #[tokio::test]
    async fn client_present_only_when_configured() {
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let mgr = TraktManager::new(Arc::clone(&store), test_key(), "http://unused".into());
        assert!(mgr.client().await.is_none());
        store
            .put_setting(keys::TRAKT_CLIENT_ID, "cid")
            .await
            .expect("id");
        store
            .put_setting(keys::TRAKT_CLIENT_SECRET, "sec")
            .await
            .expect("secret");
        assert!(mgr.client().await.is_some());
        // Blank credentials do not count as configured.
        store
            .put_setting(keys::TRAKT_CLIENT_SECRET, "   ")
            .await
            .expect("secret");
        assert!(mgr.client().await.is_none());
    }

    #[tokio::test]
    async fn access_keeps_live_and_transiently_stale_tokens() {
        let (store, user) = store_with_creds().await;
        store
            .put_trakt_auth(&linked_auth(user, now_unix() + 999_999))
            .await
            .expect("live auth");
        let mgr = TraktManager::new(Arc::clone(&store), test_key(), "http://unused".into());
        let client = mgr.client().await.expect("client");
        assert_eq!(
            revealed(mgr.access(&client, user).await).as_deref(),
            Some("acc")
        );

        store
            .put_trakt_auth(&linked_auth(user, now_unix() + 100))
            .await
            .expect("expiring auth");
        let base = serve(fixed(503, json!({ "error": "temporary" }))).await;
        let mgr = TraktManager::new(Arc::clone(&store), test_key(), base);
        let client = mgr.client().await.expect("client");
        assert_eq!(
            revealed(mgr.access(&client, user).await).as_deref(),
            Some("acc"),
            "a transient refresh failure keeps the last token available"
        );
        assert!(mgr.access(&client, i64::MAX).await.is_none());
    }

    #[tokio::test]
    async fn episode_identity_requires_its_show_and_numbering() {
        let (store, _) = store_with_creds().await;
        let library = store
            .create_library(&NewLibrary {
                name: "Shows".into(),
                kind: LibraryKind::Shows,
                paths: vec![],
                anime: false,
            })
            .await
            .expect("library");
        let show = store
            .insert_item(&NewItem {
                library_id: library.id,
                kind: ItemKind::Show,
                parent_id: None,
                title: "Severance".into(),
                year: Some(2022),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("show");
        store
            .apply_metadata(
                show,
                &MetadataPatch {
                    tmdb_id: Some(95_457),
                    ..Default::default()
                },
            )
            .await
            .expect("show id");
        let season = store
            .insert_item(&NewItem {
                library_id: library.id,
                kind: ItemKind::Season,
                parent_id: Some(show),
                title: "Season 1".into(),
                year: None,
                season_number: Some(1),
                episode_number: None,
            })
            .await
            .expect("season");
        let episode = store
            .insert_item(&NewItem {
                library_id: library.id,
                kind: ItemKind::Episode,
                parent_id: Some(season),
                title: "Good News About Hell".into(),
                year: None,
                season_number: Some(1),
                episode_number: Some(1),
            })
            .await
            .expect("episode");
        let mgr = TraktManager::new(Arc::clone(&store), test_key(), "http://unused".into());
        assert_eq!(
            mgr.ident_for(episode).await,
            Some(Ident::Episode {
                show_tmdb: 95_457,
                season: 1,
                episode: 1,
            })
        );
        assert!(mgr.ident_for(show).await.is_none());
        assert!(mgr.ident_for(i64::MAX).await.is_none());
    }

    #[tokio::test]
    async fn scrobble_session_starts_updates_and_stops_exactly_once() {
        use axum::routing::post;
        use axum::Json;
        use std::sync::atomic::AtomicUsize;

        let (store, user) = store_with_creds().await;
        store
            .put_trakt_auth(&linked_auth(user, now_unix() + 999_999))
            .await
            .expect("auth");
        let library = store
            .create_library(&NewLibrary {
                name: "Movies".into(),
                kind: LibraryKind::Movies,
                paths: vec![],
                anime: false,
            })
            .await
            .expect("library");
        let movie = store
            .insert_item(&NewItem {
                library_id: library.id,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "Heat".into(),
                year: Some(1995),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");
        store
            .apply_metadata(
                movie,
                &MetadataPatch {
                    tmdb_id: Some(949),
                    ..Default::default()
                },
            )
            .await
            .expect("movie id");

        let starts = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(Mutex::new(Vec::<Value>::new()));
        let start_hits = Arc::clone(&starts);
        let start_bodies = Arc::clone(&bodies);
        let stop_hits = Arc::clone(&stops);
        let stop_bodies = Arc::clone(&bodies);
        let app = axum::Router::new()
            .route(
                "/scrobble/start",
                post(move |Json(body): Json<Value>| {
                    let hits = Arc::clone(&start_hits);
                    let bodies = Arc::clone(&start_bodies);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        bodies.lock().await.push(body);
                        Json(json!({}))
                    }
                }),
            )
            .route(
                "/scrobble/stop",
                post(move |Json(body): Json<Value>| {
                    let hits = Arc::clone(&stop_hits);
                    let bodies = Arc::clone(&stop_bodies);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        bodies.lock().await.push(body);
                        Json(json!({}))
                    }
                }),
            );
        let base = serve(app).await;
        let mgr = Arc::new(TraktManager::new(Arc::clone(&store), test_key(), base));

        mgr.on_start(user, movie, 12.5);
        tokio::time::timeout(Duration::from_secs(2), async {
            while starts.load(Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("start scrobble");
        assert_eq!(
            mgr.sessions
                .lock()
                .await
                .get(&(user, movie))
                .expect("session")
                .pct,
            12.5
        );

        mgr.on_progress(user, movie, 50.0, false);
        tokio::task::yield_now().await;
        assert_eq!(stops.load(Ordering::SeqCst), 0);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let pct = mgr
                    .sessions
                    .lock()
                    .await
                    .get(&(user, movie))
                    .map(|session| session.pct);
                if pct == Some(50.0) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("progress update");

        mgr.on_progress(user, movie, 70.0, true);
        tokio::time::timeout(Duration::from_secs(2), async {
            while stops.load(Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("stop scrobble");
        mgr.on_progress(user, movie, 99.0, true);
        mgr.on_progress(user, i64::MAX, 99.0, true);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            stops.load(Ordering::SeqCst),
            1,
            "a watched session stops once"
        );

        let bodies = bodies.lock().await;
        assert_eq!(bodies[0]["movie"]["ids"]["tmdb"], 949);
        assert_eq!(bodies[0]["progress"], 12.5);
        assert_eq!(
            bodies[1]["progress"], 95.0,
            "plurx's watch threshold satisfies Trakt"
        );
    }

    #[tokio::test]
    async fn syncing_activity_and_unconfigured_link_are_explicit() {
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let mgr = Arc::new(TraktManager::new(store, test_key(), "http://unused".into()));
        let error = match mgr.link_start(1).await {
            Ok(_) => panic!("an unconfigured manager started a link"),
            Err(error) => error,
        };
        assert_eq!(error, "add the Trakt client id + secret first");
        assert!(mgr.activity().await.is_none());
        mgr.syncing.store(true, Ordering::SeqCst);
        assert_eq!(mgr.activity().await, Some(("Syncing Trakt".into(), None)));
        assert!(
            mgr.sync_user(1).await.is_ok(),
            "a concurrent sync is coalesced"
        );
        mgr.syncing.store(false, Ordering::SeqCst);
        assert_eq!(
            mgr.sync_user(1).await.expect_err("not configured"),
            "not configured"
        );
        assert!(!mgr.syncing.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn status_unlink_and_ident() {
        let (store, user) = store_with_creds().await;
        // Seed a movie so ident_for has something to resolve.
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
                title: "Heat".into(),
                year: Some(1995),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");
        store
            .apply_metadata(
                movie,
                &MetadataPatch {
                    tmdb_id: Some(603),
                    ..Default::default()
                },
            )
            .await
            .expect("meta");
        store
            .put_trakt_auth(&linked_auth(user, now_unix() + 999_999))
            .await
            .expect("auth");

        let mgr = TraktManager::new(Arc::clone(&store), test_key(), "http://unused".into());
        let st = mgr.status(user).await;
        assert!(st.configured);
        assert!(st.auth.is_some());
        assert!(!st.syncing);

        assert_eq!(mgr.ident_for(movie).await, Some(Ident::Movie { tmdb: 603 }));

        mgr.unlink(user).await.expect("unlink");
        assert!(store.get_trakt_auth(user).await.expect("get").is_none());
        assert!(mgr.status(user).await.auth.is_none());
    }

    #[tokio::test]
    async fn access_refreshes_a_near_expiry_token() {
        let (store, user) = store_with_creds().await;
        store
            .put_trakt_auth(&linked_auth(user, now_unix() + 100))
            .await
            .expect("auth");
        let base = serve(fixed(
            200,
            json!({
                "access_token": "fresh", "refresh_token": "nr",
                "expires_in": 7200, "created_at": now_unix()
            }),
        ))
        .await;
        let mgr = TraktManager::new(Arc::clone(&store), test_key(), base);
        let client = mgr.client().await.expect("client");
        assert_eq!(
            revealed(mgr.access(&client, user).await).as_deref(),
            Some("fresh")
        );
        // The refreshed token is persisted — sealed, not in the clear.
        let auth = store
            .get_trakt_auth(user)
            .await
            .expect("get")
            .expect("some");
        assert!(
            !auth.access_token.as_stored().contains("fresh"),
            "a rotated token must not land in the durable row as cleartext"
        );
        assert_eq!(
            auth.reveal_access_token(&test_key())
                .expect("open")
                .expose(),
            "fresh"
        );
    }

    #[tokio::test]
    async fn access_unlinks_on_dead_refresh_token() {
        let (store, user) = store_with_creds().await;
        store
            .put_trakt_auth(&linked_auth(user, now_unix() + 100))
            .await
            .expect("auth");
        // 400 on /oauth/token → AuthExpired → the link is dropped.
        let base = serve(fixed(400, json!({}))).await;
        let mgr = TraktManager::new(Arc::clone(&store), test_key(), base);
        let client = mgr.client().await.expect("client");
        assert!(mgr.access(&client, user).await.is_none());
        assert!(store.get_trakt_auth(user).await.expect("get").is_none());
    }

    #[tokio::test]
    async fn link_start_returns_pending_code() {
        let (store, user) = store_with_creds().await;
        let base = serve(fixed(
            200,
            json!({
                "device_code": "dc", "user_code": "WXYZ",
                "verification_url": "https://trakt.tv/activate",
                "expires_in": 600, "interval": 5
            }),
        ))
        .await;
        let mgr = Arc::new(TraktManager::new(Arc::clone(&store), test_key(), base));
        let pending = mgr.link_start(user).await.expect("link");
        assert_eq!(pending.user_code, "WXYZ");
        // The pending attempt shows up in status + activity.
        assert!(mgr.status(user).await.pending.is_some());
        assert!(mgr.activity().await.is_some());
    }

    #[tokio::test]
    async fn approved_device_link_is_saved_and_clears_pending_state() {
        use axum::routing::{get, post};
        use axum::Json;

        let (store, user) = store_with_creds().await;
        let app = axum::Router::new()
            .route(
                "/oauth/device/code",
                post(|| async {
                    Json(json!({
                        "device_code": "approved-code",
                        "user_code": "WXYZ",
                        "verification_url": "https://trakt.tv/activate",
                        "expires_in": 60,
                        "interval": 1
                    }))
                }),
            )
            .route(
                "/oauth/device/token",
                post(|| async {
                    Json(json!({
                        "access_token": "linked-access",
                        "refresh_token": "linked-refresh",
                        "expires_in": 7200,
                        "created_at": now_unix()
                    }))
                }),
            )
            .route(
                "/users/settings",
                get(|| async { Json(json!({ "user": { "username": "neo" } })) }),
            );
        let base = serve(app).await;
        let mgr = Arc::new(TraktManager::new(Arc::clone(&store), test_key(), base));

        mgr.link_start(user).await.expect("device link");
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if store
                    .get_trakt_auth(user)
                    .await
                    .expect("read auth")
                    .is_some()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("approved link persisted");

        let status = mgr.status(user).await;
        let auth = status.auth.expect("linked auth");
        assert!(
            !auth.access_token.as_stored().contains("linked-access")
                && !auth.refresh_token.as_stored().contains("linked-refresh"),
            "a freshly linked account must be sealed before it is stored"
        );
        assert_eq!(
            auth.reveal_access_token(&test_key())
                .expect("open access")
                .expose(),
            "linked-access"
        );
        assert_eq!(
            auth.reveal_refresh_token(&test_key())
                .expect("open refresh")
                .expose(),
            "linked-refresh"
        );
        assert_eq!(auth.trakt_username.as_deref(), Some("neo"));
        assert!(status.pending.is_none());
        assert_eq!(
            status.note.as_deref(),
            Some("linked as neo — first sync starting")
        );
    }

    #[tokio::test]
    async fn denied_device_link_remains_visible_as_an_error() {
        use axum::http::StatusCode;
        use axum::routing::post;
        use axum::Json;

        let (store, user) = store_with_creds().await;
        let app = axum::Router::new()
            .route(
                "/oauth/device/code",
                post(|| async {
                    Json(json!({
                        "device_code": "denied-code",
                        "user_code": "NOPE",
                        "verification_url": "https://trakt.tv/activate",
                        "expires_in": 60,
                        "interval": 1
                    }))
                }),
            )
            .route(
                "/oauth/device/token",
                post(|| async { StatusCode::IM_A_TEAPOT }),
            );
        let base = serve(app).await;
        let mgr = Arc::new(TraktManager::new(Arc::clone(&store), test_key(), base));

        mgr.link_start(user).await.expect("device link");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if mgr
                    .status(user)
                    .await
                    .pending
                    .as_ref()
                    .and_then(|pending| pending.error.as_deref())
                    .is_some()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("denial projected into pending status");

        let status = mgr.status(user).await;
        assert_eq!(
            status.pending.and_then(|pending| pending.error),
            Some("the code was denied on trakt.tv".into())
        );
        assert!(store
            .get_trakt_auth(user)
            .await
            .expect("read auth")
            .is_none());
        assert!(mgr.activity().await.is_none());
    }

    #[tokio::test]
    async fn sync_user_pulls_a_remote_watch_into_the_store() {
        let (store, user) = store_with_creds().await;
        store
            .put_trakt_auth(&linked_auth(user, now_unix() + 999_999))
            .await
            .expect("auth");
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
                title: "Heat".into(),
                year: Some(1995),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");
        store
            .apply_metadata(
                movie,
                &MetadataPatch {
                    tmdb_id: Some(603),
                    ..Default::default()
                },
            )
            .await
            .expect("meta");
        store
            .upsert_file(
                movie,
                "/m/Heat.mkv",
                1,
                1,
                &ProbeResult {
                    duration_ms: Some(6_000_000),
                    ..Default::default()
                },
            )
            .await
            .expect("file");

        use axum::routing::{get, post};
        use axum::Json;
        let app = axum::Router::new()
            .route(
                "/sync/last_activities",
                get(|| async {
                    Json(json!({ "movies": { "watched_at": "2026-01-01T00:00:00Z" } }))
                }),
            )
            .route(
                "/sync/watched/movies",
                get(|| async {
                    Json(json!([{
                        "last_watched_at": "2026-01-01T00:00:00.000Z",
                        "movie": { "ids": { "tmdb": 603 } }
                    }]))
                }),
            )
            .route("/sync/watched/shows", get(|| async { Json(json!([])) }))
            .route("/sync/playback", get(|| async { Json(json!([])) }))
            .route("/sync/history", post(|| async { Json(json!({})) }))
            .route("/sync/history/remove", post(|| async { Json(json!({})) }));
        let base = serve(app).await;
        let mgr = TraktManager::new(Arc::clone(&store), test_key(), base);

        mgr.sync_user(user).await.expect("sync");
        // The remote watch landed locally.
        let ws = store
            .watch_state(user, movie)
            .await
            .expect("ws")
            .expect("row");
        assert!(ws.watched);
        // Sync bookkeeping advanced.
        let auth = store
            .get_trakt_auth(user)
            .await
            .expect("get")
            .expect("some");
        assert!(auth.last_sync_at > 0);
    }

    #[tokio::test]
    async fn sync_pushes_a_local_only_watch() {
        use std::sync::atomic::AtomicUsize;
        let (store, user) = store_with_creds().await;
        store
            .put_trakt_auth(&linked_auth(user, now_unix() + 999_999))
            .await
            .expect("auth");
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
                title: "Local".into(),
                year: Some(2001),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");
        store
            .apply_metadata(
                movie,
                &MetadataPatch {
                    tmdb_id: Some(604),
                    ..Default::default()
                },
            )
            .await
            .expect("meta");
        // Watched locally, absent remotely → must be pushed to Trakt history.
        store.set_watched(user, movie, true).await.expect("watch");

        let history_hits = Arc::new(AtomicUsize::new(0));
        let hits = Arc::clone(&history_hits);
        use axum::routing::{get, post};
        use axum::Json;
        let app = axum::Router::new()
            .route(
                "/sync/last_activities",
                get(|| async { Json(json!({ "episodes": "x" })) }),
            )
            .route("/sync/watched/movies", get(|| async { Json(json!([])) }))
            .route("/sync/watched/shows", get(|| async { Json(json!([])) }))
            .route("/sync/playback", get(|| async { Json(json!([])) }))
            .route(
                "/sync/history",
                post(move || {
                    let hits = Arc::clone(&hits);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(json!({ "added": { "movies": 1 } }))
                    }
                }),
            );
        let base = serve(app).await;
        let mgr = TraktManager::new(Arc::clone(&store), test_key(), base);
        mgr.sync_user(user).await.expect("sync");
        assert_eq!(
            history_hits.load(Ordering::SeqCst),
            1,
            "history push happened"
        );
    }

    #[tokio::test]
    async fn sync_skips_when_nothing_changed() {
        use std::sync::atomic::AtomicUsize;
        let (store, user) = store_with_creds().await;
        // Gate value the mock will echo back, and a prior successful sync.
        let gate = json!({ "all": "same" }).to_string();
        let mut auth = linked_auth(user, now_unix() + 999_999);
        auth.last_sync_at = now_unix();
        auth.last_activities = Some(gate);
        store.put_trakt_auth(&auth).await.expect("auth");

        let watched_hits = Arc::new(AtomicUsize::new(0));
        let hits = Arc::clone(&watched_hits);
        use axum::routing::get;
        use axum::Json;
        let app = axum::Router::new()
            .route(
                "/sync/last_activities",
                get(|| async { Json(json!({ "all": "same" })) }),
            )
            .route(
                "/sync/watched/movies",
                get(move || {
                    let hits = Arc::clone(&hits);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(json!([]))
                    }
                }),
            );
        let base = serve(app).await;
        let mgr = TraktManager::new(Arc::clone(&store), test_key(), base);
        mgr.sync_user(user).await.expect("sync");
        // The change gate short-circuits before any heavy pull.
        assert_eq!(watched_hits.load(Ordering::SeqCst), 0, "pull was skipped");
    }
}
