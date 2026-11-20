//! Elevation on the host (WP-EXT-06.T8): the audit sink over a log port, and the save gate
//! through the real document engine on the in-memory backend. The library's own rules are
//! `spot-server-scripts/tests/elevation.rs`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_app_api::prelude::OffsetDateTime;
use spot_app_api::{AppError, AppResult, BoxFuture, ErrorClass};
use spot_cli::services::scripts::elevation::{ChainElevationAudit, ElevationEntry, ElevationLog};
use spot_server_scripts::elevation::{ElevatedKind, ElevatedRun, ElevationAudit};

struct MemoryLog {
    entries: Mutex<Vec<ElevationEntry>>,
    fail: Mutex<bool>,
}

impl ElevationLog for MemoryLog {
    fn append<'a>(&'a self, entry: ElevationEntry) -> BoxFuture<'a, AppResult<()>> {
        let failing = *self.fail.lock().unwrap();
        if !failing {
            self.entries.lock().unwrap().push(entry);
        }
        Box::pin(async move {
            if failing {
                Err(AppError::new(
                    ErrorClass::Internal,
                    "test.chain_down",
                    "down",
                ))
            } else {
                Ok(())
            }
        })
    }
}

fn run() -> ElevatedRun {
    ElevatedRun {
        script: "rates".to_owned(),
        kind: ElevatedKind::Api,
        triggering_user: "u1".to_owned(),
        doctype: None,
        operation: "call".to_owned(),
        started: OffsetDateTime::UNIX_EPOCH,
        outcome: "ok".to_owned(),
        host_calls: 1,
        written_documents: 0,
    }
}

#[tokio::test]
async fn an_elevated_run_reaches_the_chain_twice_and_a_down_chain_refuses_it() {
    let log = Arc::new(MemoryLog {
        entries: Mutex::new(Vec::new()),
        fail: Mutex::new(false),
    });
    let audit = ChainElevationAudit::new(log.clone());
    audit.reserve(&run()).await.unwrap();
    audit.record(&run()).await.unwrap();
    let entries = log.entries.lock().unwrap().clone();
    assert_eq!(
        entries.iter().map(|e| e.stage).collect::<Vec<_>>(),
        ["reserved", "finished"]
    );
    assert_eq!(entries[1].run.triggering_user, "u1");

    *log.fail.lock().unwrap() = true;
    let error = audit.reserve(&run()).await.unwrap_err();
    assert_eq!(error.code, spot_server_scripts::ELEVATION_AUDIT_FAILED);
    assert!(
        !error.message.contains("down"),
        "the chain's own message is not passed on"
    );
    let _ = Duration::ZERO;
}
