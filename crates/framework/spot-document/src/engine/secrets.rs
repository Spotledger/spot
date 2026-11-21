//! The engine's secret path (WP-SEC-03.T3): write, read, look up and re-wrap the sealed value
//! of a `secret` field, through a [`SecretAccess`].
//!
//! # What happens to a value
//!
//! - **Write.** [`Engine::set_secret`] and [`Engine::create_with_secrets`] seal the value
//!   *before* the transaction opens (the value never enters the transaction's working state),
//!   then write the sealed bytes and, for a `lookup = true` field, the blind index, in the
//!   record's normal write stage: the version check, the hooks (which see a document that holds
//!   no secret) and the lifecycle event (which names the field and shows no value) are the
//!   ordinary ones. A rollback of the surrounding transaction leaves neither the secret nor its
//!   index.
//! - **Every other write** of the record carries the sealed columns over unchanged, so an
//!   ordinary edit never unsets a credential.
//! - **Read.** [`Engine::open_secret`] opens one value for one record. There is no bulk read.
//! - **Lookup.** [`Engine::find_by_secret`] derives the blind index of the value and does one
//!   indexed equality on `<field>__bidx`. A field that is not a lookup field is refused, never
//!   read as "found nothing".
//! - **Rotation.** [`Engine::rewrap_secrets`] moves sealed values and blind indexes onto the
//!   current key, a bounded batch per transaction.
//!
//! # The audit journal (WP-SEC-03.T4)
//!
//! Every call, successful or refused, writes one `sys_secret_audit` event to the outbox: the
//! operation, DocType, field, caller kind, site, outcome and error code, with the actor in the
//! event's metadata. Never a value, a sealed blob or an index.
//!
//! - **set, create, rewrap:** the event is enqueued **in the transaction of the operation**, so
//!   it commits with it or not at all, and a failed audit write rolls the operation back.
//! - **open, find:** the operation reads, so there is no business transaction; the event is
//!   written in a transaction of its own **before the value is returned**, and a failed write
//!   fails the call (the opened value is dropped, zeroed). No secret is opened without its row.
//! - **a refused call** has no business transaction to join (it rolled back): its event is
//!   written on its own, and a failure to write it replaces the refusal with
//!   `secretfield.audit_failed`.
//!
//! [`Engine::secret_audit_journal`] reads the journal back, for the site of the [`SecretAccess`]
//! it is given and no other. [`Engine::secret_audit_log`] is only
//! this process's recent view of the same calls (the last 1,024, for diagnostics): it is
//! bounded and in memory, and is not the record.
//!
//! # The lookup window of a rotation
//!
//! After `rotate()` a stored blind index is under the previous key version until a rewrap
//! moves it. [`Engine::find_by_secret`] therefore asks for the value under the current **and**
//! the previous version while a rewrap of the DocType has not completed, so nobody is logged
//! out by a rotation. A sweep (`rewrap_secrets` from the first record to a report whose `next`
//! is `None`, with no value failed) closes the window for that key version, in this process,
//! **for the site that ran it**: the site is part of the blind-index input, so the window is
//! kept per `(site, DocType)` and a sweep under one site never closes another's.

use serde_json::json;
use spot_app_api::context::Invocation;
use spot_app_api::error::StorageOp;
use spot_app_api::prelude::{Name, RecordId, Value, Version};
use spot_app_api::{AppError, AppResult, Document, ErrorClass};
use spot_events_contract::PayloadDocument;
use spot_events_contract::lifecycle::Operation as EventOperation;
use spot_permissions::Action;
use spot_secrets::{SecretError, SecretString};
use spot_storage_contract::{
    CmpOp, DocumentStore, Expr, FeedPosition, Isolation, NodeRef, Nulls, Order, OrderDir, Outbox,
    OutboxEvent, Query, QueryExecutor, QueryPath, RowLock, Selection, StorageBackend, Transaction,
    TxOptions, Wait,
};

use super::ops::{Failed, Req, label, title};
use super::outbox::{self, TOPIC_SECRET_AUDIT};
use super::work::SealedWrites;
use super::{Engine, Snapshot, lock};
use crate::lookup::{name, resolve};
use crate::mapping::{from_storage, secret_columns};
use crate::secrets::{
    NOT_RECORDED, RewrapBatch, RewrapReport, SET, SecretAccess, SecretAuditRecord, SecretCaller,
    SecretOp, SecretVault, UNSET, aad, codes, index_input,
};

/// Entries kept in the in-process view of the secret calls; older ones are dropped. The journal
/// in the outbox is the record and has no such bound.
const AUDIT_LOG: usize = 1024;

/// Journal records read per page.
const JOURNAL_PAGE: u32 = 500;

/// The longest DocType or field text an audit event keeps (a refused call may carry any text).
const AUDIT_TEXT: usize = 128;

/// Where one site's rewrap sweep of one DocType stands (see the module's lookup window).
#[derive(Clone, Debug)]
pub(crate) struct Sweep {
    /// The key version the sweep moves records onto.
    version: u32,
    /// Where the next batch must start for the sweep to have seen every record so far.
    expect_after: Option<RecordId>,
    /// Whether every batch so far continued the last one and no value failed.
    clean: bool,
    /// Whether a clean sweep reached the end of the DocType under `version`.
    done: bool,
}

/// A resolved `secret` field of a DocType.
struct Target {
    doctype: String,
    field: String,
    sealed: Name,
    /// The blind-index column, for a `lookup = true` field.
    bidx: Option<Name>,
    /// The blind-index scope, `<doctype>.<field>`.
    scope: String,
}

fn refuse(class: ErrorClass, code: &'static str, msg: impl Into<String>) -> AppError {
    AppError::new(class, code, msg)
}

