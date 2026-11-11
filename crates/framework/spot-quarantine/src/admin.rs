//! The admin operations API: serialisable view structs and the permission checks behind them.
//!
//! The HTTP layer and the console screen are separate tasks (WP-EDG-06.T3 and the Admin Center);
//! this module is what they call. Every function is plain Rust, returns view structs that cross a
//! process boundary as JSON, and checks the acting principal through `spot-permissions`.
//!
//! # What a permission is asked against
//!
//! A quarantine item is not a document, so the decision is asked against **the quarantined
//! record's own entity** ([`RecordRef::entity`](crate::item::RecordRef::entity)): to see the
//! queue you need `read` on the record's DocType, to retry or release it you need `update`, and to
//! discard it you need `delete`. That keeps the queue inside the ordinary permission model
//! (ADR-070) with no new DocType. Whether a site instead wants a dedicated `quarantine_item`
//! DocType is a policy question for ADR-067's health surface; see the PR's open questions.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use spot_permissions::{Action, DocFacts, PermissionEngine, Principal};

use crate::error::QuarantineError;
use crate::item::{ClosureOutcome, ClosureRecord, ItemFilter, ItemId, QuarantineItem, State};
use crate::reason;
use crate::retry::{Condition, RetryPolicy, Severity};
use crate::store::QuarantineStore;

/// One operation a caller may ask the queue for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// See the queue and one item.
    View,
    /// Retry one item or all held items.
    Retry,
    /// Release one item (re-apply it).
    Release,
    /// Discard one item.
    Discard,
}

impl Access {
    /// The stable text form, used in messages and reasons.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Access::View => "view",
            Access::Retry => "retry",
            Access::Release => "release",
            Access::Discard => "discard",
        }
    }

    /// The lifecycle action this access needs.
    ///
    /// `None` when the engine does not know the action; the guard then denies (fail closed),
    /// rather than downgrading a mutation to a plain read.
    fn action(self) -> Option<Action> {
        match self {
            Access::View => Some(Action::Read),
            Access::Retry | Access::Release => Action::parse("update"),
            Access::Discard => Action::parse("delete"),
        }
    }
}

/// The seal on [`QuarantineGuard`]: only the types named in this module can implement it.
mod sealed {
    /// Implemented by the permission-engine adapter and the test doubles, nothing else.
    pub trait Sealed {}
}

/// The permission seam: whether a principal may do `access` on one item.
///
/// The trait is **sealed**: the only guard a production build can hold is
/// [`EngineGuard`], which asks the permission engine. A [`CloseAuthority`] is therefore always
/// proof of a real permission check, never of an allow-all. The test doubles (`AllowAll`,
/// `DenyAll`) exist only under `cfg(test)` or the `testing` feature.
///
/// With the default features the test guard cannot even be named:
///
#[cfg_attr(
    not(feature = "testing"),
    doc = "```compile_fail\nuse spot_quarantine::AllowAll;\n```\n"
)]
/// ```compile_fail
/// use spot_permissions::Principal;
/// use spot_quarantine::{Access, QuarantineGuard, QuarantineItem};
///
/// struct Anything;
/// impl QuarantineGuard for Anything {
///     fn may(&self, _: &Principal, _: Access, _: &QuarantineItem) -> bool {
///         true
///     }
/// }
/// ```
pub trait QuarantineGuard: sealed::Sealed + Send + Sync {
    /// Whether `principal` may perform `access` on `item`.
    fn may(&self, principal: &Principal, access: Access, item: &QuarantineItem) -> bool;
}

/// The real guard: `spot-permissions` asked against the quarantined record's entity.
pub struct EngineGuard {
    engine: Arc<PermissionEngine>,
}

impl EngineGuard {
    /// A guard over a permission engine.
    #[must_use]
    pub fn new(engine: Arc<PermissionEngine>) -> Self {
        Self { engine }
    }
}

impl sealed::Sealed for EngineGuard {}

impl QuarantineGuard for EngineGuard {
    fn may(&self, principal: &Principal, access: Access, item: &QuarantineItem) -> bool {
        let facts = DocFacts {
            doctype: item.record_ref.entity.clone(),
            id: Some(item.record_ref.id.clone()),
            ..DocFacts::default()
        };
        let Some(action) = access.action() else {
            return false;
        };
        self.engine.can(principal, action, &facts).allowed
    }
}

