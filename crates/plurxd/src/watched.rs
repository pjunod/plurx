//! Pushing watch state to monarr (master plan §11.1).
//!
//! The only thing plurx tells another application to do. Everything else in
//! the integration is inbound, or a read; this is a push, and pushes are
//! where the interesting failure lives: the moment worth reporting is often
//! the moment the far side is restarting, and there is no caller waiting to
//! retry on our behalf. So the queue is a table (`watched_outbox`), and the
//! backoff is a column.
//!
//! **Off by default, and per-user by decision.** The plan's sketch was an
//! aggregate any-user signal with no usernames; Paul chose per-user, because
//! that is the only shape that can later mean "everyone who could has
//! watched it". The cost is real — viewing history is personal, and this
//! copies it into an application with no other reason to hold it — so
//! nothing is enqueued unless an admin turned `monarr.watched_sync` on, and
//! the settings page says in as many words what it sends.

use std::sync::Arc;
use std::time::Duration;

use plurx_core::domain::ItemKind;
use plurx_core::store::{keys, OutboxEntry, Store};
use serde::{Deserialize, Serialize};

/// Attempt schedule. Long enough to ride out a monarr restart, short enough
/// that a genuinely dead one is marked failed while somebody could still act
/// on it. Same shape as monarr's own delivery queue, deliberately: two
/// applications retrying on wildly different clocks is a thing nobody can
/// reason about at 1 a.m.
const BACKOFF: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(30),
    Duration::from_secs(120),
];

/// How many to drain per tick. A backlog clears over several passes rather
/// than opening fifty connections to a monarr that has just come back up and
/// is, by definition, least able to take them.
const BATCH: i64 = 20;

/// What monarr receives. Field names are the contract (master plan §11.1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WatchedEvent {
    /// Always `"watched"` today; present so monarr's webhook can grow other
    /// events without a second route.
    pub event: String,
    /// `movie` | `episode`.
    pub kind: String,
    /// For an episode this is the SHOW's id — an episode's own id is not
    /// what identifies the series it belongs to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmdb: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imdb: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub season: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode: Option<i32>,
    /// Unix seconds.
    pub watched_at: i64,
    /// Who watched it. Per the §11.1 decision; the reason this whole feature
    /// is opt-in.
    pub user: String,
}

/// Builds and delivers watched notifications.
pub struct WatchedNotifier {
    store: Arc<dyn Store>,
}

impl WatchedNotifier {
    pub fn new(store: Arc<dyn Store>) -> Arc<Self> {
        Arc::new(Self { store })
    }

