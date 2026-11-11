//! The stable `workload_routing.*` error codes of the routing policy engine.
//!
//! Every code here has one entry in `spot-errors`' `catalogue/workload_routing.toml` with its
//! class, a one-sentence summary and a remedy. The codes carry the `workload_routing.` prefix
//! because the catalogue requires a file's name to be its entries' prefix
//! (`crates/framework/spot-errors/README.md`).
//!
//! Most of these describe a *fallback*: the router refused to send work somewhere and served it
//! locally instead. They are recorded as metrics ([`crate::metrics::RoutingEvent`]) and, when a
//! caller needs a typed failure, raised as [`crate::Error`]. None of them names a secret or a
//! raw SQL string.

/// A policy document failed validation (empty rule, duplicate matcher, zero hop budget, ...).
pub const INVALID_POLICY: &str = "workload_routing.invalid_policy";
/// A workload class or node role name in the policy is not one of the known values.
pub const UNKNOWN_CLASS: &str = "workload_routing.unknown_class";
/// A path rule matched, but no node in the topology carries the target label set.
pub const NO_TARGET: &str = "workload_routing.no_target";
/// A forward was refused because the request has already used its whole hop budget.
pub const HOP_LIMIT: &str = "workload_routing.hop_limit";
/// The forwarding transport failed to reach the target node.
pub const FORWARD_FAILED: &str = "workload_routing.forward_failed";
/// A replica was eligible in shape, but its replication lag is over the rule's limit.
pub const REPLICA_LAG: &str = "workload_routing.replica_lag";
/// No replica was reachable or registered, so the read fell back to the primary.
pub const NO_REPLICA: &str = "workload_routing.no_replica";
/// A request that carries a write was refused a replica read (it must read the primary).
pub const WRITE_TO_REPLICA: &str = "workload_routing.write_to_replica";
/// The session wrote recently and no replica has caught up to its high-water token yet.
pub const READ_YOUR_WRITES: &str = "workload_routing.read_your_writes";
