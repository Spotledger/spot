//! The tenant directory, the result sink and the audit history over the result table.
//!
//! All three are synchronous (the job's traits are) and reach the database through the worker
//! thread of [`SyncPg`].

use std::sync::Arc;

use spot_journal_audit::{Position, VerifiedPosition};
use spot_journal_audit_job::{
    AuditJobError, Cadence, ResultSink, TenantAuditPlan, TenantDirectory, TenantOutcome,
    TenantResult,
};
use spot_journal_audit_run::AuditRun;
use spot_journal_recovery::{AuditFailure, AuditHistory, FactResult, FactUnreadable};
use spot_journal_store::TenantId;

use crate::error::{HostError, db_error};
use crate::schema::RETENTION_MILLIS;
use crate::worker::SyncPg;

/// What the host knows about one tenant's schedule: whether its migration replay policy is
/// `always` (audited daily instead of weekly).
pub trait ReplayPolicies: Send + Sync {
    /// Whether `tenant` is on the `always` replay policy.
    fn is_always(&self, tenant: &str) -> bool;
}

/// Every tenant weekly (the default when no policy source exists).
pub struct AllWeekly;

impl ReplayPolicies for AllWeekly {
    fn is_always(&self, _tenant: &str) -> bool {
        false
    }
}

/// The sites on this host that have a journal, with their cadence and last run.
pub struct PgTenantDirectory {
    db: Arc<SyncPg>,
    tenants: Vec<TenantId>,
    policies: Arc<dyn ReplayPolicies>,
}

impl PgTenantDirectory {
    /// A directory over `tenants` (the journaled sites of the host; the host lists them).
    #[must_use]
    pub fn new(db: Arc<SyncPg>, tenants: Vec<TenantId>, policies: Arc<dyn ReplayPolicies>) -> Self {
        Self {
            db,
            tenants,
            policies,
        }
    }
}

impl TenantDirectory for PgTenantDirectory {
    fn tenants(&self) -> Result<Vec<TenantAuditPlan>, AuditJobError> {
        let failed = || AuditJobError::ProviderFailed {
            provider: "directory",
        };
        let last: Vec<(String, i64)> = self
            .db
            .run(|pool| async move {
                let client = pool.get().await.map_err(|_| HostError::Worker)?;
                let rows = client
                    .query(
                        "SELECT tenant, max(started_millis) FROM spot_journal_audit_result \
                         GROUP BY tenant",
                        &[],
                    )
                    .await
                    .map_err(|e| db_error(&e))?;
                rows.iter()
                    .map(|row| {
                        Ok((
                            row.try_get::<_, String>(0).map_err(|e| db_error(&e))?,
                            row.try_get::<_, i64>(1).map_err(|e| db_error(&e))?,
                        ))
                    })
                    .collect()
            })
            .map_err(|_| failed())?;
        Ok(self
            .tenants
            .iter()
            .map(|tenant| TenantAuditPlan {
                tenant: tenant.clone(),
                cadence: if self.policies.is_always(tenant.as_str()) {
                    Cadence::Daily
                } else {
                    Cadence::Weekly
                },
                last_run_millis: last
                    .iter()
                    .find(|(t, _)| t == tenant.as_str())
                    .map(|(_, at)| *at),
            })
            .collect())
    }
}

