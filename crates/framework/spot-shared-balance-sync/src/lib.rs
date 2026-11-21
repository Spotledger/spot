//! The sync-layer wiring of the shared-balance policies (WP-EDG-09.T2, ADR-062).
//!
//! [`spot-shared-balance`](spot_shared_balance) decides what a branch may spend and what head
//! office does with the entries it delivers, but it deliberately cannot know **who** delivered
//! them: [`Batch::node`](spot_shared_balance::Batch) is a field of the payload, and a payload can
//! lie. This crate is the seam between the transport and the policies, and it closes two holes:
//!
//! - **Sender authentication.** [`receive_batch`] takes the [`NodeIdentity`] the transport
//!   authenticated (mutual TLS, resolved through the hub's node directory) and refuses the
//!   **whole** batch — applying nothing — unless the batch's node, every entry's node, and the
//!   node's standing in the [`NodeRegistry`] all agree with it. Every refusal is audited with
//!   both ids through the [`AuditSink`] the caller supplies.
//! - **A durable place for refusals.** [`QuarantineAdapter`] is the
//!   [`QuarantineSink`](spot_shared_balance::QuarantineSink) that puts each entry head office
//!   refused into the platform's quarantine queue ([`spot_quarantine`]), one item per
//!   `(node, entry id)`, with the entry as an immutable payload and the reason code beside it.
//!
//! # How a delivery is handled
//!
//! 1. The sync layer resolves the client certificate to a node and builds a [`NodeIdentity`].
//! 2. It calls [`receive_and_flush`] with the hub, the policy, the batch, the hub's time, its
//!    [`QuarantineAdapter`] and its [`AuditSink`]. This is **the entry point**: it
//!    authenticates the sender, hands the batch to
//!    [`BalancePolicy::settle`](spot_shared_balance::BalancePolicy::settle) (a repeated batch is
//!    recognised entry by entry: an entry id already on a ledger, or already quarantined, is a
//!    duplicate and changes nothing), then awaits [`QuarantineAdapter::flush`].
//! 3. It returns an [`Acknowledgeable`] **only after the flush succeeded**, and the sync layer's
//!    acknowledgement takes that value, so an entry head office refused can never be reported as
//!    handled while the queue is still missing it.
//! 4. If the queue cannot take an item, the call returns [`SyncError::Quarantine`], the items stay
//!    pending in the adapter and offered (not held) in the hub, and nothing is acknowledged. The
//!    sender's redelivery of the same batch flushes exactly those items, through the same adapter
//!    or a new one after a restart (they are idempotent on `(node, entry id)`, so none is stored
//!    twice). Only when the flush succeeds does the hub mark the entries held.
//!
//! [`receive_batch`] is the first half of that, kept public for the sync-layer tests. It settles
//! the batch but queues the refusals only **in memory**: its result is not an acknowledgement.
//!
//! # What it never does
//!
//! It never trusts a node id from the payload for authorisation, never reads a clock (time is an
//! argument), never uses a float for money, and never applies an entry a refusal covered: a
//! mismatched or unregistered sender leaves the hub exactly as it was.

#![warn(missing_docs)]

mod acknowledge;
pub mod codes;
mod entry_payload;
mod error;
mod identity;
mod queue;

use std::collections::BTreeMap;

use spot_audit_chain::{AuditRecord, PayloadValue};
use spot_offline_identity::{NodeId as RegistryNodeId, NodeState};
use spot_quarantine::QuarantineError;
use spot_shared_balance::{BalancePolicy, Batch, Hub, HubReply, Millis, NodeId, QuarantineSink};

pub use acknowledge::Acknowledgeable;
pub use entry_payload::{ENTRY_ENTITY, ENTRY_NAME_PREFIX, entry_of};
pub use error::SyncError;
pub use identity::NodeIdentity;
pub use queue::QuarantineAdapter;

// Re-exported so the sync layer and this crate are wired from one set of values.
pub use spot_offline_identity::{AuditSink, MemoryAuditSink, NodeRegistry};
pub use spot_quarantine::{InMemoryStore, ItemId, QuarantineItem, QuarantineStore};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-shared-balance-sync";

/// Head office's side of one delivery: what [`receive_batch`] needs to judge and settle it.
///
/// Bundled so the entry point's signature names the two things that decide the outcome — the
/// authenticated sender and the batch — and the rest stays one value the sync layer holds.
pub struct HeadOffice<'a> {
    /// The hub's registry of its nodes: registration and standing.
    pub registry: &'a NodeRegistry,
    /// The authoritative ledgers, grants and receivables.
    pub hub: &'a mut Hub,
    /// The policy of the balance types involved.
    pub policy: &'a dyn BalancePolicy,
    /// Where a refusal is audited.
    pub audit: &'a mut dyn AuditSink,
}

