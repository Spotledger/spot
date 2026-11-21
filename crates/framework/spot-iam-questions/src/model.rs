//! The read model the four questions evaluate: roles, duties, assignments, rules, field
//! grants, share records and history (ADR-070 section 8, rule 5).
//!
//! [`AccessModel`] is a trait, not a struct: a question never loads storage itself. The host
//! (the identity service) implements it over the tenant's metadata and audit tables; the
//! tests implement it over a seeded matrix. Everything the questions read is here, so a
//! question is reproducible from the model and the instant alone.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use spot_iam_governance::{ChangeEvent, ChangeKind};
use spot_iam_roles::{Assignment, Registry};
use spot_meta::EffectiveMeta;
use spot_permissions::{FieldFacts, FieldGrant, ProtectionGroup, Share, ShareRight};
use spot_sod::RuleSet;
use spot_storage_contract::{RecordId, Value};

/// One document as the questions see it: the facts a permission decision reads, never the
/// whole record.
///
/// It carries the fields `spot-permissions` consults — the creator (owner rules), the state
/// (lifecycle rules) and the link/select values (scope restrictions) — and nothing else. A
/// question's answer therefore never echoes a document value (section 4: results never
/// include secret material).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentRecord {
    /// The DocType's storage name.
    pub doctype: String,
    /// The document's human name, as `who_can_see` is asked about it.
    pub name: String,
    /// The record id, when it has one; the share records and the query filter match on it.
    pub id: Option<RecordId>,
    /// Who created it, by user name.
    pub created_by: Option<String>,
    /// Its lifecycle state (`draft`, `posted`, `cancelled`), when it has one.
    pub state: Option<String>,
    /// The link and select values the scope restrictions read, by field name.
    pub values: BTreeMap<String, Value>,
}

impl DocumentRecord {
    /// A document with no id, state or values: the record a `can` question is asked about.
    #[must_use]
    pub fn named(doctype: &str, name: &str) -> DocumentRecord {
        DocumentRecord {
            doctype: doctype.to_owned(),
            name: name.to_owned(),
            id: None,
            created_by: None,
            state: None,
            values: BTreeMap::new(),
        }
    }

    /// The same document created by `user`.
    #[must_use]
    pub fn created_by(mut self, user: &str) -> DocumentRecord {
        self.created_by = Some(user.to_owned());
        self
    }

    /// The same document, given the record id `id`.
    #[must_use]
    pub fn with_id(mut self, id: RecordId) -> DocumentRecord {
        self.id = Some(id);
        self
    }

    /// The same document, with one link or select value set.
    #[must_use]
    pub fn with_value(mut self, field: &str, value: Value) -> DocumentRecord {
        self.values.insert(field.to_owned(), value);
        self
    }

    /// Sets the record id in place.
    pub fn set_id(&mut self, id: RecordId) {
        self.id = Some(id);
    }

    /// The document's facts for the decision core.
    #[must_use]
    pub fn facts(&self) -> spot_permissions::DocFacts {
        spot_permissions::DocFacts {
            doctype: self.doctype.clone(),
            id: self.id.map(|id| id.to_string()),
            created_by: self.created_by.clone(),
            state: self.state.clone(),
            values: self.values.clone(),
        }
    }
}

/// One share: rights on one document, granted to one user. The runtime layer's `Share` record
/// reduced to what a permission decision reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareRecord {
    /// The shared document's record id.
    pub doc: RecordId,
    /// The user who holds the rights.
    pub user: String,
    /// The rights granted, in canonical order and non-empty.
    pub rights: Vec<ShareRight>,
}

impl ShareRecord {
    /// A read share on `doc` for `user`: what `who_can_see` adds as its first reason.
    #[must_use]
    pub fn read(doc: RecordId, user: &str) -> ShareRecord {
        ShareRecord {
            doc,
            user: user.to_owned(),
            rights: vec![ShareRight::Read],
        }
    }

    /// The engine's share record.
    #[must_use]
    pub fn to_share(&self) -> Share {
        Share {
            doc: self.doc,
            user: self.user.clone(),
            rights: self.rights.clone(),
        }
    }
}