/// A vault failure as an error that names the stable code of the cause and nothing else: a
/// provider's own text could name a key file or a variable, and nothing here may carry a value.
fn vault_error(code: &'static str, what: &str, cause: &SecretError) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        code,
        format!("The secret could not be {what}."),
    )
    .detail("cause", cause.code())
}

fn internal(msg: &str) -> AppError {
    AppError::new(ErrorClass::Internal, "document.internal_error", msg)
}

/// The failure of an audit write: it names the stable code of the cause and nothing else.
fn audit_failed(cause: &AppError) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        codes::AUDIT_FAILED,
        "The audit row of the secret operation could not be written, so the operation did not happen.",
    )
    .detail("cause", cause.code.clone())
}

/// At most [`AUDIT_TEXT`] characters of a caller-supplied name.
fn clip(text: &str) -> String {
    text.chars().take(AUDIT_TEXT).collect()
}

/// An actor as text for the journal: `user:<id>` (` via <id>` when impersonated) or
/// `system:<component>`.
fn actor_text(actor: &spot_app_api::context::Actor) -> String {
    use spot_app_api::context::Actor;
    match actor {
        Actor::User { id, via: None } => format!("user:{id}"),
        Actor::User { id, via: Some(via) } => format!("user:{id} via {via}"),
        Actor::Consumer { id } => format!("consumer:{id}"),
        Actor::System { component } => format!("system:{}", component.as_str()),
    }
}

/// The site a journal event names, without reading the rest of it.
fn journal_site(event: &OutboxEvent) -> AppResult<Option<String>> {
    let payload = PayloadDocument::from_outbox_event(event)
        .map_err(|_| internal("a secret audit event this build cannot read"))?;
    Ok(payload
        .data
        .get("site")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned))
}

/// A journal event as a record.
fn read_journal_event(event: &OutboxEvent) -> AppResult<SecretAuditRecord> {
    let unreadable = || internal("a secret audit event this build cannot read");
    let payload = PayloadDocument::from_outbox_event(event).map_err(|_| unreadable())?;
    let text = |key: &str| payload.data.get(key).and_then(serde_json::Value::as_str);
    let op = text("op")
        .and_then(SecretOp::from_name)
        .ok_or_else(unreadable)?;
    let caller = text("caller")
        .and_then(SecretCaller::from_name)
        .ok_or_else(unreadable)?;
    let state = match text("state") {
        Some(SET) => SET,
        Some(UNSET) => UNSET,
        _ => NOT_RECORDED,
    };
    Ok(SecretAuditRecord {
        op,
        caller,
        actor: actor_text(&payload.meta.actor),
        site: text("site").ok_or_else(unreadable)?.to_owned(),
        doctype: text("doctype").ok_or_else(unreadable)?.to_owned(),
        field: text("field").ok_or_else(unreadable)?.to_owned(),
        id: text("record").and_then(outbox::parse_id),
        error: text("error").map(str::to_owned),
        state,
        at: payload.meta.occurred_at,
    })
}

impl<B: StorageBackend> Engine<B> {
    /// The capability to use the secret path for `inv`, for the named caller.
    ///
    /// It is the only way to get a [`SecretAccess`]. Only a platform service that handles
    /// credentials (the sign-in service, the bootstrap) asks for one; it is never passed to a
    /// hook, a controller, a script or an API handler.
    ///
    /// The brief's `secret_access(who)` takes the invocation as well: the lifecycle event of a
    /// write needs the actor and the time, and the invocation's site is part of every value's
    /// associated data.
    pub fn secret_access(&self, who: SecretCaller, inv: &Invocation) -> SecretAccess {
        SecretAccess::new(who, inv)
    }

    /// This process's recent secret calls (the last 1,024), oldest first: a diagnostic view.
    ///
    /// It is bounded and in memory, so it is **not** the audit record; that is the journal
    /// ([`secret_audit_journal`](Self::secret_audit_journal)).
    pub fn secret_audit_log(&self) -> Vec<SecretAuditRecord> {
        lock(&self.secret_audit).clone()
    }

    /// Every secret operation of the capability's site ever journalled, oldest first, read back
    /// from the outbox.
    ///
    /// It needs the same capability as the secret operations themselves, and returns only the
    /// rows of the site that capability was made for: the outbox may hold other sites' rows,
    /// and they are neither returned nor parsed. For tests and for an audit view; it reads the
    /// whole feed, so it is not for a hot path.
    ///
    /// # Errors
    ///
    /// A mapped storage error, or `document.internal_error` for a journal event of this site
    /// that this build cannot read.
    pub async fn secret_audit_journal(
        &self,
        access: &SecretAccess,
    ) -> AppResult<Vec<SecretAuditRecord>> {
        let site = access.invocation().site.as_str();
        let mut out = Vec::new();
        let mut after = FeedPosition::START;
        loop {
            let page = self
                .backend
                .read_outbox(after, JOURNAL_PAGE)
                .await
                .map_err(|e| spot_app_api::error::map_storage_error(&e, StorageOp::Other))?;
            let Some(last) = page.last() else { break };
            after = last.position;
            for record in &page {
                if record.event.topic.as_str() != TOPIC_SECRET_AUDIT {
                    continue;
                }
                // Another site's row is skipped on its site alone; a row with no readable
                // site fails the call, as any unreadable journal row does.
                if journal_site(&record.event)?
                    .as_deref()
                    .is_some_and(|s| s != site)
                {
                    continue;
                }
                out.push(read_journal_event(&record.event)?);
            }
        }
        Ok(out)
    }

    fn vault_of(&self) -> AppResult<&dyn SecretVault> {
        self.vault.as_deref().ok_or_else(|| {
            refuse(
                ErrorClass::Internal,
                codes::NOT_CONFIGURED,
                "The engine has no secrets vault.",
            )
        })
    }

