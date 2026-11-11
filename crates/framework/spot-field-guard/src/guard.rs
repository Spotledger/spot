//! [`FieldGuard`]: the one enforcement point for every channel that is not the document read
//! path (ADR-070 section 12, rule 6).
//!
//! The guard decides **nothing**. Every question goes to the [`PermissionEngine`]
//! (`field_access`, `filter_ok`) and every mask to [`spot_permissions::mask`]; this module
//! only chooses, per channel, which of the engine's answers applies and removes, masks or
//! keeps the value accordingly. No decision is cached: the engine and the protection source
//! are asked again on every call, so a snapshot or metadata swap shows on the very next call.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spot_app_api::Document;
use spot_app_api::document::ChildRow;
use spot_app_api::prelude::{Clock, Name, Value};
use spot_permissions::{
    DocFacts, FieldDecision, FieldFacts, FieldLevel, GroupName, MaskFormat, PermissionEngine,
    Principal, mask,
};

use crate::channel::{Channel, Need};
use crate::error::GuardError;
use crate::records::{AuditSink, FieldAccessRecord, Offer, TraceSink, Via};
use crate::source::{DocProtection, ProtectionSource};

/// What a channel does with one field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shown {
    /// The field does not leave.
    Hidden,
    /// Only the masked form leaves.
    Masked(MaskFormat),
    /// The real value leaves.
    Value,
}

/// The channel's reading of the engine's answer for one field.
pub(crate) fn shown(channel: Channel, facts: &FieldFacts, d: &FieldDecision) -> Shown {
    if !channel.carries_values() {
        return Shown::Hidden;
    }
    let by_level = match d.level {
        FieldLevel::Hidden => Shown::Hidden,
        FieldLevel::Masked(format) => Shown::Masked(format),
        FieldLevel::Read | FieldLevel::ReadReveal => Shown::Value,
    };
    if channel.need() == Need::Report && !d.can_report {
        return match by_level {
            Shown::Hidden => Shown::Hidden,
            Shown::Masked(f) if channel.degrades_to_masked() => Shown::Masked(f),
            _ if channel.degrades_to_masked() => {
                Shown::Masked(facts.mask.unwrap_or(MaskFormat::Placeholder))
            }
            _ => Shown::Hidden,
        };
    }
    by_level
}

/// A value in its masked form. Text is masked by the permission core's `mask`; a value with
/// no text form is dropped to null rather than shown in a way that leaks its shape (the same
/// choice the document read path makes).
pub(crate) fn masked_value(value: &Value, format: &MaskFormat) -> Value {
    match value {
        Value::Text(text) => Value::Text(mask(text, format)),
        _ => Value::Null,
    }
}

/// One checked field: what the channel does with it and whether it is traced.
pub(crate) struct Checked {
    pub(crate) shown: Shown,
    pub(crate) traced: bool,
}

/// The guard. Cheap to clone (all state is behind `Arc`).
#[derive(Clone)]
pub struct FieldGuard {
    pub(crate) engine: Arc<PermissionEngine>,
    pub(crate) source: Arc<dyn ProtectionSource>,
    pub(crate) trace: Arc<dyn TraceSink>,
    pub(crate) audit: Arc<dyn AuditSink>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) fail_closed: bool,
}

impl std::fmt::Debug for FieldGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FieldGuard")
            .field("fail_closed", &self.fail_closed)
            .finish_non_exhaustive()
    }
}

impl FieldGuard {
    /// A guard over an engine and a protection source. `fail_closed` is **on**: a trace
    /// record that cannot be queued withholds the value.
    #[must_use]
    pub fn new(
        engine: Arc<PermissionEngine>,
        source: Arc<dyn ProtectionSource>,
        trace: Arc<dyn TraceSink>,
        audit: Arc<dyn AuditSink>,
        clock: Arc<dyn Clock>,
    ) -> FieldGuard {
        FieldGuard {
            engine,
            source,
            trace,
            audit,
            clock,
            fail_closed: true,
        }
    }

    /// Sets whether a full trace queue withholds the value of a trace-group field (default
    /// on). Off: the access proceeds and the lost record is the operator's alert.
    #[must_use]
    pub fn with_fail_closed(mut self, fail_closed: bool) -> FieldGuard {
        self.fail_closed = fail_closed;
        self
    }

