//! Node-local playback telemetry storage shared by both durable backends.
//!
//! Single-node SQLite carries this schema as migration v17. A hiqlite voter
//! owns the same table in a separate SQLite sidecar because submitting an
//! operational event through hiqlite's write API would replicate it via Raft.

#[cfg(any(test, feature = "hiqlite-store"))]
use std::path::Path;
#[cfg(any(test, feature = "hiqlite-store"))]
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection};

use crate::domain::{
    CredentialGeneration, NetworkPrior, NetworkPriorObservation, PlaybackEvent, PlaybackEventQuery,
    NETWORK_PRIOR_STARVED_TTL_MS,
};
use crate::error::StoreError;

pub(crate) const PLAYBACK_EVENTS_SCHEMA: &str = "
CREATE TABLE playback_events (
    id             INTEGER PRIMARY KEY,
    at_unix_ms     INTEGER NOT NULL,
    user_id        INTEGER,
    session_id     TEXT,
    file_id        INTEGER,
    event          TEXT NOT NULL,
    level          TEXT,
    method         TEXT,
    encoder        TEXT,
    height         INTEGER,
    ms             INTEGER,
    runway_ds      INTEGER,
    bandwidth_kbps INTEGER,
    speed_recent   REAL,
    ahead_seconds  INTEGER,
    suspended      INTEGER,
    hold_reason    TEXT,
    delivered_bps  INTEGER,
    readrate        REAL,
    detail         TEXT,
    attempt        TEXT,
    reason         TEXT,
    ua             TEXT,
    extra          TEXT
) STRICT;
CREATE INDEX playback_events_by_event ON playback_events(event, at_unix_ms);
CREATE INDEX playback_events_by_file ON playback_events(file_id, at_unix_ms);";

/// Old integer-key network priors schema (sidecar v2).
/// Replaced by [`NETWORK_PRIORS_SCHEMA`] in sidecar v3.
pub(crate) const NETWORK_PRIORS_V2_SCHEMA: &str = "
CREATE TABLE network_priors (
    user_id             INTEGER NOT NULL,
    client_class        TEXT NOT NULL,
    network_fingerprint TEXT NOT NULL,
    sustained_kbps      INTEGER,
    worst_rung_height   INTEGER,
    starved_at_ms       INTEGER,
    sample_count        INTEGER NOT NULL DEFAULT 0,
    updated_at_ms       INTEGER NOT NULL,
    PRIMARY KEY (user_id, client_class, network_fingerprint)
) STRICT;
CREATE INDEX network_priors_by_updated
    ON network_priors(updated_at_ms, user_id, client_class);";

/// New credential-generation-key network priors schema (sidecar v3+).
#[cfg(any(test, feature = "hiqlite-store"))]
pub(crate) const NETWORK_PRIORS_SCHEMA: &str = "
CREATE TABLE network_priors (
    user_id               INTEGER NOT NULL,
    credential_generation TEXT NOT NULL,
    client_class          TEXT NOT NULL,
    network_fingerprint   TEXT NOT NULL,
    sustained_kbps        INTEGER,
    worst_rung_height     INTEGER,
    starved_at_ms         INTEGER,
    sample_count          INTEGER NOT NULL DEFAULT 0,
    updated_at_ms         INTEGER NOT NULL,
    PRIMARY KEY (credential_generation, client_class, network_fingerprint)
) STRICT;
CREATE INDEX network_priors_by_updated
    ON network_priors(updated_at_ms, user_id, client_class);";

#[cfg(any(test, feature = "hiqlite-store"))]
const SIDECAR_SCHEMA_VERSION: i64 = 3;
const MAX_QUERY_ROWS: i64 = 2_000;
const MAX_PRUNE_ROWS: i64 = 10_000;
const MAX_PRIORS_PER_USER_CLIENT: i64 = 64;

