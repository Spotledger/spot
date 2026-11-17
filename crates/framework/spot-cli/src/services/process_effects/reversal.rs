//! How a reversal reaches the lifecycle: the durable wrapper and the live port.
//!
//! `spot-bpm-effects` decides the order of an undo and calls a **synchronous** [`ReversalPort`].
//! Two things sit between that call and the document engine:
//!
//! - [`DurableReversal`] makes every reversal **exactly-once per receipt**, across a restart: the
//!   request key `undo:<request>:<entry>:<reversal>` is looked up in the ledger before the
//!   lifecycle is called, and a completed call is written to the ledger straight after. A resumed
//!   undo therefore finds what is already done and returns its stored receipt instead of
//!   reversing again. It runs on a blocking thread, so it reaches the database through the
//!   runtime handle.
//! - [`LiveReversal`] is the port over the live document engine: it runs the reversal **as the
//!   acting principal** (never as the system), through the record type's own lifecycle
//!   (`cancel` for a posted record, `delete` for a draft), or calls the named action. A refusal
//!   of the lifecycle (dependants, a closed period, a permission) is a [`Refusal`] and stops the
//!   undo for a decision; an engine that is down is [`PortError::Unavailable`] and the undo stays
//!   resumable.

use std::sync::Arc;

use spot_app_api::context::{Actor, Invocation};
use spot_app_api::prelude::{CommandId, RecordId, SiteId};
use spot_app_api::{ActionRef, AppError, ErrorClass, Operation, OperationReceipt};
use spot_bpm_documents::ids::parse_record_id;
use spot_bpm_effects::{PortError, Refusal, ReversalCall, ReversalPort};
use tokio::runtime::Handle;

use super::ledger::EffLedger;
use crate::live::SiteLive;

/// The code a durable lookup that cannot be answered reports (the undo stays resumable).
const LEDGER_DOWN: &str = "ledger_unavailable";

/// Wraps a port with the durable once-per-key journal.
pub struct DurableReversal {
    inner: Arc<dyn ReversalPort>,
    ledger: EffLedger,
    runtime: Handle,
}

impl DurableReversal {
    /// A wrapper over `inner`, journaling in `ledger`. The runtime handle is the one the host
    /// runs on; the wrapper is only ever called from a blocking thread, never from a runtime
    /// worker.
    #[must_use]
    pub fn new(inner: Arc<dyn ReversalPort>, ledger: EffLedger, runtime: Handle) -> Self {
        Self {
            inner,
            ledger,
            runtime,
        }
    }

    fn once(
        &self,
        call: &ReversalCall<'_>,
        run: impl FnOnce() -> Result<OperationReceipt, PortError>,
    ) -> Result<OperationReceipt, PortError> {
        let unavailable = || PortError::Unavailable(LEDGER_DOWN.to_owned());
        let stored = self
            .runtime
            .block_on(self.ledger.done(&call.key))
            .map_err(|_| unavailable())?;
        if let Some(text) = stored {
            // A stored receipt that does not parse is not a reason to reverse again.
            return serde_json::from_str(&text).map_err(|_| unavailable());
        }
        let receipt = run()?;
        let text = serde_json::to_string(&receipt).map_err(|_| unavailable())?;
        let at = super::micros(call.now);
        self.runtime
            .block_on(self.ledger.mark_done(&call.key, &text, at))
            .map_err(|_| unavailable())?;
        Ok(receipt)
    }
}

impl ReversalPort for DurableReversal {
    fn reverse(&self, call: &ReversalCall<'_>) -> Result<OperationReceipt, PortError> {
        self.once(call, || self.inner.reverse(call))
    }

    fn compensate(&self, call: &ReversalCall<'_>) -> Result<OperationReceipt, PortError> {
        self.once(call, || self.inner.compensate(call))
    }
}

/// The reversal port over the live document engine.
pub struct LiveReversal {
    live: Arc<SiteLive>,
    site: SiteId,
    runtime: Handle,
}

impl LiveReversal {
    /// The port of `site` over `live`, driving the engine on `runtime`.
    #[must_use]
    pub fn new(live: Arc<SiteLive>, site: SiteId, runtime: Handle) -> Self {
        Self {
            live,
            site,
            runtime,
        }
    }

    fn invocation(&self, call: &ReversalCall<'_>) -> Result<Invocation, PortError> {
        let actor = actor_of(call.principal)
            .ok_or_else(|| PortError::Unavailable("principal_not_a_user".to_owned()))?;
        Ok(Invocation {
            site: self.site.clone(),
            actor,
            command_id: CommandId::new(&call.key).ok(),
            language: None,
            now: call.now,
        })
    }

