//! The rehearsal ledger: the report of a rehearsal by plan hash and clock.

use spot_migration_ops::{LifecycleError, PlanHash};

use crate::schema::failed;
use crate::store::{PgMigrationStore, READ_DEADLINE, WRITE_DEADLINE};

pub use spot_migration_ops::{RehearsalLedger, RehearsalRecord};

type Row = (String, i64, String, String);

fn record_of(row: Row) -> Result<RehearsalRecord, LifecycleError> {
    Ok(RehearsalRecord {
        plan_hash: PlanHash::parse(&row.0).map_err(|_| LifecycleError::Store)?,
        at: row.1,
        verdict: row.2,
        report_json: row.3,
    })
}

impl PgMigrationStore {
    fn rehearsals(
        &self,
        hash: &PlanHash,
        newest_only: bool,
    ) -> Result<Vec<RehearsalRecord>, LifecycleError> {
        let key = hash.as_str().to_owned();
        let rows: Vec<Row> = self
            .bridge
            .run(READ_DEADLINE, move |conn| {
                Box::pin(async move {
                    let sql = if newest_only {
                        "SELECT plan_hash, at, verdict, report_json FROM spot_migration_rehearsal \
                         WHERE plan_hash = $1 ORDER BY at DESC LIMIT 1"
                    } else {
                        "SELECT plan_hash, at, verdict, report_json FROM spot_migration_rehearsal \
                         WHERE plan_hash = $1 ORDER BY at LIMIT 1000"
                    };
                    let rows = conn.query(sql, &[&key]).await?;
                    Ok::<_, tokio_postgres::Error>(
                        rows.iter()
                            .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
                            .collect(),
                    )
                })
            })
            .map_err(failed)?;
        rows.into_iter().map(record_of).collect()
    }
}

impl RehearsalLedger for PgMigrationStore {
    fn record(&self, record: RehearsalRecord) -> Result<(), LifecycleError> {
        let RehearsalRecord {
            plan_hash,
            at,
            verdict,
            report_json,
        } = record;
        let hash = plan_hash.as_str().to_owned();
        self.bridge
            .run(WRITE_DEADLINE, move |conn| {
                Box::pin(async move {
                    conn.execute(
                        "INSERT INTO spot_migration_rehearsal (plan_hash, at, verdict, report_json) \
                         VALUES ($1, $2, $3, $4) ON CONFLICT (plan_hash, at) DO NOTHING",
                        &[&hash, &at, &verdict, &report_json],
                    )
                    .await
                })
            })
            .map(|_| ())
            .map_err(failed)
    }

    fn latest(&self, hash: &PlanHash) -> Result<Option<RehearsalRecord>, LifecycleError> {
        Ok(self.rehearsals(hash, true)?.pop())
    }

    fn history(&self, hash: &PlanHash) -> Result<Vec<RehearsalRecord>, LifecycleError> {
        self.rehearsals(hash, false)
    }
}
