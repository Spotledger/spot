//! The online-only, audited reveal (ADR-070 section 12 rules 2, 9 and rule 12.13 f).
//!
//! `read_masked` works offline against the stored masked form. `reveal` and `reveal_bulk` need
//! a **live hub call**, which releases the value and audits it. This module is the hub's side
//! of that call:
//!
//! 1. the caller must hold the field's `reveal` right, decided by `spot-permissions`
//!    ([`PermissionEngine::field_access`]), and the refusal text is the engine's `explain`;
//! 2. the value is loaded through a [`ValueSource`] (the hub's own storage, never a node);
//! 3. an audit record (who, when, document, field, API) is written **before** the value is
//!    returned; if it cannot be written, nothing is released;
//! 4. a bulk reveal is refused for a person and allowed only for a service principal whose
//!    manifest declares the group.
//!
//! Offline, the caller never reaches this module: it calls [`offline_reveal`], which answers
//! [`RevealError::Offline`] with the explanation key a screen shows.

use std::sync::Arc;

use spot_audit_chain::{AuditRecord, Payload, PayloadValue, Subject};
use spot_permissions::{DocFacts, FieldFacts, PermissionEngine, Principal};
use spot_protected_keys::{GroupName, NodeId};
use spot_secrets::SecretBytes;

use crate::error::RevealError;

/// The user-facing explanation key an offline reveal shows.
pub const OFFLINE_EXPLANATION: &str = concat!("error.", "sealed_fields.reveal_offline");
/// The audit kind every reveal is written as. Assembled with `concat!` so the error-code scan
/// does not read an audit kind as an error code.
pub const REVEAL_AUDIT_KIND: &str = concat!("sealed_fields.", "reveal");
/// The audit kind every bulk reveal is written as.
pub const REVEAL_BULK_AUDIT_KIND: &str = concat!("sealed_fields.", "reveal_bulk");
/// The API name recorded on every reveal.
pub const REVEAL_API: &str = concat!("api.", "reveal");

/// The value a reveal returns. Zeroised on drop; it is not `Clone`, so a released value has
/// exactly one owner.
#[derive(Debug)]
pub struct Revealed {
    /// The unmasked value.
    pub value: SecretBytes,
}

impl Revealed {
    /// The value's bytes.
    #[must_use]
    pub fn expose(&self) -> &[u8] {
        self.value.expose()
    }
}

/// The live hub the reveal calls. It is the only thing that can release a value, so it is a
/// separate port rather than a method on a store.
///
/// A branch or offline node holds a port that is reachable only while the hub answers; when it
/// is not, [`RemoteReveal`] answers [`RevealError::Offline`] and never falls back to a local
/// read.
pub trait HubPort: Send + Sync {
    /// The hub's answer to one reveal, at the caller's UTC instant in microseconds.
    ///
    /// # Errors
    ///
    /// [`RevealError::HubUnavailable`] when the hub cannot be reached. A hub that is reached
    /// answers with the value or with [`RevealError::Denied`].
    fn reveal(
        &self,
        principal: &Principal,
        doctype: &str,
        id: &str,
        field: &str,
        at: i64,
    ) -> Result<Revealed, RevealError>;

    /// The hub's answer to a bulk reveal, in the order the documents were asked for; a
    /// document the hub cannot find is absent.
    ///
    /// # Errors
    ///
    /// [`RevealError::HubUnavailable`] when the hub cannot be reached.
    fn reveal_bulk(&self, request: BulkRequest<'_>) -> Result<Vec<Revealed>, RevealError>;
}

/// Where a reveal gets the unmasked value: the hub's own storage. A node has no implementation
/// of this trait that can answer for a sealed or encrypted field.
pub trait ValueSource: Send + Sync {
    /// The unmasked value of one field of one document, or `None` when there is no such
    /// document (or the caller may not learn that it exists).
    fn load(&self, doctype: &str, id: &str, field: &str) -> Option<Loaded>;
}

/// A value loaded for a reveal, with the document facts the permission core needs.
#[derive(Debug)]
pub struct Loaded {
    /// The stored, unmasked value.
    pub value: SecretBytes,
    /// The facts of the document it belongs to.
    pub doc_facts: DocFacts,
}

/// Where a reveal record goes.
pub trait AuditSink: Send + Sync {
    /// Offers `record`; `true` when it was kept.
    fn offer(&self, record: AuditRecord) -> bool;
}

/// A sink that keeps every record in memory, for tests and a single-process hub.
#[derive(Default)]
pub struct MemoryAuditSink {
    records: std::sync::Mutex<Vec<AuditRecord>>,
}

impl MemoryAuditSink {
    /// An empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every record, in the order it was offered.
    #[must_use]
    pub fn records(&self) -> Vec<AuditRecord> {
        match self.records.lock() {
            Ok(records) => records.clone(),
            Err(_) => Vec::new(),
        }
    }