    /// Queue a watched notification for one item, if the feature is on.
    ///
    /// Deliberately swallowing its own errors: this is called from the
    /// playback path, and a monarr that is unreachable — or a settings read
    /// that failed — must never make marking something watched fail. The
    /// worst outcome here is a notification that does not happen.
    pub async fn on_watched(self: &Arc<Self>, user_id: i64, item_id: i64) {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(e) = me.enqueue(user_id, item_id).await {
                tracing::warn!(
                    target: "plurxd::integrate",
                    item = item_id, error = %e,
                    "could not queue a watched notification"
                );
            }
        });
    }

    async fn enqueue(&self, user_id: i64, item_id: i64) -> Result<(), String> {
        if !self.enabled().await {
            return Ok(());
        }
        let Some(event) = self.build(user_id, item_id).await? else {
            return Ok(());
        };
        let payload = serde_json::to_string(&event).map_err(|e| e.to_string())?;
        self.store
            .enqueue_watched(&payload)
            .await
            .map_err(|e| e.to_string())?;
        tracing::info!(
            target: "plurxd::integrate",
            item = item_id, kind = %event.kind, user = %event.user,
            "queued a watched notification for monarr"
        );
        Ok(())
    }

    async fn enabled(&self) -> bool {
        let on = self
            .store
            .get_setting(keys::MONARR_WATCHED_SYNC)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        on == "1"
    }

    /// Assemble the event, or `None` when there is nothing monarr could act
    /// on.
    ///
    /// An item with no TMDB id is skipped rather than sent: monarr matches on
    /// ids, and a title alone would make it guess — which is the mistake this
    /// whole integration exists to stop, just pointed the other way.
    async fn build(&self, user_id: i64, item_id: i64) -> Result<Option<WatchedEvent>, String> {
        let store = self.store.as_ref();
        let Some(item) = store.get_item(item_id).await.map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        let Some(user) = store.get_user(user_id).await.map_err(|e| e.to_string())? else {
            return Ok(None);
        };

        let (kind, tmdb, imdb, season, episode) = match item.kind {
            ItemKind::Movie => ("movie", item.tmdb_id, item.imdb_id.clone(), None, None),
            ItemKind::Episode => {
                // Walk to the show: the ids that identify a series live on
                // the show row, never on the episode.
                let show = show_of(store, &item).await.map_err(|e| e.to_string())?;
                let (tmdb, imdb) = match show {
                    Some(s) => (s.tmdb_id, s.imdb_id.clone()),
                    None => (None, None),
                };
                (
                    "episode",
                    tmdb,
                    imdb,
                    item.season_number,
                    item.episode_number,
                )
            }
            // Shows, seasons, folders, photos: nothing monarr tracks.
            _ => return Ok(None),
        };
        if tmdb.is_none() && imdb.is_none() {
            tracing::debug!(
                target: "plurxd::integrate",
                item = item_id,
                "not notifying: the item has no ids, and monarr matches on ids"
            );
            return Ok(None);
        }
        Ok(Some(WatchedEvent {
            event: "watched".to_owned(),
            kind: kind.to_owned(),
            tmdb,
            imdb,
            season,
            episode,
            watched_at: now(),
            user: user.username,
        }))
    }

    /// Drain the outbox until `ctx` ends. Run in a task beside the others.
    pub async fn run(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            self.deliver_due().await;
        }
    }

    async fn deliver_due(&self) {
        let due = match self.store.due_watched(BATCH).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(target: "plurxd::integrate", error = %e, "reading the watched outbox");
                return;
            }
        };
        if due.is_empty() {
            return;
        }
        let url = self
            .store
            .get_setting(keys::MONARR_URL)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let key = self
            .store
            .get_setting(keys::MONARR_API_KEY)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        for entry in due {
            self.attempt(entry, &url, &key).await;
        }
    }

    async fn attempt(&self, mut entry: OutboxEntry, url: &str, key: &str) {
        let err = if url.is_empty() || key.is_empty() {
            // Unpaired while rows are waiting. Permanent: nothing about
            // trying again fixes a URL that was never set, and leaving rows
            // pending forever would make the outbox look busy.
            Some(("monarr is not configured".to_owned(), true))
        } else {
            match post(url, key, &entry.payload).await {
                Ok(()) => None,
                Err((msg, permanent)) => Some((msg, permanent)),
            }
        };

        entry.attempts += 1;
        match err {
            None => {
                entry.status = "ok".to_owned();
                entry.last_error.clear();
                entry.next_at = 0;
            }
            Some((msg, permanent)) if permanent || entry.attempts as usize > BACKOFF.len() => {
                entry.status = "failed".to_owned();
                entry.last_error = msg.clone();
                entry.next_at = 0;
                tracing::warn!(
                    target: "plurxd::integrate",
                    attempts = entry.attempts, error = %msg,
                    "giving up on a watched notification"
                );
            }
            Some((msg, _)) => {
                entry.status = "pending".to_owned();
                entry.last_error = msg;
                entry.next_at = now() + BACKOFF[entry.attempts as usize - 1].as_secs() as i64;
            }
        }
        if let Err(e) = self.store.settle_watched(&entry).await {
            tracing::warn!(target: "plurxd::integrate", error = %e, "recording a delivery outcome");
        }
    }
}

/// POST one event. `Err((message, permanent))` — permanent means retrying
/// cannot help, so the queue should stop rather than spend its schedule.
async fn post(url: &str, key: &str, payload: &str) -> Result<(), (String, bool)> {
    let url = crate::http::comingsoon::normalize_monarr_url(url);
    let target = format!("{url}/api/v1/webhooks/plurx");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("plurx/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| (e.to_string(), true))?;
    let resp = client
        .post(&target)
        .header("X-Api-Key", key)
        .header("content-type", "application/json")
        .body(payload.to_owned())
        .send()
        .await
        .map_err(|e| {
            let mut cause: &dyn std::error::Error = &e;
            let mut deepest = cause.to_string();
            while let Some(next) = cause.source() {
                deepest = next.to_string();
                cause = next;
            }
            (format!("cannot reach monarr at {url}: {deepest}"), false)
        })?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    // 401/403 is a key that will not grow rights; 404 is a monarr too old to
    // have the route; 400/422 is a payload it will reject just as firmly the
    // fourth time. 5xx is monarr having a moment.
    let permanent = status.is_client_error();
    Err((format!("monarr returned {status}"), permanent))
}

async fn show_of(
    store: &dyn Store,
    episode: &plurx_core::domain::Item,
) -> Result<Option<plurx_core::domain::Item>, plurx_core::error::StoreError> {
    let mut current = episode.clone();
    for _ in 0..4 {
        let Some(parent) = current.parent_id else {
            break;
        };
        match store.get_item(parent).await? {
            Some(p) => current = p,
            None => break,
        }
        if current.kind == ItemKind::Show {
            return Ok(Some(current));
        }
    }
    Ok(None)
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
