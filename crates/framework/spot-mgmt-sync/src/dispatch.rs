//! [`dispatch`]: maps a validated invocation to the one [`SyncOps`] method it names.
//!
//! The dispatcher trusts nothing about its input beyond the type: it refuses an operation it
//! does not know, a parameter the operation does not declare, a missing or wrongly typed
//! parameter, a `confirm` that is not equal to its target, and the combinations the schema
//! cannot express. It selects no tenant and checks no permission (the executor is bound to
//! both).

use serde::Serialize;
use serde_json::Value;
use spot_mgmt_ops::{AcceptedParam, ParamValue, Validated};

use crate::args::{
    EnrolArgs, NodeArgs, NodeRole, PolicyShowArgs, Position, QuarantineDiscardArgs,
    QuarantineRetryArgs, ResendArgs, RoleChangeArgs, bad,
};
use crate::catalogue::MAX_VALID_HOURS;
use crate::error::OpError;
use crate::ids;
use crate::ops::SyncOps;

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

    fn find(&self, name: &str) -> Option<&'a AcceptedParam> {
        self.validated
            .params
            .iter()
            .find(|accepted| accepted.name == name)
    }

    fn value(&self, name: &str) -> Result<&'a AcceptedParam, OpError> {
        self.find(name)
            .ok_or_else(|| bad(self.op, name, "required parameter is missing"))
    }

    fn text(&self, name: &str) -> Result<&'a str, OpError> {
        match &self.value(name)?.value {
            ParamValue::Text(text) => Ok(text),
            _ => Err(bad(self.op, name, "expected text")),
        }
    }

    /// A text parameter that may be absent; present but wrongly typed is still refused.
    fn optional_text(&self, name: &str) -> Result<Option<&'a str>, OpError> {
        match self.find(name) {
            None => Ok(None),
            Some(accepted) => match &accepted.value {
                ParamValue::Text(text) => Ok(Some(text)),
                _ => Err(bad(self.op, name, "expected text")),
            },
        }
    }

    fn int(&self, name: &str) -> Result<i64, OpError> {
        match self.value(name)?.value {
            ParamValue::Int(number) => Ok(number),
            _ => Err(bad(self.op, name, "expected an integer")),
        }
    }

    fn texts(&self, name: &str) -> Result<Vec<String>, OpError> {
        match &self.value(name)?.value {
            ParamValue::List(items) => items
                .iter()
                .map(|item| match item {
                    ParamValue::Text(text) => Ok(text.clone()),
                    _ => Err(bad(self.op, name, "expected a list of text")),
                })
                .collect(),
            _ => Err(bad(self.op, name, "expected a list")),
        }
    }

    /// The `node` parameter alone, for the operations that name one node.
    fn node(&self) -> Result<NodeArgs, OpError> {
        self.only(&["node"])?;
        Ok(NodeArgs {
            node: self.text("node")?.to_owned(),
        })
    }

    /// Refuses a `confirm` that is not exactly `target` (a forged or stale value fails closed).
    fn confirmed(&self, target: &str) -> Result<(), OpError> {
        let confirm = self.text("confirm")?;
        if confirm.is_empty() || confirm != target {
            return Err(bad(self.op, "confirm", "must equal the target"));
        }
        Ok(())
    }

    fn role(&self, name: &str) -> Result<NodeRole, OpError> {
        NodeRole::from_name(self.text(name)?)
            .ok_or_else(|| bad(self.op, name, "not one of edge, hub"))
    }
}

/// Serialises a typed result to the stable JSON the management API returns.
fn json<T: Serialize>(op: &str, result: &T) -> Result<Value, OpError> {
    serde_json::to_value(result).map_err(|_| OpError::Serialise { op: op.to_owned() })
}

