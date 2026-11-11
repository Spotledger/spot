//! Scoped, time-boxed support sessions (WP-AI-06.T2, blueprint WS13): the pure model and
//! the check.
//!
//! A support agent (or a support person, for example someone connecting through the MCP
//! server on their own plan) acts on a customer's tenant only inside a **session grant**:
//! scoped to named DocTypes and actions, with a tier ceiling, limited in time, approved by
//! someone on the customer side, and revocable at any moment. In the shape of ADR-070's
//! emergency access: time-boxed, every transition recorded, reviewed afterwards.
//!
//! # Shape
//!
//! - [`tier`]: the [`Tier`](tier::Tier) ladder (`T0` to `T3`), re-exported from the agent
//!   authoriser's crate so there is one definition, and the `t0` to `t3` wire form
//!   ([`tier_snake`](tier::tier_snake)) stored grants keep.
//! - [`model`]: [`Subject`](model::Subject), [`Scope`](model::Scope) with
//!   [`DocTypeScope`](model::DocTypeScope) and [`ActionScope`](model::ActionScope),
//!   [`SessionSpec`](model::SessionSpec), [`SessionPolicy`](model::SessionPolicy),
//!   [`Approver`](model::Approver), [`SessionStatus`](model::SessionStatus) and
//!   [`DerivedStatus`](model::DerivedStatus), all serde-loadable.
//! - [`check`]: the one action question, [`ActionRequest`](check::ActionRequest) and
//!   [`Refusal`](check::Refusal).
//! - [`error`]: the one error type, with stable `session.*` codes.
//! - [`event`]: [`SessionEvent`](event::SessionEvent), one per transition, journalable.
//! - [`SessionGrant`], [`PendingGrant`]: the lifecycle and [`SessionGrant::check`].
//!
//! # The time convention (the boundary choice, stated once)
//!
//! Time is always an argument: the crate has no clock, no background job and no network.
//! A session runs over the **half-open window** `[starts_at, expires_at)` in unix
//! seconds: `starts_at` is inclusive (at its start instant the session may act — the
//! approval should not leave the customer waiting past the second), while `expires_at`
//! is **exclusive-failure**: exactly at `expires_at` the session is already expired and
//! every check refuses with `session.expired`, as do the status derivations. Extending a
//! session whose window has therefore closed is refused (`SessionError::NotExtendable`);
//! the way back is a new grant. This keeps "runs until 16:30" honest in every display
//! (it means through 16:29) and makes the boundary test exact.
//!
//! # Purity
//!
//! No storage, no clock, no ambient reads, no floats, no `unsafe` (the project coding rules).
//! Every decision carries a reason; every refusal carries a stable `session.*` code.

#![forbid(unsafe_code)]

pub mod check;
pub mod error;
pub mod event;
pub mod model;
pub mod tier;

use std::fmt;

use serde::{Deserialize, Serialize};

pub use check::{ActionRequest, Refusal};
pub use error::SessionError;
pub use event::SessionEvent;
pub use model::{
    ActionScope, Approver, DerivedStatus, DocTypeScope, Extension, Scope, SessionPolicy,
    SessionSpec, SessionStatus, Subject, SubjectKind,
};
pub use tier::Tier;

/// The canonical text of a unix-seconds instant for summaries and events: `2026-10-08
/// 16:30 UTC`, the civil form a customer reads (no time-zone maths beyond UTC's own).
///
/// A pure civil conversion from the epoch (no calendar library, so the crate keeps no
/// dependencies beyond the ones the brief allows): days since 1970-01-01 by the civil
/// calendar. Deterministic, and only ever used for people-facing text.
#[must_use]
pub fn format_epoch(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days, as `chrono`'s doc example states; correct for
    // every proleptic Gregorian date.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let (hh, mm) = (secs / 3600, (secs % 3600) / 60);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02} UTC")
}