/// One change of the read model's history, flattened to what a "what changed between two
/// dates" answer needs. The before/after snapshots are deliberately dropped: they may name
/// any record's fields, and the answer must never carry a value a permission hides.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Change {
    /// When the change happened, in integer epoch seconds.
    pub at: i64,
    /// `role`, `duty` or `grant`.
    pub kind: String,
    /// `created`, `updated`, `deleted`, `copied`, `upgraded` or `ended`.
    pub op: String,
    /// The changed record's name or assignment id.
    pub subject: String,
    /// The user the change is about, for a grant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// The role involved, for a role or grant change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The assignment source, for a grant change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Who caused the change: a user id, or `system:<component>`.
    pub actor: String,
    /// The real person, when the actor impersonated one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// Why, when a reason was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Why a grant ended, when it ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
}

impl Change {
    /// Flattens one audit event into an answer row.
    #[must_use]
    pub fn of(event: &ChangeEvent) -> Change {
        let (actor, via) = match &event.actor {
            spot_iam_governance::ChangeActor::User { id, via } => (id.clone(), via.clone()),
            spot_iam_governance::ChangeActor::System { component } => {
                (format!("system:{component}"), None)
            }
        };
        Change {
            at: event.occurred_at,
            kind: match event.kind {
                ChangeKind::Role => "role",
                ChangeKind::Duty => "duty",
                ChangeKind::Grant => "grant",
            }
            .to_owned(),
            op: event.op.token().to_owned(),
            subject: event.subject.clone(),
            user: event.user.clone(),
            role: event.role.clone(),
            source: event.source.map(|s| source_token(s).to_owned()),
            actor,
            via,
            reason: event.reason.clone(),
            cause: event.cause.clone(),
        }
    }
}

fn source_token(source: spot_iam_roles::Source) -> &'static str {
    match source {
        spot_iam_roles::Source::Manual => "manual",
        spot_iam_roles::Source::Rule => "rule",
        spot_iam_roles::Source::Idp => "idp",
        spot_iam_roles::Source::Emergency => "emergency",
    }
}

/// The read model the four questions evaluate.
///
/// Every method is a pure read. A host owns the snapshot and its consistency: the model
/// version it holds is the one every question of one call is answered against (the swap is
/// the identity service's job, exactly as for the permission engine).
pub trait AccessModel {
    /// The tenant's roles, duties, their grants and their inheritance.
    fn registry(&self) -> &Registry;
    /// Every assignment, active or not, all users. The questions filter by the instant.
    fn assignments(&self) -> &[Assignment];
    /// The ids of the users whose type is portal (ADR-083 Barrier 3).
    ///
    /// **Required, with no default:** an implementor that forgot it would silently protect
    /// nobody, and the questions would show a portal user holding desk rights that the
    /// permission engine refuses. [`crate::build`] applies the portal rule to these users.
    fn portal_users(&self) -> &[String];
    /// Field grants not attached to a role (tenant-wide grants), when the tenant has any.
    fn field_grants(&self) -> &[FieldGrant] {
        &[]
    }
    /// The protection groups the field grants and field facts name.
    fn groups(&self) -> &[ProtectionGroup] {
        &[]
    }
    /// The per-document share records.
    fn shares(&self) -> &[ShareRecord] {
        &[]
    }
    /// The SoD rules the model was built with.
    fn rules(&self) -> &RuleSet;
    /// The permission-change audit history the "what changed" question reads.
    fn history(&self) -> &[ChangeEvent];
    /// The effective metadata (the DocTypes and their fields) the engine snapshot is built
    /// from.
    fn meta(&self) -> &EffectiveMeta;
    /// The field protection facts of one DocType, when they are richer than the plain
    /// metadata (groups, masks, derived-field inheritance).
    fn field_facts(&self, doctype: &str) -> Vec<FieldFacts> {
        let _ = doctype;
        Vec::new()
    }
    /// Every known DocType's name, in a deterministic order.
    fn doctypes(&self) -> Vec<String> {
        self.meta()
            .doctypes()
            .map(|d| d.name.as_str().to_owned())
            .collect()
    }
    /// One document by its human name, for `who_can_see`.
    fn document(&self, doctype: &str, name: &str) -> Option<&DocumentRecord> {
        let _ = (doctype, name);
        None
    }
    /// Every document the model holds: what `who_can_see` walks when it answers "who else
    /// could see this one".
    fn documents(&self) -> &[DocumentRecord] {
        &[]
    }
}
