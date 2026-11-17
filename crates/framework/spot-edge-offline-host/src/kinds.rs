//! The wire kinds of the carrier, in one place.
//!
//! The sync contract has not registered these `sys_`-class record kinds yet (an item for the
//! architecture owner). Every other module names them through this one, so the day the registered
//! names differ is a one-file change.

/// The record kind of a lease request (edge to hub).
pub const LEASE_REQUEST_KIND: &str = "sys_lease_request";
/// The record kind of a lease answer (hub to edge).
pub const LEASE_ANSWER_KIND: &str = "sys_lease_answer";
/// The record kind of the permission snapshot exchange. Both directions use it: the edge's
/// request and the hub's issue are told apart by the payload's `type` tag
/// ([`crate::carrier::SnapshotMsg`]), so the registered set stays at three kinds.
pub const SNAPSHOT_KIND: &str = "sys_permission_snapshot";
