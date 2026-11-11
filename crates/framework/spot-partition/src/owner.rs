//! Ownership enforcement: the pre-write hook and the replica check, both over one map.
//!
//! ADR-062 G1 and ADR-23: a record is changed only by its owner, and the receiver rejects
//! anything else. This module enforces that at the two places a write can enter the system:
//!
//! * [`OwnerCheck::allow_write`] is the **pre-write** check: a node may change a record only if
//!   it owns every scope key the record carries. [`WriteGate`] is the hook interface the
//!   document engine's pre-write seam holds, so the engine calls it in phase 1 (`before_<op>`)
//!   before anything is written. [`OwnerCheck`] is the implementation.
//! * [`OwnerCheck::allow_replica`] is the **receive** check used by the apply path (WP-EDG-06):
//!   an incoming record is accepted only if the sender owns it, according to the same map.
//!
//! Both consult the same [`PartitionMap`], so a record refused locally is refused on arrival,
//! and a mirror stays read-only wherever it lands. The refusal names the owner
//! ([`NotOwner::owner`]) so a host can say "owned by head office" in plain language.
//!
//! # The engine seam
//!
//! [`OWNER_CHECK_SEAM`] is the named, versioned seam (`partition:owner_check` v1) an engine
//! calls at its pre-write point; [`OwnerCheckSeam`] provides it from a [`PartitionRegistry`].
//! The call carries the acting node, the DocType, the record's scope keys and the operation's
//! instant; the instant is an argument, never read from a clock (ADR-077). The seam is the
//! platform-side enforcement point, so it is a platform seam an app may not replace.

use std::sync::Arc;

use serde_json::{Map as JsonMap, Value as Json};
use spot_app_api::seams::SeamProvider;
use spot_app_api::{AppError, AppResult, BoxFuture, ErrorClass, Seam};
use spot_sync_protocol::NodeId;
use time::OffsetDateTime;

use crate::error::NotOwner;
use crate::map::{PartitionMap, PartitionRegistry};
use crate::scope::ScopeKey;

/// The document engine's pre-write hook: a host holds an `Arc<dyn WriteGate>` and calls
/// [`WriteGate::before_write`] in the write path before a document is written.
pub trait WriteGate: Send + Sync {
    /// Refuses a write of `doctype` by `node` when `node` does not own the record's scope keys.
    ///
    /// # Errors
    ///
    /// [`NotOwner`] naming the acting node, the scope key and the owner the map holds.
    fn before_write(
        &self,
        node: &NodeId,
        doctype: &str,
        scope_values: &[ScopeKey],
    ) -> Result<(), NotOwner>;
}

/// A received record, as the apply path (WP-EDG-06) presents it to the ownership check.
///
/// The sync wire record ([`spot_sync_protocol::Record`]) carries an opaque payload, so the
/// apply path decodes the parts ownership needs and hands them over here: the DocType, the
/// record's origin node and the scope keys the record carries. A record with no scope keys is
/// owned by its origin node.
pub trait ReplicaRecord: Send + Sync {
    /// The DocType the record belongs to.
    fn doctype(&self) -> &str;
    /// The node that created the record.
    fn origin_node(&self) -> &NodeId;
    /// The scope keys the record carries (empty when it has none).
    fn scope_values(&self) -> &[ScopeKey];
}

/// A plain [`ReplicaRecord`] for callers and tests that do not have a decoded document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplicaView {
    /// The DocType.
    pub doctype: String,
    /// The origin node.
    pub origin_node: NodeId,
    /// The scope keys the record carries.
    pub scope_values: Vec<ScopeKey>,
}

impl ReplicaView {
    /// A view from its parts.
    #[must_use]
    pub fn new(
        doctype: impl Into<String>,
        origin_node: NodeId,
        scope_values: Vec<ScopeKey>,
    ) -> Self {
        Self {
            doctype: doctype.into(),
            origin_node,
            scope_values,
        }
    }
}

impl ReplicaRecord for ReplicaView {
    fn doctype(&self) -> &str {
        &self.doctype
    }

    fn origin_node(&self) -> &NodeId {
        &self.origin_node
    }

    fn scope_values(&self) -> &[ScopeKey] {
        &self.scope_values
    }
}

/// The pre-write and receive ownership check over one [`PartitionMap`].
///
/// The map is the ownership snapshot for the operation's instant; a host builds it with
/// [`PartitionRegistry::map_at`]. Because the snapshot is fixed, the checks need no clock.
#[derive(Clone, Debug)]
pub struct OwnerCheck {
    map: Arc<PartitionMap>,
}

