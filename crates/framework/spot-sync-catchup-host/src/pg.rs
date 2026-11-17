//! The durable progress table of the director: one row per node, so a restart of the hub resumes
//! a bootstrap or a catch-up where it was.

use std::sync::Arc;

use spot_pg_bridge::Bridge;

use crate::director::{Phase, Progress, ProgressStore};
use crate::error::HostError;

/// The idempotent DDL (one statement). The host's `schema::extra_tables` carries it, so
/// `spot migrate` creates the table and the serving host never does. One database is one site:
/// no tenant column.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS spot_sync_catchup_progress (
    node              text PRIMARY KEY,
    phase             text NOT NULL CHECK (phase IN ('catch_up', 'bootstrap')),
    total_chunks      bigint NOT NULL CHECK (total_chunks >= 0),
    done_chunks       bigint NOT NULL CHECK (done_chunks >= 0),
    updated_at        timestamptz NOT NULL DEFAULT now()
);
"#;

const GET: &str =
    "SELECT phase, total_chunks, done_chunks FROM spot_sync_catchup_progress WHERE node = $1";
const PUT: &str = "INSERT INTO spot_sync_catchup_progress (node, phase, total_chunks, done_chunks) \
                   VALUES ($1, $2, $3, $4) \
                   ON CONFLICT (node) DO UPDATE SET phase = EXCLUDED.phase, \
                   total_chunks = EXCLUDED.total_chunks, done_chunks = EXCLUDED.done_chunks, \
                   updated_at = now()";
const DEL: &str = "DELETE FROM spot_sync_catchup_progress WHERE node = $1";

/// The progress table over a bridge (the hub's shared one).
#[derive(Clone)]
pub struct PgProgress {
    bridge: Arc<Bridge>,
}

impl PgProgress {
    /// A store reaching the database through `bridge`.
    #[must_use]
    pub fn new(bridge: Arc<Bridge>) -> Self {
        Self { bridge }
    }
}

fn phase_text(phase: Phase) -> &'static str {
    match phase {
        Phase::CatchUp => "catch_up",
        Phase::Bootstrap => "bootstrap",
    }
}

impl ProgressStore for PgProgress {
    fn load(&self, node: &str) -> Result<Option<Progress>, HostError> {
        let node = node.to_owned();
        let row = self
            .bridge
            .run_default(move |conn| {
                Box::pin(async move {
                    let row = conn.query_opt(GET, &[&node]).await?;
                    row.map(|r| {
                        Ok::<(String, i64, i64), tokio_postgres::Error>((
                            r.try_get(0)?,
                            r.try_get(1)?,
                            r.try_get(2)?,
                        ))
                    })
                    .transpose()
                })
            })
            .map_err(|_| HostError::Store)?;
        let Some((phase, total, done)) = row else {
            return Ok(None);
        };
        let phase = match phase.as_str() {
            "catch_up" => Phase::CatchUp,
            "bootstrap" => Phase::Bootstrap,
            _ => return Err(HostError::Store),
        };
        Ok(Some(Progress {
            phase,
            total_chunks: u64::try_from(total).map_err(|_| HostError::Store)?,
            done_chunks: u64::try_from(done).map_err(|_| HostError::Store)?,
        }))
    }

    fn save(&self, node: &str, progress: &Progress) -> Result<(), HostError> {
        let node = node.to_owned();
        let phase = phase_text(progress.phase);
        let total = i64::try_from(progress.total_chunks).map_err(|_| HostError::Store)?;
        let done = i64::try_from(progress.done_chunks).map_err(|_| HostError::Store)?;
        self.bridge
            .run_default(move |conn| {
                Box::pin(async move { conn.execute(PUT, &[&node, &phase, &total, &done]).await })
            })
            .map(|_| ())
            .map_err(|_| HostError::Store)
    }

    fn clear(&self, node: &str) -> Result<(), HostError> {
        let node = node.to_owned();
        self.bridge
            .run_default(move |conn| Box::pin(async move { conn.execute(DEL, &[&node]).await }))
            .map(|_| ())
            .map_err(|_| HostError::Store)
    }
}
