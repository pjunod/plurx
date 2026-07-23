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
    base: String,
    sessions: Mutex<HashMap<(i64, i64), ScrobbleSession>>,
    pending: Mutex<Option<PendingLink>>,
    syncing: AtomicBool,
    note: Mutex<Option<String>>,
    kick: Notify,
}

impl TraktManager {
    pub fn new(store: Arc<dyn Store>, base: String) -> Self {
        TraktManager {
            store,
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

    /// A live access token for the user, refreshing (and persisting) when
    /// it's close to expiry. `None` = not linked / creds gone / refresh dead.
    async fn access(&self, client: &TraktClient, user_id: i64) -> Option<String> {
        let auth = self.store.get_trakt_auth(user_id).await.ok().flatten()?;
        if auth.expires_at - now_unix() > REFRESH_MARGIN_SECS {
            return Some(auth.access_token);
        }
        match client.refresh(&auth.refresh_token).await {
            Ok(tok) => {
                let _ = self
                    .store
                    .update_trakt_tokens(
                        user_id,
                        &tok.access_token,
                        &tok.refresh_token,
                        tok.expires_at(),
                    )
                    .await;
                Some(tok.access_token)
            }
            Err(TraktError::AuthExpired) => {
                tracing::warn!("trakt: refresh token rejected — unlinking user {user_id}");
                let _ = self.store.delete_trakt_auth(user_id).await;
                *self.note.lock().await = Some("Trakt link expired — connect again".to_owned());
                None
            }
            Err(e) => {
                // Transient (network, 5xx): keep the stale token; a request
                // with it may still succeed, and the next pass retries.
                tracing::warn!("trakt: token refresh failed: {e}");
                Some(auth.access_token)
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
                .scrobble(&access, ScrobbleAction::Start, ident, pct)
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
                .scrobble(&access, ScrobbleAction::Stop, ident, send)
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
                    .scrobble(&access, ScrobbleAction::Pause, ident, pct)
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
                        let auth = TraktAuth {
                            user_id,
                            access_token: tok.access_token.clone(),
                            refresh_token: tok.refresh_token.clone(),
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
            .last_activities(&access)
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

        let remote_watched = client.watched(&access).await.map_err(|e| e.to_string())?;
        let remote_playback = client.playback(&access).await.map_err(|e| e.to_string())?;
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
                .history_add(&access, chunk)
                .await
                .map_err(|e| e.to_string())?;
        }
        if !plan.push_remove.is_empty() {
            client
                .history_remove(&access, &plan.push_remove)
                .await
                .map_err(|e| e.to_string())?;
        }

        // The push just changed remote history — refresh the gate value so the
        // next run doesn't see our own writes as foreign changes.
        let activities = if plan.push_add.is_empty() && plan.push_remove.is_empty() {
            activities
        } else {
            client.last_activities(&access).await.unwrap_or(activities)
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