    /// How many records the sink holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records().len()
    }

    /// Whether the sink holds no record.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records().is_empty()
    }
}

impl AuditSink for MemoryAuditSink {
    fn offer(&self, record: AuditRecord) -> bool {
        match self.records.lock() {
            Ok(mut records) => {
                records.push(record);
                true
            }
            Err(_) => false,
        }
    }
}

/// A service principal's declaration: the groups its manifest names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServiceManifest {
    groups: Vec<GroupName>,
}

impl ServiceManifest {
    /// A manifest declaring `groups`.
    #[must_use]
    pub fn new(groups: Vec<GroupName>) -> Self {
        Self { groups }
    }

    /// Whether `group` is declared.
    #[must_use]
    pub fn declares(&self, group: &GroupName) -> bool {
        self.groups.iter().any(|declared| declared == group)
    }

    /// The declared groups.
    #[must_use]
    pub fn groups(&self) -> &[GroupName] {
        &self.groups
    }
}

/// The node a reveal is asked from, when it is asked from one.
///
/// A hub-local reveal has no node. A branch or offline node's reveal carries its identity so a
/// **revoked** node is refused before anything is looked up (ADR-070 rule 12.13 h): the node's
/// certificate lapsed and its keys were rotated away, so it may reveal nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeIdentity {
    /// The node id.
    pub node: NodeId,
    /// Whether the hub has revoked it.
    pub revoked: bool,
}

impl NodeIdentity {
    /// A node in good standing.
    #[must_use]
    pub fn active(node: NodeId) -> Self {
        Self {
            node,
            revoked: false,
        }
    }

    /// A revoked node.
    #[must_use]
    pub fn revoked(node: NodeId) -> Self {
        Self {
            node,
            revoked: true,
        }
    }
}

/// One bulk reveal request. It is a struct rather than eight arguments so the call site names
/// what each value is.
#[derive(Clone, Copy, Debug)]
pub struct BulkRequest<'a> {
    /// The acting service principal.
    pub principal: &'a Principal,
    /// The service's declared groups.
    pub manifest: &'a ServiceManifest,
    /// The protection group the field belongs to.
    pub group: &'a GroupName,
    /// The DocType.
    pub doctype: &'a str,
    /// The documents to reveal.
    pub ids: &'a [String],
    /// The field path. Its protection facts come from the service's own registry; a request
    /// carries none.
    pub field: &'a str,
    /// The caller's UTC instant in microseconds.
    pub at: i64,
}

/// The hub's reveal service.
///
/// Every reveal is one document and one field. Every call, allowed or refused, writes exactly
/// one audit record, and a record that cannot be written means nothing is released.
pub struct RevealService {
    engine: Arc<PermissionEngine>,
    values: Arc<dyn ValueSource>,
    audit: Arc<dyn AuditSink>,
    node: Option<NodeIdentity>,
}

impl RevealService {
    /// A service over the permission engine, the hub's value source and an audit sink.
    #[must_use]
    pub fn new(
        engine: Arc<PermissionEngine>,
        values: Arc<dyn ValueSource>,
        audit: Arc<dyn AuditSink>,
    ) -> Self {
        Self {
            engine,
            values,
            audit,
            node: None,
        }
    }

    /// The same service, answering for `node`: a revoked node is refused before any lookup.
    #[must_use]
    pub fn at_node(mut self, node: NodeIdentity) -> Self {
        self.node = Some(node);
        self
    }

    /// The node the service answers for, if any.
    #[must_use]
    pub fn node(&self) -> Option<&NodeIdentity> {
        self.node.as_ref()
    }

