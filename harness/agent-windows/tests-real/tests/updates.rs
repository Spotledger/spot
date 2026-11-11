//! Real Windows Update Agent conformance (WP-ADM-05.T3, `updates` in `matrix.toml`).
//!
//! Read-only: the suite never installs an update and never schedules a reboot, because a
//! conformance run must not change what the machine is running. It exercises the real COM
//! path to the Windows Update Agent and the local reboot-required state.

use spot_agent_windows::{Updates, WindowsError};

#[test]
fn reboot_required_is_readable() {
    let api = spot_agent_windows_real::api();
    // A local read (the pending-reboot key); it must always answer with a typed value.
    let _reboot_required: bool = api
        .reboot_required()
        .expect("reboot-required must answer on a Windows machine");
}

#[test]
fn update_agent_is_reachable() {
    let api = spot_agent_windows_real::api();
    let updates = Updates::new(api.clone());

    // `pending` searches the real Windows Update Agent over COM. On the matrix's target
    // the update service is present; a search still needs a reachable source, so an
    // offline VM (run.ps1 sets SPOT_HARNESS_OFFLINE=1) is allowed a typed API error — but
    // never a panic and never a silent pass.
    match updates.pending() {
        Ok(pending) => {
            for update in &pending {
                assert!(!update.id.is_empty(), "an offered update must have an id");
                assert!(!update.installed, "a pending update must not be installed");
            }
        }
        Err(WindowsError::Api { .. }) if spot_agent_windows_real::offline() => {}
        Err(other) => panic!(
            "the Windows Update Agent search must answer (or be offline-typed): {other:?}"
        ),
    }
}