impl OwnerCheck {
    /// A check over an ownership snapshot.
    #[must_use]
    pub fn new(map: Arc<PartitionMap>) -> Self {
        Self { map }
    }

    /// The snapshot the check consults.
    #[must_use]
    pub fn map(&self) -> &PartitionMap {
        &self.map
    }

    /// Refuses a write by `node` unless it owns every scope key in `scope_values`.
    ///
    /// A record with no scope keys is a local record: any node may write it, so the check
    /// allows it. A scope key with no assignment is refused, naming no owner.
    ///
    /// # Errors
    ///
    /// [`NotOwner`] for the first scope key `node` does not own.
    pub fn allow_write(
        &self,
        node: &NodeId,
        doctype: &str,
        scope_values: &[ScopeKey],
    ) -> Result<(), NotOwner> {
        for scope in scope_values {
            let owner = self.map.owner(scope);
            if owner != Some(node) {
                return Err(NotOwner::new(
                    node.clone(),
                    doctype.to_owned(),
                    Some(scope.clone()),
                    owner.cloned(),
                ));
            }
        }
        Ok(())
    }

    /// Refuses a received record unless `sender` owns it according to the map.
    ///
    /// A record with scope keys must be sent by the node that owns every one of them. A record
    /// with no scope keys is owned by its origin node, so only the origin may send it.
    ///
    /// # Errors
    ///
    /// [`NotOwner`] naming the sender, the scope key and the owner the map holds.
    pub fn allow_replica(
        &self,
        sender: &NodeId,
        record: &dyn ReplicaRecord,
    ) -> Result<(), NotOwner> {
        let scope_values = record.scope_values();
        if scope_values.is_empty() {
            let origin = record.origin_node();
            if sender != origin {
                return Err(NotOwner::new(
                    sender.clone(),
                    record.doctype().to_owned(),
                    None,
                    Some(origin.clone()),
                ));
            }
            return Ok(());
        }
        for scope in scope_values {
            let owner = self.map.owner(scope);
            if owner != Some(sender) {
                return Err(NotOwner::new(
                    sender.clone(),
                    record.doctype().to_owned(),
                    Some(scope.clone()),
                    owner.cloned(),
                ));
            }
        }
        Ok(())
    }
}

impl WriteGate for OwnerCheck {
    fn before_write(
        &self,
        node: &NodeId,
        doctype: &str,
        scope_values: &[ScopeKey],
    ) -> Result<(), NotOwner> {
        self.allow_write(node, doctype, scope_values)
    }
}

/// The one pre-write decision: resolve the ownership map at `at` and refuse unless `node` owns
/// every key in `scope_values`.
///
/// Both [`OwnerCheckSeam`] and the engine's `OwnerParticipant` call this, so the seam and the
/// transaction participant cannot disagree. A key with no assignment at `at` is refused.
///
/// # Errors
///
/// `partition.not_owner` (class `PermissionDenied`) for the first key `node` does not own.
pub fn check_write(
    registry: &dyn PartitionRegistry,
    node: &NodeId,
    doctype: &str,
    scope_values: &[ScopeKey],
    at: OffsetDateTime,
) -> Result<(), AppError> {
    let map = Arc::new(registry.map_at(at));
    OwnerCheck::new(map)
        .allow_write(node, doctype, scope_values)
        .map_err(|e| AppError::new(ErrorClass::PermissionDenied, e.code(), e.to_string()))
}

/// The named, versioned pre-write ownership seam (ADR-08): `partition:owner_check` v1.
///
/// The engine calls it before a write with a JSON request:
///
/// ```json
/// {
///   "node": "edge-khi#branch_01",
///   "doctype": "stock_move",
///   "scope_values": [{ "key_type": "warehouse", "value": "W7" }],
///   "at_unix_seconds": 1790000000,
///   "at_nanos": 0
/// }
/// ```
///
/// It answers `{}` when the write is allowed, and fails with `partition.not_owner` otherwise.
pub const OWNER_CHECK_SEAM: Seam<Json, Json> = Seam::new("partition:owner_check", 1);

/// Provides [`OWNER_CHECK_SEAM`] from a [`PartitionRegistry`].
///
/// A host registers it in the seam registry; the engine's pre-write seam then enforces
/// ownership without knowing anything about scope keys.
#[derive(Clone)]
pub struct OwnerCheckSeam {
    registry: Arc<dyn PartitionRegistry>,
}

impl OwnerCheckSeam {
    /// A provider over the ownership registry.
    #[must_use]
    pub fn new(registry: Arc<dyn PartitionRegistry>) -> Self {
        Self { registry }
    }