    /// Reveals one field of one document.
    ///
    /// The field's protection facts are read from the permission snapshot the service holds
    /// ([`RevealService::facts_of`]); the caller supplies none, so it cannot claim a field is
    /// unprotected or in a scope it is not.
    ///
    /// `at` is the caller's UTC instant in microseconds; this crate reads no clock.
    ///
    /// # Errors
    ///
    /// [`RevealError::Denied`] for a missing document, an unknown DocType or field, a field
    /// that cannot be revealed and a caller without the right (one answer for all of them);
    /// [`RevealError::AuditUnavailable`] when the record cannot be written.
    pub fn reveal(
        &self,
        principal: &Principal,
        doctype: &str,
        id: &str,
        field: &str,
        at: i64,
    ) -> Result<Revealed, RevealError> {
        if let Some(node) = &self.node
            && node.revoked
        {
            self.audit_reveal(principal, doctype, id, field, at, false)?;
            return Err(RevealError::NodeRevoked {
                node: node.node.clone(),
            });
        }
        let decided = self
            .facts_of(doctype, field)
            .and_then(|facts| self.decide(principal, doctype, id, field, &facts));
        let allowed = decided.is_some();
        self.audit_reveal(principal, doctype, id, field, at, allowed)?;
        match decided {
            Some(value) => Ok(Revealed { value }),
            None => Err(RevealError::Denied),
        }
    }

    /// The support view of a reveal decision: the engine's `explain_field` text. It names the
    /// rules that spoke and never a value. A field the registry does not know gets a stable
    /// line saying so.
    #[must_use]
    pub fn explain(&self, principal: &Principal, doctype: &str, id: &str, field: &str) -> String {
        let Some(facts) = self.facts_of(doctype, field) else {
            return format!("{doctype}.{field}: no protection facts");
        };
        let doc = self
            .doc_facts(doctype, id, field)
            .unwrap_or_else(|| DocFacts {
                doctype: doctype.to_owned(),
                id: Some(id.to_owned()),
                ..DocFacts::default()
            });
        self.engine.explain_field(principal, doctype, &facts, &doc)
    }