/// Receives one delivery from a branch node and settles it at head office.
///
/// `authenticated` is the node the transport identified; `batch` is what it sent. The batch is
/// accepted only when both name the same node, every entry carries that node's id, and the node
/// is registered and active. A refused batch is audited and returns an error with a stable
/// [`code`](SyncError::code); **nothing is applied**, so the sender may resend it unchanged.
///
/// What it returns is head office's [`HubReply`]: what it did with the batch **and** its totals
/// for the balances the batch named. The node applies that reply whole
/// ([`NodeView::apply_hub_reply`](spot_shared_balance::NodeView::apply_hub_reply)), so a node
/// cannot acknowledge a delivery without learning the balance that came with it.
///
/// # Errors
/// [`SyncError::SenderMismatch`] when the batch or an entry names another node,
/// [`SyncError::NodeNotRegistered`] or [`SyncError::NodeNotActive`] for a node the hub will not
/// vouch for, [`SyncError::AuditFailed`] when a refusal cannot be recorded (it still stands),
/// [`SyncError::Ledger`] when a ledger cannot take an entry (a repeat resumes safely), and
/// [`SyncError::Quarantine`] from the sink.
///
/// # Not an acknowledgement
/// The refusals this queues in `sink` are still only in memory when it returns. Acknowledging the
/// batch now, then crashing, loses them. The reconciliation may be sent to the sender only after
/// the sink's flush succeeded, and [`receive_and_flush`] is the function that does both in that
/// order and returns the proof ([`Acknowledgeable`]); production code uses it.
#[must_use = "this is not an acknowledgement: the refused entries are only in memory until the \
              quarantine sink is flushed; use `receive_and_flush`"]
pub fn receive_batch(
    authenticated: &NodeIdentity,
    batch: Batch,
    now: Millis,
    sink: &mut dyn QuarantineSink,
    head_office: HeadOffice<'_>,
) -> Result<HubReply, SyncError> {
    let HeadOffice {
        registry,
        hub,
        policy,
        audit,
    } = head_office;
    let sender = authenticated.node();

    // The sender's node id comes from the connection, never from the payload: a batch that
    // disagrees is refused whole, so a node cannot file entries under another node's id.
    if &batch.node != sender {
        return Err(refuse(
            audit,
            SyncError::SenderMismatch {
                authenticated: sender.clone(),
                claimed: batch.node.clone(),
                entry: None,
            },
            now,
        ));
    }
    if let Some(entry) = batch.entries.iter().find(|entry| &entry.id.node != sender) {
        return Err(refuse(
            audit,
            SyncError::SenderMismatch {
                authenticated: sender.clone(),
                claimed: entry.id.node.clone(),
                entry: Some(entry.id.clone()),
            },
            now,
        ));
    }

    match standing(registry, sender) {
        Standing::Active => {}
        Standing::NotRegistered => {
            return Err(refuse(
                audit,
                SyncError::NodeNotRegistered {
                    node: sender.clone(),
                },
                now,
            ));
        }
        Standing::NotActive(state) => {
            return Err(refuse(
                audit,
                SyncError::NodeNotActive {
                    node: sender.clone(),
                    state,
                },
                now,
            ));
        }
    }

    Ok(policy.settle(hub, &batch, now, sink)?)
}

