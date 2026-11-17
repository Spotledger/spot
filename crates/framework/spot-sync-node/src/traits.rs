//! The seams: the apply path, the parking store and the catalog.

use spot_sync_closure::{Bundle, BundleRecord, ClosureError, Position, RecordKey};
use spot_sync_skew::Incoming;

use crate::model::{AppliedSet, ApplyRefusal, ParkedBundle};

/// Where bundles are applied. The real apply path (WP-EDG-06) implements it later.
pub trait BundleApply {
    /// Whether a version of `key` at or above `min_version` has been applied **for `tenant`**.
    /// `false` when the record was never applied for that tenant, whatever another tenant holds
    /// under the same entity and id. `min_version = 0` therefore means "applied at all".
    fn has(&self, tenant: &str, key: &RecordKey, min_version: u64) -> bool;

    /// Whether a delete of `key` at a version at or above `min_version` has been applied **for
    /// `tenant`** (and so the record is absent, or was re-created at a later version: then `has`
    /// answers too). The intake asks it to tell a replayed or outdated delete from a new one and to
    /// refuse an old image that a later delete has superseded (a replay must not bring a deleted
    /// record back). `false` for a record never deleted for that tenant.
    fn has_tombstone(&self, tenant: &str, key: &RecordKey, min_version: u64) -> bool;

    /// Apply every record of the bundle, for `tenant`, in one transaction: all or none. A record
    /// with `deleted` set is a tombstone: the sink applies it as [`delete`](Self::delete) (the
    /// same effect, inside the bundle's transaction) and reports it in the returned set.
    ///
    /// # Errors
    /// `ApplyRefusal` when the whole bundle is refused (nothing stored).
    fn apply_bundle(&mut self, tenant: &str, bundle: &Bundle) -> Result<AppliedSet, ApplyRefusal>;

    /// Delete `key` for `tenant`: the record was deleted at the hub by the event at `position`,
    /// and `version` is the version the sink then holds for the deletion (the same value the
    /// sink reports in `AppliedRecord::version`: the position of the record's latest event).
    ///
    /// The effect must be idempotent: a delete the sink already applied (`has_tombstone` true at
    /// `version`) changes nothing a second time, and a record re-created at a later version is
    /// not deleted by an older tombstone. Deleting a record the sink never had is not an error
    /// (the destination has nothing to delete); the delete is still remembered, so that a
    /// replayed older image does not bring the record back.
    ///
    /// # Errors
    /// `ApplyRefusal` when the delete is refused (nothing changed).
    fn delete(
        &mut self,
        tenant: &str,
        key: &RecordKey,
        version: u64,
        position: Position,
    ) -> Result<(), ApplyRefusal>;
}

/// Where bundles wait for the records they reference. One queue per tenant, never mixed.
pub trait ParkingStore {
    /// Park a bundle in its tenant's queue; the queue stays ordered by `seq`.
    fn park(&mut self, parked: ParkedBundle);

    /// Mark `applied_keys` as no longer missing in the tenant's queue, then remove and return
    /// the bundles with nothing left missing, in arrival order.
    fn ready(&mut self, tenant: &str, applied_keys: &[RecordKey]) -> Vec<ParkedBundle>;

    /// Remove and return the oldest parked bundle of the tenant.
    fn take_oldest(&mut self, tenant: &str) -> Option<ParkedBundle>;

    /// Number of bundles parked for the tenant.
    fn len(&self, tenant: &str) -> usize;

    /// Whether nothing is parked for the tenant.
    fn is_empty(&self, tenant: &str) -> bool {
        self.len(tenant) == 0
    }

    /// Image bytes parked for the tenant.
    fn bytes(&self, tenant: &str) -> usize;

    /// Remove and return every bundle (all tenants) parked for more than `max_age_ms` at `now_ms`.
    fn expire(&mut self, now_ms: u64, max_age_ms: u64) -> Vec<ParkedBundle>;
}

/// What the node needs from the metadata catalog.
pub trait NodeCatalog {
    /// The declared references of one record image (the same rule the sender used).
    ///
    /// # Errors
    /// `ClosureError::ReferenceUnreadable` when a dynamic link names an undeclared entity.
    fn references(&self, record: &BundleRecord) -> Result<Vec<RecordKey>, ClosureError>;

    /// The topic, schema major and metadata requirement of a record, for version-skew triage.
    /// Never called for a tombstone (`BundleRecord::deleted`): it has no image to decode, so it
    /// cannot be held for skew, and `references` is not called for it either.
    fn classify(&self, record: &BundleRecord) -> Incoming;
}
