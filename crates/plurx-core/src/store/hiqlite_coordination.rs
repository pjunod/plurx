use async_trait::async_trait;
use hiqlite::macros::params;
use hiqlite::Row;

use super::hiqlite::{database_error, validate_sql, HiqliteAuthStore};
use super::CoordinationStore;
use crate::cluster::coordination::{
    validate_lease_identity, validate_lease_input, validate_lease_renewal, Lease, LeaseClaim,
};
use crate::error::StoreError;

pub(super) const JOB_LEASES_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS job_leases (
    resource       TEXT PRIMARY KEY,
    owner_node_id  TEXT NOT NULL,
    fence          INTEGER NOT NULL CHECK (fence > 0),
    revision       INTEGER NOT NULL CHECK (revision > 0),
    expires_at_ms  INTEGER NOT NULL,
    updated_at_ms  INTEGER NOT NULL
) STRICT";

const ACQUIRE_SQL: &str = "INSERT INTO job_leases
    (resource, owner_node_id, fence, revision, expires_at_ms, updated_at_ms)
    VALUES ($1, $2, 1, 1, $3, $4)
    ON CONFLICT(resource) DO UPDATE SET
        owner_node_id = excluded.owner_node_id,
        fence = job_leases.fence + 1,
        revision = job_leases.revision + 1,
        expires_at_ms = excluded.expires_at_ms,
        updated_at_ms = excluded.updated_at_ms
    WHERE job_leases.expires_at_ms <= $4
      AND job_leases.fence < 9223372036854775807
      AND job_leases.revision < 9223372036854775807
    RETURNING resource, owner_node_id, fence, revision, expires_at_ms";

#[async_trait]
impl CoordinationStore for HiqliteAuthStore {
    async fn acquire_lease(
        &self,
        resource: &str,
        owner_node_id: &str,
        now_unix_ms: i64,
        expires_at_unix_ms: i64,
    ) -> Result<LeaseClaim, StoreError> {
        validate_lease_input(resource, owner_node_id, now_unix_ms, expires_at_unix_ms)?;
        validate_sql(ACQUIRE_SQL)?;
        let acquired = self
            .client()
            .execute_returning_map::<_, LeaseRow>(
                ACQUIRE_SQL,
                params!(resource, owner_node_id, expires_at_unix_ms, now_unix_ms),
            )
            .await?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?;
        if let Some(row) = acquired.into_iter().next() {
            return Ok(LeaseClaim::Acquired(row.try_into()?));
        }

        let rows = self
            .client()
            .query_consistent_map::<LeaseRow, _>(
                "SELECT resource, owner_node_id, fence, revision, expires_at_ms
                 FROM job_leases WHERE resource = $1",
                params!(resource),
            )
            .await?;
        let held: Lease = rows
            .into_iter()
            .next()
            .ok_or_else(|| {
                StoreError::Database(
                    "lease acquire changed no row and found no current row".to_owned(),
                )
            })?
            .try_into()?;
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
        let sql = "UPDATE job_leases
                   SET expires_at_ms = $1, revision = revision + 1, updated_at_ms = $2
                   WHERE resource = $3 AND owner_node_id = $4 AND fence = $5
                     AND expires_at_ms > $2
                     AND revision = $6
                     AND expires_at_ms = $7
                     AND revision < 9223372036854775807
                   RETURNING resource, owner_node_id, fence, revision, expires_at_ms";
        validate_sql(sql)?;
        let rows = self
            .client()
            .execute_returning_map::<_, LeaseRow>(
                sql,
                params!(
                    expires_at_unix_ms,
                    now_unix_ms,
                    &lease.resource,
                    &lease.owner_node_id,
                    fence,
                    revision,
                    lease.expires_at_unix_ms
                ),
            )
            .await?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?;
        rows.into_iter().next().map(TryInto::try_into).transpose()
    }

    async fn release_lease(&self, lease: &Lease, now_unix_ms: i64) -> Result<bool, StoreError> {
        validate_lease_identity(&lease.resource, &lease.owner_node_id)?;
        let fence = sql_fence(lease.fence)?;
        let revision = sql_revision(lease.revision)?;
        Ok(self
            .client()
            .execute(
                "UPDATE job_leases
                 SET expires_at_ms = CASE
                         WHEN expires_at_ms < $1 THEN expires_at_ms ELSE $1 END,
                     revision = revision + 1,
                     updated_at_ms = $1
                 WHERE resource = $2 AND owner_node_id = $3 AND fence = $4
                   AND revision = $5
                   AND expires_at_ms = $6
                   AND revision < 9223372036854775807",
                params!(
                    now_unix_ms,
                    &lease.resource,
                    &lease.owner_node_id,
                    fence,
                    revision,
                    lease.expires_at_unix_ms
                ),
            )
            .await?
            == 1)
    }
}

struct LeaseRow {
    resource: String,
    owner_node_id: String,
    fence: i64,
    revision: i64,
    expires_at_unix_ms: i64,
}

impl From<&mut Row<'_>> for LeaseRow {
    fn from(row: &mut Row<'_>) -> Self {
        Self {
            resource: row.get("resource"),
            owner_node_id: row.get("owner_node_id"),
            fence: row.get("fence"),
            revision: row.get("revision"),
            expires_at_unix_ms: row.get("expires_at_ms"),
        }
    }
}

impl TryFrom<LeaseRow> for Lease {
    type Error = StoreError;

    fn try_from(row: LeaseRow) -> Result<Self, Self::Error> {
        let fence = u64::try_from(row.fence).map_err(|error| {
            StoreError::Database(format!("replicated lease fence is invalid: {error}"))
        })?;
        if fence == 0 {
            return Err(StoreError::Database(
                "replicated lease fence must be greater than zero".to_owned(),
            ));
        }
        let revision = u64::try_from(row.revision).map_err(|error| {
            StoreError::Database(format!("replicated lease revision is invalid: {error}"))
        })?;
        if revision == 0 {
            return Err(StoreError::Database(
                "replicated lease revision must be greater than zero".to_owned(),
            ));
        }
        Ok(Self {
            resource: row.resource,
            owner_node_id: row.owner_node_id,
            fence,
            revision,
            expires_at_unix_ms: row.expires_at_unix_ms,
        })
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