/// A guard that admits everything, for tests that do not exercise permissions.
///
/// It exists only under `cfg(test)` or the `testing` feature; a default build cannot name it, so
/// no production code path can mint a [`CloseAuthority`] from it.
#[cfg(any(test, feature = "testing"))]
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAll;

#[cfg(any(test, feature = "testing"))]
impl sealed::Sealed for AllowAll {}

#[cfg(any(test, feature = "testing"))]
impl QuarantineGuard for AllowAll {
    fn may(&self, _principal: &Principal, _access: Access, _item: &QuarantineItem) -> bool {
        true
    }
}

/// A guard that denies everything, for tests that prove a check is reached. Same availability as
/// `AllowAll`.
#[cfg(any(test, feature = "testing"))]
#[derive(Clone, Copy, Debug, Default)]
pub struct DenyAll;

#[cfg(any(test, feature = "testing"))]
impl sealed::Sealed for DenyAll {}

#[cfg(any(test, feature = "testing"))]
impl QuarantineGuard for DenyAll {
    fn may(&self, _principal: &Principal, _access: Access, _item: &QuarantineItem) -> bool {
        false
    }
}

/// Proof that closing one item was authorised: a token only this crate can mint.
///
/// [`QuarantineStore::close`] needs one, and the only ways to get one are [`authorize_close`]
/// (which asks the permission guard about the acting principal) and the intake's own automatic
/// retry. Its field is private, so code that holds only a store cannot close an item. A token
/// names the item and the outcome it was issued for; the store refuses it for any other.
#[derive(Debug)]
pub struct CloseAuthority {
    item: ItemId,
    outcome: ClosureOutcome,
    #[allow(dead_code)]
    seal: (),
}

impl CloseAuthority {
    /// The item the token was issued for.
    #[must_use]
    pub fn item(&self) -> ItemId {
        self.item
    }

    /// The outcome the token was issued for.
    #[must_use]
    pub fn outcome(&self) -> ClosureOutcome {
        self.outcome
    }

    /// The token for the intake's own automatic retry, which needs no operator permission.
    pub(crate) fn for_automation(item: ItemId) -> Self {
        Self {
            item,
            outcome: ClosureOutcome::Released,
            seal: (),
        }
    }
}

/// Asks `guard` whether `principal` may close `item` with `outcome`, and mints the token if so.
///
/// `access` is the operation being performed: [`Access::Retry`] or [`Access::Release`] for a
/// release, [`Access::Discard`] for a discard. Any other pairing is refused.
///
/// # Errors
/// [`QuarantineError::Denied`] when the guard refuses or the pairing is not one of the above.
pub fn authorize_close(
    guard: &dyn QuarantineGuard,
    principal: &Principal,
    access: Access,
    item: &QuarantineItem,
    outcome: ClosureOutcome,
) -> Result<CloseAuthority, QuarantineError> {
    let fits = match outcome {
        ClosureOutcome::Released => matches!(access, Access::Retry | Access::Release),
        ClosureOutcome::Discarded => access == Access::Discard,
    };
    if !fits || !guard.may(principal, access, item) {
        return Err(QuarantineError::Denied {
            access: access.as_str(),
            id: item.id,
        });
    }
    Ok(CloseAuthority {
        item: item.id,
        outcome,
        seal: (),
    })
}

/// One item as the console screen sees it: the metadata, the plain-language reason, and when the
/// next automatic retry is due.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemView {
    /// The queue id.
    pub id: ItemId,
    /// The sender.
    pub source: String,
    /// The record's stream sequence.
    pub seq: u64,
    /// The apply key's name part.
    pub name: String,
    /// The apply key's version part.
    pub version: u64,
    /// The root document's entity.
    pub entity: String,
    /// The root document's record id.
    pub record_id: String,
    /// The rejection code.
    pub reason_code: String,
    /// Whether a retry can help once the cause is fixed.
    pub retryable: bool,
    /// How urgent it is.
    pub severity: Severity,
    /// One sentence in plain language.
    pub summary: String,
    /// The suggested action.
    pub remedy: String,
    /// The apply path's own explanation, or what the item waits behind.
    pub detail: String,
    /// When it was first seen (Unix microseconds).
    pub first_seen: i64,
    /// How many automatic retries have run.
    pub attempts: u32,
    /// Where it is in its life.
    pub state: State,
    /// When its next automatic retry is due (Unix microseconds).
    pub next_attempt_at: i64,
    /// Who closed it, when it is released or discarded.
    pub closed_by: Option<String>,
    /// When it was closed (Unix microseconds), when it is released or discarded.
    pub closed_at: Option<i64>,
}

