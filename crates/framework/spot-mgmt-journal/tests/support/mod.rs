//! Shared test helpers: invocation builders and a recording fake executor.

#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::sync::Mutex;

use async_trait::async_trait;
use spot_mgmt_ops::{Catalogue, Invocation, ParamValue, Validated};

use spot_mgmt_journal::args::*;
use spot_mgmt_journal::results::*;
use spot_mgmt_journal::{ExecError, ExecKind, JournalOps, build_catalogue, ids};

pub fn catalogue() -> Catalogue {
    build_catalogue().unwrap()
}

pub fn invocation(op: &str, params: &[(&str, ParamValue)]) -> Invocation {
    let mut inv = Invocation::new(ids::op_id(op).unwrap());
    for (name, value) in params {
        inv = inv.with(*name, value.clone());
    }
    inv
}

pub fn text(value: &str) -> ParamValue {
    ParamValue::Text(value.to_owned())
}

pub fn validated(op: &str, params: &[(&str, ParamValue)]) -> Validated {
    catalogue().validate(&invocation(op, params)).unwrap()
}

/// One valid invocation per operation, used by the round-trip and catalogue tests.
pub fn samples() -> Vec<Invocation> {
    let rebuild = [("target", text("at 3:41")), ("mode", text("roll-forward"))];
    let mut run = rebuild.to_vec();
    run.push((
        "confirm_plan_hash",
        text("0123456789abcdef0123456789abcdef"),
    ));
    vec![
        invocation(ids::STATUS, &[]),
        invocation(ids::RECOVERY_REPORT, &[]),
        invocation(ids::SNAPSHOT_LIST, &[("limit", ParamValue::Int(5))]),
        invocation(ids::SNAPSHOT_TAKE, &[]),
        invocation(ids::VERIFY, &[]),
        invocation(ids::AUDIT_LAST, &[]),
        invocation(ids::AUDIT_RUN, &[]),
        invocation(ids::POLICY_GET, &[]),
        invocation(
            ids::POLICY_SET,
            &[
                ("policy", text("remote-sync")),
                ("on_unreachable", text("halt")),
            ],
        ),
        invocation(ids::RESHIP, &[]),
        invocation(ids::REBUILD_PLAN, &rebuild),
        invocation(ids::REBUILD_RUN, &run),
        invocation(ids::READER_LIST, &[]),
        invocation(
            ids::READER_GRANT,
            &[
                ("grantee", text("auditor@example.org")),
                ("days", ParamValue::Int(30)),
                ("external", ParamValue::Bool(true)),
            ],
        ),
        invocation(
            ids::READER_REVOKE,
            &[("grantee", text("auditor@example.org"))],
        ),
        invocation(
            ids::SUBJECT_ERASE,
            &[("subject", text("subject-42")), ("confirm", text("erase"))],
        ),
    ]
}

/// A fake executor that records which method ran, with what, and can be told to fail.
#[derive(Default)]
pub struct Fake {
    pub calls: Mutex<Vec<String>>,
    pub fail_with: Mutex<Option<ExecError>>,
}

impl Fake {
    pub fn failing(kind: ExecKind) -> Self {
        let fake = Self::default();
        *fake.fail_with.lock().unwrap() = Some(ExecError::new(kind, "refused_by_test"));
        fake
    }

