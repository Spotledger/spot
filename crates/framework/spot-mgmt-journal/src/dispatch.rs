//! [`dispatch`]: maps a validated invocation to the one [`JournalOps`] method it names.
//!
//! The dispatcher trusts nothing about its input beyond the type: it refuses an operation it
//! does not know, a parameter the operation does not declare, a missing or wrongly typed
//! parameter, and the combinations the schema cannot express. It selects no tenant and checks
//! no permission (the executor is bound to both).

use serde::Serialize;
use serde_json::Value;
use spot_mgmt_ops::{ParamValue, Validated};

use crate::args::{
    OnUnreachable, PolicySetting, ReaderGrantArgs, ReaderRevokeArgs, RebuildMode, RebuildPlanArgs,
    RebuildRunArgs, RebuildTarget, SnapshotListArgs, SubjectEraseArgs, bad,
};
use crate::catalogue::ERASE_CONFIRM;
use crate::error::OpError;
use crate::ids;
use crate::ops::JournalOps;

/// What the dispatcher reads out of a [`Validated`], by name.
struct Params<'a> {
    op: &'a str,
    validated: &'a Validated,
}

impl<'a> Params<'a> {
    /// Refuses a parameter the operation does not declare (fail closed on a forged value).
    fn only(&self, allowed: &[&str]) -> Result<(), OpError> {
        for accepted in &self.validated.params {
            if !allowed.contains(&accepted.name.as_str()) {
                return Err(bad(
                    self.op,
                    &accepted.name,
                    "not a parameter of this operation",
                ));
            }
        }
        Ok(())
    }

    fn value(&self, name: &str) -> Result<&'a spot_mgmt_ops::AcceptedParam, OpError> {
        self.validated
            .params
            .iter()
            .find(|accepted| accepted.name == name)
            .ok_or_else(|| bad(self.op, name, "required parameter is missing"))
    }

    fn text(&self, name: &str) -> Result<&'a str, OpError> {
        match &self.value(name)?.value {
            ParamValue::Text(text) => Ok(text),
            _ => Err(bad(self.op, name, "expected text")),
        }
    }

    fn int(&self, name: &str) -> Result<i64, OpError> {
        match self.value(name)?.value {
            ParamValue::Int(number) => Ok(number),
            _ => Err(bad(self.op, name, "expected an integer")),
        }
    }

    fn flag(&self, name: &str) -> Result<bool, OpError> {
        match self.value(name)?.value {
            ParamValue::Bool(flag) => Ok(flag),
            _ => Err(bad(self.op, name, "expected a boolean")),
        }
    }

    fn plan(&self) -> Result<RebuildPlanArgs, OpError> {
        let target = RebuildTarget::parse(self.text("target")?)
            .map_err(|reason| bad(self.op, "target", reason))?;
        let mode = RebuildMode::from_name(self.text("mode")?).ok_or_else(|| {
            bad(
                self.op,
                "mode",
                "not one of full, roll-forward, point-in-time",
            )
        })?;
        RebuildPlanArgs::new(target, mode).map_err(|reason| bad(self.op, "mode", reason))
    }

    fn policy(&self) -> Result<PolicySetting, OpError> {
        let on_unreachable = self.value("on_unreachable")?;
        let choice = match &on_unreachable.value {
            ParamValue::Text(text) => OnUnreachable::from_name(text)
                .ok_or_else(|| bad(self.op, "on_unreachable", "not degrade or halt"))?,
            _ => return Err(bad(self.op, "on_unreachable", "expected text")),
        };
        match self.text("policy")? {
            "local" | "replica-sync" if !on_unreachable.from_default => Err(bad(
                self.op,
                "on_unreachable",
                "only remote-sync takes on_unreachable",
            )),
            "local" => Ok(PolicySetting::Local),
            "replica-sync" => Ok(PolicySetting::ReplicaSync),
            "remote-sync" => Ok(PolicySetting::RemoteSync {
                on_unreachable: choice,
            }),
            _ => Err(bad(
                self.op,
                "policy",
                "not local, replica-sync or remote-sync",
            )),
        }
    }
}

