//! Without a recorder every function is a no-op and none panics. This binary never installs one
//! (it must not call `render_prometheus`, which installs it lazily).
#![allow(missing_docs)]

use std::time::Duration;

use spot_ops_signals::{conditions, journal, migration, quarantine, sync};

#[test]
fn nothing_panics_without_a_recorder() {
    journal::set_ship_lag(Duration::from_secs(1));
    journal::set_unshipped_entries(u64::MAX);
    journal::record_ship_batch(journal::BatchOutcome::Failed);
    journal::record_durability_wait(Duration::MAX);
    journal::record_durability_decision(journal::Decision::Halted);
    journal::set_snapshot_age(Duration::ZERO);
    journal::set_audit_age(Duration::MAX);
    journal::record_audit_run(journal::AuditResult::Mismatch);
    sync::record_apply(sync::ApplyOutcome::Quarantined);
    sync::set_send_queue_depth(1);
    sync::set_node_lag(&sync::NodeLag::default());
    sync::set_nodes(&sync::NodeCounts::default());
    sync::set_obligations(&sync::ObligationCounts::default());
    sync::set_holds(0);
    quarantine::set_open(0);
    quarantine::set_oldest_age(Duration::ZERO);
    conditions::set_open(&conditions::OpenCounts::default());
    migration::set_gate_closed(false);
    migration::record_verb(migration::Verb::Plan, migration::VerbOutcome::Ok);
}
