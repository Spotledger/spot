//! The permission view the native API services consult (WP-UI-01.T1).
//!
//! Both operations this crate's form-event and permission services serve are pure functions of
//! (draft, metadata, principal). [`AccessControl`] is that function's permission half: the
//! decision core's snapshot ([`PermissionEngine`]), the source that turns an [`Actor`] into a
//! [`Principal`], the effective metadata, the permission snapshot version, and the clock and
//! actor resolver the adapter shares with [`EngineResources`](crate::EngineResources).
//!
//! Nothing here stores a decision or caches one across requests. The host swaps the engine when
//! the permission snapshot changes; the next request reads the new one.
//!
//! # Optimistic summaries
//!
//! [`AccessControl::doctype_permissions`] answers the `auth_permissions` projection. Grants that
//! depend on one document (owner, scope, condition) are summarised **optimistically**, as the
//! brief requires: the question is "could any assignment grant this?", so the facts it asks
//! with have the principal as the document's owner, no value restrictions in play (an empty
//! value bag cannot block a scope), and the lifecycle state each action needs. The server stays
//! the authority on the document itself; the summary is a UI hint, never a decision.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_app_api::Document;
use spot_app_api::context::{Actor, Invocation};
use spot_app_api::meta::{DocTypeDef, FieldKind};
use spot_app_api::prelude::Clock;
use spot_app_api::prelude::Value;
use spot_events_contract::lifecycle::Operation;
use spot_http::api::permissions::{DoctypePermissions, FieldSummary, Rights};
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, RequestCtx};
use spot_meta::{EffectiveMeta, FieldProtection};
use spot_permissions::{
    Action, DocFacts, FieldFacts, FieldLevel, GroupName, MaskFormat, Model, PermErrors, Principal,
    Snapshot, Tag,
};

use crate::engine_resources::ActorResolver;
use crate::util::build_invocation;

pub use spot_document::engine::PrincipalSource;

/// The permission view shared by the native API services.
pub struct AccessControl {
    meta: Arc<EffectiveMeta>,
    permissions: Arc<spot_permissions::PermissionEngine>,
    principals: Arc<dyn PrincipalSource>,
    version: u64,
    clock: Arc<dyn Clock>,
    actor: Arc<ActorResolver>,
}

impl AccessControl {
    /// The view over one permission snapshot.
    ///
    /// `version` is the permission snapshot's version (the `etag` of the summary); `clock` and
    /// `actor` are the same values [`EngineResources`](crate::EngineResources) was built with,
    /// so both adapters build the same invocation for one request.
    pub fn new(
        meta: Arc<EffectiveMeta>,
        permissions: Arc<spot_permissions::PermissionEngine>,
        principals: Arc<dyn PrincipalSource>,
        version: u64,
        clock: Arc<dyn Clock>,
        actor: Arc<ActorResolver>,
    ) -> Self {
        Self {
            meta,
            permissions,
            principals,
            version,
            clock,
            actor,
        }
    }

    /// The effective metadata this view decides over.
    pub fn meta(&self) -> &Arc<EffectiveMeta> {
        &self.meta
    }

    /// The permission snapshot version (the summary's `etag`).
    pub fn version(&self) -> u64 {
        self.version
    }

    /// The clock the adapter reads time through.
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// The decision core in force.
    pub fn engine(&self) -> &Arc<spot_permissions::PermissionEngine> {
        &self.permissions
    }

    /// The DocType's effective definition, or `meta.unknown_doctype`.
    pub fn doctype(&self, doctype: &str) -> Result<Arc<DocTypeDef>, ApiError> {
        self.meta.doctype(doctype).ok_or_else(|| {
            ApiError::new(
                Code::MetaUnknownDoctype,
                format!("The DocType `{doctype}` does not exist."),
            )
        })
    }

    /// The invocation for one request: the resolver decides who is acting.
    pub fn invocation(&self, ctx: &RequestCtx) -> Result<Invocation, ApiError> {
        build_invocation(ctx, self.clock.as_ref(), (self.actor)(ctx)?)
    }

