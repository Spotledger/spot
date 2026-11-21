//! [`GrantedReversal`]: lets the engine's own undo pass the governance guard (WP-BPM-15.T5).
//!
//! With governance on for a record type, a direct `cancel` or `delete` is refused unless the
//! call's command id is a **grant** the guard verifies for the acting person and the current
//! attempt of a step of an open request. [`LiveReversal`](super::reversal::LiveReversal) sends
//! the request key as the command id, which the guard rejects, so an undo of a governed record
//! ended in `needs_attention`. This decorator opens a grant from the governance module's
//! [`GrantHost`] (node `undo`) and hands it to the inner port in place of the key. Any other
//! record, and a site without governance, take the old path unchanged (no grant, key as command
//! id).
//!
//! **Crash safety.** The grant is the command id, and a grant carries an expiry, so a resumed
//! call cannot rely on the engine answering a stored result for an equal command id. The
//! decorator therefore writes a `started:<key>` mark into the effects ledger *before* the
//! lifecycle call. A resumed call that finds the mark and no receipt reads the record's current
//! version first: still at the version the entry left it at, the lifecycle did not commit and
//! the reversal runs with a fresh grant; any other answer (the record moved on, or is gone) means
//! the call may have committed, and the undo stops for a person with
//! [`UNCONFIRMED`] rather than reversing twice or guessing a receipt.
//!
//! Runs on a blocking thread (like [`DurableReversal`](super::reversal::DurableReversal)) and
//! reaches the async parts through the runtime handle. Never logs a grant.

use std::collections::BTreeSet;
use std::sync::Arc;

use spot_app_api::OperationReceipt;
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::prelude::{SiteId, Version};
use spot_bpm_documents::ids::parse_record_id;
use spot_bpm_effects::{PortError, Refusal, ReversalCall, ReversalPort};
use tokio::runtime::Handle;

use super::ledger::EffLedger;
use crate::live::SiteLive;
use crate::services::process_governance::GrantHost;

/// The refusal code when a resumed governed reversal cannot tell whether it already committed.
pub const UNCONFIRMED: &str = "effects.governed_reversal_unconfirmed";
/// The step name the undo's grants are issued for.
pub const UNDO_NODE: &str = "undo";
/// The process name recorded on an undo's grant row.
const UNDO_PROCESS: &str = "process_effects_undo";

/// The record types governance governs: any entry of `[services.process_governance].governed`,
/// whatever its scope. (The policy store resolves a company per record; the effects module has
/// no company, so it asks for a grant for every type governed anywhere on the site, and a grant
/// for a type the guard then does not govern is harmless.)
#[must_use]
pub fn governed_doctypes(entries: &[(String, String)]) -> BTreeSet<String> {
    entries
        .iter()
        .filter(|(_, mode)| mode == "governed" || mode == "process_only")
        .map(|(doctype, _)| doctype.clone())
        .collect()
}

/// Reads a record's current version, so a resumed reversal can tell whether it committed.
pub trait VersionProbe: Send + Sync {
    /// The version of the record `call` names; `Ok(None)` when it is gone.
    ///
    /// # Errors
    /// [`PortError::Unavailable`] when the engine cannot answer now.
    fn version(&self, call: &ReversalCall<'_>) -> Result<Option<Version>, PortError>;
}

/// The probe over the live document engine, reading as the acting principal.
pub struct LiveProbe {
    live: Arc<SiteLive>,
    site: SiteId,
    runtime: Handle,
}

impl LiveProbe {
    /// A probe of `site` over `live`.
    #[must_use]
    pub fn new(live: Arc<SiteLive>, site: SiteId, runtime: Handle) -> Self {
        Self {
            live,
            site,
            runtime,
        }
    }
}

impl VersionProbe for LiveProbe {
    fn version(&self, call: &ReversalCall<'_>) -> Result<Option<Version>, PortError> {
        let down = |code: &str| PortError::Unavailable(code.to_owned());
        let engine = self
            .live
            .engine()
            .ok_or_else(|| down("engine_unavailable"))?;
        let actor = actor_of(call.principal).ok_or_else(|| down("principal_not_a_user"))?;
        let id = parse_record_id(&call.record.name).ok_or_else(|| down("record_id_invalid"))?;
        let inv = Invocation {
            site: self.site.clone(),
            actor,
            command_id: None,
            language: None,
            now: call.now,
        };
        let loaded = self
            .runtime
            .block_on(engine.load(&inv, call.record.doctype.as_str(), id));
        // A record that cannot be loaded is "not the version we left": the caller stops.
        Ok(loaded.ok().and_then(|outcome| outcome.value.version()))
    }
}

/// The acting principal as an engine actor; only `user:<id>` acts.
fn actor_of(principal: &str) -> Option<Actor> {
    let id = principal.strip_prefix("user:").and_then(parse_record_id)?;
    Some(Actor::User { id, via: None })
}

/// The decorator.
pub struct GrantedReversal {
    inner: Arc<dyn ReversalPort>,
    grants: Arc<GrantHost>,
    governed: BTreeSet<String>,
    ledger: EffLedger,
    probe: Arc<dyn VersionProbe>,
    runtime: Handle,
}

impl GrantedReversal {
    /// A decorator over `inner`. `governed` is the set of record types that need a grant.
    #[must_use]
    pub fn new(
        inner: Arc<dyn ReversalPort>,
        grants: Arc<GrantHost>,
        governed: BTreeSet<String>,
        ledger: EffLedger,
        probe: Arc<dyn VersionProbe>,
        runtime: Handle,
    ) -> Self {
        Self {
            inner,
            grants,
            governed,
            ledger,
            probe,
            runtime,
        }
    }

