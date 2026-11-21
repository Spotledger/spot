//! Audit helper: every send, receipt, refusal and rejection is written to the link's trail
//! before it takes effect.

use spot_tenant_link::{AuditSink, LinkEvent, LinkEventKind, LinkId, record_event};

use crate::error::{Error, Result};

/// One audited fact.
pub(crate) struct Fact {
    pub link: LinkId,
    pub kind: LinkEventKind,
    pub at: u64,
    pub revision: u64,
    pub detail: &'static str,
    pub action: &'static str,
}

/// Writes one fact; a sink failure refuses the action (fail closed). `actor` is `system`,
/// `user:<id>` or `agent:<id>`.
pub(crate) fn write(sink: &dyn AuditSink, actor: &str, fact: &Fact) -> Result<()> {
    let event = LinkEvent {
        link: fact.link,
        kind: fact.kind,
        actor: actor.to_owned(),
        at: fact.at,
        revision: fact.revision,
        detail: Some(fact.detail),
    };
    record_event(sink, &event, fact.action).map_err(|_| Error::AuditFailed {
        action: fact.action,
    })
}
