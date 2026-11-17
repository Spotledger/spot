//! The governance guard as a pre-write transaction participant.
//!
//! The guards of `spot-bpm-governance` and `spot-bpm-entry` are `before_create` controller
//! steps. A controller has no transaction, and consuming a grant and writing an override's audit
//! row must commit or roll back **with the write**, so this module runs the same guards from a
//! [`TxParticipant`]: `before_write` runs after authorisation and before the storage write, in
//! the document's own transaction, for every channel (API, import, script, process step).
//!
//! # What it decides
//!
//! For a write to a record type the settings cover:
//!
//! - mode `free` (the default, and every type the settings do not list): allowed, nothing
//!   recorded;
//! - mode `governed`: allowed only with a request grant the authority verifies for **this actor**
//!   and the **current attempt** of an open step that may write the type; the grant is consumed
//!   in this transaction. Otherwise the audited outside-process permission with a reason of at
//!   least the floor, or, when `admin_override = true` and the actor holds an administrator
//!   role, an audited override. Otherwise refused with `bpm_governance.outside_process_denied`;
//! - mode `process_only`: a create is judged by the entry guard (a grant for a step of the
//!   type's entry process, else `bpm_entry.start_process` naming the process to start), a change
//!   needs a grant. No permission and no override applies.
//!
//! The command id proves nothing: it is an idempotency key the caller chooses. It is only a
//! request context when the authority authenticates it as a grant it signed for this actor.
//!
//! # Fail closed
//!
//! A policy that cannot be read, a consume that cannot reach the database and an audit row of a
//! refusal that cannot be written each end in a refusal, never an allow. The refusal's audit row
//! is written on its own connection, because the write's transaction is rolling back.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use spot_app_api::context::Actor;
use spot_app_api::error::{StorageOp, map_storage_error};
use spot_app_api::prelude::OffsetDateTime;
use spot_app_api::{AppError, AppResult, BoxFuture, Document, Operation};
use spot_bpm_entry::{EntryAttempt, EntryError, EntryProcesses, ProcessOnlyGuard, ScopeResolver};
use spot_bpm_governance::{
    AuditSink, CreateAttempt, GovernanceAudit, GovernanceDecision, GovernanceError,
    GovernanceGuard, GovernanceMode, OutsideProcess, PolicyStore, REASON_FIELD, RequestAuthority,
    Route, VerifiedRequest, actor_text, enqueue_governance,
};
use spot_document::engine::{TxParticipant, WriteIntent};
use spot_permissions::Principal;
use spot_postgres::PgTransaction;

use super::grants::micros;
use super::ledger::{AuditRow, ConsumeError, Ledger};

/// The stable name of this participant.
pub const NAME: &str = "process_governance";

/// The reason recorded for an administrator override. It is a token, not text a person typed.
const OVERRIDE_REASON: &str = "admin_override";

/// Who the acting principal is, as the permission model sees them.
pub trait PrincipalLookup: Send + Sync {
    /// The principal acting as `actor`. An actor that cannot be resolved has no assignments
    /// (so it holds nothing); the lookup never fails open.
    fn principal<'a>(&'a self, actor: &'a Actor) -> BoxFuture<'a, Principal>;
}

/// Everything the participant is built from.
pub struct Parts {
    /// The site (the tenant key of the ledger).
    pub site: String,
    /// The ledger the grants are consumed in and the refusals are recorded in.
    pub ledger: Ledger,
    /// The mode of each record type.
    pub policy: Arc<dyn PolicyStore>,
    /// The record types the settings cover; `None` asks the policy for every write.
    pub covered: Option<BTreeSet<String>>,
    /// The entry process of each record type.
    pub entries: Arc<EntryProcesses>,
    /// The scope a create is judged in.
    pub scopes: Arc<dyn ScopeResolver>,
    /// The authority that verifies grants.
    pub authority: Arc<dyn RequestAuthority>,
    /// Who holds the outside-process permission.
    pub outside: Arc<dyn OutsideProcess>,
    /// The acting principal.
    pub principals: Arc<dyn PrincipalLookup>,
    /// Whether an administrator may write a governed record outside a process.
    pub admin_override: bool,
    /// The role names that count as administrator.
    pub admin_roles: Vec<String>,
}

/// The participant. Install it with `Engine::set_participants` through the participants table
/// (see the open question in the module's report: the registry has no hook for a module to add
/// one).
pub struct GovernanceParticipant {
    parts: Arc<Parts>,
}

impl GovernanceParticipant {
    /// A participant over `parts`.
    #[must_use]
    pub fn new(parts: Parts) -> Self {
        Self {
            parts: Arc::new(parts),
        }
    }