/// A session grant, from approval (or later): what the subject may do, until when, and
/// the lifecycle that follows.
///
/// Built only through [`SessionGrant::request`] then [`PendingGrant::approve`] (or read
/// back from serde, the control plane's shape). The check and the status derivation read
/// only the fields below; there is no clock to read anywhere in the crate.
///
/// # Window boundary
///
/// The session is active over the half-open window `[starts_at, expires_at)` and expired
/// exactly at `expires_at` (see the crate docs' time convention).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct SessionGrant {
    /// The grant's id, as the caller assigned it in the spec.
    pub id: String,
    /// The tenant the session is about.
    pub tenant: String,
    /// Who may act inside the session.
    pub subject: Subject,
    /// The tier nothing inside the session may rise above.
    #[serde(with = "tier::tier_snake")]
    pub tier_ceiling: Tier,
    /// What the session is scoped to.
    pub scope: Scope,
    /// Why the session exists; never blank (a request with one is refused).
    pub reason: String,
    /// Who asked for the session.
    pub requested_by: String,
    /// Who approved the session.
    pub approved_by: String,
    /// The window the session runs over (half-open, unix seconds), in serde wire form.
    pub starts_at: i64,
    /// The window's exclusive end (unix seconds).
    pub expires_at: i64,
    /// The stored lifecycle state; expiry from the times is
    /// [`SessionGrant::status_at`]. `Expired` is a caller-persisted observation, never
    /// the crate's authority.
    pub status: SessionStatus,
    /// The approved extensions, in approval order (the history stays in the grant).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<Extension>,
    /// The revocation, when the grant was revoked: who, when and why. Revocation is
    /// final and immediate; a grant never reactivates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked: Option<Revocation>,
}

/// A revocation: who revoked the session, when and why.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Revocation {
    /// Who revoked the session: the tenant side or the subject.
    pub by: String,
    /// When (unix seconds).
    pub at: i64,
    /// Why.
    pub reason: String,
}

/// A requested grant, awaiting approval.
///
/// The only way to a [`SessionGrant`]; an approve that the policy refuses leaves the
/// pending grant standing, so the control plane can show the refusal and a corrected
/// approval can still go through.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct PendingGrant {
    /// The grant the request built (its `status` is [`SessionStatus::Pending`]).
    grant: SessionGrant,
    /// The transition so far, for the journal.
    event: SessionEvent,
}

impl fmt::Display for PendingGrant {
    /// The request's one-line view (the summary of its times and scope).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (pending)", self.grant.summary_sentence())
    }
}

/// Validates a request against the policy, in the order the brief fixes: a blank reason,
/// an empty `Named` doctype list, an [`DocTypeScope::AllRead`] scope with a ceiling above
/// `T0`, a window ending before it starts, a duration over
/// [`SessionPolicy::max_duration_secs`], and a start before `now` (a session cannot be
/// requested to have already begun).
fn validate_request(
    spec: &SessionSpec,
    policy: &SessionPolicy,
    now: i64,
) -> Result<(), SessionError> {
    if spec.reason.trim().is_empty() {
        return Err(SessionError::ReasonBlank);
    }
    match &spec.scope.doctypes {
        DocTypeScope::Named(names) => {
            if names.iter().all(|n| n.trim().is_empty()) {
                return Err(SessionError::ScopeEmpty("doctypes"));
            }
        }
        // AllRead is read-only across the tenant, so only the read-and-diagnose ceiling
        // may carry it (the brief: "only valid with a ceiling of T0").
        DocTypeScope::AllRead if spec.tier_ceiling != Tier::T0 => {
            return Err(SessionError::ApprovalPolicy(format!(
                "an AllRead scope needs a T0 ceiling, asked {}",
                spec.tier_ceiling.as_str()
            )));
        }
        DocTypeScope::AllRead => {}
    }
    if spec.expires_at <= spec.starts_at {
        return Err(SessionError::EndBeforeStart {
            start: spec.starts_at,
            end: spec.expires_at,
        });
    }
    let duration = spec.expires_at - spec.starts_at;
    if duration > policy.max_duration_secs {
        return Err(SessionError::DurationOverPolicy {
            asked: duration,
            max: policy.max_duration_secs,
        });
    }
    if spec.starts_at < now {
        return Err(SessionError::EndBeforeStart {
            start: spec.starts_at,
            end: spec.expires_at,
        });
    }
    Ok(())
}