    /// The same guard over a newer permission engine (a snapshot swap). Nothing else carries
    /// over from the old engine: there is no cache to clear.
    #[must_use]
    pub fn with_engine(&self, engine: Arc<PermissionEngine>) -> FieldGuard {
        FieldGuard {
            engine,
            ..self.clone()
        }
    }

    /// The engine the guard asks.
    #[must_use]
    pub fn engine(&self) -> &Arc<PermissionEngine> {
        &self.engine
    }

    pub(crate) fn protection(&self, doctype: &str) -> Result<Arc<DocProtection>, GuardError> {
        self.source
            .protection(doctype)
            .ok_or_else(|| GuardError::unknown_doctype(doctype))
    }

    /// Whether a field is in a group that asks for a trace: declared by the source or flagged
    /// on the permission snapshot.
    pub(crate) fn is_traced(&self, prot: &DocProtection, facts: &FieldFacts) -> bool {
        facts
            .groups
            .iter()
            .any(|g| prot.trace_groups.contains(g) || self.engine.snapshot().traces(g))
    }

    /// The engine's answer for one field, read by the channel.
    pub(crate) fn check(
        &self,
        channel: Channel,
        principal: &Principal,
        doctype: &str,
        prot: &DocProtection,
        name: &str,
        doc: &DocFacts,
    ) -> Checked {
        let facts = prot.field(name);
        let doc = DocFacts {
            doctype: doctype.to_owned(),
            ..doc.clone()
        };
        let decision = self.engine.field_access(principal, doctype, &facts, &doc);
        let shown = shown(channel, &facts, &decision);
        let traced = self.is_traced(prot, &facts);
        Checked { shown, traced }
    }

    pub(crate) fn record(
        &self,
        principal: &Principal,
        doctype: &str,
        id: Option<&str>,
        field: &str,
        via: Via,
        authorised: bool,
    ) -> FieldAccessRecord {
        FieldAccessRecord {
            who: principal.user.clone(),
            at: self.clock.now_utc(),
            doctype: doctype.to_owned(),
            id: id.map(str::to_owned),
            field: field.to_owned(),
            via,
            authorised,
        }
    }

    /// Queues the trace records without waiting. A refused access that cannot be traced is
    /// still refused, so only a lost record of an authorised access can fail closed.
    pub(crate) fn emit_trace(&self, records: Vec<FieldAccessRecord>) -> Result<(), GuardError> {
        let mut lost_authorised = false;
        for record in records {
            let authorised = record.authorised;
            if self.trace.offer(record) == Offer::Full && authorised {
                lost_authorised = true;
            }
        }
        if lost_authorised && self.fail_closed {
            Err(GuardError::trace_unavailable())
        } else {
            Ok(())
        }
    }

    // ------------------------------------------------------------------ redact

    /// Redacts a document for a channel, in place.
    ///
    /// For every field the channel applies the engine's decision: a hidden field is
    /// **removed** (not blanked), a masked field gets its masked form, a child-table column
    /// follows its own DocType's facts, and a channel that never carries a value
    /// ([`Channel::carries_values`]) loses every field and table whatever the right (use
    /// [`FieldGuard::visible_names`] for what such a channel may say). A table the source
    /// cannot map to a child DocType is removed whole.
    ///
    /// A field of a `trace` group that leaves with its real value is traced as authorised;
    /// one the channel withheld is traced as refused; a masked one is not traced (no value
    /// left). If an authorised trace record cannot be queued while the guard is
    /// `fail_closed`, the call fails.
    ///
    /// # Errors
    ///
    /// The DocType is unknown to the protection source, or the trace queue is full under
    /// `fail_closed`. On **any** error the document is emptied (id and DocType kept) so a
    /// caller that ignores the error still leaks nothing.
    pub fn redact(
        &self,
        channel: Channel,
        principal: &Principal,
        doctype: &str,
        doc_facts: &DocFacts,
        doc: &mut Document,
    ) -> Result<(), GuardError> {
        match self.redacted(channel, principal, doctype, doc_facts, doc) {
            Ok(done) => {
                *doc = done;
                Ok(())
            }
            Err(e) => {
                *doc = rebuild(doc, BTreeMap::new(), BTreeMap::new());
                Err(e)
            }
        }
    }