impl ItemView {
    /// The view of one item under a retry policy.
    #[must_use]
    pub fn of(item: &QuarantineItem, policy: &RetryPolicy) -> Self {
        let info = reason::describe(&item.reason_code);
        Self {
            id: item.id,
            source: item.source.as_str().to_owned(),
            seq: item.seq,
            name: item.record_ref.name.as_str().to_owned(),
            version: item.record_ref.version,
            entity: item.record_ref.entity.clone(),
            record_id: item.record_ref.id.clone(),
            reason_code: item.reason_code.clone(),
            retryable: info.retryable,
            severity: info.severity,
            summary: info.summary,
            remedy: info.remedy,
            detail: item.detail.clone(),
            first_seen: item.first_seen,
            attempts: item.attempts,
            state: item.state,
            next_attempt_at: policy.due_at(item),
            closed_by: None,
            closed_at: None,
        }
    }

    /// The same view with who closed the item and when.
    #[must_use]
    pub fn with_closure(mut self, closure: Option<ClosureRecord>) -> Self {
        if let Some(closure) = closure {
            self.closed_by = Some(closure.by);
            self.closed_at = Some(closure.at);
        }
        self
    }
}

/// A condition as the console screen sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConditionView {
    /// The stable condition name.
    pub code: String,
    /// How urgent it is.
    pub severity: Severity,
    /// What it is about.
    pub subject: String,
    /// One sentence in plain language.
    pub sentence: String,
    /// The suggested action.
    pub action: String,
}

impl From<Condition> for ConditionView {
    fn from(condition: Condition) -> Self {
        Self {
            code: condition.code,
            severity: condition.severity,
            subject: condition.subject,
            sentence: condition.sentence,
            action: condition.action,
        }
    }
}

/// What an operation did to an item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationResult {
    /// Re-applied successfully.
    Released,
    /// Re-applied and refused again; still held.
    StillHeld,
    /// An operator discarded it.
    Discarded,
    /// The item was already closed.
    Skipped,
}

/// The outcome of one operation, as the console screen sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationView {
    /// The item after the operation.
    pub item: ItemView,
    /// What happened.
    pub result: OperationResult,
    /// The condition raised, when the item exhausted its automatic retries.
    pub condition: Option<ConditionView>,
}

/// Every item the principal may view, in insertion order.
///
/// # Errors
/// [`QuarantineError::Store`] when the store fails.
pub async fn list(
    store: &dyn QuarantineStore,
    guard: &dyn QuarantineGuard,
    principal: &Principal,
    filter: &ItemFilter,
    policy: &RetryPolicy,
) -> Result<Vec<ItemView>, QuarantineError> {
    let items = store.list(filter).await?;
    let mut views = Vec::new();
    for item in items
        .iter()
        .filter(|item| guard.may(principal, Access::View, item))
    {
        let closure = if item.state.is_open() {
            None
        } else {
            store.closure(item.id).await?
        };
        views.push(ItemView::of(item, policy).with_closure(closure));
    }
    Ok(views)
}

/// One item the principal may view, or `None` when it does not exist.
///
/// # Errors
/// [`QuarantineError::Denied`] when the principal may not view it, or
/// [`QuarantineError::Store`] when the store fails.
pub async fn get(
    store: &dyn QuarantineStore,
    guard: &dyn QuarantineGuard,
    principal: &Principal,
    id: ItemId,
    policy: &RetryPolicy,
) -> Result<Option<ItemView>, QuarantineError> {
    let Some(item) = store.item(id).await? else {
        return Ok(None);
    };
    if !guard.may(principal, Access::View, &item) {
        return Err(QuarantineError::Denied {
            access: Access::View.as_str(),
            id,
        });
    }
    let closure = if item.state.is_open() {
        None
    } else {
        store.closure(id).await?
    };
    Ok(Some(ItemView::of(&item, policy).with_closure(closure)))
}