impl SessionGrant {
    /// Requests a session: validates the spec against the policy and hands back the
    /// pending grant (its `Requested` event is [`PendingGrant::event`]).
    ///
    /// Refuses, in order: a blank [`SessionSpec::reason`], an empty `Named` doctype
    /// list, an [`DocTypeScope::AllRead`] scope with a ceiling above `T0`, a window
    /// ending before it starts, a start before `now` (a session cannot be requested to
    /// have already begun), and a duration over
    /// [`SessionPolicy::max_duration_secs`].
    ///
    /// # Errors
    ///
    /// [`SessionError`], with the codes above.
    pub fn request(
        spec: &SessionSpec,
        policy: &SessionPolicy,
        now: i64,
    ) -> Result<PendingGrant, SessionError> {
        validate_request(spec, policy, now)?;
        let grant = SessionGrant {
            id: spec.id.clone(),
            tenant: spec.tenant.clone(),
            subject: spec.subject.clone(),
            tier_ceiling: spec.tier_ceiling,
            scope: canonical_scope(&spec.scope),
            reason: spec.reason.trim().to_owned(),
            requested_by: spec.requested_by.clone(),
            approved_by: String::new(),
            starts_at: spec.starts_at,
            expires_at: spec.expires_at,
            status: SessionStatus::Pending,
            extensions: Vec::new(),
            revoked: None,
        };
        let event = SessionEvent::Requested {
            actor: grant.requested_by.clone(),
            at: now,
            reason: grant.reason.clone(),
        };
        Ok(PendingGrant { grant, event })
    }

    /// The liveness of the session at `now`, derived from the times only.
    ///
    /// Nothing needs a background job: the control plane derives the state on read, and
    /// a stored `Expired` status is a caller's bookkeeping, never authority.
    #[must_use]
    pub fn status_at(&self, now: i64) -> DerivedStatus {
        if now < self.starts_at {
            DerivedStatus::NotYetActive
        } else if now >= self.expires_at {
            DerivedStatus::Expired
        } else {
            DerivedStatus::Active
        }
    }

    /// Whether the session was revoked (final and immediate, never undone).
    #[must_use]
    pub fn is_revoked(&self) -> bool {
        self.revoked.is_some()
    }

    /// The revocation, when the grant was revoked.
    #[must_use]
    pub fn revocation(&self) -> Option<&Revocation> {
        self.revoked.as_ref()
    }

    /// The approved extensions, in approval order.
    #[must_use]
    pub fn extensions(&self) -> &[Extension] {
        &self.extensions
    }