    /// Adds the call to the in-process view.
    #[allow(clippy::too_many_arguments)]
    fn audit<T>(
        &self,
        access: &SecretAccess,
        op: SecretOp,
        doctype: &str,
        field: &str,
        id: Option<RecordId>,
        state: &'static str,
        result: &AppResult<T>,
    ) {
        let inv = access.invocation();
        let mut log = lock(&self.secret_audit);
        if log.len() >= AUDIT_LOG {
            log.remove(0);
        }
        log.push(SecretAuditRecord {
            op,
            caller: access.caller(),
            actor: actor_text(&inv.actor),
            site: inv.site.as_str().to_owned(),
            doctype: doctype.to_owned(),
            field: field.to_owned(),
            id,
            error: result.as_ref().err().map(|e| e.code.clone()),
            state: if result.is_ok() { state } else { NOT_RECORDED },
            at: inv.now,
        });
    }

    /// The journal event of one secret call: `error` is the code it ended with, if it failed.
    #[allow(clippy::too_many_arguments)]
    fn audit_event(
        &self,
        access: &SecretAccess,
        op: SecretOp,
        doctype: &str,
        field: &str,
        id: Option<RecordId>,
        state: &'static str,
        error: Option<&str>,
    ) -> AppResult<OutboxEvent> {
        let inv = access.invocation();
        let entity = name(doctype)
            .or_else(|_| name("unknown"))
            .map_err(|_| internal("an audit event with no DocType name"))?;
        // A record the call named, or for one that named none, a fresh id: the subject of an
        // outbox event is a node.
        let subject = NodeRef {
            entity,
            id: id.unwrap_or_else(|| self.ids.next_id()),
        };
        let data = json!({
            "op": op.as_str(),
            "doctype": clip(doctype),
            "field": clip(field),
            "record": id.map(|i| i.to_string()),
            "caller": access.caller().as_str(),
            "site": inv.site.as_str(),
            "outcome": if error.is_none() { "ok" } else { "refused" },
            "error": error,
            "state": if error.is_none() { state } else { NOT_RECORDED },
        });
        outbox::secret_audit(&*self.ids, inv, subject, data)
    }

    /// Finishes a secret call: adds it to the in-process view and, unless the journal event is
    /// written by the operation's own transaction (`in_tx`, and it succeeded), writes it in a
    /// transaction of its own. A failed audit write replaces the result: the value of an
    /// `Ok(_)` is dropped unread, and a refusal that cannot be recorded is `audit_failed`.
    #[allow(clippy::too_many_arguments)]
    async fn finish<T>(
        &self,
        access: &SecretAccess,
        op: SecretOp,
        doctype: &str,
        field: &str,
        id: Option<RecordId>,
        state: &'static str,
        in_tx: bool,
        result: AppResult<T>,
    ) -> AppResult<T> {
        self.audit(access, op, doctype, field, id, state, &result);
        if in_tx && result.is_ok() {
            return result;
        }
        let error = result.as_ref().err().map(|e| e.code.as_str());
        let event = match self.audit_event(access, op, doctype, field, id, state, error) {
            Ok(event) => event,
            Err(e) => return Err(audit_failed(&e)),
        };
        let snap = self.snapshot();
        match self.enqueue_detached(&snap, vec![event]).await {
            Ok(()) => result,
            Err(e) => Err(audit_failed(&e)),
        }
    }

    /// The secret field `doctype.field`, or the reason it cannot be used. Fails closed on
    /// anything unexpected: an unknown DocType, a missing field, a non-secret field, a child
    /// DocType, or a lookup property that is not the plain flag.
    fn target(&self, snap: &Snapshot, doctype: &str, field: &str) -> AppResult<Target> {
        let resolved = resolve(&snap.meta, doctype)?;
        if resolved.def.is_child {
            return Err(refuse(
                ErrorClass::Validation,
                codes::UNSUPPORTED,
                format!("`{doctype}` rows live in their parent's tables, which hold no secrets."),
            ));
        }
        let spec = resolved
            .def
            .fields
            .iter()
            .find(|f| f.name == field && f.ty == spot_meta::FieldType::Secret)
            .ok_or_else(|| {
                refuse(
                    ErrorClass::Validation,
                    codes::UNKNOWN_FIELD,
                    format!("`{doctype}` has no secret field `{field}`."),
                )
            })?;
        // A secret's `lookup` is the plain flag or absent; the seam table form is a definition
        // the metadata layer refuses. If one got here anyway, do not guess what it means.
        if spec.lookup.as_ref().is_some_and(|l| !l.is_flag()) {
            return Err(refuse(
                ErrorClass::Internal,
                codes::INVALID_DEFINITION,
                format!("`{doctype}.{field}` has a lookup that is not `lookup = true`."),
            ));
        }
        let columns = secret_columns(spec)?;
        let mut columns = columns.into_iter();
        let sealed = columns
            .next()
            .ok_or_else(|| internal("a secret field with no sealed column"))?;
        Ok(Target {
            doctype: doctype.to_owned(),
            field: field.to_owned(),
            sealed,
            bidx: columns.next(),
            scope: format!("{doctype}.{field}"),
        })
    }

    /// Seals `value` for `target` of record `id`, and derives the blind index of a lookup field.
    async fn seal_columns(
        &self,
        vault: &dyn SecretVault,
        site: &str,
        target: &Target,
        id: RecordId,
        value: &SecretString,
    ) -> AppResult<Vec<(Name, Value)>> {
        if value.expose().is_empty() {
            // An empty credential is never valid, and for a unique lookup field it would be
            // one more value that "has no secret" and so skips the uniqueness check.
            return Err(refuse(
                ErrorClass::Validation,
                codes::EMPTY_VALUE,
                format!(
                    "`{}.{}` cannot be set to an empty value.",
                    target.doctype, target.field
                ),
            ));
        }
        let aad = aad(site, &target.doctype, id, &target.field);
        let sealed = vault
            .seal(value, &aad)
            .await
            .map_err(|e| vault_error(codes::SEAL_FAILED, "sealed", &e))?;
        let mut columns = vec![(target.sealed.clone(), Value::Bytes(sealed))];
        if let Some(bidx) = &target.bidx {
            let input = index_input(site, value.expose().as_bytes());
            let index = vault
                .blind_index(&target.scope, &input)
                .await
                .map_err(|e| vault_error(codes::INDEX_FAILED, "indexed", &e))?;
            columns.push((bidx.clone(), Value::Bytes(index)));
        }
        Ok(columns)
    }