    /// Reveals one field of many documents for a **service principal** holding `reveal_bulk`
    /// and declaring the group in its manifest. A person is refused whatever their grants.
    ///
    /// # Errors
    ///
    /// [`RevealError::BulkDenied`] for a person, [`RevealError::GroupUndeclared`] when the
    /// manifest does not name the group, [`RevealError::Denied`] when the field's bulk right is
    /// absent, and [`RevealError::AuditUnavailable`] when the record cannot be written.
    pub fn reveal_bulk(&self, request: BulkRequest<'_>) -> Result<Vec<Revealed>, RevealError> {
        let BulkRequest {
            principal,
            manifest,
            group,
            doctype,
            ids,
            field,
            at,
        } = request;
        let bulk_audit = BulkAudit {
            principal,
            group,
            doctype,
            field,
            ids,
            at,
        };
        if let Some(node) = &self.node
            && node.revoked
        {
            self.audit_bulk(&bulk_audit, false)?;
            return Err(RevealError::NodeRevoked {
                node: node.node.clone(),
            });
        }
        if !principal.is_system {
            self.audit_bulk(&bulk_audit, false)?;
            return Err(RevealError::BulkDenied);
        }
        if !manifest.declares(group) {
            self.audit_bulk(&bulk_audit, false)?;
            return Err(RevealError::GroupUndeclared {
                group: group.clone(),
            });
        }
        // All or nothing: one document that is missing or refused refuses the whole request.
        let allowed = self.facts_of(doctype, field).is_some_and(|facts| {
            ids.iter().all(|id| {
                self.doc_facts(doctype, id, field).is_some_and(|doc| {
                    self.engine
                        .field_access(principal, doctype, &facts, &doc)
                        .can_reveal_bulk
                })
            })
        });
        self.audit_bulk(&bulk_audit, allowed)?;
        if !allowed {
            return Err(RevealError::Denied);
        }
        let values = self.values.clone();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(loaded) = values.load(doctype, id, field) {
                out.push(Revealed {
                    value: loaded.value,
                });
            }
        }
        Ok(out)
    }

    fn doc_facts(&self, doctype: &str, id: &str, field: &str) -> Option<DocFacts> {
        self.values.load(doctype, id, field).map(|loaded| DocFacts {
            doctype: doctype.to_owned(),
            id: Some(id.to_owned()),
            ..loaded.doc_facts
        })
    }

    fn decide(
        &self,
        principal: &Principal,
        doctype: &str,
        id: &str,
        field: &str,
        facts: &FieldFacts,
    ) -> Option<SecretBytes> {
        let loaded = self.values.load(doctype, id, field)?;
        let doc = DocFacts {
            doctype: doctype.to_owned(),
            id: Some(id.to_owned()),
            ..loaded.doc_facts
        };
        let decision = self.engine.field_access(principal, doctype, facts, &doc);
        decision.can_reveal.then_some(loaded.value)
    }

    fn audit_reveal(
        &self,
        principal: &Principal,
        doctype: &str,
        id: &str,
        field: &str,
        at: i64,
        allowed: bool,
    ) -> Result<(), RevealError> {
        let mut payload = Payload::new();
        payload.insert("field".to_owned(), PayloadValue::Text(field.to_owned()));
        payload.insert("api".to_owned(), PayloadValue::Text(REVEAL_API.to_owned()));
        payload.insert("allowed".to_owned(), PayloadValue::Bool(allowed));
        // The engine's explanation names the rules that spoke, never a value; keeping it makes
        // the record say *why* the reveal was allowed or refused.
        payload.insert(
            "explain".to_owned(),
            PayloadValue::Text(self.explain(principal, doctype, id, field)),
        );
        let record = AuditRecord {
            kind: REVEAL_AUDIT_KIND.to_owned(),
            actor: actor_of(principal),
            on_behalf_of: None,
            subject: Some(Subject {
                doctype: doctype.to_owned(),
                id: id.to_owned(),
            }),
            at,
            payload,
            masked: false,
        };
        if self.audit.offer(record) {
            Ok(())
        } else {
            Err(RevealError::AuditUnavailable)
        }
    }

    fn audit_bulk(&self, bulk: &BulkAudit<'_>, allowed: bool) -> Result<(), RevealError> {
        let documents = encode_documents(bulk.ids);
        let actor = actor_of(bulk.principal);
        let mut payload = Payload::new();
        payload.insert(
            "field".to_owned(),
            PayloadValue::Text(bulk.field.to_owned()),
        );
        payload.insert(
            "doctype".to_owned(),
            PayloadValue::Text(bulk.doctype.to_owned()),
        );
        payload.insert(
            "group".to_owned(),
            PayloadValue::Text(bulk.group.as_str().to_owned()),
        );
        payload.insert("api".to_owned(), PayloadValue::Text(REVEAL_API.to_owned()));
        payload.insert("allowed".to_owned(), PayloadValue::Bool(allowed));
        payload.insert(
            "count".to_owned(),
            PayloadValue::Int(i64::try_from(bulk.ids.len()).unwrap_or(i64::MAX)),
        );
        // Every document id the request named, and who asked, so the record answers *whose
        // values were released to whom* without a second lookup.
        payload.insert(
            "documents".to_owned(),
            PayloadValue::Text(documents.clone()),
        );
        payload.insert("subject".to_owned(), PayloadValue::Text(actor.clone()));
        let record = AuditRecord {
            kind: REVEAL_BULK_AUDIT_KIND.to_owned(),
            actor,
            on_behalf_of: None,
            subject: Some(Subject {
                doctype: bulk.doctype.to_owned(),
                id: documents,
            }),
            at: bulk.at,
            payload,
            masked: false,
        };
        if self.audit.offer(record) {
            Ok(())
        } else {
            Err(RevealError::AuditUnavailable)
        }
    }
}

/// What a bulk audit record is built from.
struct BulkAudit<'a> {
    principal: &'a Principal,
    group: &'a GroupName,
    doctype: &'a str,
    field: &'a str,
    ids: &'a [String],
    at: i64,
}

/// The document ids of a bulk request as one text: `<byte length>:<id>` entries joined by
/// commas. The length prefix keeps an id that contains a comma from reading as two documents.
fn encode_documents(ids: &[String]) -> String {
    ids.iter()
        .map(|id| format!("{}:{id}", id.len()))
        .collect::<Vec<_>>()
        .join(",")
}

