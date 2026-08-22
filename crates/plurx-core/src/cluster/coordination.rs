//! Backend-neutral cluster coordination primitives.
//!
//! The store owns atomic lease state; this module owns node identity, clocks,
//! and TTL policy. Keeping those concerns separate lets backend contracts race
//! explicit node ids without constructing a daemon.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::StoreError;
use crate::store::Store;

pub const MIN_LEASE_TTL: Duration = Duration::from_secs(1);
pub const MAX_LEASE_TTL: Duration = Duration::from_secs(5 * 60);
pub(crate) const MAX_LEASE_RESOURCE_BYTES: usize = 256;
pub(crate) const MAX_LEASE_OWNER_BYTES: usize = 128;
const REMOVED_JOB_OWNER_PREFIX: &str = "internal.cluster_job_owner_removed.";

pub(crate) fn removed_job_owner_key(node_id: &str) -> String {
    format!("{REMOVED_JOB_OWNER_PREFIX}{node_id}")
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Lease {
    pub resource: String,
    pub owner_node_id: String,
    pub fence: u64,
    pub revision: u64,
    pub expires_at_unix_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaseClaim {
    Acquired(Lease),
    Held {
        owner_node_id: String,
        fence: u64,
        expires_at_unix_ms: i64,
    },
}

/// Supplies the local node id, wall clock, and bounded TTL around raw store
/// operations. Durable publishers still carry the returned [`Lease`] into the
/// fenced transaction that publishes their work.
#[derive(Clone)]
pub struct StoreCoordinator {
    store: Arc<dyn Store>,
    node_id: String,
}

impl StoreCoordinator {
    pub fn new(store: Arc<dyn Store>, node_id: impl Into<String>) -> Result<Self, StoreError> {
        let node_id = node_id.into();
        validate_component("lease owner node id", &node_id, MAX_LEASE_OWNER_BYTES)?;
        Ok(Self { store, node_id })
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub async fn acquire(&self, resource: &str, ttl: Duration) -> Result<LeaseClaim, StoreError> {
        let now = unix_ms()?;
        self.store
            .acquire_lease(resource, &self.node_id, now, expiry_from(now, ttl)?)
            .await
    }

    pub async fn renew(&self, lease: &Lease, ttl: Duration) -> Result<Option<Lease>, StoreError> {
        if lease.owner_node_id != self.node_id {
            return Ok(None);
        }
        let now = unix_ms()?;
        self.store
            .renew_lease(lease, now, expiry_from(now, ttl)?)
            .await
    }

    pub async fn release(&self, lease: &Lease) -> Result<bool, StoreError> {
        if lease.owner_node_id != self.node_id {
            return Ok(false);
        }
        self.store.release_lease(lease, unix_ms()?).await
    }
}

pub(crate) fn validate_lease_input(
    resource: &str,
    owner_node_id: &str,
    now_unix_ms: i64,
    expires_at_unix_ms: i64,
) -> Result<(), StoreError> {
    validate_lease_identity(resource, owner_node_id)?;
    if expires_at_unix_ms <= now_unix_ms {
        return Err(StoreError::Database(format!(
            "lease expiry {expires_at_unix_ms} must be later than now {now_unix_ms}"
        )));
    }
    Ok(())
}

pub(crate) fn validate_lease_renewal(
    lease: &Lease,
    now_unix_ms: i64,
    expires_at_unix_ms: i64,
) -> Result<(), StoreError> {
    validate_lease_input(
        &lease.resource,
        &lease.owner_node_id,
        now_unix_ms,
        expires_at_unix_ms,
    )?;
    if expires_at_unix_ms <= lease.expires_at_unix_ms {
        return Err(StoreError::Database(format!(
            "lease renewal expiry {expires_at_unix_ms} must be later than token expiry {}",
            lease.expires_at_unix_ms
        )));
    }
    Ok(())
}

pub(crate) fn validate_lease_identity(
    resource: &str,
    owner_node_id: &str,
) -> Result<(), StoreError> {
    validate_component("lease resource", resource, MAX_LEASE_RESOURCE_BYTES)?;
    validate_component("lease owner node id", owner_node_id, MAX_LEASE_OWNER_BYTES)
}

fn validate_component(label: &str, value: &str, max_bytes: usize) -> Result<(), StoreError> {
    if value.is_empty() || value.len() > max_bytes {
        return Err(StoreError::Database(format!(
            "{label} must contain 1..={max_bytes} bytes"
        )));
    }
    Ok(())
}

fn expiry_from(now_unix_ms: i64, ttl: Duration) -> Result<i64, StoreError> {
    let ttl = ttl.clamp(MIN_LEASE_TTL, MAX_LEASE_TTL);
    let ttl_ms = i64::try_from(ttl.as_millis())
        .map_err(|error| StoreError::Task(format!("lease TTL is out of range: {error}")))?;
    now_unix_ms
        .checked_add(ttl_ms)
        .ok_or_else(|| StoreError::Task("lease expiry overflowed i64".to_owned()))
}

pub(crate) fn unix_ms() -> Result<i64, StoreError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| StoreError::Task(format!("system clock precedes unix epoch: {error}")))?
        .as_millis();
    i64::try_from(millis)
        .map_err(|error| StoreError::Task(format!("unix millisecond clock overflowed: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_rejects_empty_oversized_and_nonfuture_inputs() {
        assert!(validate_lease_input("", "node-a", 10, 11).is_err());
        assert!(validate_lease_input("scan", "", 10, 11).is_err());
        assert!(validate_lease_input(&"x".repeat(257), "node-a", 10, 11).is_err());
        assert!(validate_lease_input("scan", &"x".repeat(129), 10, 11).is_err());
        assert!(validate_lease_input("scan", "node-a", 10, 10).is_err());
        validate_lease_input("scan", "node-a", 10, 11).expect("valid lease input");

        let lease = Lease {
            resource: "scan".to_owned(),
            owner_node_id: "node-a".to_owned(),
            fence: 1,
            revision: 1,
            expires_at_unix_ms: 20,
        };
        assert!(validate_lease_renewal(&lease, 10, 20).is_err());
        assert!(validate_lease_renewal(&lease, 10, 19).is_err());
        validate_lease_renewal(&lease, 10, 21).expect("monotone renewal");
    }

    #[test]
    fn ttl_is_clamped_and_overflow_is_rejected() {
        assert_eq!(
            expiry_from(100, Duration::ZERO).expect("minimum TTL expiry"),
            1_100
        );
        assert_eq!(
            expiry_from(100, Duration::from_secs(60 * 60)).expect("maximum TTL expiry"),
            300_100
        );
        assert!(expiry_from(i64::MAX, Duration::from_secs(1)).is_err());
    }
}
