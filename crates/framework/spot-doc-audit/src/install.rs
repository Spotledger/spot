//! Registering the audit step with a controller registry (WP-DOC-10.T1).
//!
//! The audit step is an ordinary [`Controller`](spot_app_api::Controller): a host registers it
//! for a DocType like any other controller, then installs the set's steps into the seam registry
//! with [`ControllerSet::install_into`](spot_controllers::ControllerSet::install_into). Nothing
//! here is controller-specific: this module only saves a caller from naming `Arc` and
//! [`ControllerSource`](spot_controllers::ControllerSource).

use std::sync::Arc;

use spot_controllers::{ControllerRegistry, ControllerSource};
use spot_document::engine::TxParticipant;
use spot_storage_contract::Transaction;

use crate::error::DocAuditError;
use crate::participant::AuditParticipant;
use crate::step::{AuditStep, is_audit_step_name};

/// Registers `step` for `app`.
pub fn register(registry: &mut ControllerRegistry, app: &str, step: AuditStep) {
    registry.register(app, Arc::new(step), ControllerSource::Native);
}

/// The participant list for an engine that supports the transaction seam: the audit participant
/// first, then `others`. Hand the result to `Engine::set_participants`; it writes the audit event
/// in each document's own transaction, so do **not** also [`register`] an
/// [`AuditStep`](crate::AuditStep) for the same DocType.
#[must_use]
pub fn with_audit_participant<Tx: Transaction + 'static>(
    participant: AuditParticipant,
    others: Vec<Arc<dyn TxParticipant<Tx>>>,
) -> Vec<Arc<dyn TxParticipant<Tx>>> {
    let mut all: Vec<Arc<dyn TxParticipant<Tx>>> = Vec::with_capacity(others.len() + 1);
    all.push(Arc::new(participant));
    all.extend(others);
    all
}

/// Where a participant sits in the order (WP-SEC-06.T3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The audit participant.
    Audit,
    /// A partition-ownership check.
    OwnerCheck,
    /// Any other participant: keeps its relative order.
    Other,
    /// Edge sync (it derives from the final document).
    EdgeSync,
}

/// A participant that says where it sits in the order.
pub trait ParticipantRole {
    /// The role.
    fn role(&self) -> Role;
}

impl ParticipantRole for AuditParticipant {
    fn role(&self) -> Role {
        Role::Audit
    }
}

/// A transaction participant with its [`Role`]. A participant that does not say is
/// [`Role::Other`] (`Placed::from(participant)`).
pub struct Placed<Tx: Transaction> {
    role: Role,
    participant: Arc<dyn TxParticipant<Tx>>,
}

impl<Tx: Transaction> Placed<Tx> {
    /// `participant` in `role`.
    #[must_use]
    pub fn new(role: Role, participant: Arc<dyn TxParticipant<Tx>>) -> Self {
        Self { role, participant }
    }
}

impl<Tx: Transaction> From<Arc<dyn TxParticipant<Tx>>> for Placed<Tx> {
    fn from(participant: Arc<dyn TxParticipant<Tx>>) -> Self {
        Self::new(Role::Other, participant)
    }
}

impl<Tx: Transaction> ParticipantRole for Placed<Tx> {
    fn role(&self) -> Role {
        self.role
    }
}

/// The participant list for an engine, checked and ordered: **audit, then the owner checks, then
/// the rest, edge sync last**; each group keeps its relative order. The host hands the result to
/// `Engine::set_participants` and calls nothing else.
///
/// Why this order. The audit must see the write the owner check lets through (a write the owner
/// check refuses never happens, so it must not be audited as made), and it must have recorded
/// the write when a *later* participant refuses it: that refusal rolls the audit event back
/// with the write, so nothing is left behind. Edge sync derives edges from the final document
/// and so runs after everything that can still refuse.
///
/// # What it refuses
///
/// - `docaudit.double_registration` when a controller in `registry` is an [`AuditStep`] on one
///   of the DocTypes the participant was told it audits
///   ([`AuditParticipant::with_doctypes`]). The registry cannot list its DocTypes, so the host
///   names them; a participant told none checks none. The step is recognised by its declared
///   step name (`platform:doc_audit_*`), which needs no change outside this crate.
/// - the same code when `others` holds a second audit participant.
///
/// # Errors
/// [`DocAuditError::DoubleRegistration`], naming the app and DocType (never a value).
pub fn checked_participants<Tx: Transaction + 'static>(
    participant: AuditParticipant,
    others: Vec<Placed<Tx>>,
    registry: &ControllerRegistry,
) -> Result<Vec<Arc<dyn TxParticipant<Tx>>>, DocAuditError> {
    for doctype in participant.doctypes() {
        for registered in registry.controllers_for(doctype) {
            let is_audit_step = registered
                .controller
                .steps()
                .iter()
                .any(|decl| is_audit_step_name(decl.name.as_str()));
            if is_audit_step {
                return Err(DocAuditError::DoubleRegistration {
                    app: registered.app,
                    doctype: doctype.clone(),
                });
            }
        }
    }
    if others.iter().any(|p| p.role() == Role::Audit) {
        return Err(DocAuditError::DoubleRegistration {
            app: String::new(),
            doctype: String::new(),
        });
    }
    let mut all: Vec<Arc<dyn TxParticipant<Tx>>> = Vec::with_capacity(others.len() + 1);
    all.push(Arc::new(participant));
    for role in [Role::OwnerCheck, Role::Other, Role::EdgeSync] {
        all.extend(
            others
                .iter()
                .filter(|p| p.role() == role)
                .map(|p| Arc::clone(&p.participant)),
        );
    }
    Ok(all)
}