    fn redacted(
        &self,
        channel: Channel,
        principal: &Principal,
        doctype: &str,
        doc_facts: &DocFacts,
        doc: &Document,
    ) -> Result<Document, GuardError> {
        let prot = self.protection(doctype)?;
        let id = doc.id().to_string();
        let mut records: Vec<FieldAccessRecord> = Vec::new();
        let mut fields: BTreeMap<Name, Value> = BTreeMap::new();
        let mut tables: BTreeMap<Name, Vec<ChildRow>> = BTreeMap::new();
        if channel.carries_values() {
            for (name, value) in doc.fields() {
                let c = self.check(channel, principal, doctype, &prot, name.as_str(), doc_facts);
                self.keep(
                    &mut records,
                    principal,
                    doctype,
                    &id,
                    channel,
                    name.as_str(),
                    &c,
                );
                match c.shown {
                    Shown::Value => {
                        fields.insert(name.clone(), value.clone());
                    }
                    Shown::Masked(f) => {
                        fields.insert(name.clone(), masked_value(value, &f));
                    }
                    Shown::Hidden => {}
                }
            }
            for (table, rows) in doc.tables() {
                let t = self.check(
                    channel,
                    principal,
                    doctype,
                    &prot,
                    table.as_str(),
                    doc_facts,
                );
                if t.shown != Shown::Value {
                    continue;
                }
                let Some(child) = prot.child_doctype(table.as_str()) else {
                    continue;
                };
                let child_prot = self.protection(child)?;
                let row_facts = DocFacts {
                    doctype: child.to_owned(),
                    ..DocFacts::default()
                };
                let mut kept = Vec::with_capacity(rows.len());
                for row in rows {
                    let mut kept_fields: BTreeMap<Name, Value> = BTreeMap::new();
                    for (col, value) in &row.fields {
                        let c = self.check(
                            channel,
                            principal,
                            child,
                            &child_prot,
                            col.as_str(),
                            &row_facts,
                        );
                        let path = format!("{}.{}", table.as_str(), col.as_str());
                        self.keep(&mut records, principal, doctype, &id, channel, &path, &c);
                        match c.shown {
                            Shown::Value => {
                                kept_fields.insert(col.clone(), value.clone());
                            }
                            Shown::Masked(f) => {
                                kept_fields.insert(col.clone(), masked_value(value, &f));
                            }
                            Shown::Hidden => {}
                        }
                    }
                    kept.push(ChildRow {
                        id: row.id,
                        fields: kept_fields,
                    });
                }
                tables.insert(table.clone(), kept);
            }
        }
        self.emit_trace(records)?;
        Ok(rebuild(doc, fields, tables))
    }

    /// Pushes the trace record one checked field earns: authorised when the real value
    /// leaves, refused when a traced field is withheld, none when only the mask leaves.
    #[allow(clippy::too_many_arguments)]
    fn keep(
        &self,
        records: &mut Vec<FieldAccessRecord>,
        principal: &Principal,
        doctype: &str,
        id: &str,
        channel: Channel,
        path: &str,
        c: &Checked,
    ) {
        if !c.traced {
            return;
        }
        let via = Via::Channel(channel);
        match c.shown {
            Shown::Value => {
                records.push(self.record(principal, doctype, Some(id), path, via, true))
            }
            Shown::Hidden => {
                records.push(self.record(principal, doctype, Some(id), path, via, false));
            }
            Shown::Masked(_) => {}
        }
    }

    /// Which of `names` the principal may know exist on this document (the channels that
    /// carry field names only, [`Channel::Notification`] and [`Channel::Journal`], say these
    /// and nothing else). A hidden field's name is not told.
    ///
    /// # Errors
    ///
    /// The DocType is unknown to the protection source.
    pub fn visible_names(
        &self,
        principal: &Principal,
        doctype: &str,
        doc_facts: &DocFacts,
        names: &[String],
    ) -> Result<Vec<String>, GuardError> {
        let prot = self.protection(doctype)?;
        let doc = DocFacts {
            doctype: doctype.to_owned(),
            ..doc_facts.clone()
        };
        Ok(names
            .iter()
            .filter(|n| {
                let facts = prot.field(n);
                self.engine
                    .field_access(principal, doctype, &facts, &doc)
                    .level
                    != FieldLevel::Hidden
            })
            .cloned()
            .collect())
    }

    // ------------------------------------------------------------------ columns

