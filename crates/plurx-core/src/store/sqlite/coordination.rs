use async_trait::async_trait;
use rusqlite::{params, OptionalExtension, Row};

use super::SqliteStore;
use crate::cluster::coordination::{
    validate_lease_identity, validate_lease_input, validate_lease_renewal, Lease, LeaseClaim,
};
use crate::error::StoreError;
use crate::store::CoordinationStore;

const ACQUIRE_SQL: &str = "INSERT INTO job_leases
    (resource, owner_node_id, fence, revision, expires_at_ms, updated_at_ms)
    VALUES (?1, ?2, 1, 1, ?4, ?3)
    ON CONFLICT(resource) DO UPDATE SET
        owner_node_id = excluded.owner_node_id,
        fence = job_leases.fence + 1,
        revision = job_leases.revision + 1,
        expires_at_ms = excluded.expires_at_ms,
        updated_at_ms = excluded.updated_at_ms
    WHERE job_leases.expires_at_ms <= ?3
      AND job_leases.fence < 9223372036854775807
      AND job_leases.revision < 9223372036854775807
    RETURNING resource, owner_node_id, fence, revision, expires_at_ms";

#[async_trait]
impl CoordinationStore for SqliteStore {
    async fn acquire_lease(
        &self,
        resource: &str,
        owner_node_id: &str,
        now_unix_ms: i64,
        expires_at_unix_ms: i64,
    ) -> Result<LeaseClaim, StoreError> {
        validate_lease_input(resource, owner_node_id, now_unix_ms, expires_at_unix_ms)?;
        let resource = resource.to_owned();
        let owner_node_id = owner_node_id.to_owned();
        self.with_conn(move |conn| {
            let acquired = conn
                .query_row(
                    ACQUIRE_SQL,
                    params![resource, owner_node_id, now_unix_ms, expires_at_unix_ms],
                    lease_from_row,
                )
                .optional()?;
            if let Some(lease) = acquired {
                return Ok(LeaseClaim::Acquired(lease));
            }

            let held = conn
                .query_row(
                    "SELECT resource, owner_node_id, fence, revision, expires_at_ms
                     FROM job_leases WHERE resource = ?1",
                    params![resource],
                    lease_from_row,
                )
                .optional()?
                .ok_or_else(|| {
                    StoreError::Database(
                        "lease acquire changed no row and found no current row".to_owned(),
                    )
                })?;
            if held.expires_at_unix_ms <= now_unix_ms {
                return Err(StoreError::Database(format!(
                    "lease counter exhausted for resource {}",
                    held.resource
                )));
            }
            Ok(LeaseClaim::Held {
                owner_node_id: held.owner_node_id,
                fence: held.fence,
                expires_at_unix_ms: held.expires_at_unix_ms,
            })
        })
        .await
    }

    async fn renew_lease(
        &self,
        lease: &Lease,
        now_unix_ms: i64,
        expires_at_unix_ms: i64,
    ) -> Result<Option<Lease>, StoreError> {
        validate_lease_renewal(lease, now_unix_ms, expires_at_unix_ms)?;
        let fence = sql_fence(lease.fence)?;
        let revision = sql_revision(lease.revision)?;
        let lease = lease.clone();
        self.with_conn(move |conn| {
            Ok(conn
                .query_row(
                    "UPDATE job_leases
                     SET expires_at_ms = ?4,
                         revision = revision + 1,
                         updated_at_ms = ?3
                     WHERE resource = ?1 AND owner_node_id = ?2 AND fence = ?5
                       AND expires_at_ms > ?3
                       AND revision = ?6
                       AND expires_at_ms = ?7
                       AND revision < 9223372036854775807
                     RETURNING resource, owner_node_id, fence, revision, expires_at_ms",
                    params![
                        lease.resource,
                        lease.owner_node_id,
                        now_unix_ms,
                        expires_at_unix_ms,
                        fence,
                        revision,
                        lease.expires_at_unix_ms
                    ],
                    lease_from_row,
                )
                .optional()?)
        })
        .await
    }

    async fn release_lease(&self, lease: &Lease, now_unix_ms: i64) -> Result<bool, StoreError> {
        validate_lease_identity(&lease.resource, &lease.owner_node_id)?;
        let fence = sql_fence(lease.fence)?;
        let revision = sql_revision(lease.revision)?;
        let lease = lease.clone();
        self.with_conn(move |conn| {
            Ok(conn.execute(
                "UPDATE job_leases
                 SET expires_at_ms = CASE
                         WHEN expires_at_ms < ?4 THEN expires_at_ms ELSE ?4 END,
                     revision = revision + 1,
                     updated_at_ms = ?4
                 WHERE resource = ?1 AND owner_node_id = ?2 AND fence = ?3
                   AND revision = ?5
                   AND expires_at_ms = ?6
                   AND revision < 9223372036854775807",
                params![
                    lease.resource,
                    lease.owner_node_id,
                    fence,
                    now_unix_ms,
                    revision,
                    lease.expires_at_unix_ms
                ],
            )? == 1)
        })
        .await
    }
}

fn sql_fence(fence: u64) -> Result<i64, StoreError> {
    sql_positive_i64("lease fence", fence)
}

fn sql_revision(revision: u64) -> Result<i64, StoreError> {
    sql_positive_i64("lease revision", revision)
}

fn sql_positive_i64(label: &str, value: u64) -> Result<i64, StoreError> {
    if value == 0 {
        return Err(StoreError::Database(format!(
            "{label} must be greater than zero"
        )));
    }
    i64::try_from(value)
        .map_err(|error| StoreError::Database(format!("{label} is out of range: {error}")))
}

fn lease_from_row(row: &Row<'_>) -> rusqlite::Result<Lease> {
    let fence = row.get::<_, i64>(2)?;
    let fence = u64::try_from(fence).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })?;
    let revision = row.get::<_, i64>(3)?;
    let revision = u64::try_from(revision).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })?;
    Ok(Lease {
        resource: row.get(0)?,
        owner_node_id: row.get(1)?,
        fence,
        revision,
        expires_at_unix_ms: row.get(4)?,
    })
}
