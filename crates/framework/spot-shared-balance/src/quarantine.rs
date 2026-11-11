//! Where an entry goes when head office refuses it on arrival.
//!
//! A refused entry is never dropped: money someone already handed over is evidence. It goes
//! through [`QuarantineSink`] to the platform's quarantine queue (`spot-quarantine`, WP-EDG-06.T2),
//! whose items are keyed to replica-apply records. The adapter from this trait to that queue
//! belongs to the code that wires sync to balances, so this crate stays free of the storage
//! stack; [`VecQuarantine`] is the in-memory reference used by the tests.

use crate::codes;
use crate::ledger::Entry;

/// Why an arriving entry was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RejectReason {
    /// The balance did not cover the entry under a strict policy.
    Insufficient,
    /// There is no open quota grant behind the entry.
    NoGrant,
    /// The entry would take the node past its granted quota.
    OverQuota,
    /// The entry was made at or after its quota's expiry.
    AfterExpiry,
    /// Head office holds no such balance.
    UnknownBalance,
    /// A node may only send its own debits, unchanged: not credits, not another node's entries, not a changed copy of an entry already held.
    NotPermitted,
}

impl RejectReason {
    /// The stable code from [`codes`] for this reason.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            RejectReason::Insufficient => codes::REJECTED_INSUFFICIENT,
            RejectReason::NoGrant => codes::REJECTED_NO_GRANT,
            RejectReason::OverQuota => codes::REJECTED_OVER_QUOTA,
            RejectReason::AfterExpiry => codes::REJECTED_AFTER_EXPIRY,
            RejectReason::UnknownBalance => codes::REJECTED_UNKNOWN_BALANCE,
            RejectReason::NotPermitted => codes::REJECTED_NOT_PERMITTED,
        }
    }
}

/// The quarantine sink could not take an entry.
///
/// It carries no entry content: the entry is named by the caller, and the sender redelivers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the quarantine sink could not take the entry")]
pub struct SinkError;

impl SinkError {
    /// The stable code from [`codes`] for this error.
    #[must_use]
    pub fn code(self) -> &'static str {
        codes::SINK_FAILED
    }
}

/// Takes an entry head office refused, with the reason. Implementations must keep it.
///
/// A refusal is two steps for the hub. When it hands an entry to the sink it marks the entry
/// *offered*; the mark becomes *held* (a redelivery is then a repeat) only once the sink has the
/// entry in custody. A sink that keeps the entry durably before it returns says so by returning
/// `Ok(())` and leaving [`QuarantineSink::confirms_on_accept`] at its default. A sink that only
/// buffers it (the sync layer's adapter, whose queue write is asynchronous) returns `false` from
/// that method, and the caller confirms with [`Hub::confirm_held`](crate::Hub::confirm_held) when
/// the write has completed. Until then a redelivery of the entry is offered to the sink again, so
/// a sink that is lost, or a queue write that fails, never leaves an entry held nowhere.
pub trait QuarantineSink {
    /// Holds `entry` for a person to resolve.
    ///
    /// # Errors
    /// [`SinkError`] when the sink cannot take the entry; the hub then lists the entry as
    /// deferred, does not mark it held and does not acknowledge it.
    fn quarantine(&mut self, entry: &Entry, reason: RejectReason) -> Result<(), SinkError>;

    /// Whether `Ok(())` from [`quarantine`](QuarantineSink::quarantine) means the entry is
    /// already in durable custody. `true` by default; a buffering sink returns `false`.
    fn confirms_on_accept(&self) -> bool {
        true
    }
}

/// An in-memory [`QuarantineSink`] that keeps what it is given, in order.
#[derive(Clone, Debug, Default)]
pub struct VecQuarantine {
    /// The refused entries with their reasons, in arrival order.
    pub held: Vec<(Entry, RejectReason)>,
    /// For tests: the next this-many calls fail with [`SinkError`] and keep nothing.
    pub fail_next: u32,
}

impl VecQuarantine {
    /// A sink whose next `calls` calls fail.
    #[must_use]
    pub fn failing(calls: u32) -> Self {
        Self {
            held: Vec::new(),
            fail_next: calls,
        }
    }
}

impl QuarantineSink for VecQuarantine {
    fn quarantine(&mut self, entry: &Entry, reason: RejectReason) -> Result<(), SinkError> {
        if self.fail_next > 0 {
            self.fail_next -= 1;
            return Err(SinkError);
        }
        self.held.push((entry.clone(), reason));
        Ok(())
    }
}