    /// Writes `value` into the secret field of an existing record: sealed, in one transaction
    /// with the record's update (version check, hooks, lifecycle event), and nothing else. The
    /// event carries the field's name, never the value.
    ///
    /// Returns the record's new version. A lookup field's blind index is written in the same
    /// transaction.
    ///
    /// # Errors
    ///
    /// `secretfield.not_configured`, `.unknown_field`, `.unsupported_doctype`, `.empty_value`,
    /// `.duplicate` (a unique lookup field already holds the value), `document.not_found`,
    /// `document.stale_version`, or a storage error.
    pub async fn set_secret(
        &self,
        access: &SecretAccess,
        doctype: &str,
        id: RecordId,
        field: &str,
        value: SecretString,
        expected: Version,
    ) -> AppResult<Version> {
        let result = self
            .set_secret_inner(access, doctype, id, field, value, expected)
            .await;
        self.finish(
            access,
            SecretOp::Set,
            doctype,
            field,
            Some(id),
            SET,
            true,
            result,
        )
        .await
    }

    async fn set_secret_inner(
        &self,
        access: &SecretAccess,
        doctype: &str,
        id: RecordId,
        field: &str,
        value: SecretString,
        expected: Version,
    ) -> AppResult<Version> {
        let vault = self.vault_of()?;
        let snap = self.snapshot();
        let target = self.target(&snap, doctype, field)?;
        let site = access.invocation().site.as_str();
        let columns = self.seal_columns(vault, site, &target, id, &value).await?;
        drop(value);
        let indexes = index_columns(&columns);
        let audit =
            vec![self.audit_event(access, SecretOp::Set, doctype, field, Some(id), SET, None)?];
        let writes = SealedWrites {
            fields: vec![field.to_owned()],
            columns,
            audit,
        };
        let req = Req::SetSecrets {
            doctype: doctype.to_owned(),
            id,
            writes,
            expected,
        };
        let done = match self.top(access.invocation(), req).await {
            Ok(done) => done,
            Err(e) => return Err(self.secret_duplicate(&snap, doctype, &indexes, e).await),
        };
        let doc = done.into_doc()?.value;
        doc.version()
            .ok_or_else(|| internal("a saved document with no version"))
    }

    /// Creates `doc` with its initial secrets: the record and the sealed values in one insert.
    ///
    /// `doc` must not hold a secret value itself (the document layer refuses one). Each
    /// `(field, value)` is sealed bound to the new record's id. Returns the document as saved.
    ///
    /// # Errors
    ///
    /// As [`set_secret`](Self::set_secret), and the create lifecycle's own errors. A field named
    /// twice is refused with `secretfield.unknown_field`.
    pub async fn create_with_secrets(
        &self,
        access: &SecretAccess,
        doc: Document,
        secrets: Vec<(String, SecretString)>,
    ) -> AppResult<Document> {
        let doctype = doc.doctype().as_str().to_owned();
        let id = doc.id();
        let result = self.create_with_secrets_inner(access, doc, secrets).await;
        self.finish(
            access,
            SecretOp::Set,
            &doctype,
            "*",
            Some(id),
            SET,
            true,
            result,
        )
        .await
    }

    async fn create_with_secrets_inner(
        &self,
        access: &SecretAccess,
        doc: Document,
        secrets: Vec<(String, SecretString)>,
    ) -> AppResult<Document> {
        let vault = self.vault_of()?;
        let snap = self.snapshot();
        let doctype = doc.doctype().as_str().to_owned();
        let site = access.invocation().site.as_str();
        if secrets.is_empty() {
            // Without a secret this would be a plain create under the capability alone.
            return Err(refuse(
                ErrorClass::Validation,
                codes::EMPTY_VALUE,
                format!("Creating `{doctype}` with secrets needs at least one secret."),
            ));
        }
        let mut fields = Vec::new();
        let mut columns = Vec::new();
        let mut audit = Vec::new();
        for (field, value) in &secrets {
            if fields.contains(field) {
                return Err(refuse(
                    ErrorClass::Validation,
                    codes::UNKNOWN_FIELD,
                    format!("The secret `{doctype}.{field}` is given twice."),
                ));
            }
            let target = self.target(&snap, &doctype, field)?;
            columns.extend(
                self.seal_columns(vault, site, &target, doc.id(), value)
                    .await?,
            );
            audit.push(self.audit_event(
                access,
                SecretOp::Set,
                &doctype,
                field,
                Some(doc.id()),
                SET,
                None,
            )?);
            fields.push(field.clone());
        }
        drop(secrets);
        let indexes = index_columns(&columns);
        let req = Req::CreateWithSecrets {
            doc,
            writes: SealedWrites {
                fields,
                columns,
                audit,
            },
        };
        let done = match self.top(access.invocation(), req).await {
            Ok(done) => done,
            Err(e) => return Err(self.secret_duplicate(&snap, &doctype, &indexes, e).await),
        };
        Ok(done.into_doc()?.value)
    }