/// Receives one delivery from a branch node, settles it at head office, **flushes the refused
/// entries into the quarantine queue**, and only then returns the proof that the delivery may be
/// acknowledged.
///
/// This is [`receive_batch`] with `adapter` as the sink, followed by an awaited
/// [`QuarantineAdapter::flush`] at `flush_at_micros` (the hub's time for the items' first-seen
/// stamp; no clock is read). A batch the sender check refuses never reaches the flush, so a
/// refused batch queues and stores nothing.
///
/// # Errors
/// Everything [`receive_batch`] returns, with the same meaning, and [`SyncError::Quarantine`] when
/// the queue cannot take a refused entry. In that last case nothing is acknowledged, the
/// unflushed items stay pending in `adapter`, and the redelivery of the same batch through the
/// same adapter flushes exactly them.
///
/// # A failed call, a lost adapter
/// The hub marks a refused entry *offered* when it hands it to the adapter, and *held* only
/// when the flush has put it in the queue ([`Hub::confirm_held`](spot_shared_balance::Hub::confirm_held)).
/// Until then a redelivery is checked again and offered to the adapter again, so the pending
/// items do not live only in `adapter`: an adapter that is dropped with items pending (a
/// restart, a lost queue handle) is replaced by a new one, and the sender's redelivery offers it
/// the same entries. [`Hub::offered_unconfirmed`](spot_shared_balance::Hub::offered_unconfirmed)
/// lists them. The hub's marks are in memory in this crate; a host that persists the hub must
/// persist both states.
pub async fn receive_and_flush(
    authenticated: &NodeIdentity,
    batch: Batch,
    now: Millis,
    adapter: &mut QuarantineAdapter,
    head_office: HeadOffice<'_>,
    flush_at_micros: i64,
) -> Result<Acknowledgeable, SyncError> {
    let node = authenticated.node().clone();
    let HeadOffice {
        registry,
        hub,
        policy,
        audit,
    } = head_office;
    let reply = receive_batch(
        authenticated,
        batch,
        now,
        adapter,
        HeadOffice {
            registry,
            hub: &mut *hub,
            policy,
            audit,
        },
    )?;
    let flushed = adapter.flush(flush_at_micros).await;
    // Whatever the queue holds is confirmed to the hub, even when the flush failed part-way:
    // those entries are in custody, and only the rest stay offered for the redelivery.
    for id in adapter.take_confirmed() {
        hub.confirm_held(&id);
    }
    flushed?;
    // The flush stores or recognises every pending item. A count that is not zero would mean an
    // acknowledgement over a refusal the queue does not hold, so fail closed.
    if adapter.pending() > 0 {
        return Err(SyncError::Quarantine(QuarantineError::Store {
            why: "refused entries are still pending after the flush".to_owned(),
        }));
    }
    Ok(Acknowledgeable::new(node, reply))
}

/// What the hub's registry says about one node.
enum Standing {
    Active,
    NotRegistered,
    NotActive(&'static str),
}

/// The node's standing, read from the registry under the registry's own node-id type.
fn standing(registry: &NodeRegistry, node: &NodeId) -> Standing {
    let Ok(node) = RegistryNodeId::new(node.0.as_str()) else {
        // An id the platform's own type will not accept cannot have been registered.
        return Standing::NotRegistered;
    };
    match registry.state(&node) {
        None => Standing::NotRegistered,
        Some(NodeState::Active) => Standing::Active,
        Some(NodeState::Suspended) => Standing::NotActive("suspended"),
        Some(NodeState::Decommissioned) => Standing::NotActive("decommissioned"),
    }
}

/// Audits `refusal` with both node ids and returns it. A failing audit never turns a refusal
/// into an acceptance: it replaces the refusal with [`SyncError::AuditFailed`], and either way
/// nothing is applied.
fn refuse(audit: &mut dyn AuditSink, refusal: SyncError, now: Millis) -> SyncError {
    let code = refusal.code();
    let (authenticated, claimed, entry) = match &refusal {
        SyncError::SenderMismatch {
            authenticated,
            claimed,
            entry,
        } => (
            Some(authenticated.clone()),
            Some(claimed.clone()),
            entry.clone(),
        ),
        SyncError::NodeNotRegistered { node } | SyncError::NodeNotActive { node, .. } => {
            (Some(node.clone()), None, None)
        }
        _ => (None, None, None),
    };
    let mut payload = BTreeMap::new();
    payload.insert("reason".to_owned(), PayloadValue::Text(code.to_owned()));
    if let Some(node) = &authenticated {
        payload.insert(
            "authenticated".to_owned(),
            PayloadValue::Text(node.0.clone()),
        );
    }
    if let Some(node) = &claimed {
        payload.insert("claimed".to_owned(), PayloadValue::Text(node.0.clone()));
    }
    if let Some(entry) = &entry {
        payload.insert("entry".to_owned(), PayloadValue::Text(entry.to_string()));
    }
    let record = AuditRecord {
        kind: codes::BATCH_REJECTED.to_owned(),
        actor: authenticated
            .as_ref()
            .map_or_else(|| "system".to_owned(), |node| format!("node:{}", node.0)),
        on_behalf_of: None,
        subject: None,
        at: audit_micros(now),
        payload,
        masked: true,
    };
    match audit.record(record) {
        Ok(()) => refusal,
        Err(_) => SyncError::AuditFailed { code },
    }
}

/// The audit stamp: the hub time in milliseconds, widened to the audit chain's microseconds.
fn audit_micros(now: Millis) -> i64 {
    i64::try_from(now.0)
        .unwrap_or(i64::MAX / 1_000)
        .saturating_mul(1_000)
}