    fn needs_grant(&self, call: &ReversalCall<'_>) -> bool {
        self.governed.contains(call.record.doctype.as_str())
    }

    /// The attempt number of a call: derived from the key's `<entry>:<reversal>` tail, so
    /// reversals of one undo get increasing numbers and a resumed one gets its own again.
    fn attempt_of(key: &str) -> u32 {
        let mut tail = key.rsplit(':');
        let reversal: u32 = tail.next().and_then(|t| t.parse().ok()).unwrap_or(0);
        let entry: u32 = tail.next().and_then(|t| t.parse().ok()).unwrap_or(0);
        entry
            .saturating_mul(1_000)
            .saturating_add(reversal)
            .saturating_add(1)
    }

    fn started_key(call: &ReversalCall<'_>) -> String {
        format!("started:{}", call.key)
    }

    /// Whether a previous attempt of this reversal started and left no receipt: the record's
    /// version decides whether it may have committed.
    fn resumed_is_safe(&self, call: &ReversalCall<'_>) -> Result<(), PortError> {
        let down = |code: &str| PortError::Unavailable(code.to_owned());
        let unconfirmed = || {
            PortError::Refused(Refusal {
                code: UNCONFIRMED.to_owned(),
                dependants: Vec::new(),
                alternatives: Vec::new(),
            })
        };
        let marked = self
            .runtime
            .block_on(self.ledger.done(&Self::started_key(call)))
            .map_err(|_| down("ledger_unavailable"))?;
        if marked.is_none() {
            return Ok(());
        }
        match self.probe.version(call)? {
            Some(version) if version == call.expected => Ok(()),
            _ => Err(unconfirmed()),
        }
    }

    fn run(
        &self,
        call: &ReversalCall<'_>,
        op: impl FnOnce(&ReversalCall<'_>) -> Result<OperationReceipt, PortError>,
    ) -> Result<OperationReceipt, PortError> {
        if !self.needs_grant(call) {
            return op(call);
        }
        self.resumed_is_safe(call)?;
        let doctype = call.record.doctype.clone();
        let opened = self.runtime.block_on(self.grants.open_attempt(
            &call.request.to_string(),
            UNDO_PROCESS,
            UNDO_NODE,
            Self::attempt_of(&call.key),
            &[doctype],
            call.principal,
        ));
        let grant = opened.map_err(|error| {
            // The guard's own code text, never a generic failure.
            PortError::Refused(Refusal {
                code: error.code().to_owned(),
                dependants: Vec::new(),
                alternatives: Vec::new(),
            })
        })?;
        let started = Self::started_key(call);
        self.runtime
            .block_on(
                self.ledger
                    .mark_done(&started, "started", super::micros(call.now)),
            )
            .map_err(|_| PortError::Unavailable("ledger_unavailable".to_owned()))?;
        // The grant stands where the key stood: the inner port sends `key` as the command id.
        let granted = ReversalCall {
            key: grant,
            ..call.clone()
        };
        op(&granted)
    }
}

impl ReversalPort for GrantedReversal {
    fn reverse(&self, call: &ReversalCall<'_>) -> Result<OperationReceipt, PortError> {
        self.run(call, |c| self.inner.reverse(c))
    }

    fn compensate(&self, call: &ReversalCall<'_>) -> Result<OperationReceipt, PortError> {
        self.run(call, |c| self.inner.compensate(c))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempts_follow_the_entry_and_reversal_tail_of_the_key() {
        assert_eq!(GrantedReversal::attempt_of("undo:req:3:2"), 3_003);
        assert_eq!(GrantedReversal::attempt_of("undo:req:0:0"), 1);
        assert_eq!(GrantedReversal::attempt_of("junk"), 1);
        assert!(
            GrantedReversal::attempt_of("undo:r:1:5") < GrantedReversal::attempt_of("undo:r:2:0")
        );
    }

    #[test]
    fn governed_and_process_only_need_a_grant_free_does_not() {
        let set = governed_doctypes(&[
            ("invoice".to_owned(), "governed".to_owned()),
            ("order".to_owned(), "process_only".to_owned()),
            ("note".to_owned(), "free".to_owned()),
        ]);
        assert!(set.contains("invoice") && set.contains("order") && !set.contains("note"));
    }
}