    fn record(&self, call: String) -> Result<(), ExecError> {
        self.calls.lock().unwrap().push(call);
        match self.fail_with.lock().unwrap().clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

fn pos() -> Position {
    Position {
        generation: 3,
        seq: 41,
    }
}

fn audit() -> AuditResult {
    AuditResult {
        outcome: AuditOutcome::Match,
        ran_at: Some("2026-11-12T01:00:00Z".to_owned()),
        records_compared: 10,
        first_difference: None,
        reason: None,
    }
}

#[async_trait]
impl JournalOps for Fake {
    async fn status(&self) -> Result<StatusResult, ExecError> {
        self.record("status".into())?;
        Ok(StatusResult {
            state: JournalState::Ok,
            head: Some(pos()),
            shipped: Some(pos()),
            unshipped_segments: 0,
            policy: PolicySetting::ReplicaSync,
        })
    }
    async fn recovery_report(&self) -> Result<RecoveryReportResult, ExecError> {
        self.record("recovery_report".into())?;
        Ok(RecoveryReportResult {
            target_configured: true,
            region: Some("eu-1".into()),
            policy: PolicySetting::Local,
            last_verified: Some(pos()),
            last_verified_at: None,
            oldest_snapshot: None,
            key_backup: KeyBackupState::Current,
            excluded_entities: 2,
        })
    }
    async fn snapshot_list(&self, args: SnapshotListArgs) -> Result<SnapshotListResult, ExecError> {
        self.record(format!("snapshot_list {}", args.limit))?;
        Ok(SnapshotListResult { snapshots: vec![] })
    }
    async fn snapshot_take(&self) -> Result<SnapshotTakeResult, ExecError> {
        self.record("snapshot_take".into())?;
        Ok(SnapshotTakeResult {
            id: "snap-1".into(),
            position: pos(),
        })
    }
    async fn verify(&self) -> Result<VerifyResult, ExecError> {
        self.record("verify".into())?;
        Ok(VerifyResult {
            ok: true,
            segments_checked: 7,
            first_failure: None,
            reason: None,
        })
    }
    async fn audit_last(&self) -> Result<AuditResult, ExecError> {
        self.record("audit_last".into())?;
        Ok(audit())
    }
    async fn audit_run(&self) -> Result<AuditResult, ExecError> {
        self.record("audit_run".into())?;
        Ok(audit())
    }
    async fn policy_get(&self) -> Result<PolicyResult, ExecError> {
        self.record("policy_get".into())?;
        Ok(PolicyResult {
            policy: PolicySetting::Local,
            previous: None,
        })
    }
    async fn policy_set(&self, policy: PolicySetting) -> Result<PolicyResult, ExecError> {
        self.record(format!("policy_set {policy:?}"))?;
        Ok(PolicyResult {
            policy,
            previous: Some(PolicySetting::Local),
        })
    }
    async fn reship(&self) -> Result<ReshipResult, ExecError> {
        self.record("reship".into())?;
        Ok(ReshipResult {
            queued: 2,
            shipped: 1,
        })
    }
    async fn rebuild_plan(&self, args: RebuildPlanArgs) -> Result<RebuildPlanResult, ExecError> {
        self.record(format!(
            "rebuild_plan {} {}",
            args.target.render(),
            args.mode.name()
        ))?;
        Ok(RebuildPlanResult {
            plan_hash: "0123456789abcdef0123456789abcdef".into(),
            mode: args.mode,
            end: pos(),
            snapshot: None,
            segments: 4,
            records: 90,
        })
    }
    async fn rebuild_run(&self, args: RebuildRunArgs) -> Result<RebuildRunResult, ExecError> {
        self.record(format!("rebuild_run {}", args.confirm_plan_hash))?;
        Ok(RebuildRunResult {
            plan_hash: args.confirm_plan_hash,
            records_applied: 90,
            end: pos(),
            verified: true,
        })
    }
    async fn reader_list(&self) -> Result<ReaderListResult, ExecError> {
        self.record("reader_list".into())?;
        Ok(ReaderListResult { readers: vec![] })
    }
    async fn reader_grant(&self, args: ReaderGrantArgs) -> Result<ReaderGrantResult, ExecError> {
        self.record(format!(
            "reader_grant {} {} {}",
            args.grantee, args.days, args.external
        ))?;
        Ok(ReaderGrantResult {
            grantee: args.grantee,
            expires_at: "2026-12-12T00:00:00Z".into(),
        })
    }
    async fn reader_revoke(&self, args: ReaderRevokeArgs) -> Result<ReaderRevokeResult, ExecError> {
        self.record(format!("reader_revoke {}", args.grantee))?;
        Ok(ReaderRevokeResult {
            grantee: args.grantee,
            revoked: true,
        })
    }
    async fn subject_erase(
        &self,
        _args: SubjectEraseArgs,
    ) -> Result<SubjectEraseResult, ExecError> {
        self.record("subject_erase".into())?;
        Ok(SubjectEraseResult {
            keys_destroyed: 1,
            fields_erased: 5,
            fields_retained: 2,
        })
    }
}