    fn id_of(call: &ReversalCall<'_>) -> Result<RecordId, PortError> {
        parse_record_id(&call.record.name)
            .ok_or_else(|| PortError::Unavailable("record_id_invalid".to_owned()))
    }
}

impl ReversalPort for LiveReversal {
    fn reverse(&self, call: &ReversalCall<'_>) -> Result<OperationReceipt, PortError> {
        let engine = self
            .live
            .engine()
            .ok_or_else(|| PortError::Unavailable("engine_unavailable".to_owned()))?;
        let inv = self.invocation(call)?;
        let id = Self::id_of(call)?;
        let doctype = call.record.doctype.as_str();
        self.runtime.block_on(async {
            let done = match call.operation {
                Operation::Cancel => engine
                    .cancel_with_receipt(&inv, doctype, id, call.expected)
                    .await
                    .map(|done| done.receipt.receipt),
                Operation::Delete => engine
                    .delete_with_receipt(&inv, doctype, id, call.expected)
                    .await
                    .map(|done| done.receipt.receipt),
                // Only the two reversing lifecycle operations exist; anything else is a
                // declaration the host cannot carry out, and it never guesses.
                _ => return Err(PortError::Unavailable("operation_unsupported".to_owned())),
            };
            done.map_err(port_error)
        })
    }

    fn compensate(&self, call: &ReversalCall<'_>) -> Result<OperationReceipt, PortError> {
        let engine = self
            .live
            .engine()
            .ok_or_else(|| PortError::Unavailable("engine_unavailable".to_owned()))?;
        let action: &ActionRef = call
            .action
            .ok_or_else(|| PortError::Unavailable("action_missing".to_owned()))?;
        // `<app>:<action>`: the whitelisted method is the action's own name.
        let method = action
            .as_str()
            .split_once(':')
            .map(|(_, name)| name)
            .ok_or_else(|| PortError::Unavailable("action_invalid".to_owned()))?;
        let inv = self.invocation(call)?;
        let id = Self::id_of(call)?;
        let doctype = call.record.doctype.as_str();
        let done = self.runtime.block_on(async {
            use spot_app_api::context::DocumentService;
            engine
                .call_method(
                    &inv,
                    doctype,
                    id,
                    method,
                    serde_json::Value::Null,
                    Some(call.expected),
                )
                .await
        });
        let outcome = done.map_err(port_error)?;
        let (document, _result) = outcome.value;
        Ok(OperationReceipt {
            operation: call.operation,
            doctype: call.record.doctype.clone(),
            name: call.record.name.clone(),
            version: document.version().unwrap_or(call.expected),
            effects: Vec::new(),
            committed_at: call.now,
        })
    }
}

/// The acting principal as an engine actor: only a `user:<id>` reference acts. A consumer or the
/// system never runs a reversal, so a caller can never obtain the system actor by any input.
fn actor_of(principal: &str) -> Option<Actor> {
    let id = principal.strip_prefix("user:").and_then(parse_record_id)?;
    Some(Actor::User { id, via: None })
}

/// Maps an engine error: a refusal of the lifecycle is a decision; anything that is not the
/// lifecycle's answer (the engine is down, storage failed) leaves the undo resumable.
fn port_error(error: AppError) -> PortError {
    match error.class {
        ErrorClass::Unavailable | ErrorClass::Internal => {
            PortError::Unavailable(error.code.clone())
        }
        _ => PortError::Refused(Refusal {
            code: error.code.clone(),
            dependants: Vec::new(),
            alternatives: Vec::new(),
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn only_a_user_reference_acts() {
        let id = RecordId::new_v7();
        assert!(actor_of(&format!("user:{id}")).is_some());
        assert!(actor_of("system").is_none());
        assert!(actor_of(&format!("consumer:{id}")).is_none());
        assert!(actor_of("user:not-an-id").is_none());
    }

    #[test]
    fn an_unavailable_engine_is_not_a_refusal() {
        let down = AppError::new(ErrorClass::Unavailable, "server.unavailable", "down");
        assert!(matches!(port_error(down), PortError::Unavailable(_)));
        let refused = AppError::new(ErrorClass::Linked, "document.linked", "linked");
        assert!(matches!(port_error(refused), PortError::Refused(_)));
    }
}