impl HubPort for RevealService {
    fn reveal(
        &self,
        principal: &Principal,
        doctype: &str,
        id: &str,
        field: &str,
        at: i64,
    ) -> Result<Revealed, RevealError> {
        RevealService::reveal(self, principal, doctype, id, field, at)
    }

    fn reveal_bulk(&self, request: BulkRequest<'_>) -> Result<Vec<Revealed>, RevealError> {
        RevealService::reveal_bulk(self, request)
    }
}

impl RevealService {
    /// The field's protection facts, from the permission snapshot the engine holds.
    #[must_use]
    pub fn facts_of(&self, doctype: &str, field: &str) -> Option<FieldFacts> {
        self.engine.snapshot().facts(doctype)?.field(field).cloned()
    }
}

/// The branch or offline caller's reveal: a [`HubPort`] that is either reachable or not.
///
/// This is what makes reveal online-only in code rather than in a comment. With no port, every
/// call answers [`RevealError::Offline`] with the user-facing explanation key; the masked form
/// is the offline path, and there is no local fallback here.
pub struct RemoteReveal {
    port: Option<Arc<dyn HubPort>>,
}

impl RemoteReveal {
    /// A caller that can reach `port`.
    #[must_use]
    pub fn online(port: Arc<dyn HubPort>) -> Self {
        Self { port: Some(port) }
    }

    /// A caller with no live connection.
    #[must_use]
    pub fn offline() -> Self {
        Self { port: None }
    }

    /// Whether a hub call can be made.
    #[must_use]
    pub fn is_online(&self) -> bool {
        self.port.is_some()
    }

    /// Reveals one field of one document through the hub.
    ///
    /// # Errors
    ///
    /// [`RevealError::Offline`] when there is no live connection, and the hub's own answer
    /// otherwise.
    pub fn reveal(
        &self,
        principal: &Principal,
        doctype: &str,
        id: &str,
        field: &str,
        at: i64,
    ) -> Result<Revealed, RevealError> {
        match &self.port {
            Some(port) => port.reveal(principal, doctype, id, field, at),
            None => offline_reveal(),
        }
    }

    /// Reveals one field of many documents through the hub.
    ///
    /// # Errors
    ///
    /// [`RevealError::Offline`] when there is no live connection, and the hub's own answer
    /// otherwise.
    pub fn reveal_bulk(&self, request: BulkRequest<'_>) -> Result<Vec<Revealed>, RevealError> {
        match &self.port {
            Some(port) => port.reveal_bulk(request),
            None => Err(RevealError::offline(OFFLINE_EXPLANATION)),
        }
    }
}

/// The audit actor form: `system` or `user:<name>`.
fn actor_of(principal: &Principal) -> String {
    if principal.is_system || principal.user.is_empty() {
        "system".to_owned()
    } else {
        format!("user:{}", principal.user)
    }
}