    /// The principal acting on `inv` (no check, no error; unknown users have no rights).
    pub async fn principal(&self, inv: &Invocation) -> Principal {
        match &inv.actor {
            Actor::System { .. } => Principal::system(),
            Actor::User { id, .. } => self.principals.resolve(id).await,
            // A consumer is no `User` and holds no role (ADR-083 section 2.2): it resolves to a
            // principal with no assignments, whose rights are the surface grants and owner scope.
            Actor::Consumer { .. } => Principal::user(&inv.actor.principal_text(), Vec::new()),
        }
    }

    /// Whether `action` is allowed on a document with these facts.
    pub fn can(&self, principal: &Principal, action: Action, facts: &DocFacts) -> bool {
        self.permissions.can(principal, action, facts).allowed
    }

    /// How visible `field` of `doctype` is to `principal` on a document with these facts.
    pub fn field_level(
        &self,
        principal: &Principal,
        doctype: &str,
        field: &str,
        facts: &DocFacts,
    ) -> FieldLevel {
        self.field_decision(principal, doctype, field, facts).level
    }

    /// The decision core's answer for one field.
    pub fn field_decision(
        &self,
        principal: &Principal,
        doctype: &str,
        field: &str,
        facts: &DocFacts,
    ) -> spot_permissions::FieldDecision {
        self.permissions
            .field_access(principal, doctype, &self.field_facts(doctype, field), facts)
    }

    /// The protection facts of one field, `plain` when the metadata does not protect it.
    pub fn field_facts(&self, doctype: &str, field: &str) -> FieldFacts {
        self.permissions
            .snapshot()
            .facts(doctype)
            .as_deref()
            .and_then(|facts| facts.field(field))
            .cloned()
            .unwrap_or_else(|| FieldFacts::plain(field))
    }

    /// The `auth_permissions` entry for one DocType, summarised optimistically.
    pub fn doctype_permissions(
        &self,
        principal: &Principal,
        def: &DocTypeDef,
    ) -> DoctypePermissions {
        let name = def.name.as_str();
        let read = self.allowed(principal, Action::Read, def);
        let create = self.allowed(principal, Action::Op(Operation::Create), def);
        let write = self.allowed(principal, Action::Op(Operation::Update), def);
        let delete = self.allowed(principal, Action::Op(Operation::Delete), def);
        // A DocType without a draft stage has no submit, cancel or correct to speak of.
        let submit = def.is_posted && self.allowed(principal, Action::Op(Operation::Post), def);
        let cancel = def.is_posted && self.allowed(principal, Action::Op(Operation::Cancel), def);
        let correct = def.is_posted && self.allowed(principal, Action::Correct, def);
        let rename = self.allowed(principal, Action::Op(Operation::Rename), def);

        let facts = self.optimistic_facts(def, principal, Action::Op(Operation::Update));
        let mut fields = FieldSummary::default();
        for field in &def.fields {
            let access = self.field_decision(principal, name, field.name.as_str(), &facts);
            let path = field.name.as_str().to_owned();
            match access.level {
                FieldLevel::Hidden => fields.hidden.push(path),
                FieldLevel::Masked(_) => {
                    fields.masked.push(path.clone());
                    if access.can_reveal {
                        fields.revealable.push(path);
                    }
                }
                FieldLevel::Read | FieldLevel::ReadReveal => {
                    if !access.can_update || field.read_only {
                        fields.read_only.push(path);
                    }
                }
            }
        }

        DoctypePermissions {
            doctype: def.name.clone(),
            rights: Rights {
                read,
                create,
                write,
                delete,
                submit,
                cancel,
                correct,
                rename,
            },
            fields,
            attach_limits: BTreeMap::new(),
        }
    }

    /// Whether `principal` holds any right at all on `def`.
    pub fn has_any_right(&self, principal: &Principal, def: &DocTypeDef) -> bool {
        let rights = self.doctype_permissions(principal, def).rights;
        rights.read
            || rights.create
            || rights.write
            || rights.delete
            || rights.submit
            || rights.cancel
            || rights.correct
            || rights.rename
    }

