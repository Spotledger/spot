//! The ports an undo runs through: the lifecycle, and the partner.
//!
//! An undo never writes a store. Every reversal is a lifecycle operation (or a named action that
//! is itself one) run **as the acting principal**, so the record type's own rules apply: its
//! dependants check, its period close, its permissions. The host implements [`ReversalPort`]
//! over the document engine's `DocumentService`; this crate only decides the order and what to
//! do when the lifecycle says no.

use spot_app_api::prelude::{OffsetDateTime, Version};
use spot_app_api::{ActionRef, Operation, OperationReceipt};
use spot_bpm_engine::CaseId;

use crate::model::{MAX_ALTERNATIVES, MAX_DEPENDANTS, RecordRef};

/// A token naming an alternative the record type offers: `[a-z][a-z0-9_]{0,63}`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AlternativeId(String);

impl AlternativeId {
    /// Validates the token.
    #[must_use]
    pub fn new(raw: &str) -> Option<Self> {
        is_token(raw).then(|| Self(raw.to_owned()))
    }

    /// The token.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What carrying out an alternative runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AlternativeKind {
    /// A lifecycle operation on the same record.
    Lifecycle(Operation),
    /// A named action (for example "issue a reversing record dated today").
    Action(ActionRef),
}

/// One thing the record type offers instead of the refused reversal (`ReversalAlternative`).
///
/// It carries a token and what to run, never free text: a person reads the token's translation,
/// and no peer text can ride in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alternative {
    /// The token a person picks by.
    pub id: AlternativeId,
    /// What picking it runs.
    pub kind: AlternativeKind,
}

/// Why the lifecycle refused one reversal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The stable code of the refusal (`dependants_outside_request`, `period_closed` ...). Reduced
    /// to a code shape before it is kept.
    pub code: String,
    /// The records that stand in the way.
    pub dependants: Vec<RecordRef>,
    /// The record type's declared alternatives.
    pub alternatives: Vec<Alternative>,
}

/// Why a port call did not produce a receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortError {
    /// The lifecycle refused: the undo stops and asks for a decision.
    Refused(Refusal),
    /// The port could not be reached or crashed: the undo stays held and can be resumed. The
    /// code is reduced to a code shape before it is kept.
    Unavailable(String),
}

/// How a call was chosen: declared on the entry, or picked by a person from the alternatives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Via {
    /// The entry's declared reversal.
    Declared,
    /// The alternative a person picked.
    Alternative(AlternativeId),
}

/// One reversal for the port to run.
#[derive(Clone, Debug)]
pub struct ReversalCall<'a> {
    /// The request.
    pub request: &'a CaseId,
    /// The record to reverse.
    pub record: &'a RecordRef,
    /// The version the entry left it at: the version the port expects.
    pub expected: Version,
    /// The operation to run (for a lifecycle reversal); the entry's own operation otherwise.
    pub operation: Operation,
    /// The action to run, for a compensation.
    pub action: Option<&'a ActionRef>,
    /// Declared or picked.
    pub via: Via,
    /// The acting principal; the port runs the operation as this principal and no other.
    pub principal: &'a str,
    /// The request key: `(request, entry, reversal)`. The port makes the call idempotent by it,
    /// so a resumed undo never reverses twice.
    pub key: String,
    /// The instant of the call; time is an argument.
    pub now: OffsetDateTime,
}

/// Runs reversals through the record type's lifecycle.
pub trait ReversalPort: Send + Sync {
    /// Runs a lifecycle operation on a record and returns its receipt.
    ///
    /// # Errors
    /// [`PortError::Refused`] when the lifecycle says no; [`PortError::Unavailable`] when the
    /// call did not complete.
    fn reverse(&self, call: &ReversalCall<'_>) -> Result<OperationReceipt, PortError>;

    /// Runs a named compensating action and returns the receipt of what it did.
    ///
    /// # Errors
    /// As [`reverse`](Self::reverse).
    fn compensate(&self, call: &ReversalCall<'_>) -> Result<OperationReceipt, PortError>;
}

/// The cancellation message of a waiting conversation (WP-BPM-12).
#[derive(Clone, Debug)]
pub struct CancellationCall<'a> {
    /// The request.
    pub request: &'a CaseId,
    /// The conversation.
    pub conversation: &'a str,
    /// The contract's cancellation message name.
    pub message: &'a str,
    /// The acting principal.
    pub principal: &'a str,
    /// Idempotency key for the send.
    pub key: String,
    /// The instant of the call.
    pub now: OffsetDateTime,
}

/// Sends a contract's cancellation message to the partner.
pub trait PartnerPort: Send + Sync {
    /// Sends the message.
    ///
    /// # Errors
    /// A stable failure code; the undo stays held and can be resumed.
    fn send_cancellation(&self, call: &CancellationCall<'_>) -> Result<(), String>;
}

/// Reduces a code a port reported to the stable code shape `[a-z0-9_.]{1,64}`; anything else
/// becomes `fallback`, so text a peer sent can never ride along.
#[must_use]
pub(crate) fn stable_code(raw: &str, fallback: &str) -> String {
    let ok = !raw.is_empty()
        && raw.len() <= 64
        && raw
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.');
    if ok {
        raw.to_owned()
    } else {
        fallback.to_owned()
    }
}

/// Keeps the dependants and alternatives of a refusal within their bounds.
pub(crate) fn bound(refusal: &Refusal) -> (Vec<RecordRef>, Vec<Alternative>) {
    let mut dependants = refusal.dependants.clone();
    dependants.sort();
    dependants.dedup();
    dependants.truncate(MAX_DEPENDANTS);
    let mut alternatives: Vec<Alternative> = Vec::new();
    for a in &refusal.alternatives {
        if !alternatives.iter().any(|b| b.id == a.id) {
            alternatives.push(a.clone());
        }
    }
    alternatives.truncate(MAX_ALTERNATIVES);
    (dependants, alternatives)
}

fn is_token(raw: &str) -> bool {
    let mut chars = raw.chars();
    raw.len() <= 64
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}
