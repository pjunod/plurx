use async_trait::async_trait;

use super::SqliteStore;
use crate::domain::{NetworkPrior, NetworkPriorObservation, PlaybackEvent, PlaybackEventQuery};
use crate::error::StoreError;
use crate::store::{NetworkPriorStore, PlaybackTelemetryStore};

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

#[async_trait]
impl NetworkPriorStore for SqliteStore {
    async fn observe_network_prior(
        &self,
        observation: &NetworkPriorObservation,
    ) -> Result<NetworkPrior, StoreError> {
        let observation = observation.clone();
        self.with_conn(move |conn| crate::store::telemetry::observe_prior(conn, &observation))
            .await
    }

    async fn network_prior(
        &self,
        credential_generation: &str,
        client_class: &str,
        network_fingerprint: &str,
    ) -> Result<Option<NetworkPrior>, StoreError> {
        let credential_generation = credential_generation.to_owned();
        let client_class = client_class.to_owned();
        let network_fingerprint = network_fingerprint.to_owned();
        self.with_read(move |conn| {
            crate::store::telemetry::get_prior(conn, &credential_generation, &client_class, &network_fingerprint)
        })
        .await
    }

    async fn prune_network_priors(&self, before_ms: i64, limit: i64) -> Result<u64, StoreError> {
        self.with_conn(move |conn| crate::store::telemetry::prune_priors(conn, before_ms, limit))
            .await
    }
}