/// One row, flattened.
struct Row {
    tenant: String,
    started: i64,
    finished: i64,
    outcome: &'static str,
    stage: Option<&'static str>,
    code: Option<&'static str>,
    verified: Option<(i64, i64)>,
    live: Option<i64>,
    rebuilt: Option<i64>,
    differences: Option<i64>,
    first: Option<(String, String, &'static str)>,
    rebuild_millis: Option<i64>,
    rto_breached: Option<bool>,
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn row_of(tenant: &TenantId, result: &TenantResult) -> Row {
    let mut row = Row {
        tenant: tenant.as_str().to_owned(),
        started: result.started_millis,
        finished: result.finished_millis,
        outcome: "failed",
        stage: None,
        code: None,
        verified: None,
        live: None,
        rebuilt: None,
        differences: None,
        first: None,
        rebuild_millis: None,
        rto_breached: None,
    };
    match result.outcome {
        TenantOutcome::Clean => row.outcome = "clean",
        TenantOutcome::Mismatch => row.outcome = "mismatch",
        TenantOutcome::Failed(info) => {
            row.stage = Some(info.stage);
            row.code = Some(info.code);
        }
    }
    if let Some(AuditRun::Completed(report)) = &result.run {
        let totals = &report.result.totals;
        row.live = Some(to_i64(totals.live));
        row.rebuilt = Some(to_i64(totals.rebuilt));
        row.differences = Some(to_i64(totals.differences()));
        row.rebuild_millis = Some(to_i64(report.rebuild_millis));
        row.rto_breached = Some(!report.rto.is_within());
        row.verified = report
            .verified
            .map(|v| (to_i64(v.position.generation), to_i64(v.position.seq)));
        if let Some(first) = report.result.outcome.first_difference() {
            // The identity of the record and the kind of difference: never a field value.
            row.first = Some((first.entity.clone(), first.id.clone(), first.kind.name()));
        }
    } else if let Some(AuditRun::Failed(failure)) = &result.run {
        row.stage = row.stage.or(Some("rebuild"));
        row.code = row.code.or(Some(failure.code));
    }
    row
}

/// Records the per-tenant result rows and keeps fifty-two weeks.
pub struct PgResultSink {
    db: Arc<SyncPg>,
}

impl PgResultSink {
    /// A sink over `db`.
    #[must_use]
    pub fn new(db: Arc<SyncPg>) -> Self {
        Self { db }
    }
}

impl ResultSink for PgResultSink {
    fn record(&self, tenant: &TenantId, result: &TenantResult) -> Result<(), AuditJobError> {
        let row = row_of(tenant, result);
        let cutoff = result.finished_millis.saturating_sub(RETENTION_MILLIS);
        self.db
            .run(move |pool| async move {
                let mut client = pool.get().await.map_err(|_| HostError::Worker)?;
                let tx = client.transaction().await.map_err(|e| db_error(&e))?;
                let (first_entity, first_id, first_kind) = match &row.first {
                    Some((e, i, k)) => (Some(e.clone()), Some(i.clone()), Some((*k).to_owned())),
                    None => (None, None, None),
                };
                let (vg, vs) = match row.verified {
                    Some((g, s)) => (Some(g), Some(s)),
                    None => (None, None),
                };
                tx.execute(
                    "INSERT INTO spot_journal_audit_result \
                       (tenant, started_millis, finished_millis, outcome, failure_stage, \
                        failure_code, verified_generation, verified_seq, records_live, \
                        records_rebuilt, differences, first_entity, first_id, first_kind, \
                        rebuild_millis, rto_breached) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
                    &[
                        &row.tenant,
                        &row.started,
                        &row.finished,
                        &row.outcome,
                        &row.stage,
                        &row.code,
                        &vg,
                        &vs,
                        &row.live,
                        &row.rebuilt,
                        &row.differences,
                        &first_entity,
                        &first_id,
                        &first_kind,
                        &row.rebuild_millis,
                        &row.rto_breached,
                    ],
                )
                .await
                .map_err(|e| db_error(&e))?;
                tx.execute(
                    "DELETE FROM spot_journal_audit_result WHERE tenant = $1 AND started_millis < $2",
                    &[&row.tenant, &cutoff],
                )
                .await
                .map_err(|e| db_error(&e))?;
                tx.commit().await.map_err(|e| db_error(&e))
            })
            .map_err(|_| AuditJobError::ProviderFailed { provider: "sink" })
    }
}

/// The audit history of one tenant, for the recovery report.
pub struct PgAuditHistory {
    db: Arc<SyncPg>,
    tenant: String,
}

impl PgAuditHistory {
    /// The history of `tenant`.
    #[must_use]
    pub fn new(db: Arc<SyncPg>, tenant: &TenantId) -> Self {
        Self {
            db,
            tenant: tenant.as_str().to_owned(),
        }
    }
}

impl AuditHistory for PgAuditHistory {
    fn last_verified(&self) -> FactResult<Option<VerifiedPosition>> {
        let tenant = self.tenant.clone();
        let found: Option<(i64, i64, i64)> = self
            .db
            .run(move |pool| async move {
                let client = pool.get().await.map_err(|_| HostError::Worker)?;
                let rows = client
                    .query(
                        "SELECT verified_generation, verified_seq, finished_millis \
                         FROM spot_journal_audit_result \
                         WHERE tenant = $1 AND outcome = 'clean' AND verified_seq IS NOT NULL \
                         ORDER BY started_millis DESC LIMIT 1",
                        &[&tenant],
                    )
                    .await
                    .map_err(|e| db_error(&e))?;
                match rows.first() {
                    None => Ok(None),
                    Some(row) => Ok(Some((
                        row.try_get(0).map_err(|e| db_error(&e))?,
                        row.try_get(1).map_err(|e| db_error(&e))?,
                        row.try_get(2).map_err(|e| db_error(&e))?,
                    ))),
                }
            })
            .map_err(|_| FactUnreadable::new("audit history"))?;
        found
            .map(|(generation, seq, at)| {
                Ok(VerifiedPosition {
                    position: Position::new(
                        u64::try_from(generation)
                            .map_err(|_| FactUnreadable::new("audit history"))?,
                        u64::try_from(seq).map_err(|_| FactUnreadable::new("audit history"))?,
                    ),
                    at_millis: at,
                })
            })
            .transpose()
    }

    fn last_failure(&self) -> FactResult<Option<AuditFailure>> {
        let tenant = self.tenant.clone();
        let found: Option<(i64, i64)> = self
            .db
            .run(move |pool| async move {
                let client = pool.get().await.map_err(|_| HostError::Worker)?;
                // Only the latest run decides: a clean run after a mismatch clears it.
                let rows = client
                    .query(
                        "SELECT finished_millis, coalesce(differences, 0), outcome \
                         FROM spot_journal_audit_result WHERE tenant = $1 \
                         ORDER BY started_millis DESC LIMIT 1",
                        &[&tenant],
                    )
                    .await
                    .map_err(|e| db_error(&e))?;
                match rows.first() {
                    None => Ok(None),
                    Some(row) => {
                        let outcome: String = row.try_get(2).map_err(|e| db_error(&e))?;
                        if outcome == "mismatch" {
                            Ok(Some((
                                row.try_get(0).map_err(|e| db_error(&e))?,
                                row.try_get(1).map_err(|e| db_error(&e))?,
                            )))
                        } else {
                            Ok(None)
                        }
                    }
                }
            })
            .map_err(|_| FactUnreadable::new("audit history"))?;
        Ok(found.map(|(at, differences)| AuditFailure {
            at_millis: at,
            differing_records: u64::try_from(differences).unwrap_or(0),
        }))
    }
}