/// Serialises a typed result to the stable JSON the management API returns.
fn json<T: Serialize>(op: &str, result: &T) -> Result<Value, OpError> {
    serde_json::to_value(result).map_err(|_| OpError::Serialise { op: op.to_owned() })
}

/// Runs the operation `validated` names on `ops` and returns its typed result as JSON.
///
/// # Errors
/// [`OpError::UnknownOperation`] for an id that is not a journal operation;
/// [`OpError::BadArguments`] when the parameters do not parse into the typed arguments;
/// [`OpError::Exec`] when the executor refuses or fails; [`OpError::Serialise`] when a result
/// cannot become JSON.
pub async fn dispatch(validated: &Validated, ops: &dyn JournalOps) -> Result<Value, OpError> {
    let op = validated.op.as_str();
    let p = Params { op, validated };
    match op {
        ids::STATUS => {
            p.only(&[])?;
            json(op, &ops.status().await?)
        }
        ids::RECOVERY_REPORT => {
            p.only(&[])?;
            json(op, &ops.recovery_report().await?)
        }
        ids::SNAPSHOT_LIST => {
            p.only(&["limit"])?;
            let limit = u32::try_from(p.int("limit")?)
                .ok()
                .filter(|limit| (1..=200).contains(limit))
                .ok_or_else(|| bad(op, "limit", "out of range"))?;
            json(op, &ops.snapshot_list(SnapshotListArgs { limit }).await?)
        }
        ids::SNAPSHOT_TAKE => {
            p.only(&[])?;
            json(op, &ops.snapshot_take().await?)
        }
        ids::VERIFY => {
            p.only(&[])?;
            json(op, &ops.verify().await?)
        }
        ids::AUDIT_LAST => {
            p.only(&[])?;
            json(op, &ops.audit_last().await?)
        }
        ids::AUDIT_RUN => {
            p.only(&[])?;
            json(op, &ops.audit_run().await?)
        }
        ids::POLICY_GET => {
            p.only(&[])?;
            json(op, &ops.policy_get().await?)
        }
        ids::POLICY_SET => {
            p.only(&["policy", "on_unreachable"])?;
            json(op, &ops.policy_set(p.policy()?).await?)
        }
        ids::RESHIP => {
            p.only(&[])?;
            json(op, &ops.reship().await?)
        }
        ids::REBUILD_PLAN => {
            p.only(&["target", "mode"])?;
            json(op, &ops.rebuild_plan(p.plan()?).await?)
        }
        ids::REBUILD_RUN => {
            p.only(&["target", "mode", "confirm_plan_hash"])?;
            let confirm = p.text("confirm_plan_hash")?;
            if confirm.is_empty() {
                return Err(bad(op, "confirm_plan_hash", "required"));
            }
            let args = RebuildRunArgs {
                plan: p.plan()?,
                confirm_plan_hash: confirm.to_owned(),
            };
            json(op, &ops.rebuild_run(args).await?)
        }
        ids::READER_LIST => {
            p.only(&[])?;
            json(op, &ops.reader_list().await?)
        }
        ids::READER_GRANT => {
            p.only(&["grantee", "days", "external"])?;
            let days = u16::try_from(p.int("days")?)
                .ok()
                .filter(|days| (1..=365).contains(days))
                .ok_or_else(|| bad(op, "days", "out of range"))?;
            let args = ReaderGrantArgs {
                grantee: p.text("grantee")?.to_owned(),
                days,
                external: p.flag("external")?,
            };
            json(op, &ops.reader_grant(args).await?)
        }
        ids::READER_REVOKE => {
            p.only(&["grantee"])?;
            let args = ReaderRevokeArgs {
                grantee: p.text("grantee")?.to_owned(),
            };
            json(op, &ops.reader_revoke(args).await?)
        }
        ids::SUBJECT_ERASE => {
            p.only(&["subject", "confirm"])?;
            if p.text("confirm")? != ERASE_CONFIRM {
                return Err(bad(op, "confirm", "must be the word erase"));
            }
            let args = SubjectEraseArgs {
                subject: p.text("subject")?.to_owned(),
            };
            json(op, &ops.subject_erase(args).await?)
        }
        other => Err(OpError::UnknownOperation {
            op: other.to_owned(),
        }),
    }
}