    /// Opens the secret `field` of one record: `None` when it was never set.
    ///
    /// One record, one field. The value is zeroed when the [`SecretString`] drops.
    ///
    /// # Errors
    ///
    /// `secretfield.not_configured`, `.unknown_field`, `document.not_found`,
    /// `secretfield.open_failed` (the stored value was altered or moved, or its key is gone), or
    /// a storage error.
    pub async fn open_secret(
        &self,
        access: &SecretAccess,
        doctype: &str,
        id: RecordId,
        field: &str,
    ) -> AppResult<Option<SecretString>> {
        let result = self.open_secret_inner(access, doctype, id, field).await;
        let state = if matches!(result, Ok(None)) {
            UNSET
        } else {
            SET
        };
        // The row is written before the value is returned; if it cannot be, the value is
        // dropped (and zeroed) here.
        self.finish(
            access,
            SecretOp::Open,
            doctype,
            field,
            Some(id),
            state,
            false,
            result,
        )
        .await
    }

    async fn open_secret_inner(
        &self,
        access: &SecretAccess,
        doctype: &str,
        id: RecordId,
        field: &str,
    ) -> AppResult<Option<SecretString>> {
        let vault = self.vault_of()?;
        let snap = self.snapshot();
        let target = self.target(&snap, doctype, field)?;
        let stored = self
            .read_raw(&snap, &target.doctype, id)
            .await?
            .ok_or_else(super::ops::not_found)?;
        let Some(bytes) = sealed_bytes(&stored.data.fields, &target.sealed)? else {
            return Ok(None);
        };
        let aad = aad(access.invocation().site.as_str(), doctype, id, field);
        let plain = vault
            .open(bytes, &aad)
            .await
            .map_err(|e| vault_error(codes::OPEN_FAILED, "opened", &e))?;
        Ok(Some(plain))
    }

    /// Whether the secret `field` of a record is set: what a document may show.
    ///
    /// Unlike the other secret calls this one takes the invocation and the permission model's
    /// `read` right on the record, because it reveals only that a credential exists.
    ///
    /// # Errors
    ///
    /// `permission.denied`, `document.not_found`, `secretfield.unknown_field`, or a storage
    /// error.
    pub async fn secret_is_set(
        &self,
        inv: &Invocation,
        doctype: &str,
        id: RecordId,
        field: &str,
    ) -> AppResult<bool> {
        let snap = self.snapshot();
        let target = self.target(&snap, doctype, field)?;
        let doc = self
            .read_document(&snap, doctype, id)
            .await?
            .ok_or_else(super::ops::not_found)?;
        self.authorize(inv, Action::Read, &doc).await?;
        let stored = self
            .read_raw(&snap, &target.doctype, id)
            .await?
            .ok_or_else(super::ops::not_found)?;
        Ok(sealed_bytes(&stored.data.fields, &target.sealed)?.is_some())
    }

    /// Finds the record whose lookup secret `field` equals `value`: one indexed equality on the
    /// blind index. `None` when no record holds it.
    ///
    /// Only for `lookup = true` fields; any other secret field is refused with
    /// `secretfield.not_lookup` (never answered as "not found"). If more than one record holds
    /// the value (a lookup field that is not unique), the answer is `secretfield.ambiguous`
    /// rather than one of them.
    ///
    /// After a key rotation, a record whose index has not been recomputed
    /// ([`rewrap_secrets`](Self::rewrap_secrets)) is still found: while a rewrap of the DocType
    /// has not completed, the value is looked up under the current **and the previous** key
    /// version (one record found under either is one record). Once a rewrap sweep reports
    /// completion, only the current version is consulted. A record whose index is two or more
    /// versions old is not found: the lookup fails closed.
    ///
    /// # Errors
    ///
    /// `secretfield.not_configured`, `.unknown_field`, `.not_lookup`, `.empty_value`,
    /// `.ambiguous`, or a storage error.
    pub async fn find_by_secret(
        &self,
        access: &SecretAccess,
        doctype: &str,
        field: &str,
        value: &[u8],
    ) -> AppResult<Option<RecordId>> {
        let result = self.find_inner(access, doctype, field, value).await;
        self.finish(
            access,
            SecretOp::Find,
            doctype,
            field,
            None,
            SET,
            false,
            result,
        )
        .await
    }

    async fn find_inner(
        &self,
        access: &SecretAccess,
        doctype: &str,
        field: &str,
        value: &[u8],
    ) -> AppResult<Option<RecordId>> {
        let vault = self.vault_of()?;
        let snap = self.snapshot();
        let target = self.target(&snap, doctype, field)?;
        let Some(bidx) = &target.bidx else {
            return Err(refuse(
                ErrorClass::Validation,
                codes::NOT_LOOKUP,
                format!("`{doctype}.{field}` does not declare `lookup = true`."),
            ));
        };
        if value.is_empty() {
            return Err(refuse(
                ErrorClass::Validation,
                codes::EMPTY_VALUE,
                format!("`{doctype}.{field}` cannot be looked up by an empty value."),
            ));
        }
        let input = index_input(access.invocation().site.as_str(), value);
        let index = vault
            .blind_index(&target.scope, &input)
            .await
            .map_err(|e| vault_error(codes::INDEX_FAILED, "indexed", &e))?;
        let mut wanted = vec![Value::Bytes(index)];
        if self.window_open(access.invocation().site.as_str(), doctype, vault) {
            let previous = vault
                .blind_index_previous(&target.scope, &input)
                .await
                .map_err(|e| vault_error(codes::INDEX_FAILED, "indexed", &e))?;
            wanted.extend(previous.map(Value::Bytes));
        }
        let entity = name(doctype)?;
        let alias = name("d")?;
        let id_col = name("id")?;
        let mut query = Query::from(&entity, &alias, QueryPath::Transactional);
        query.select = vec![Selection {
            expr: Expr::field(&alias, &id_col),
            alias: id_col.clone(),
        }];
        // `=` on a null column is never true, so a record with no secret never matches. One row
        // holds one index, so a record found under either version is one row.
        query.filter = Some(match wanted.as_slice() {
            [only] => Expr::field(&alias, bidx).eq(Expr::value(only.clone())),
            _ => Expr::In(Box::new(Expr::field(&alias, bidx)), wanted),
        });
        query.order_by = vec![Order {
            expr: Expr::field(&alias, &id_col),
            dir: OrderDir::Asc,
            nulls: Nulls::Last,
        }];
        query.limit = Some(2);
        let rows = self.query_detached(&snap, &query).await?;
        let mut ids = rows.rows.into_iter().filter_map(|row| match row.first() {
            Some(Value::Id(id)) => Some(*id),
            _ => None,
        });
        let first = ids.next();
        if ids.next().is_some() {
            return Err(refuse(
                ErrorClass::Internal,
                codes::AMBIGUOUS,
                format!("More than one `{doctype}` record holds this `{field}`."),
            ));
        }
        Ok(first)
    }

