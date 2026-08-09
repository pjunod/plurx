use async_trait::async_trait;

use super::SqliteStore;
use crate::domain::{PlaybackEvent, PlaybackEventQuery};
use crate::error::StoreError;
use crate::store::PlaybackTelemetryStore;

#[async_trait]
impl PlaybackTelemetryStore for SqliteStore {
    async fn record_playback_event(&self, event: &PlaybackEvent) -> Result<i64, StoreError> {
        let event = event.clone();
        self.with_conn(move |conn| crate::store::telemetry::insert(conn, &event))
            .await
    }

    async fn prune_playback_events(&self, before_ms: i64, limit: i64) -> Result<u64, StoreError> {
        self.with_conn(move |conn| crate::store::telemetry::prune(conn, before_ms, limit))
            .await
    }

    async fn playback_events(
        &self,
        query: &PlaybackEventQuery,
    ) -> Result<Vec<PlaybackEvent>, StoreError> {
        let query = query.clone();
        self.with_read(move |conn| crate::store::telemetry::query(conn, &query))
            .await
    }
}