/// Runs the operation `validated` names on `ops` and returns its typed result as JSON.
///
/// # Errors
/// [`OpError::UnknownOperation`] for an id that is not a sync operation;
/// [`OpError::BadArguments`] when the parameters do not parse into the typed arguments;
/// [`OpError::Exec`] when the executor refuses or fails; [`OpError::Serialise`] when a result
/// cannot become JSON.
pub async fn dispatch(validated: &Validated, ops: &dyn SyncOps) -> Result<Value, OpError> {
    let op = validated.op.as_str();
    let p = Params { op, validated };
    match op {
        ids::HEALTH => {
            p.only(&[])?;
            json(op, &ops.health().await?)
        }
        ids::NODE_LIST => {
            p.only(&[])?;
            json(op, &ops.node_list().await?)
        }
        ids::NODE_SHOW => json(op, &ops.node_show(p.node()?).await?),
        ids::NODE_ENROL => {
            p.only(&["node", "role", "partitions", "subscription", "valid_hours"])?;
            let role = p.role("role")?;
            if role != NodeRole::Edge {
                return Err(bad(op, "role", "only edge nodes are enrolled"));
            }
            let partitions = p.texts("partitions")?;
            if partitions.is_empty() {
                return Err(bad(op, "partitions", "at least one partition is required"));
            }
            let valid_hours = u16::try_from(p.int("valid_hours")?)
                .ok()
                .filter(|hours| (1..=MAX_VALID_HOURS).contains(&i64::from(*hours)))
                .ok_or_else(|| bad(op, "valid_hours", "out of range"))?;
            let args = EnrolArgs {
                node: p.text("node")?.to_owned(),
                role,
                partitions,
                subscription: p.text("subscription")?.to_owned(),
                valid_hours,
            };
            json(op, &ops.node_enrol(args).await?)
        }
        ids::NODE_SUSPEND => json(op, &ops.node_suspend(p.node()?).await?),
        ids::NODE_RESUME => json(op, &ops.node_resume(p.node()?).await?),
        ids::NODE_REVOKE => {
            p.only(&["node", "confirm"])?;
            let node = p.text("node")?;
            p.confirmed(node)?;
            let args = NodeArgs {
                node: node.to_owned(),
            };
            json(op, &ops.node_revoke(args).await?)
        }
        ids::NODE_ROTATE_CERTIFICATE => json(op, &ops.node_rotate_certificate(p.node()?).await?),
        ids::NODE_RESEND => {
            p.only(&["node", "from"])?;
            let from = match p.optional_text("from")? {
                None => None,
                Some(text) => {
                    Some(Position::parse(text).map_err(|reason| bad(op, "from", reason))?)
                }
            };
            let args = ResendArgs {
                node: p.text("node")?.to_owned(),
                from,
            };
            json(op, &ops.node_resend(args).await?)
        }
        ids::NODE_NOTIFY_CONTACT => json(op, &ops.node_notify_contact(p.node()?).await?),
        ids::CONDITION_LIST => {
            p.only(&[])?;
            json(op, &ops.condition_list().await?)
        }
        ids::QUARANTINE_LIST => {
            p.only(&[])?;
            json(op, &ops.quarantine_list().await?)
        }
        ids::QUARANTINE_RETRY => {
            p.only(&["item"])?;
            let args = QuarantineRetryArgs {
                item: p.text("item")?.to_owned(),
            };
            json(op, &ops.quarantine_retry(args).await?)
        }
        ids::QUARANTINE_DISCARD => {
            p.only(&["item", "reason_code", "confirm"])?;
            let item = p.text("item")?;
            p.confirmed(item)?;
            let reason_code = p.text("reason_code")?;
            if reason_code.is_empty() {
                return Err(bad(op, "reason_code", "required"));
            }
            let args = QuarantineDiscardArgs {
                item: item.to_owned(),
                reason_code: reason_code.to_owned(),
            };
            json(op, &ops.quarantine_discard(args).await?)
        }
        ids::POLICY_SHOW => {
            p.only(&["doctype"])?;
            let args = PolicyShowArgs {
                doctype: p.text("doctype")?.to_owned(),
            };
            json(op, &ops.policy_show(args).await?)
        }
        ids::POLICY_LINT => {
            p.only(&[])?;
            json(op, &ops.policy_lint().await?)
        }
        ids::ROLE_CHANGE => {
            p.only(&["to", "confirm"])?;
            let to = p.role("to")?;
            p.confirmed(to.name())?;
            json(op, &ops.role_change(RoleChangeArgs { to }).await?)
        }
        ids::BOOTSTRAP_STATUS => {
            p.only(&[])?;
            json(op, &ops.bootstrap_status().await?)
        }
        other => Err(OpError::UnknownOperation {
            op: other.to_owned(),
        }),
    }
}
