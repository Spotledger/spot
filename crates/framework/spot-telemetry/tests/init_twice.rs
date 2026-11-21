//! `init` installs a process-global subscriber, so it gets its own test binary.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use spot_telemetry::{TelemetryConfig, TelemetryError, init};

#[test]
fn second_init_fails_with_a_typed_error() {
    let _guard = init(TelemetryConfig::default()).unwrap();
    let second = init(TelemetryConfig::default());
    assert!(matches!(second, Err(TelemetryError::AlreadyInitialised)));
}