    /// Whether a lookup in `doctype` for `site` must also try the previous key version: until a
    /// clean rewrap sweep of the DocType **under that site** has completed under the vault's
    /// current version. The site is part of the blind-index input, so another site's sweep
    /// proves nothing about this site's records.
    fn window_open(&self, site: &str, doctype: &str, vault: &dyn SecretVault) -> bool {
        let version = vault.current_key_version();
        !lock(&self.rewrap_sweeps)
            .get(&(site.to_owned(), doctype.to_owned()))
            .is_some_and(|sweep| sweep.done && sweep.version == version)
    }

    /// Records what a committed rewrap batch did to the site's sweep of the DocType.
    fn note_sweep(
        &self,
        site: &str,
        doctype: &str,
        version: u32,
        batch: &RewrapBatch,
        report: &RewrapReport,
    ) {
        let mut sweeps = lock(&self.rewrap_sweeps);
        let sweep = sweeps
            .entry((site.to_owned(), doctype.to_owned()))
            .or_insert(Sweep {
                version,
                expect_after: None,
                clean: batch.after.is_none(),
                done: false,
            });
        if sweep.version != version {
            *sweep = Sweep {
                version,
                expect_after: None,
                clean: batch.after.is_none(),
                done: false,
            };
        } else if batch.after.is_none() {
            // A new sweep from the start; a completed one stays completed (every record was
            // already current), until the version changes.
            sweep.clean = true;
        } else if sweep.expect_after != batch.after {
            // A batch that did not continue the last one: the records between were not seen.
            sweep.clean = false;
        }
        if report.failed > 0 {
            sweep.clean = false;
        }
        sweep.expect_after = report.next;
        if report.next.is_none() && sweep.clean {
            sweep.done = true;
        }
    }

    /// Moves one batch of a DocType's sealed values onto the current key, and recomputes the
    /// blind indexes of its lookup fields that were derived under an older one.
    ///
    /// The batch is at most `batch.size` records after `batch.after`, in id order, in **one
    /// transaction**: it commits whole or not at all. Records that are already current are
    /// examined and left alone. A value that cannot be rewrapped (altered, or its key retired)
    /// is left untouched, counted in [`RewrapReport::failed`] and written to the audit log; it
    /// does not stop the batch. Call again with the report's `next` until it is `None`; after a
    /// crash, start again (from `None`, or from the last `next` seen): finished records are
    /// skipped.
    ///
    /// A record the batch changes gets a new version and the lifecycle event of an update that
    /// changed only its sealed columns (`changed_fields` names the secret fields, never a
    /// value), in the same transaction, so replicas and sync see the change. The batch itself
    /// and each value that failed are journalled in that transaction too.
    ///
    /// # Errors
    ///
    /// `secretfield.not_configured`, `.invalid_batch`, `meta.unknown_doctype`, or a storage
    /// error (the batch changed nothing).
    pub async fn rewrap_secrets(
        &self,
        access: &SecretAccess,
        doctype: &str,
        batch: RewrapBatch,
    ) -> AppResult<RewrapReport> {
        let result = self.rewrap_inner(access, doctype, batch).await;
        self.finish(
            access,
            SecretOp::Rewrap,
            doctype,
            "*",
            None,
            SET,
            true,
            result,
        )
        .await
    }

    async fn rewrap_inner(
        &self,
        access: &SecretAccess,
        doctype: &str,
        batch: RewrapBatch,
    ) -> AppResult<RewrapReport> {
        let vault = self.vault_of()?;
        if batch.size == 0 || batch.size > RewrapBatch::MAX_SIZE {
            return Err(refuse(
                ErrorClass::Validation,
                codes::INVALID_BATCH,
                format!(
                    "A rewrap batch holds 1 to {} records.",
                    RewrapBatch::MAX_SIZE
                ),
            ));
        }
        let snap = self.snapshot();
        let resolved = resolve(&snap.meta, doctype)?;
        let mut targets = Vec::new();
        for spec in crate::mapping::secret_fields(resolved.def) {
            targets.push(self.target(&snap, doctype, &spec.name)?);
        }
        if targets.is_empty() {
            return Ok(RewrapReport {
                examined: 0,
                rewrapped: 0,
                reindexed: 0,
                failed: 0,
                next: None,
            });
        }
        let entity = name(doctype)?;
        let label = label(doctype, "rewrap");
        let (report, failures) = self
            .retrying(&label, || {
                self.rewrap_once(&snap, vault, access, &label, &entity, &targets, batch)
            })
            .await?;
        self.note_sweep(
            access.invocation().site.as_str(),
            doctype,
            vault.current_key_version(),
            &batch,
            &report,
        );
        // The journal rows of the failed values were written in the batch's transaction; this
        // adds them to the in-process view, once.
        for (field, id) in failures {
            let outcome: AppResult<()> = Err(vault_error(
                codes::REWRAP_FAILED,
                "rewrapped",
                &SecretError::Tampered,
            ));
            self.audit(
                access,
                SecretOp::Rewrap,
                doctype,
                &field,
                Some(id),
                SET,
                &outcome,
            );
        }
        Ok(report)
    }