    /// Sorts the requested columns of a list, export, print or report into those that show
    /// their value, those that show the masked form, and those that are refused.
    ///
    /// A column is `field` or `table.column` for a child-table column. A column the channel
    /// would hide is **refused, never silently dropped**: the call fails with
    /// `permission.field_denied` naming every such path. [`Channel::Print`] does not fail: it
    /// prints the masked form where it may not print the value, and a column hidden
    /// outright is listed in `refused` and left off the printout.
    ///
    /// # Errors
    ///
    /// `permission.field_denied` listing the refused paths, or an unknown DocType.
    pub fn columns(
        &self,
        channel: Channel,
        principal: &Principal,
        doctype: &str,
        requested: &[String],
    ) -> Result<Columns, GuardError> {
        let prot = self.protection(doctype)?;
        let doc = DocFacts {
            doctype: doctype.to_owned(),
            ..DocFacts::default()
        };
        let mut out = Columns::default();
        for path in requested {
            let class = match self.resolve(&prot, doctype, path) {
                Some((owner, column, owner_prot)) => {
                    let row_doc = DocFacts {
                        doctype: owner.clone(),
                        ..DocFacts::default()
                    };
                    let d = if owner == doctype { &doc } else { &row_doc };
                    self.check(channel, principal, &owner, &owner_prot, &column, d)
                        .shown
                }
                None => Shown::Hidden,
            };
            match class {
                Shown::Value => out.allowed.push(path.clone()),
                Shown::Masked(_) => out.masked.push(path.clone()),
                Shown::Hidden => out.refused.push(path.clone()),
            }
        }
        if channel.degrades_to_masked() || out.refused.is_empty() {
            Ok(out)
        } else {
            Err(GuardError::field_denied(out.refused))
        }
    }

    /// The DocType and column a request path names: `field` on the DocType, or `table.col`
    /// on the table's child DocType.
    fn resolve(
        &self,
        prot: &Arc<DocProtection>,
        doctype: &str,
        path: &str,
    ) -> Option<(String, String, Arc<DocProtection>)> {
        match path.split_once('.') {
            None => Some((doctype.to_owned(), path.to_owned(), Arc::clone(prot))),
            Some((table, column)) => {
                let child = prot.child_doctype(table)?;
                let child_prot = self.source.protection(child)?;
                Some((child.to_owned(), column.to_owned(), child_prot))
            }
        }
    }

    // ------------------------------------------------------------------ queries

    /// The no-inference rule (ADR-070 section 12, rule 7) for a channel that takes a user
    /// query (report and export): a field may be filtered, sorted, grouped or aggregated
    /// only if the principal may use it **unmasked** *and* the channel's right is held. For
    /// a channel whose need is [`Need::Report`] that is the `report` right (`can_report`),
    /// not `read`: a principal who may read a field but not report it must learn nothing
    /// about its ordering, equality classes or range from the result. Both conditions are
    /// required. A channel that never carries a value ([`Need::NamesOnly`]) refuses every
    /// field. A child-table path (`table.column`) is resolved as in [`FieldGuard::columns`].
    ///
    /// # Errors
    ///
    /// `permission.field_denied` naming every refused path (sorted, deduplicated), or an
    /// unknown DocType.
    pub fn check_query_fields_for(
        &self,
        channel: Channel,
        principal: &Principal,
        doctype: &str,
        used: &[QueryField],
    ) -> Result<(), GuardError> {
        let prot = self.protection(doctype)?;
        let mut refused: BTreeSet<String> = BTreeSet::new();
        for field in used {
            let ok = match self.resolve(&prot, doctype, &field.path) {
                Some((owner, column, owner_prot)) => {
                    let facts = owner_prot.field(&column);
                    let doc = DocFacts {
                        doctype: owner.clone(),
                        ..DocFacts::default()
                    };
                    let d = self.engine.field_access(principal, &owner, &facts, &doc);
                    let unmasked = matches!(d.level, FieldLevel::Read | FieldLevel::ReadReveal);
                    let right = match channel.need() {
                        Need::Read => true,
                        Need::Report => d.can_report,
                        Need::NamesOnly => false,
                    };
                    unmasked && right
                }
                None => false,
            };
            if !ok {
                refused.insert(field.path.clone());
            }
        }
        if refused.is_empty() {
            Ok(())
        } else {
            Err(GuardError::field_denied(refused.into_iter().collect()))
        }
    }

    /// [`FieldGuard::check_query_fields_for`] with [`Channel::Report`]: the check for a
    /// caller that has not yet been moved to name its channel.
    ///
    /// # Errors
    ///
    /// As [`FieldGuard::check_query_fields_for`].
    pub fn check_query_fields(
        &self,
        principal: &Principal,
        doctype: &str,
        used: &[QueryField],
    ) -> Result<(), GuardError> {
        self.check_query_fields_for(Channel::Report, principal, doctype, used)
    }