    /// The participant, shared.
    #[must_use]
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }

    /// Decides one write; the participant's `before_write` is this, boxed.
    ///
    /// # Errors
    /// The refusal as the engine's error.
    pub async fn check(&self, tx: &mut PgTransaction, write: &WriteIntent<'_>) -> AppResult<()> {
        let parts = &*self.parts;
        if !parts.covers(write.doctype) {
            return Ok(());
        }
        // A delete carries no new document; the old one names the id and the company.
        let Some(doc) = write.new.or(write.old) else {
            return Ok(());
        };
        let inv = write.invocation;
        let who = Who {
            actor: &inv.actor,
            text: actor_text(&inv.actor),
            command_id: inv.command_id.as_ref().map(|c| c.as_str().to_owned()),
            now: inv.now,
        };
        let company = company_of(doc, &parts.site);
        let mode = match parts.policy.mode_at(&company, write.doctype, who.now) {
            Ok(mode) => mode,
            Err(error) => {
                return Err(parts
                    .refuse(
                        &who,
                        write.doctype,
                        doc,
                        &company,
                        "policy_unavailable",
                        &error,
                    )
                    .await);
            }
        };
        match mode {
            GovernanceMode::Free => Ok(()),
            GovernanceMode::Governed => self.governed(tx, write, doc, &who, &company).await,
            GovernanceMode::ProcessOnly => self.process_only(tx, write, doc, &who, &company).await,
        }
    }

    async fn governed(
        &self,
        tx: &mut PgTransaction,
        write: &WriteIntent<'_>,
        doc: &Document,
        who: &Who<'_>,
        company: &str,
    ) -> AppResult<()> {
        let parts = &*self.parts;
        let request = who.request(&*parts.authority);
        let usable = request
            .as_ref()
            .filter(|step| parts.authority.may_create(step, write.doctype));
        if let Some(step) = usable {
            return parts
                .consume(tx, step, write.doctype, doc, who, company)
                .await;
        }
        // No usable grant: who is asking decides what is left.
        let principal = parts.principals.principal(who.actor).await;
        if parts.admin_override && parts.is_admin(&principal) {
            let entry = GovernanceAudit::outside_process(
                who.actor,
                company,
                write.doctype,
                doc.id(),
                OVERRIDE_REASON,
                who.command_id.as_deref(),
                who.now,
            );
            return parts.audit_in_tx(tx, &entry, "admin_override").await;
        }
        let sink = Arc::new(BufferSink::default());
        let guard = GovernanceGuard::new(
            Arc::clone(&parts.policy),
            Arc::clone(&parts.authority),
            Arc::clone(&parts.outside),
            Arc::clone(&sink) as Arc<dyn AuditSink>,
        );
        let reason = doc.text(REASON_FIELD).ok().flatten();
        let attempt = CreateAttempt {
            company,
            doctype: write.doctype,
            document_id: doc.id(),
            actor: who.actor,
            principal: &principal,
            reason,
            request: request.as_ref(),
            command_id: who.command_id.as_deref(),
            now: who.now,
        };
        match guard.before_create(&attempt) {
            Ok(GovernanceDecision::Allow {
                route: Route::OutsideProcess,
            }) => {
                for entry in sink.take() {
                    parts.audit_in_tx(tx, &entry, "outside_process").await?;
                }
                Ok(())
            }
            Ok(GovernanceDecision::Allow { .. } | GovernanceDecision::Defer { .. }) => Ok(()),
            Ok(GovernanceDecision::Refuse { reason }) => {
                let error = match reason {
                    spot_bpm_governance::RefusalReason::ReasonRequired { min_chars } => {
                        GovernanceError::ReasonRequired {
                            doctype: write.doctype.to_owned(),
                            min_chars,
                        }
                    }
                    _ => GovernanceError::OutsideProcessDenied {
                        doctype: write.doctype.to_owned(),
                    },
                };
                let rule = match &error {
                    GovernanceError::ReasonRequired { .. } => "reason_required",
                    _ => "outside_process_denied",
                };
                Err(parts
                    .refusal(sink.take(), rule, &error, who, write.doctype, doc, company)
                    .await)
            }
            Err(error) => Err(parts
                .refusal(
                    sink.take(),
                    "guard_unavailable",
                    &error,
                    who,
                    write.doctype,
                    doc,
                    company,
                )
                .await),
        }
    }

    async fn process_only(
        &self,
        tx: &mut PgTransaction,
        write: &WriteIntent<'_>,
        doc: &Document,
        who: &Who<'_>,
        company: &str,
    ) -> AppResult<()> {
        let parts = &*self.parts;
        let request = who.request(&*parts.authority);
        if write.operation != Operation::Create {
            // Changing a process_only record is the process's work: a grant, nothing else.
            let ok = request
                .as_ref()
                .is_some_and(|step| parts.authority.may_create(step, write.doctype));
            return match (ok, request.as_ref()) {
                (true, Some(step)) => {
                    parts
                        .consume(tx, step, write.doctype, doc, who, company)
                        .await
                }
                _ => {
                    let error = GovernanceError::OutsideProcessDenied {
                        doctype: write.doctype.to_owned(),
                    };
                    let entry = refusal_entry(
                        who,
                        company,
                        write.doctype,
                        doc,
                        GovernanceMode::ProcessOnly,
                        "a process_only record is changed only by its process",
                    );
                    Err(parts
                        .refusal(
                            vec![entry],
                            "outside_process_denied",
                            &error,
                            who,
                            write.doctype,
                            doc,
                            company,
                        )
                        .await)
                }
            };
        }
        let claimed = doc.text("company").ok().flatten();
        let Some(scope) = parts.scopes.resolve(&who.text, claimed) else {
            let error = EntryError::ScopeInvalid {
                doctype: write.doctype.to_owned(),
            };
            let entry = refusal_entry(
                who,
                company,
                write.doctype,
                doc,
                GovernanceMode::ProcessOnly,
                "the company the record names is not one the actor may create in",
            );
            let app = error.to_app_error();
            parts
                .record(&[entry], "scope_invalid", who, write.doctype, doc, company)
                .await;
            return Err(app);
        };
        let sink = Arc::new(BufferSink::default());
        let guard = ProcessOnlyGuard::new(
            Arc::clone(&parts.policy),
            Arc::clone(&parts.entries),
            Arc::clone(&parts.authority),
            Arc::clone(&sink) as Arc<dyn AuditSink>,
        );
        let attempt = EntryAttempt {
            scope: &scope,
            doctype: write.doctype,
            document_id: doc.id(),
            actor: who.actor,
            request: request.as_ref(),
            command_id: who.command_id.as_deref(),
            now: who.now,
        };
        match guard.check_create(&attempt) {
            Ok(()) => match request.as_ref() {
                // The mode can only be `process_only` here, so the entry guard allowed a
                // request step: its grant is consumed like any other.
                Some(step) => {
                    parts
                        .consume(tx, step, write.doctype, doc, who, &scope)
                        .await
                }
                None => Ok(()),
            },
            Err(error) => {
                let app = error.to_app_error();
                let rule = match &error {
                    EntryError::StartProcess { .. } => "start_process",
                    _ => "entry_unavailable",
                };
                parts
                    .record(&sink.take(), rule, who, write.doctype, doc, &scope)
                    .await;
                Err(app)
            }
        }
    }
}