    #[allow(clippy::too_many_arguments)]
    async fn rewrap_once(
        &self,
        snap: &Snapshot,
        vault: &dyn SecretVault,
        access: &SecretAccess,
        label: &Name,
        entity: &Name,
        targets: &[Target],
        batch: RewrapBatch,
    ) -> Result<(RewrapReport, Vec<(String, RecordId)>), Failed> {
        let mut tx = self
            .backend
            .begin(
                snap.catalog.clone(),
                TxOptions::new(label.clone(), Isolation::ReadCommitted),
            )
            .await
            .map_err(|e| Failed::Storage(e, StorageOp::Other))?;
        let outcome = self
            .rewrap_rows(snap, &mut tx, vault, access, entity, targets, batch)
            .await;
        let outcome = match outcome {
            Ok(done) => self
                .journal_rewrap(&mut tx, access, targets, &done)
                .await
                .map(|()| done),
            Err(failed) => Err(failed),
        };
        match outcome {
            Ok(done) => {
                tx.commit()
                    .await
                    .map_err(|e| Failed::Storage(e, StorageOp::Write))?;
                Ok(done)
            }
            Err(failed) => {
                let _ = tx.rollback().await;
                Err(failed)
            }
        }
    }

    /// Enqueues the journal rows of a batch: one for each value that failed, then the batch.
    async fn journal_rewrap(
        &self,
        tx: &mut B::Tx,
        access: &SecretAccess,
        targets: &[Target],
        (_, failures): &(RewrapReport, Vec<(String, RecordId)>),
    ) -> Result<(), Failed> {
        let Some(first) = targets.first() else {
            return Ok(());
        };
        let doctype = first.doctype.as_str();
        let mut events = Vec::new();
        for (field, id) in failures {
            events.push(
                self.audit_event(
                    access,
                    SecretOp::Rewrap,
                    doctype,
                    field,
                    Some(*id),
                    SET,
                    Some(codes::REWRAP_FAILED),
                )
                .map_err(Failed::App)?,
            );
        }
        events.push(
            self.audit_event(access, SecretOp::Rewrap, doctype, "*", None, SET, None)
                .map_err(Failed::App)?,
        );
        for event in &events {
            tx.enqueue(event)
                .await
                .map_err(|e| Failed::Storage(e, StorageOp::Write))?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn rewrap_rows(
        &self,
        snap: &Snapshot,
        tx: &mut B::Tx,
        vault: &dyn SecretVault,
        access: &SecretAccess,
        entity: &Name,
        targets: &[Target],
        batch: RewrapBatch,
    ) -> Result<(RewrapReport, Vec<(String, RecordId)>), Failed> {
        let storage = |e| Failed::Storage(e, StorageOp::Other);
        let alias = name("d").map_err(Failed::App)?;
        let id_col = name("id").map_err(Failed::App)?;
        let mut query = Query::from(entity, &alias, QueryPath::Transactional);
        query.select = vec![Selection {
            expr: Expr::field(&alias, &id_col),
            alias: id_col.clone(),
        }];
        if let Some(after) = batch.after {
            query.filter = Some(Expr::field(&alias, &id_col).cmp(CmpOp::Gt, Expr::value(after)));
        }
        query.order_by = vec![Order {
            expr: Expr::field(&alias, &id_col),
            dir: OrderDir::Asc,
            nulls: Nulls::Last,
        }];
        query.limit = Some(batch.size);
        let ids: Vec<RecordId> = tx
            .query(&query)
            .await
            .map_err(storage)?
            .rows
            .into_iter()
            .filter_map(|row| match row.first() {
                Some(Value::Id(id)) => Some(*id),
                _ => None,
            })
            .collect();
        let site = access.invocation().site.as_str();
        let mut report = RewrapReport {
            examined: 0,
            rewrapped: 0,
            reindexed: 0,
            failed: 0,
            next: None,
        };
        let mut failures = Vec::new();
        for id in &ids {
            let Some(mut stored) = tx
                .load(entity, *id, RowLock::Update(Wait::Block))
                .await
                .map_err(storage)?
            else {
                continue;
            };
            report.examined += 1;
            let mut changed = false;
            let mut changed_fields = Vec::new();
            for target in targets {
                match self
                    .rewrap_value(vault, site, target, *id, &mut stored.data.fields)
                    .await
                {
                    Ok((rewrapped, reindexed)) => {
                        changed |= rewrapped || reindexed;
                        if rewrapped || reindexed {
                            changed_fields.push(target.field.clone());
                        }
                        report.rewrapped += u32::from(rewrapped);
                        report.reindexed += u32::from(reindexed);
                    }
                    Err(_) => {
                        report.failed += 1;
                        failures.push((target.field.clone(), *id));
                    }
                }
            }
            if changed {
                let version = tx
                    .update(&stored.data, stored.version)
                    .await
                    .map_err(storage)?;
                // The update every other write of a record emits, for replicas and sync: the
                // names of the sealed fields, never a value.
                let doc = from_storage(&snap.meta, stored.clone()).map_err(Failed::App)?;
                let def = snap.doctype(entity.as_str()).map_err(Failed::App)?;
                let data = json!({"name": title(&def, &doc), "changed_fields": changed_fields});
                let event = outbox::lifecycle(
                    &*self.ids,
                    access.invocation(),
                    EventOperation::Update,
                    &doc,
                    Some(version),
                    data,
                )
                .map_err(Failed::App)?;
                tx.enqueue(&event)
                    .await
                    .map_err(|e| Failed::Storage(e, StorageOp::Write))?;
                // The journal's image of the record, at the version just written.
                if self.capture.captures(entity.as_str()) {
                    let mut written = doc.clone();
                    written.mark_saved(version, written.state());
                    let state = super::capture::state_event(
                        &*self.ids,
                        access.invocation(),
                        &snap.meta,
                        "update",
                        &written,
                        version,
                    )
                    .map_err(Failed::App)?;
                    tx.enqueue(&state)
                        .await
                        .map_err(|e| Failed::Storage(e, StorageOp::Write))?;
                }
            }
        }
        if ids.len() == usize::try_from(batch.size).unwrap_or(usize::MAX) {
            report.next = ids.last().copied();
        }
        Ok((report, failures))
    }

    /// Brings one field of one loaded row onto the current key. Returns whether the sealed
    /// value was rewrapped and whether the blind index was recomputed; on failure the row's
    /// columns are untouched.
    async fn rewrap_value(
        &self,
        vault: &dyn SecretVault,
        site: &str,
        target: &Target,
        id: RecordId,
        fields: &mut spot_storage_contract::Fields,
    ) -> Result<(bool, bool), SecretError> {
        let Some(sealed) = sealed_bytes(fields, &target.sealed)
            .map_err(|_| SecretError::Malformed("sealed column is not bytes"))?
        else {
            return Ok((false, false));
        };
        let sealed = sealed.to_vec();
        let need_rewrap = vault.needs_rewrap(&sealed)?;
        let index = match &target.bidx {
            None => None,
            Some(col) => match fields.get(col) {
                Some(Value::Bytes(b)) => Some(b.clone()),
                Some(Value::Null) | None => None,
                Some(_) => return Err(SecretError::Malformed("index column is not bytes")),
            },
        };
        let need_reindex = match (&target.bidx, &index) {
            (None, _) => false,
            // A sealed value with no index is repaired.
            (Some(_), None) => true,
            (Some(_), Some(index)) => vault.needs_reindex(index)?,
        };
        if !need_rewrap && !need_reindex {
            return Ok((false, false));
        }
        let aad = aad(site, &target.doctype, id, &target.field);
        let mut new_index = None;
        if need_reindex {
            // The index is a MAC of the plain value, so it takes opening the value.
            let plain = vault.open(&sealed, &aad).await?;
            let input = index_input(site, plain.expose().as_bytes());
            new_index = Some(vault.blind_index(&target.scope, &input).await?);
        }
        let new_sealed = if need_rewrap {
            Some(vault.rewrap(&sealed, &aad).await?)
        } else {
            None
        };
        if let Some(bytes) = new_sealed {
            fields.insert(target.sealed.clone(), Value::Bytes(bytes));
        }
        if let (Some(col), Some(bytes)) = (&target.bidx, new_index) {
            fields.insert(col.clone(), Value::Bytes(bytes));
        }
        Ok((need_rewrap, need_reindex))
    }

    /// A record as storage holds it, secret columns included, through a short read-only
    /// transaction. Callers keep the result inside this module.
    async fn read_raw(
        &self,
        snap: &Snapshot,
        doctype: &str,
        id: RecordId,
    ) -> AppResult<Option<spot_storage_contract::Document>> {
        let entity = name(doctype)?;
        let label = label(doctype, "secret");
        self.retrying(&label, || async {
            let mut tx = self
                .backend
                .begin(
                    snap.catalog.clone(),
                    TxOptions::new(label.clone(), Isolation::ReadCommitted).read_only(),
                )
                .await
                .map_err(|e| Failed::Storage(e, StorageOp::Other))?;
            let stored = tx.load(&entity, id, RowLock::None).await;
            let _ = tx.rollback().await;
            stored.map_err(|e| Failed::Storage(e, StorageOp::Other))
        })
        .await
    }
}

/// The sealed bytes in a loaded row's column: `None` when unset, an error when the column holds
/// anything but bytes.
fn sealed_bytes<'a>(
    fields: &'a spot_storage_contract::Fields,
    column: &Name,
) -> AppResult<Option<&'a [u8]>> {
    match fields.get(column) {
        Some(Value::Bytes(bytes)) => Ok(Some(bytes)),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(refuse(
            ErrorClass::Internal,
            codes::INCONSISTENT,
            "A sealed column holds something other than bytes.",
        )),
    }
}