    // ------------------------------------------------------------------ system actors

    /// A job, sync or migration reads a field. The actor's manifest must **declare** every
    /// protection group of the field (rule 4); an undeclared access is refused and recorded
    /// to the audit sink. A declared access to a `trace` group field is traced (via
    /// [`Via::System`]).
    ///
    /// # Errors
    ///
    /// `fieldguard.undeclared_system_access` naming the field path; the trace-queue error
    /// under `fail_closed`; or an unknown DocType.
    pub fn system_access(
        &self,
        manifest: &ActorManifest,
        doctype: &str,
        field: &str,
    ) -> Result<(), GuardError> {
        let prot = self.protection(doctype)?;
        let (_, column, owner_prot) = self
            .resolve(&prot, doctype, field)
            .ok_or_else(|| GuardError::undeclared_access(field))?;
        let facts = owner_prot.field(&column);
        let principal = Principal {
            user: manifest.actor.clone(),
            assignments: Vec::new(),
            is_system: true,
        };
        let undeclared: Vec<&GroupName> = facts
            .groups
            .iter()
            .filter(|g| !manifest.declared_groups.contains(*g))
            .collect();
        let traced = self.is_traced(&owner_prot, &facts);
        if let Some(group) = undeclared.first() {
            let _ = self
                .audit
                .offer(crate::records::AuditRecord::UndeclaredSystemAccess {
                    actor: manifest.actor.clone(),
                    at: self.clock.now_utc(),
                    doctype: doctype.to_owned(),
                    field: field.to_owned(),
                    group: group.as_str().to_owned(),
                });
            if traced {
                let _ = self.emit_trace(vec![self.record(
                    &principal,
                    doctype,
                    None,
                    field,
                    Via::System,
                    false,
                )]);
            }
            return Err(GuardError::undeclared_access(field));
        }
        if traced {
            self.emit_trace(vec![self.record(
                &principal,
                doctype,
                None,
                field,
                Via::System,
                true,
            )])?;
        }
        Ok(())
    }
}

/// What a query does with a field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryUse {
    /// In a `where`.
    Filter,
    /// In an `order by`.
    Sort,
    /// In a `group by`.
    Group,
    /// Under an aggregate function (sum, count distinct, min, max...).
    Aggregate,
}

/// One field a user query touches, with how.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryField {
    /// `field`, or `table.column` for a child-table column.
    pub path: String,
    /// How the query uses it. Every use is refused alike; it is kept for the error trail.
    pub usage: QueryUse,
}

impl QueryField {
    /// A query field.
    #[must_use]
    pub fn new(path: &str, usage: QueryUse) -> Self {
        QueryField {
            path: path.to_owned(),
            usage,
        }
    }
}

/// The columns of a request, sorted by what the channel shows of them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Columns {
    /// Columns that show their value.
    pub allowed: Vec<String>,
    /// Columns that show the masked form.
    pub masked: Vec<String>,
    /// Columns the channel hides (for a channel that refuses, the call fails instead).
    pub refused: Vec<String>,
}

/// What a job, sync or migration declared up front: the protection groups it reads. It acts
/// as the platform, so the engine would allow everything; the manifest is the check (rule 4).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActorManifest {
    /// The actor's name (a job or migration id), for the audit record.
    pub actor: String,
    /// The protection groups the actor declared.
    pub declared_groups: BTreeSet<GroupName>,
}

impl ActorManifest {
    /// A manifest declaring `groups`.
    #[must_use]
    pub fn new(actor: &str, groups: &[&str]) -> Self {
        ActorManifest {
            actor: actor.to_owned(),
            declared_groups: groups.iter().map(|g| GroupName((*g).to_owned())).collect(),
        }
    }
}

/// The document with other fields and tables, keeping identity, version and state.
fn rebuild(
    doc: &Document,
    fields: BTreeMap<Name, Value>,
    tables: BTreeMap<Name, Vec<ChildRow>>,
) -> Document {
    match doc.version() {
        Some(version) => Document::loaded(
            doc.doctype().clone(),
            doc.id(),
            version,
            doc.state(),
            fields,
            tables,
        ),
        None => {
            let mut out = Document::new(doc.doctype().clone(), doc.id());
            for (name, value) in fields {
                out.set(name, value);
            }
            for (table, rows) in tables {
                *out.rows_mut(table) = rows;
            }
            out
        }
    }
}
