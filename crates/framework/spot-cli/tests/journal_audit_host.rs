//! The hosted journal audit: its settings and its run-now gate. The database parts (result
//! table, directory, history, scratch database lifecycle and the leak sweep) are proved against
//! real PostgreSQL in `crates/backends/spot-journal-audit-host-pg/tests/audit_host.rs`; the job,
//! the runner and the recovery report have their own suites. Wiring them over a whole site waits
//! for the journal service's `AuditWiring` port (see the task report).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_cli::services::journal_audit::{RUN_NOW_MIN_GAP_MILLIS, RunNowGate, Settings};

#[test]
fn settings_default_to_one_pass_at_a_time_and_refuse_unknown_keys() {
    let defaults = Settings::default();
    assert!(defaults.enabled);
    assert_eq!(defaults.max_parallel, 1);
    assert_eq!(defaults.check_seconds, 3600);
    let parsed: Settings = toml::from_str("max_parallel = 2\nscratch_role = \"scratch\"").unwrap();
    assert_eq!(parsed.max_parallel, 2);
    assert_eq!(parsed.scratch_role, "scratch");
    // The password is never a setting: only the name of the variable that holds it.
    assert!(toml::from_str::<Settings>("scratch_password = \"x\"").is_err());
    assert!(toml::from_str::<Settings>("bogus = 1").is_err());
}

#[test]
fn run_now_is_one_per_hour_unless_the_previous_run_failed() {
    let gate = RunNowGate::default();
    assert_eq!(gate.try_start(1_000), Ok(()));
    gate.finished(false);
    // Too soon after a run that finished well.
    let wait = gate.try_start(1_000 + 60_000).unwrap_err();
    assert!((3500..=3600).contains(&wait), "{wait}");
    // After the hour it is allowed again.
    assert_eq!(gate.try_start(1_000 + RUN_NOW_MIN_GAP_MILLIS), Ok(()));
    gate.finished(true);
    // A failed run frees the next one at once.
    assert_eq!(gate.try_start(1_000 + RUN_NOW_MIN_GAP_MILLIS + 5), Ok(()));
}