/// The blind-index columns among sealed `columns`, with their bytes.
fn index_columns(columns: &[(Name, Value)]) -> Vec<(Name, Value)> {
    columns
        .iter()
        .filter(|(n, _)| n.as_str().ends_with(spot_meta::BLIND_INDEX_SUFFIX))
        .cloned()
        .collect()
}

impl<B: StorageBackend> Engine<B> {
    /// A unique violation on a secret write as `secretfield.duplicate`, which carries no detail
    /// of what it collided with: but only when a blind-index column collided. A collision on an
    /// ordinary unique field of the record keeps its own `document.duplicate`. The storage
    /// error does not name the constraint, so the blind indexes the write set are looked up:
    /// another record holding one of them is the collision.
    async fn secret_duplicate(
        &self,
        snap: &Snapshot,
        doctype: &str,
        indexes: &[(Name, Value)],
        e: AppError,
    ) -> AppError {
        if e.code != "document.duplicate" {
            return e;
        }
        let (Ok(entity), Ok(alias), Ok(id_col)) = (name(doctype), name("d"), name("id")) else {
            return e;
        };
        for (column, value) in indexes {
            let mut query = Query::from(&entity, &alias, QueryPath::Transactional);
            query.select = vec![Selection {
                expr: Expr::field(&alias, &id_col),
                alias: id_col.clone(),
            }];
            query.filter = Some(Expr::field(&alias, column).eq(Expr::value(value.clone())));
            query.limit = Some(1);
            if self
                .query_detached(snap, &query)
                .await
                .is_ok_and(|rows| !rows.rows.is_empty())
            {
                return AppError::new(
                    ErrorClass::Duplicate,
                    codes::DUPLICATE,
                    "Another record already holds this secret.",
                );
            }
        }
        e
    }
}