    /// The optimistic document facts for one action: the principal owns the document, no value
    /// restrictions block it, and the lifecycle state is the one the action needs.
    pub fn optimistic_facts(
        &self,
        def: &DocTypeDef,
        principal: &Principal,
        action: Action,
    ) -> DocFacts {
        let state = if def.is_posted {
            let state = match action {
                Action::Op(Operation::Cancel)
                | Action::Op(Operation::UpdatePosted)
                | Action::Correct => "posted",
                _ => "draft",
            };
            Some(state.to_owned())
        } else {
            None
        };
        DocFacts {
            doctype: def.name.as_str().to_owned(),
            id: None,
            created_by: (!principal.is_system).then(|| principal.user.clone()),
            state,
            values: BTreeMap::new(),
        }
    }

    fn allowed(&self, principal: &Principal, action: Action, def: &DocTypeDef) -> bool {
        self.can(
            principal,
            action,
            &self.optimistic_facts(def, principal, action),
        )
    }
}

/// The facts of a draft or loaded document, as the decision core reads them. `id` is present
/// only when the draft came from a stored document.
pub fn doc_facts(doc: &Document, id: Option<&str>) -> DocFacts {
    DocFacts {
        doctype: doc.doctype().as_str().to_owned(),
        id: id.map(str::to_owned),
        created_by: doc.fields().iter().find_map(|(name, value)| {
            if name.as_str() != "created_by" {
                return None;
            }
            match value {
                Value::Text(text) => Some(text.clone()),
                _ => None,
            }
        }),
        state: doc.state().map(|s| s.as_str().to_owned()),
        values: doc
            .fields()
            .iter()
            .map(|(name, value)| (name.as_str().to_owned(), value.clone()))
            .collect(),
    }
}

/// The text form of a masked value, or [`Value::Null`] when the value has no text form. The
/// same choice the engine's read filter makes: a non-text value is dropped rather than leaking
/// its shape.
pub fn masked(value: &Value, format: MaskFormat) -> Value {
    match value {
        Value::Text(text) => Value::Text(spot_permissions::mask(text, &format)),
        _ => Value::Null,
    }
}

/// Build a permission engine whose snapshot carries the metadata's protection facts (groups,
/// tags, masks) — the mapping every host needs to evaluate field access from `EffectiveMeta`.
///
/// # Errors
///
/// The decision core's [`PermErrors`] when the model names an unknown DocType, field or group.
pub fn engine_for(
    meta: &EffectiveMeta,
    model: &Model,
) -> Result<spot_permissions::PermissionEngine, PermErrors> {
    let mut snapshot = Snapshot::build(meta, model)?;
    for view in meta.doctypes() {
        let name = view.name.as_str();
        let prefix = format!("{name}.");
        let facts: Vec<FieldFacts> = meta
            .field_protection()
            .iter()
            .filter_map(|p| protection_facts(p, &prefix))
            .collect();
        snapshot = snapshot.with_field_facts(name, facts)?;
    }
    Ok(spot_permissions::PermissionEngine::new(Arc::new(snapshot)))
}

fn protection_facts(protection: &FieldProtection, prefix: &str) -> Option<FieldFacts> {
    let path = protection.path.strip_prefix(prefix)?;
    Some(FieldFacts {
        path: path.to_owned(),
        groups: protection.groups.iter().cloned().map(GroupName).collect(),
        tags: protection.tags.iter().cloned().map(Tag).collect(),
        mask: protection.mask.as_deref().map(parse_mask),
    })
}

/// `placeholder` or `last_chars(n)`, the text `EffectiveMeta::field_protection` writes.
fn parse_mask(text: &str) -> MaskFormat {
    text.strip_prefix("last_chars(")
        .and_then(|rest| rest.strip_suffix(')'))
        .and_then(|n| n.parse::<u8>().ok())
        .map_or(MaskFormat::Placeholder, MaskFormat::LastChars)
}

/// The child DocType of a `table` field, when the field is a table.
pub fn child_doctype(def: &DocTypeDef, table: &str) -> Option<String> {
    def.fields.iter().find_map(|field| {
        if field.name.as_str() != table {
            return None;
        }
        match &field.kind {
            FieldKind::Table { doctype } | FieldKind::TableMultiselect { doctype, .. } => {
                Some(doctype.as_str().to_owned())
            }
            _ => None,
        }
    })
}