impl TxParticipant<PgTransaction> for GovernanceParticipant {
    fn before_write<'a>(
        &'a self,
        tx: &'a mut PgTransaction,
        write: &'a WriteIntent<'a>,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move { self.check(tx, write).await })
    }
}

/// Who is writing, as the guards see them.
struct Who<'a> {
    actor: &'a Actor,
    text: String,
    command_id: Option<String>,
    now: OffsetDateTime,
}

impl Who<'_> {
    /// The request step the authority verifies for this actor: the command id by itself proves
    /// nothing.
    fn request(&self, authority: &dyn RequestAuthority) -> Option<VerifiedRequest> {
        self.command_id
            .as_deref()
            .and_then(|key| authority.authenticate(key, &self.text))
    }
}

impl Parts {
    fn covers(&self, doctype: &str) -> bool {
        self.covered
            .as_ref()
            .is_none_or(|covered| covered.contains(doctype))
    }

    fn is_admin(&self, principal: &Principal) -> bool {
        principal
            .assignments
            .iter()
            .flat_map(|assignment| assignment.roles.iter())
            .any(|role| self.admin_roles.iter().any(|name| name == role.as_str()))
    }

    /// Consumes the grant of `step` in `tx`. A stale, replayed or unreachable one is a refusal.
    async fn consume(
        &self,
        tx: &mut PgTransaction,
        step: &VerifiedRequest,
        doctype: &str,
        doc: &Document,
        who: &Who<'_>,
        company: &str,
    ) -> AppResult<()> {
        let outcome = self
            .ledger
            .consume(
                tx,
                step,
                doctype,
                &doc.id().to_string(),
                &who.text,
                micros(who.now),
            )
            .await;
        match outcome {
            Ok(()) => Ok(()),
            Err(ConsumeError::Storage(error)) => Err(map_storage_error(&error, StorageOp::Other)),
            Err(failure) => {
                let (rule, detail) = match failure {
                    ConsumeError::Replay => (
                        "grant_replayed",
                        "the request grant was already used for a write",
                    ),
                    _ => (
                        "grant_stale",
                        "the request grant is closed, expired, for another attempt or for another record type",
                    ),
                };
                let error = GovernanceError::OutsideProcessDenied {
                    doctype: doctype.to_owned(),
                };
                let entry =
                    refusal_entry(who, company, doctype, doc, GovernanceMode::Governed, detail);
                Err(self
                    .refusal(vec![entry], rule, &error, who, doctype, doc, company)
                    .await)
            }
        }
    }