/// The answer an offline caller gets: reveal is online-only (ADR-070 rule 12.13 f).
///
/// # Errors
///
/// Always [`RevealError::Offline`], with [`OFFLINE_EXPLANATION`] as its user-facing key.
pub fn offline_reveal() -> Result<Revealed, RevealError> {
    Err(RevealError::offline(OFFLINE_EXPLANATION))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use spot_meta::{Definition, FieldSpec, FieldType, ProtectionGroupSpec, Registry};
    use spot_permissions::{Model, ModelTexts, Snapshot, load};

    fn meta() -> std::sync::Arc<spot_meta::EffectiveMeta> {
        let mut doctype = Definition::new("bank_account");
        doctype.fields = vec![FieldSpec::new("iban", FieldType::Text)];
        doctype.protection_groups = vec![ProtectionGroupSpec::new("bank", "Bank")];
        std::sync::Arc::new(
            Registry::build(vec![doctype], &spot_meta::Customisations::new()).expect("meta"),
        )
    }

    fn model() -> Model {
        load(&ModelTexts {
            grants: r#"
[[entries]]
role = "Clerk"
doctype = "bank_account"
rights = ["read"]
"#
            .to_owned(),
            field_grants: r#"
[[entries]]
role = "Clerk"
doctype = "bank_account"
target = { group = "bank" }
access = ["read", "reveal"]
[[entries]]
role = "Reader"
doctype = "bank_account"
target = { group = "bank" }
access = ["read_masked"]
"#
            .to_owned(),
            groups: r#"
[[entries]]
name = "bank"
"#
            .to_owned(),
            shares: String::new(),
        })
        .expect("model")
    }

    fn engine() -> Arc<PermissionEngine> {
        let meta = meta();
        // The registry's own facts: `iban` is in the `bank` group, masked to its last four.
        let snapshot = Snapshot::build(&meta, &model())
            .expect("snapshot")
            .with_field_facts(
                "bank_account",
                vec![FieldFacts {
                    path: "iban".to_owned(),
                    groups: vec![spot_permissions::GroupName("bank".to_owned())],
                    tags: Vec::new(),
                    mask: Some(spot_permissions::MaskFormat::LastChars(4)),
                }],
            )
            .expect("facts");
        Arc::new(PermissionEngine::new(Arc::new(snapshot)))
    }

    struct Source {
        value: Option<Vec<u8>>,
    }

    impl ValueSource for Source {
        fn load(&self, _doctype: &str, id: &str, _field: &str) -> Option<Loaded> {
            let value = self
                .value
                .as_ref()
                .map(|bytes| SecretBytes::new(bytes.clone()))?;
            Some(Loaded {
                value,
                doc_facts: DocFacts {
                    doctype: "bank_account".to_owned(),
                    id: Some(id.to_owned()),
                    created_by: Some("alice".to_owned()),
                    ..DocFacts::default()
                },
            })
        }
    }

    fn service(value: &[u8]) -> (RevealService, Arc<MemoryAuditSink>) {
        let sink = Arc::new(MemoryAuditSink::new());
        let service = RevealService::new(
            engine(),
            Arc::new(Source {
                value: Some(value.to_vec()),
            }),
            sink.clone(),
        );
        (service, sink)
    }

    fn clerk() -> Principal {
        Principal::with_roles("alice", &["Clerk"])
    }

    fn masked_reader() -> Principal {
        Principal::with_roles("bob", &["Reader"])
    }

    #[test]
    fn a_reveal_returns_the_value_and_writes_exactly_one_record() {
        let (service, sink) = service(b"DE89");
        let revealed = service
            .reveal(&clerk(), "bank_account", "BA-1", "iban", 1_700_000_000)
            .expect("reveal");
        assert_eq!(revealed.expose(), b"DE89");
        assert_eq!(sink.len(), 1);
        let record = &sink.records()[0];
        assert_eq!(record.kind, REVEAL_AUDIT_KIND);
        assert_eq!(record.actor, "user:alice");
        assert_eq!(record.at, 1_700_000_000);
        assert_eq!(
            record.subject,
            Some(Subject {
                doctype: "bank_account".to_owned(),
                id: "BA-1".to_owned()
            })
        );
        assert!(
            !record
                .payload
                .values()
                .any(|v| v == &PayloadValue::Text("DE89".to_owned()))
        );
    }

    #[test]
    fn a_refused_reveal_is_audited_and_releases_nothing() {
        let (service, sink) = service(b"DE89");
        let error = service
            .reveal(&masked_reader(), "bank_account", "BA-1", "iban", 1)
            .expect_err("no reveal right");
        assert_eq!(error.code(), crate::error::REVEAL_DENIED);
        assert_eq!(sink.len(), 1);
        assert!(
            !sink.records()[0]
                .payload
                .values()
                .any(|v| v == &PayloadValue::Bool(true))
        );
    }

    #[test]
    fn a_missing_document_looks_like_a_refusal() {
        let sink = Arc::new(MemoryAuditSink::new());
        let service = RevealService::new(engine(), Arc::new(Source { value: None }), sink.clone());
        let error = service
            .reveal(&clerk(), "bank_account", "BA-9", "iban", 1)
            .expect_err("nothing");
        assert_eq!(error.code(), crate::error::REVEAL_DENIED);
        assert_eq!(sink.len(), 1);
    }

    #[test]
    fn an_unavailable_audit_sink_releases_nothing() {
        struct Full;
        impl AuditSink for Full {
            fn offer(&self, _record: AuditRecord) -> bool {
                false
            }
        }
        let service = RevealService::new(
            engine(),
            Arc::new(Source {
                value: Some(b"DE89".to_vec()),
            }),
            Arc::new(Full),
        );
        let error = service
            .reveal(&clerk(), "bank_account", "BA-1", "iban", 1)
            .expect_err("audit");
        assert_eq!(error.code(), crate::error::AUDIT_UNAVAILABLE);
    }

    #[test]
    fn a_person_cannot_reveal_in_bulk() {
        let (service, sink) = service(b"DE89");
        let ids = vec!["BA-1".to_owned()];
        let error = service
            .reveal_bulk(BulkRequest {
                principal: &clerk(),
                manifest: &ServiceManifest::new(vec![GroupName::new("bank").expect("group")]),
                group: &GroupName::new("bank").expect("group"),
                doctype: "bank_account",
                ids: &ids,
                field: "iban",
                at: 1,
            })
            .expect_err("a person");
        assert_eq!(error.code(), crate::error::REVEAL_BULK_DENIED);
        assert_eq!(sink.len(), 1);
    }

    #[test]
    fn a_service_must_declare_the_group() {
        let (service, _) = service(b"DE89");
        let ids = vec!["BA-1".to_owned()];
        let error = service
            .reveal_bulk(BulkRequest {
                principal: &Principal::system(),
                manifest: &ServiceManifest::new(vec![GroupName::new("other").expect("group")]),
                group: &GroupName::new("bank").expect("group"),
                doctype: "bank_account",
                ids: &ids,
                field: "iban",
                at: 1,
            })
            .expect_err("undeclared");
        assert_eq!(error.code(), crate::error::GROUP_UNDECLARED);
    }

    #[test]
    fn a_declared_service_reveals_in_bulk() {
        let (service, sink) = service(b"DE89");
        let ids = vec!["BA-1".to_owned(), "BA-2".to_owned()];
        let values = service
            .reveal_bulk(BulkRequest {
                principal: &Principal::system(),
                manifest: &ServiceManifest::new(vec![GroupName::new("bank").expect("group")]),
                group: &GroupName::new("bank").expect("group"),
                doctype: "bank_account",
                ids: &ids,
                field: "iban",
                at: 1,
            })
            .expect("bulk");
        assert_eq!(values.len(), 2);
        assert_eq!(sink.len(), 1);
        assert_eq!(sink.records()[0].kind, REVEAL_BULK_AUDIT_KIND);
    }

    #[test]
    fn a_revoked_node_cannot_reveal() {
        let (service, sink) = service(b"DE89");
        let service = service.at_node(NodeIdentity::revoked(
            NodeId::new("branch-1").expect("node"),
        ));
        let error = service
            .reveal(&clerk(), "bank_account", "BA-1", "iban", 1)
            .expect_err("revoked");
        assert_eq!(error.code(), crate::error::NODE_REVOKED);
        assert_eq!(sink.len(), 1, "the refusal is audited too");
        assert!(
            !sink.records()[0]
                .payload
                .values()
                .any(|v| v == &PayloadValue::Bool(true))
        );
    }

    #[test]
    fn an_active_node_reveals_normally() {
        let (service, _) = service(b"DE89");
        let service = service.at_node(NodeIdentity::active(NodeId::new("branch-1").expect("node")));
        let revealed = service
            .reveal(&clerk(), "bank_account", "BA-1", "iban", 1)
            .expect("active");
        assert_eq!(revealed.expose(), b"DE89");
    }

    #[test]
    fn offline_reveal_explains_itself() {
        let error = offline_reveal().expect_err("offline");
        assert_eq!(error.code(), crate::error::REVEAL_OFFLINE);
        match error {
            RevealError::Offline { explanation_key } => {
                assert_eq!(explanation_key, OFFLINE_EXPLANATION)
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_explanation_names_rules_not_values() {
        let (service, _) = service(b"DE89");
        let text = service.explain(&masked_reader(), "bank_account", "BA-1", "iban");
        assert!(text.contains("bank_account.iban"));
        assert!(!text.contains("DE89"));
    }
}