    /// The one action question, at `now`.
    ///
    /// The refusals, first failing reason wins, in the fixed order:
    /// `wrong_tenant`, `not_yet_active`, `expired`, `revoked`, `above_ceiling`,
    /// `action_not_in_scope`, `doctype_not_in_scope`, `write_in_read_only_scope`.
    /// (Time is checked before revocation so an expired window is reported even when a
    /// revoked grant sits in a journal; both refuse everything anyway.)
    ///
    /// # Errors
    ///
    /// [`Refusal`], the first failing reason.
    pub fn check(&self, action: &ActionRequest, now: i64) -> Result<(), Refusal> {
        if action.tenant != self.tenant {
            return Err(Refusal::WrongTenant {
                tier: action.tier,
                action: action.tenant.clone(),
                grant: self.tenant.clone(),
            });
        }
        match self.status_at(now) {
            DerivedStatus::NotYetActive => {
                return Err(Refusal::NotYetActive {
                    tier: action.tier,
                    starts_at: self.starts_at,
                    now,
                });
            }
            DerivedStatus::Expired => {
                return Err(Refusal::Expired {
                    tier: action.tier,
                    expires_at: self.expires_at,
                    now,
                });
            }
            DerivedStatus::Active => {}
        }
        if let Some(r) = &self.revoked {
            return Err(Refusal::Revoked {
                at: r.at,
                by: r.by.clone(),
            });
        }
        if action.tier > self.tier_ceiling {
            return Err(Refusal::AboveCeiling {
                action: action.tier,
                ceiling: self.tier_ceiling,
            });
        }
        if let ActionScope::Only(actions) = &self.scope.actions
            && !actions.iter().any(|a| a == &action.action)
        {
            return Err(Refusal::ActionNotInScope {
                action: action.action.clone(),
            });
        }
        if let Some(doctype) = &action.doctype {
            match &self.scope.doctypes {
                DocTypeScope::Named(names) => {
                    if !names.iter().any(|n| n == doctype) {
                        return Err(Refusal::DoctypeNotInScope {
                            doctype: doctype.clone(),
                        });
                    }
                }
                DocTypeScope::AllRead => {
                    if crate::check::is_write(&action.action) {
                        return Err(Refusal::WriteInReadOnlyScope {
                            action: action.action.clone(),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Revokes the session, by the tenant side or by the subject: final and immediate,
    /// at any time, whatever the state. Returns the `Revoked` event.
    ///
    /// A grant already revoked cannot be revoked again (the state keeps the first
    /// revocation; a second call is a [`SessionError::WrongState`]).
    ///
    /// # Errors
    ///
    /// [`SessionError::WrongState`] when the grant is already revoked.
    pub fn revoke(
        &mut self,
        by: &str,
        reason: &str,
        now: i64,
    ) -> Result<SessionEvent, SessionError> {
        if self.revoked.is_some() {
            return Err(SessionError::WrongState(
                "the grant is already revoked; revocation is final".to_owned(),
            ));
        }
        self.revoked = Some(Revocation {
            by: by.to_owned(),
            at: now,
            reason: reason.trim().to_owned(),
        });
        self.status = SessionStatus::Revoked;
        Ok(SessionEvent::Revoked {
            actor: by.to_owned(),
            at: now,
            reason: reason.trim().to_owned(),
        })
    }

    /// Extends the session's end, as a **new approval**: the approver must satisfy the
    /// policy exactly as at approval time (distinct from the requester, tenant owner
    /// above [`SessionPolicy::max_tier_without_owner_approval`]).
    ///
    /// # Errors
    ///
    /// Fails when the grant is expired at `now`, revoked, the extension count would
    /// pass [`SessionPolicy::max_extensions`], the new end would push the total
    /// duration beyond [`SessionPolicy::max_duration_secs`] counted from the **original
    /// start**, the proposed end is at or before the current one, or the approver
    /// fails the policy. The extension history stays in the grant.
    pub fn extend(
        &mut self,
        new_expires_at: i64,
        approver: &Approver,
        now: i64,
        policy: &SessionPolicy,
    ) -> Result<SessionEvent, SessionError> {
        if let Some(r) = &self.revoked {
            return Err(SessionError::NotExtendable(format!(
                "revoked at {} by {}",
                format_epoch(r.at),
                r.by
            )));
        }
        match self.status_at(now) {
            DerivedStatus::Expired => {
                return Err(SessionError::NotExtendable(format!(
                    "expired at {}",
                    format_epoch(self.expires_at)
                )));
            }
            DerivedStatus::NotYetActive | DerivedStatus::Active => {}
        }
        if self.extensions.len() >= policy.max_extensions {
            return Err(SessionError::ExtensionPolicy(format!(
                "{} extension(s) already approved, the policy allows {}",
                self.extensions.len(),
                policy.max_extensions
            )));
        }
        if new_expires_at <= self.expires_at {
            return Err(SessionError::ExtensionNotLater {
                current: self.expires_at,
                end: new_expires_at,
            });
        }
        let total = new_expires_at - self.starts_at;
        if total > policy.max_duration_secs {
            return Err(SessionError::ExtensionPolicy(format!(
                "the total session would run {} s, over the policy maximum of {} s from the original start",
                total, policy.max_duration_secs
            )));
        }
        if policy.require_distinct_approver && approver.id == self.requested_by {
            return Err(SessionError::ApprovalPolicy(
                "the approver must differ from the requester".to_owned(),
            ));
        }
        if approver.id == self.subject.id {
            return Err(SessionError::ApprovalPolicy(
                "the approver must differ from the subject".to_owned(),
            ));
        }
        if self.tier_ceiling > policy.max_tier_without_owner_approval && !approver.is_tenant_owner {
            return Err(SessionError::ApprovalPolicy(format!(
                "a ceiling of {} needs a tenant owner's approval",
                self.tier_ceiling.as_str()
            )));
        }
        self.expires_at = new_expires_at;
        let extension = Extension {
            approver: approver.id.clone(),
            at: now,
            extended_to: new_expires_at,
        };
        let event = SessionEvent::extended(&extension);
        self.extensions.push(extension);
        self.status = SessionStatus::Active;
        Ok(event)
    }

    /// The one plain sentence a customer reads before approving, deterministic and free
    /// of clock reads: who may do what, up to which tier, until when, approved by whom.
    ///
    /// The wording names the scope in plain words: named DocTypes are listed, `AllRead`
    /// is "read and diagnose across the tenant", an `Only` action list is "and their
    /// listed actions" with the names, `Any` actions are "and any action it can".
    #[must_use]
    pub fn summary(&self, _now: i64) -> String {
        self.summary_sentence()
    }

    /// The sentence itself: the part of the summary that never depends on `now`.
    fn summary_sentence(&self) -> String {
        let subject = self.subject.kind.as_str();
        let doctypes = match &self.scope.doctypes {
            DocTypeScope::Named(names) => {
                if names.is_empty() {
                    "no DocType".to_owned()
                } else {
                    names.join(" and ")
                }
            }
            DocTypeScope::AllRead => "across the tenant".to_owned(),
        };
        let verb = match &self.scope.doctypes {
            DocTypeScope::AllRead => "read and diagnose".to_owned(),
            _ => match &self.scope.actions {
                ActionScope::Any => "act on".to_owned(),
                ActionScope::Only(actions) => {
                    if actions.is_empty() {
                        "list".to_owned()
                    } else {
                        format!("{} on", actions.join(" and "))
                    }
                }
            },
        };
        let actions = match (&self.scope.actions, &self.scope.doctypes) {
            (ActionScope::Only(actions), DocTypeScope::AllRead) if !actions.is_empty() => {
                format!(" through {}", actions.join(" and "))
            }
            _ => String::new(),
        };
        let approver = if self.approved_by.is_empty() {
            "not yet approved".to_owned()
        } else {
            format!("approved by {}", self.approved_by)
        };
        format!(
            "{subject} {} may {verb}{actions} {doctypes} up to {} until {}, {approver}",
            self.subject.id,
            self.tier_ceiling.as_str(),
            format_epoch(self.expires_at),
        )
    }
}

impl fmt::Display for SessionGrant {
    /// The summary sentence (the [`SessionGrant::summary`] form).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.summary_sentence())
    }
}

/// Trims, keeps non-blank entries, deduplicates and sorts a name list, so equal scopes
/// compare, serialise and summarise the same whatever order they were written in.
fn canonical_names(names: Vec<String>) -> Vec<String> {
    let set: std::collections::BTreeSet<String> = names
        .into_iter()
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
        .collect();
    set.into_iter().collect()
}

/// The canonical wire form of a scope: both name lists canonicalised.
fn canonical_scope(scope: &Scope) -> Scope {
    Scope {
        doctypes: match &scope.doctypes {
            DocTypeScope::Named(names) => DocTypeScope::Named(canonical_names(names.clone())),
            DocTypeScope::AllRead => DocTypeScope::AllRead,
        },
        actions: match &scope.actions {
            ActionScope::Any => ActionScope::Any,
            ActionScope::Only(actions) => ActionScope::Only(canonical_names(actions.clone())),
        },
    }
}

impl PendingGrant {
    /// The pending grant's id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.grant.id
    }

    /// The pending grant itself, read-only.
    #[must_use]
    pub fn grant(&self) -> &SessionGrant {
        &self.grant
    }

    /// The `Requested` event the request produced.
    #[must_use]
    pub fn event(&self) -> &SessionEvent {
        &self.event
    }

    /// Approves the request: the session becomes a [`SessionGrant`] with stored status
    /// [`SessionStatus::Active`], plus the `Approved` event.
    ///
    /// The approver must differ from the requester when
    /// [`SessionPolicy::require_distinct_approver`] (the default); a ceiling above
    /// [`SessionPolicy::max_tier_without_owner_approval`] needs the approver to be
    /// flagged `is_tenant_owner`.
    ///
    /// # Errors
    ///
    /// [`SessionError::ApprovalPolicy`] when the approver does not satisfy the policy.
    pub fn approve(
        self,
        approver: &Approver,
        now: i64,
        policy: &SessionPolicy,
    ) -> Result<(SessionGrant, SessionEvent), SessionError> {
        if policy.require_distinct_approver && approver.id == self.grant.requested_by {
            return Err(SessionError::ApprovalPolicy(
                "the approver must differ from the requester".to_owned(),
            ));
        }
        if approver.id == self.grant.subject.id {
            return Err(SessionError::ApprovalPolicy(
                "the approver must differ from the subject".to_owned(),
            ));
        }
        if self.grant.tier_ceiling > policy.max_tier_without_owner_approval
            && !approver.is_tenant_owner
        {
            return Err(SessionError::ApprovalPolicy(format!(
                "a ceiling of {} needs a tenant owner's approval",
                self.grant.tier_ceiling.as_str()
            )));
        }
        let mut grant = self.grant;
        grant.approved_by = approver.id.clone();
        grant.status = SessionStatus::Active;
        let event = SessionEvent::Approved {
            actor: approver.id.clone(),
            at: now,
            reason: format!(
                "approved {} up to {}",
                if matches!(grant.scope.doctypes, DocTypeScope::AllRead) {
                    "read and diagnose across the tenant".to_owned()
                } else {
                    "the session scope".to_owned()
                },
                grant.tier_ceiling.as_str()
            ),
        };
        Ok((grant, event))
    }
}