    /// Writes `entry` inside the write's transaction (an override: it commits with the write)
    /// as an outbox event and a ledger row.
    async fn audit_in_tx(
        &self,
        tx: &mut PgTransaction,
        entry: &GovernanceAudit,
        rule: &str,
    ) -> AppResult<()> {
        enqueue_governance(tx, entry)
            .await
            .map_err(|error| error.to_app_error())?;
        self.ledger
            .audit_in_tx(tx, &audit_row(entry, rule))
            .await
            .map_err(|error| map_storage_error(&error, StorageOp::Other))
    }

    /// Records refusals on their own connection. A failure to record is logged by code and does
    /// not change the outcome: the write is refused either way.
    async fn record(
        &self,
        entries: &[GovernanceAudit],
        rule: &str,
        who: &Who<'_>,
        doctype: &str,
        doc: &Document,
        company: &str,
    ) {
        let rows: Vec<AuditRow> = if entries.is_empty() {
            vec![AuditRow {
                at_us: micros(who.now),
                kind: "refused".to_owned(),
                rule: rule.to_owned(),
                actor: who.text.clone(),
                doctype: doctype.to_owned(),
                document_id: doc.id().to_string(),
                company: company.to_owned(),
                detail: String::new(),
            }]
        } else {
            entries.iter().map(|entry| audit_row(entry, rule)).collect()
        };
        for row in rows {
            if self.ledger.record_audit(&row).await.is_err() {
                tracing::error!(rule, "the governance refusal could not be recorded");
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn refusal(
        &self,
        entries: Vec<GovernanceAudit>,
        rule: &str,
        error: &GovernanceError,
        who: &Who<'_>,
        doctype: &str,
        doc: &Document,
        company: &str,
    ) -> AppError {
        self.record(&entries, rule, who, doctype, doc, company)
            .await;
        error.to_app_error()
    }

    /// A refusal for an error the policy raised (it could not be read): fail closed.
    async fn refuse(
        &self,
        who: &Who<'_>,
        doctype: &str,
        doc: &Document,
        company: &str,
        rule: &str,
        error: &GovernanceError,
    ) -> AppError {
        self.record(&[], rule, who, doctype, doc, company).await;
        error.to_app_error()
    }
}

/// The company a record names, or the site when it names none.
fn company_of(doc: &Document, site: &str) -> String {
    doc.text("company")
        .ok()
        .flatten()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map_or_else(|| site.to_owned(), str::to_owned)
}

fn refusal_entry(
    who: &Who<'_>,
    company: &str,
    doctype: &str,
    doc: &Document,
    mode: GovernanceMode,
    detail: &str,
) -> GovernanceAudit {
    GovernanceAudit::refused(
        who.actor,
        company,
        doctype,
        doc.id(),
        mode,
        detail,
        who.command_id.as_deref(),
        who.now,
    )
}

/// The ledger row of an audit entry: ids and tokens only, never the reason text.
fn audit_row(entry: &GovernanceAudit, rule: &str) -> AuditRow {
    AuditRow {
        at_us: micros(entry.at),
        kind: entry.kind.as_str().to_owned(),
        rule: rule.to_owned(),
        actor: actor_text(&entry.actor),
        doctype: entry.doctype.clone().unwrap_or_default(),
        document_id: entry
            .document_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
        company: entry.company.clone(),
        detail: entry.detail.clone().unwrap_or_default(),
    }
}

/// Collects what a guard wants audited, so the participant writes it where it belongs: inside
/// the transaction for an allow, on its own connection for a refusal.
#[derive(Default)]
struct BufferSink(Mutex<Vec<GovernanceAudit>>);

impl BufferSink {
    fn take(&self) -> Vec<GovernanceAudit> {
        std::mem::take(
            &mut *self
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }
}

impl AuditSink for BufferSink {
    fn write(&self, entry: &GovernanceAudit) -> Result<(), GovernanceError> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(entry.clone());
        Ok(())
    }
}