    /// The synchronous body of a seam call: parse, resolve the map at the request's instant and
    /// run the check. Kept separate from [`SeamProvider::call`] so it can be tested without a
    /// runtime.
    fn check(&self, input: &Json) -> Result<Json, AppError> {
        let node = field_str(input, "node")?;
        let doctype = field_str(input, "doctype")?;
        let scope_values = scope_values(input.get("scope_values"))?;
        let at = instant(input)?;
        let node = NodeId::new(node)
            .map_err(|e| bad(format!("the owner check seam node is invalid: {e}")))?;
        check_write(self.registry.as_ref(), &node, doctype, &scope_values, at)?;
        Ok(Json::Object(JsonMap::new()))
    }
}

impl SeamProvider for OwnerCheckSeam {
    fn call(&self, input: Json) -> BoxFuture<'_, AppResult<Json>> {
        Box::pin(async move { self.check(&input) })
    }
}

fn field_str<'a>(input: &'a Json, name: &str) -> Result<&'a str, AppError> {
    input
        .get(name)
        .and_then(Json::as_str)
        .ok_or_else(|| bad(format!("the owner check seam needs a string `{name}`")))
}

fn scope_values(value: Option<&Json>) -> Result<Vec<ScopeKey>, AppError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let array = value
        .as_array()
        .ok_or_else(|| bad("the owner check seam `scope_values` is not an array"))?;
    let mut keys = Vec::with_capacity(array.len());
    for entry in array {
        let key_type = entry
            .get("key_type")
            .and_then(Json::as_str)
            .ok_or_else(|| bad("an owner check scope key needs a string `key_type`"))?;
        let value = entry
            .get("value")
            .and_then(Json::as_str)
            .ok_or_else(|| bad("an owner check scope key needs a string `value`"))?;
        keys.push(
            ScopeKey::new(key_type, value)
                .map_err(|e| bad(format!("an owner check scope key is invalid: {e}")))?,
        );
    }
    Ok(keys)
}

fn instant(input: &Json) -> Result<OffsetDateTime, AppError> {
    let seconds = input
        .get("at_unix_seconds")
        .and_then(Json::as_i64)
        .ok_or_else(|| bad("the owner check seam needs an integer `at_unix_seconds`"))?;
    let nanos = input
        .get("at_nanos")
        .and_then(Json::as_u64)
        .ok_or_else(|| bad("the owner check seam needs an integer `at_nanos`"))?;
    let nanos =
        u32::try_from(nanos).map_err(|_| bad("the owner check seam `at_nanos` is out of range"))?;
    OffsetDateTime::from_unix_timestamp(seconds)
        .and_then(|at| at.replace_nanosecond(nanos))
        .map_err(|e| bad(format!("the owner check seam instant is invalid: {e}")))
}

fn bad(message: impl Into<String>) -> AppError {
    AppError::new(ErrorClass::Internal, "partition.invalid_argument", message)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::map::{AssignmentRequest, MemoryPartitionRegistry, PartitionRegistry};
    use serde_json::json;

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(seconds).unwrap()
    }

    fn registry_with_b7() -> Arc<dyn PartitionRegistry> {
        let registry = MemoryPartitionRegistry::new();
        registry
            .assign(AssignmentRequest {
                scope: ScopeKey::new("warehouse", "W7").unwrap(),
                node: NodeId::new("edge-khi#branch_07").unwrap(),
                effective_from: at(1_000),
                effective_to: None,
                actor: "admin".to_owned(),
                reason: "branch 7 opens".to_owned(),
                at: at(1_000),
            })
            .unwrap();
        Arc::new(registry)
    }

    #[test]
    fn seam_allows_the_owner_and_refuses_another_node() {
        let seam = OwnerCheckSeam::new(registry_with_b7());
        let owner = json!({
            "node": "edge-khi#branch_07",
            "doctype": "stock_move",
            "scope_values": [{ "key_type": "warehouse", "value": "W7" }],
            "at_unix_seconds": 2_000,
            "at_nanos": 0
        });
        assert!(seam.check(&owner).is_ok());

        let other = json!({
            "node": "edge-khi#branch_09",
            "doctype": "stock_move",
            "scope_values": [{ "key_type": "warehouse", "value": "W7" }],
            "at_unix_seconds": 2_000,
            "at_nanos": 0
        });
        let error = seam.check(&other).unwrap_err();
        assert_eq!(error.code, "partition.not_owner");
        assert!(error.message.contains("branch_07"), "names the owner");
    }

    #[test]
    fn seam_refuses_a_malformed_request() {
        let seam = OwnerCheckSeam::new(registry_with_b7());
        let error = seam.check(&json!({ "doctype": "stock_move" })).unwrap_err();
        assert_eq!(error.code, "partition.invalid_argument");
    }
}