pub(crate) fn insert(conn: &Connection, event: &PlaybackEvent) -> Result<i64, StoreError> {
    conn.execute(
        "INSERT INTO playback_events (
             at_unix_ms, user_id, session_id, file_id, event, level, method,
             encoder, height, ms, runway_ds, bandwidth_kbps, speed_recent,
             ahead_seconds, suspended, hold_reason, delivered_bps, readrate,
             detail, attempt, reason, ua, extra
         ) VALUES (
             ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
             ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23
         )",
        params![
            event.at_unix_ms,
            event.user_id,
            event.session_id,
            event.file_id,
            event.event,
            event.level,
            event.method,
            event.encoder,
            event.height,
            event.ms,
            event.runway_ds,
            event.bandwidth_kbps,
            event.speed_recent,
            event.ahead_seconds,
            event.suspended.map(i64::from),
            event.hold_reason,
            event.delivered_bps,
            event.readrate,
            event.detail,
            event.attempt,
            event.reason,
            event.ua,
            event.extra,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub(crate) fn prune(conn: &Connection, before_ms: i64, limit: i64) -> Result<u64, StoreError> {
    if limit <= 0 {
        return Ok(0);
    }
    let limit = limit.min(MAX_PRUNE_ROWS);
    let changed = conn.execute(
        "DELETE FROM playback_events WHERE id IN (
             SELECT id FROM playback_events
             WHERE at_unix_ms < ?1
             ORDER BY at_unix_ms, id
             LIMIT ?2
         )",
        params![before_ms, limit],
    )?;
    u64::try_from(changed).map_err(|error| StoreError::Task(error.to_string()))
}

pub(crate) fn query(
    conn: &Connection,
    query: &PlaybackEventQuery,
) -> Result<Vec<PlaybackEvent>, StoreError> {
    let limit = query.limit.clamp(1, MAX_QUERY_ROWS);
    let mut statement = conn.prepare(
        "SELECT id, at_unix_ms, user_id, session_id, file_id, event, level,
                method, encoder, height, ms, runway_ds, bandwidth_kbps,
                speed_recent, ahead_seconds, suspended, hold_reason,
                delivered_bps, readrate, detail, attempt, reason, ua, extra
         FROM playback_events
         WHERE (?1 IS NULL OR at_unix_ms >= ?1)
           AND (?2 IS NULL OR event = ?2)
         ORDER BY at_unix_ms DESC, id DESC
         LIMIT ?3",
    )?;
    let rows = statement.query_map(params![query.since_ms, query.event, limit], |row| {
        Ok(PlaybackEvent {
            id: row.get(0)?,
            at_unix_ms: row.get(1)?,
            user_id: row.get(2)?,
            session_id: row.get(3)?,
            file_id: row.get(4)?,
            event: row.get(5)?,
            level: row.get(6)?,
            method: row.get(7)?,
            encoder: row.get(8)?,
            height: row.get(9)?,
            ms: row.get(10)?,
            runway_ds: row.get(11)?,
            bandwidth_kbps: row.get(12)?,
            speed_recent: row.get(13)?,
            ahead_seconds: row.get(14)?,
            suspended: row.get::<_, Option<i64>>(15)?.map(|value| value != 0),
            hold_reason: row.get(16)?,
            delivered_bps: row.get(17)?,
            readrate: row.get(18)?,
            detail: row.get(19)?,
            attempt: row.get(20)?,
            reason: row.get(21)?,
            ua: row.get(22)?,
            extra: row.get(23)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn prior_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NetworkPrior> {
    let sustained_kbps = row
        .get::<_, Option<i64>>(3)?
        .and_then(|value| u32::try_from(value).ok());
    let sample_count = u32::try_from(row.get::<_, i64>(6)?).unwrap_or(u32::MAX);
    Ok(NetworkPrior {
        credential_generation: CredentialGeneration::from(row.get::<_, String>(0)?),
        client_class: row.get(1)?,
        network_fingerprint: row.get(2)?,
        sustained_kbps,
        worst_rung_height: row.get(4)?,
        starved_at_ms: row.get(5)?,
        sample_count,
        updated_at_ms: row.get(7)?,
    })
}

pub(crate) fn observe_prior(
    conn: &Connection,
    observation: &NetworkPriorObservation,
) -> Result<NetworkPrior, StoreError> {
    if observation.credential_generation.as_str().trim().is_empty()
        || observation.client_class.trim().is_empty()
        || observation.network_fingerprint.trim().is_empty()
        || (observation.throughput_kbps.is_none() && observation.starved_rung_height.is_none())
    {
        return Err(StoreError::Task(
            "network prior observation is missing its key or measurement".to_owned(),
        ));
    }

    let transaction = conn.unchecked_transaction()?;
    // The starvation verdict is still the stronger signal while it is fresh —
    // a `min` that a later stall can only lower — but it now expires. Past
    // `NETWORK_PRIOR_STARVED_TTL_MS` from the starvation that set it, the next
    // observation to touch the row retires it: a healthy sample clears the
    // verdict, and a new stall replaces it outright instead of being `min`ed
    // against history the link has since outgrown. Expiry is measured against
    // the observation's own server-stamped time, so it needs no clock here and
    // stays deterministic under test.
    transaction.execute(
        "INSERT INTO network_priors (
             user_id, credential_generation, client_class, network_fingerprint, sustained_kbps,
             worst_rung_height, starved_at_ms, sample_count, updated_at_ms
         ) VALUES (
             ?1, ?2, ?3, ?4, ?5, ?6,
             CASE WHEN ?6 IS NULL THEN NULL ELSE ?7 END,
             CASE WHEN ?5 IS NULL THEN 0 ELSE 1 END,
             ?7
         )
         ON CONFLICT(credential_generation, client_class, network_fingerprint) DO UPDATE SET
             sustained_kbps = CASE
                 WHEN excluded.sustained_kbps IS NULL THEN network_priors.sustained_kbps
                 WHEN network_priors.sustained_kbps IS NULL THEN excluded.sustained_kbps
                 ELSE (network_priors.sustained_kbps * 3 + excluded.sustained_kbps + 2) / 4
             END,
             worst_rung_height = CASE
                 WHEN network_priors.starved_at_ms IS NULL
                      OR excluded.updated_at_ms - network_priors.starved_at_ms > ?8
                     THEN excluded.worst_rung_height
                 WHEN excluded.worst_rung_height IS NULL THEN network_priors.worst_rung_height
                 ELSE min(network_priors.worst_rung_height, excluded.worst_rung_height)
             END,
             starved_at_ms = CASE
                 WHEN excluded.starved_at_ms IS NOT NULL THEN excluded.starved_at_ms
                 WHEN network_priors.starved_at_ms IS NULL
                      OR excluded.updated_at_ms - network_priors.starved_at_ms > ?8
                     THEN NULL
                 ELSE network_priors.starved_at_ms
             END,
             sample_count = min(
                 network_priors.sample_count
                     + CASE WHEN excluded.sustained_kbps IS NULL THEN 0 ELSE 1 END,
                 4294967295
             ),
             updated_at_ms = excluded.updated_at_ms
         WHERE excluded.updated_at_ms >= network_priors.updated_at_ms",
        params![
            observation.user_id,
            observation.credential_generation.as_str(),
            observation.client_class,
            observation.network_fingerprint,
            observation.throughput_kbps.map(i64::from),
            observation.starved_rung_height,
            observation.observed_at_ms,
            NETWORK_PRIOR_STARVED_TTL_MS,
        ],
    )?;
    transaction.execute(
        "DELETE FROM network_priors
         WHERE (credential_generation, client_class, network_fingerprint) IN (
             SELECT credential_generation, client_class, network_fingerprint FROM network_priors
             WHERE user_id = ?1 AND client_class = ?2
             ORDER BY updated_at_ms DESC, credential_generation DESC, network_fingerprint DESC
             LIMIT -1 OFFSET ?3
           )",
        params![
            observation.user_id,
            observation.client_class,
            MAX_PRIORS_PER_USER_CLIENT,
        ],
    )?;
    let prior = transaction.query_row(
        "SELECT credential_generation, client_class, network_fingerprint, sustained_kbps,
                worst_rung_height, starved_at_ms, sample_count, updated_at_ms
         FROM network_priors
         WHERE credential_generation = ?1 AND client_class = ?2 AND network_fingerprint = ?3",
        params![
            observation.credential_generation.as_str(),
            observation.client_class,
            observation.network_fingerprint,
        ],
        prior_from_row,
    )?;
    transaction.commit()?;
    Ok(prior)
}

pub(crate) fn get_prior(
    conn: &Connection,
    credential_generation: &str,
    client_class: &str,
    network_fingerprint: &str,
) -> Result<Option<NetworkPrior>, StoreError> {
    use rusqlite::OptionalExtension;

    conn.query_row(
        "SELECT credential_generation, client_class, network_fingerprint, sustained_kbps,
                worst_rung_height, starved_at_ms, sample_count, updated_at_ms
         FROM network_priors
         WHERE credential_generation = ?1 AND client_class = ?2 AND network_fingerprint = ?3",
        params![credential_generation, client_class, network_fingerprint],
        prior_from_row,
    )
    .optional()
    .map_err(Into::into)
}

pub(crate) fn prune_priors(
    conn: &Connection,
    before_ms: i64,
    limit: i64,
) -> Result<u64, StoreError> {
    if limit <= 0 {
        return Ok(0);
    }
    let changed = conn.execute(
        "DELETE FROM network_priors
         WHERE (credential_generation, client_class, network_fingerprint) IN (
             SELECT credential_generation, client_class, network_fingerprint
             FROM network_priors
             WHERE updated_at_ms < ?1
             ORDER BY updated_at_ms, credential_generation, client_class, network_fingerprint
             LIMIT ?2
         )",
        params![before_ms, limit.min(MAX_PRUNE_ROWS)],
    )?;
    u64::try_from(changed).map_err(|error| StoreError::Task(error.to_string()))
}

/// Whether `name` is already a table in this sidecar.
///
/// The sidecar migration uses this to stay re-runnable against a database an
/// earlier build left half-upgraded.
#[cfg(any(test, feature = "hiqlite-store"))]
fn table_exists(conn: &Connection, name: &str) -> Result<bool, StoreError> {
    let found: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![name],
        |row| row.get(0),
    )?;
    Ok(found > 0)
}

/// One hiqlite voter's explicitly node-local telemetry database.
#[derive(Clone)]
#[cfg(any(test, feature = "hiqlite-store"))]
pub(crate) struct NodeLocalTelemetry {
    conn: Arc<Mutex<Connection>>,
}

#[cfg(any(test, feature = "hiqlite-store"))]
impl NodeLocalTelemetry {
    pub(crate) fn open(path: &Path) -> Result<Self, StoreError> {
        Self::initialize(Connection::open(path)?)
    }

    fn initialize(conn: Connection) -> Result<Self, StoreError> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if current > SIDECAR_SCHEMA_VERSION {
            return Err(StoreError::Migration(format!(
                "telemetry sidecar schema is v{current}, but this binary only knows \
                 v{SIDECAR_SCHEMA_VERSION}"
            )));
        }
        // The schema and the `user_version` that describes it have to commit
        // together. `PRAGMA user_version` writes the database header through
        // the pager, so it rolls back with the DDL when it shares the
        // transaction; stamping it after a separate COMMIT leaves a window
        // where a kill strands a v1 sidecar that already owns `network_priors`,
        // and the next startup re-runs the CREATE and refuses to open at all.
        //
        // Each CREATE is also skipped when its table is already there, so a
        // sidecar torn by a build that predates this stays recoverable instead
        // of failing every restart forever.
        if current < SIDECAR_SCHEMA_VERSION {
            let mut migration = String::from("BEGIN;\n");
            let mut needs_prior_upgrade = current >= 2 || table_exists(&conn, "network_priors")?;
            if !table_exists(&conn, "playback_events")? {
                migration.push_str(PLAYBACK_EVENTS_SCHEMA);
                migration.push('\n');
            }
            if current < 2 && !table_exists(&conn, "network_priors")? {
                // v0/v1: create the legacy integer-key prior table. The
                // upgrade step below handles the v2→v3 transition.
                migration.push_str(NETWORK_PRIORS_V2_SCHEMA);
                migration.push('\n');
                needs_prior_upgrade = true;
            }
            if needs_prior_upgrade {
                // v2→v3: drop the old integer-key prior table and recreate
                // with the credential-generation text key. Old numeric-key
                // rows cannot be translated because the user_id alone is not
                // enough material to recover the credential generation.
                migration.push_str("DROP TABLE IF EXISTS network_priors;\n");
                migration.push_str(NETWORK_PRIORS_SCHEMA);
                migration.push('\n');
            }
            migration.push_str(&format!(
                "PRAGMA user_version = {SIDECAR_SCHEMA_VERSION};\nCOMMIT;"
            ));
            conn.execute_batch(&migration).map_err(|error| {
                StoreError::Migration(format!(
                    "migrating telemetry sidecar from v{current} to \
                     v{SIDECAR_SCHEMA_VERSION}: {error}"
                ))
            })?;
        }
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    async fn with_conn<T, F>(&self, operation: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, StoreError> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .map_err(|_| StoreError::Task("telemetry sidecar mutex poisoned".to_owned()))?;
            operation(&guard)
        })
        .await
        .map_err(|error| StoreError::Task(error.to_string()))?
    }

    pub(crate) async fn record(&self, event: PlaybackEvent) -> Result<i64, StoreError> {
        self.with_conn(move |conn| insert(conn, &event)).await
    }

    pub(crate) async fn prune(&self, before_ms: i64, limit: i64) -> Result<u64, StoreError> {
        self.with_conn(move |conn| prune(conn, before_ms, limit))
            .await
    }

    pub(crate) async fn events(
        &self,
        query_value: PlaybackEventQuery,
    ) -> Result<Vec<PlaybackEvent>, StoreError> {
        self.with_conn(move |conn| query(conn, &query_value)).await
    }

    pub(crate) async fn observe_prior(
        &self,
        observation: NetworkPriorObservation,
    ) -> Result<NetworkPrior, StoreError> {
        self.with_conn(move |conn| observe_prior(conn, &observation))
            .await
    }

    pub(crate) async fn prior(
        &self,
        credential_generation: String,
        client_class: String,
        network_fingerprint: String,
    ) -> Result<Option<NetworkPrior>, StoreError> {
        self.with_conn(move |conn| {
            get_prior(
                conn,
                &credential_generation,
                &client_class,
                &network_fingerprint,
            )
        })
        .await
    }

    pub(crate) async fn prune_priors(&self, before_ms: i64, limit: i64) -> Result<u64, StoreError> {
        self.with_conn(move |conn| prune_priors(conn, before_ms, limit))
            .await
    }

    #[cfg(feature = "hiqlite-store")]
    pub(crate) async fn clear(&self) -> Result<(), StoreError> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM playback_events", [])?;
            conn.execute("DELETE FROM network_priors", [])?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(name: &str, at_unix_ms: i64) -> PlaybackEvent {
        PlaybackEvent {
            at_unix_ms,
            event: name.to_owned(),
            ..PlaybackEvent::default()
        }
    }

    fn observation(
        network: &str,
        throughput_kbps: Option<u32>,
        starved_rung_height: Option<i64>,
        at_ms: i64,
    ) -> NetworkPriorObservation {
        NetworkPriorObservation {
            user_id: 42,
            credential_generation: CredentialGeneration::from("test-gen".to_owned()),
            client_class: "safari".to_owned(),
            network_fingerprint: network.to_owned(),
            throughput_kbps,
            starved_rung_height,
            observed_at_ms: at_ms,
        }
    }

    fn prior_connection() -> Connection {
        let conn = Connection::open_in_memory().expect("prior connection");
        conn.execute_batch(NETWORK_PRIORS_SCHEMA)
            .expect("prior schema");
        conn
    }

    #[test]
    fn prior_updates_use_a_conservative_ewma_and_lowest_starved_rung() {
        let conn = prior_connection();
        let first = observe_prior(
            &conn,
            &observation("192.0.2.0/24", Some(8_000), Some(1080), 100),
        )
        .expect("first observation");
        assert_eq!(first.sustained_kbps, Some(8_000));
        assert_eq!(first.worst_rung_height, Some(1080));

        let second = observe_prior(
            &conn,
            &observation("192.0.2.0/24", Some(4_000), Some(720), 200),
        )
        .expect("second observation");
        assert_eq!(second.sustained_kbps, Some(7_000));
        assert_eq!(second.worst_rung_height, Some(720));
        assert_eq!(second.sample_count, 2);

        let stale = observe_prior(
            &conn,
            &observation("192.0.2.0/24", Some(100), Some(360), 99),
        )
        .expect("stale observation returns current row");
        assert_eq!(stale, second, "late telemetry must not rewrite the prior");
    }

    /// The verdict has to be able to expire. As a permanent monotonic `min` it
    /// made one transient stall cap a tuple for the row's whole life, and
    /// because every observation refreshes `updated_at_ms`, retention never
    /// rescued an actively used row either.
    #[test]
    fn a_starvation_verdict_ages_out_and_a_later_stall_re_arms_it() {
        let conn = prior_connection();
        let starved = observe_prior(
            &conn,
            &observation("192.0.2.0/24", Some(4_000), Some(1080), 1_000),
        )
        .expect("starved observation");
        assert_eq!(starved.worst_rung_height, Some(1080));
        assert_eq!(starved.starved_at_ms, Some(1_000));

        // Healthy traffic inside the horizon keeps the verdict, and keeps its
        // original stamp rather than sliding the horizon forward.
        let within = observe_prior(
            &conn,
            &observation(
                "192.0.2.0/24",
                Some(20_000),
                None,
                1_000 + NETWORK_PRIOR_STARVED_TTL_MS,
            ),
        )
        .expect("observation inside the horizon");
        assert_eq!(within.worst_rung_height, Some(1080));
        assert_eq!(within.starved_at_ms, Some(1_000));

        // The first observation past the horizon retires it.
        let expired = observe_prior(
            &conn,
            &observation(
                "192.0.2.0/24",
                Some(20_000),
                None,
                1_001 + NETWORK_PRIOR_STARVED_TTL_MS,
            ),
        )
        .expect("observation past the horizon");
        assert_eq!(expired.worst_rung_height, None);
        assert_eq!(expired.starved_at_ms, None);
        assert!(
            expired.sustained_kbps.is_some_and(|kbps| kbps > 4_000),
            "retiring the verdict must not disturb the throughput estimate"
        );

        // A genuinely still-bad link re-arms it, and a verdict recorded after
        // the old one expired replaces it instead of being `min`ed against
        // history the link has since outgrown.
        let rearmed = observe_prior(
            &conn,
            &observation(
                "192.0.2.0/24",
                None,
                Some(720),
                2_000 + NETWORK_PRIOR_STARVED_TTL_MS,
            ),
        )
        .expect("re-armed observation");
        assert_eq!(rearmed.worst_rung_height, Some(720));
        assert_eq!(
            rearmed.starved_at_ms,
            Some(2_000 + NETWORK_PRIOR_STARVED_TTL_MS)
        );

        let raised = observe_prior(
            &conn,
            &observation(
                "192.0.2.0/24",
                None,
                Some(1080),
                3_000 + NETWORK_PRIOR_STARVED_TTL_MS,
            ),
        )
        .expect("higher starvation inside the horizon");
        assert_eq!(
            raised.worst_rung_height,
            Some(720),
            "inside the horizon the lowest starved rung still wins"
        );
        assert_eq!(
            raised.starved_at_ms,
            Some(3_000 + NETWORK_PRIOR_STARVED_TTL_MS),
            "any starvation is evidence the link still starves, so it re-stamps"
        );

        let replaced = observe_prior(
            &conn,
            &observation(
                "192.0.2.0/24",
                None,
                Some(1080),
                3_001 + 2 * NETWORK_PRIOR_STARVED_TTL_MS,
            ),
        )
        .expect("starvation past the horizon");
        assert_eq!(
            replaced.worst_rung_height,
            Some(1080),
            "an expired verdict is replaced, not min'd"
        );
    }

    #[test]
    fn the_expiry_horizon_is_the_one_the_consult_reads() {
        use crate::domain::NetworkPrior;

        let prior = NetworkPrior {
            worst_rung_height: Some(720),
            starved_at_ms: Some(1_000),
            ..NetworkPrior::default()
        };
        assert_eq!(
            prior.active_starved_rung(1_000 + NETWORK_PRIOR_STARVED_TTL_MS),
            Some(720)
        );
        assert_eq!(
            prior.active_starved_rung(1_001 + NETWORK_PRIOR_STARVED_TTL_MS),
            None
        );
        assert_eq!(
            NetworkPrior {
                starved_at_ms: None,
                ..prior.clone()
            }
            .active_starved_rung(1_000),
            None,
            "an unstamped verdict must not bind forever"
        );
        assert_eq!(
            prior.active_starved_rung(0),
            Some(720),
            "a backwards clock step must not retire a fresh verdict"
        );
    }

    #[test]
    fn priors_are_cardinality_bounded_and_pruned_in_bounded_batches() {
        let conn = prior_connection();
        for octet in 0..=64 {
            let network = format!("192.0.{octet}.0/24");
            observe_prior(&conn, &observation(&network, Some(1_000), None, octet + 1))
                .expect("observation");
        }
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM network_priors", [], |row| row.get(0))
            .expect("count priors");
        assert_eq!(count, MAX_PRIORS_PER_USER_CLIENT);
        assert!(get_prior(&conn, "test-gen", "safari", "192.0.0.0/24")
            .expect("oldest lookup")
            .is_none());

        assert_eq!(prune_priors(&conn, 40, 3).expect("bounded prune"), 3);
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM network_priors", [], |row| row.get(0))
            .expect("remaining priors");
        assert_eq!(remaining, MAX_PRIORS_PER_USER_CLIENT - 3);
    }

    #[test]
    fn prior_bound_spans_credential_generations_without_warming_the_replacement() {
        let conn = prior_connection();
        for generation in ["old-gen", "middle-gen"] {
            for network in 0..40 {
                let mut sample = observation(
                    &format!("192.0.{network}.0/24"),
                    Some(1_000),
                    None,
                    network + if generation == "old-gen" { 1 } else { 101 },
                );
                sample.credential_generation = CredentialGeneration::from(generation.to_owned());
                observe_prior(&conn, &sample).expect("rotated-generation observation");
            }
        }

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM network_priors WHERE user_id = 42 AND client_class = 'safari'",
                [],
                |row| row.get(0),
            )
            .expect("count priors across generations");
        assert_eq!(count, MAX_PRIORS_PER_USER_CLIENT);
        assert!(get_prior(&conn, "new-gen", "safari", "192.0.39.0/24")
            .expect("new generation lookup")
            .is_none());
    }

    #[tokio::test]
    async fn sidecar_survives_reopen_and_does_not_share_rows() {
        let root = tempfile::tempdir().expect("sidecar root");
        let first_path = root.path().join("node-1.db");
        let second_path = root.path().join("node-2.db");
        let first = NodeLocalTelemetry::open(&first_path).expect("first sidecar");
        first.record(event("ttff", 10)).await.expect("record");
        drop(first);

        let reopened = NodeLocalTelemetry::open(&first_path).expect("reopen first");
        assert_eq!(
            reopened
                .events(PlaybackEventQuery {
                    limit: 10,
                    ..PlaybackEventQuery::default()
                })
                .await
                .expect("read reopened")
                .len(),
            1
        );
        let second = NodeLocalTelemetry::open(&second_path).expect("second sidecar");
        assert!(second
            .events(PlaybackEventQuery {
                limit: 10,
                ..PlaybackEventQuery::default()
            })
            .await
            .expect("read second")
            .is_empty());
    }

    #[test]
    fn sidecar_refuses_a_future_schema_version() {
        let directory = tempfile::tempdir().expect("sidecar directory");
        let path = directory.path().join("telemetry.db");
        let conn = Connection::open(&path).expect("seed sidecar");
        conn.pragma_update(None, "user_version", SIDECAR_SCHEMA_VERSION + 1)
            .expect("future version");
        drop(conn);
        let error = NodeLocalTelemetry::open(&path)
            .err()
            .expect("future sidecar schema must be refused");
        assert!(error.to_string().contains("only knows v3"), "{error}");
    }

    #[tokio::test]
    async fn sidecar_v1_migrates_network_priors_without_losing_events() {
        let directory = tempfile::tempdir().expect("sidecar directory");
        let path = directory.path().join("telemetry.db");
        let conn = Connection::open(&path).expect("seed sidecar");
        conn.execute_batch(PLAYBACK_EVENTS_SCHEMA)
            .expect("v1 telemetry schema");
        conn.execute(
            "INSERT INTO playback_events (at_unix_ms, event) VALUES (10, 'ttff')",
            [],
        )
        .expect("seed event");
        conn.pragma_update(None, "user_version", 1)
            .expect("v1 marker");
        drop(conn);

        let sidecar = NodeLocalTelemetry::open(&path).expect("migrate sidecar");
        assert_eq!(
            sidecar
                .events(PlaybackEventQuery {
                    limit: 10,
                    ..PlaybackEventQuery::default()
                })
                .await
                .expect("preserved events")
                .len(),
            1
        );
        let prior = sidecar
            .observe_prior(observation("192.0.2.0/24", Some(5_000), None, 20))
            .await
            .expect("v2 prior");
        assert_eq!(prior.sustained_kbps, Some(5_000));
        assert_eq!(
            sidecar
                .prior(
                    "test-gen".to_owned(),
                    "safari".to_owned(),
                    "192.0.2.0/24".to_owned()
                )
                .await
                .expect("read migrated prior"),
            Some(prior)
        );
        assert_eq!(
            sidecar
                .prune_priors(21, 1)
                .await
                .expect("prune migrated prior"),
            1
        );
    }

    /// A sidecar killed between the v1→v2 DDL commit and its `user_version`
    /// stamp must still open. Before the migration became one transaction that
    /// skips a table it already has, this state made every subsequent startup
    /// fail with `table network_priors already exists` — a voter that could
    /// never boot again.
    #[tokio::test]
    async fn sidecar_recovers_from_an_interrupted_v1_migration() {
        let directory = tempfile::tempdir().expect("sidecar directory");
        let path = directory.path().join("telemetry.db");
        let conn = Connection::open(&path).expect("seed sidecar");
        conn.execute_batch(PLAYBACK_EVENTS_SCHEMA)
            .expect("v1 telemetry schema");
        conn.execute(
            "INSERT INTO playback_events (at_unix_ms, event) VALUES (10, 'ttff')",
            [],
        )
        .expect("seed event");
        // The interrupted upgrade: the old integer-key table is committed,
        // the version is not.
        conn.execute_batch(NETWORK_PRIORS_V2_SCHEMA)
            .expect("committed v2 table");
        conn.execute(
            "INSERT INTO network_priors              (user_id, client_class, network_fingerprint, sustained_kbps,               worst_rung_height, starved_at_ms, sample_count, updated_at_ms)              VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7)",
            rusqlite::params![15, "safari", "192.0.2.0/24", 4_000i64, 720i64, 15i64, 15i64],
        )
        .expect("seed prior");
        conn.pragma_update(None, "user_version", 1)
            .expect("stale v1 marker");
        drop(conn);

        let sidecar = NodeLocalTelemetry::open(&path).expect("repair interrupted migration");
        assert_eq!(
            sidecar
                .events(PlaybackEventQuery {
                    limit: 10,
                    ..PlaybackEventQuery::default()
                })
                .await
                .expect("preserved events")
                .len(),
            1
        );
        // The old integer-key prior is dropped because the user_id alone is
        // not enough material to recover the credential generation.
        assert!(
            sidecar
                .prior(
                    "test-gen".to_owned(),
                    "safari".to_owned(),
                    "192.0.2.0/24".to_owned()
                )
                .await
                .expect("read old prior")
                .is_none(),
            "old integer-key prior rows must be dropped, not translated"
        );

        // The repair finished the upgrade rather than leaving it to be retried
        // on every restart.
        let stamped = Connection::open(&path).expect("reopen sidecar");
        let version: i64 = stamped
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("stamped version");
        assert_eq!(version, SIDECAR_SCHEMA_VERSION);
    }
}
