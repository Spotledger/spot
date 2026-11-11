//! The queue-affinity port (WP-PLT-20.T2): which job queues the local node's workers should
//! consume, as decided by the workload-routing policy (`spot-workload-routing`,
//! `services.workload_routing` in `spot-cli`).
//!
//! `spot-http` does not depend on the routing engine: it defines the seam, and the host's
//! `workload_routing` module provides it from the live policy and this node's role.
//!
//! **Advisory today.** The job worker (`spot-cli`'s `jobs` module) does not read this port yet —
//! that consuming change touches a file owned by another task (see the task's Open questions) —
//! so a node still starts its worker on the queues `services.jobs.queues` names, whatever this
//! port answers. A future change makes the worker ask this port instead and refuse to start a
//! queue the policy does not assign it.

use super::PortKey;

/// The port key of [`QueueAffinityPort`].
pub const KEY: PortKey = PortKey::of::<dyn QueueAffinityPort>();

/// Which queues the local node's job workers should consume, per the live routing policy.
pub trait QueueAffinityPort: Send + Sync {
    /// The queues this node's workers should claim, in policy order (first match wins across
    /// the matching affinity rules, de-duplicated). Never empty: a node no rule matches still
    /// gets the `default` queue.
    fn queues_for_local(&self) -> Vec<String>;

    /// This node's id in the routing topology, for logs and an operator listing.
    fn local_node_id(&self) -> String;
}
