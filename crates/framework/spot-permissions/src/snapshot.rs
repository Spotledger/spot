//! The world one evaluation series sees: grants, field grants, protection groups, shares and
//! the DocType permission-relevant facts, immutable and `Arc`-shareable.
//!
//! A [`Snapshot`] is built once (from `spot_meta::EffectiveMeta` plus the loaded model) and
//! then only read. A change builds a new snapshot and swaps it in (the swap is WP-IAM-03's
//! job); an operation holding the old `Arc` keeps one consistent view.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_app_api::meta::{DocTypeDef, FieldKind};
use spot_meta::EffectiveMeta;

use crate::app_permission::{
    AppIndex, AppPermissionCatalogue, AppPermissionGrant, AppPermissionName,
};
use crate::error::{PermError, PermErrors};
use crate::model::{
    FieldFacts, FieldGrant, FieldTarget, GroupName, Model, ProtectionGroup, Role, RoleGrant, Share,
};

/// A field a user permission can value-restrict: a link field (records) or a select field
/// (values).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkField {
    /// The field's storage name.
    pub name: String,
    /// The DocType the link points at (`None` for a dynamic link until the document names it;
    /// `None` on a select field, whose values are text, not records).
    pub target: Option<String>,
    /// A select field's values, when the restriction is over a pick list.
    pub select_values: Option<Vec<String>>,
}

/// The permission-relevant facts of one DocType, extracted from its `DocTypeDef`.
///
/// `hidden` is deliberately absent: hiding is a presentation matter, never security (ADR-068).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoctypeFacts {
    /// Storage name.
    pub name: String,
    /// The DocType has a draft stage and a post action (ADR-42).
    pub is_posted: bool,
    /// The fields' protection facts, in definition order. A field the metadata does not
    /// protect has no groups; [`Snapshot::with_field_facts`] supplies the real ones.
    pub fields: Vec<FieldFacts>,
    /// Every link (and table-multiselect) field, in definition order; dynamic links carry
    /// `target: None`.
    pub links: Vec<LinkField>,
}

impl DoctypeFacts {
    /// The facts of one DocType from its app view.
    #[must_use]
    pub fn of(doctype: &DocTypeDef) -> DoctypeFacts {
        let fields = doctype
            .fields
            .iter()
            .map(|f| FieldFacts::plain(f.name.as_str()))
            .collect();
        let links = doctype
            .fields
            .iter()
            .filter_map(|f| match &f.kind {
                FieldKind::Link { doctype } | FieldKind::TableMultiselect { doctype, .. } => {
                    Some(LinkField {
                        name: f.name.as_str().to_owned(),
                        target: Some(doctype.as_str().to_owned()),
                        select_values: None,
                    })
                }
                FieldKind::DynamicLink { .. } => Some(LinkField {
                    name: f.name.as_str().to_owned(),
                    target: None,
                    select_values: None,
                }),
                FieldKind::Select { values } => Some(LinkField {
                    name: f.name.as_str().to_owned(),
                    target: None,
                    select_values: Some(values.clone()),
                }),
                _ => None,
            })
            .collect();
        DoctypeFacts {
            name: doctype.name.as_str().to_owned(),
            is_posted: doctype.is_posted,
            fields,
            links,
        }
    }

    /// The facts of one field by name.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&FieldFacts> {
        self.fields.iter().find(|f| f.path == name)
    }
}

/// The immutable evaluation world.
#[derive(Clone, Debug)]
pub struct Snapshot {
    facts: BTreeMap<String, Arc<DoctypeFacts>>,
    grants: Vec<RoleGrant>,
    field_grants: Vec<FieldGrant>,
    groups: Vec<ProtectionGroup>,
    shares: Vec<Share>,
    apps: AppIndex,
}

impl Snapshot {
    /// Builds a snapshot from effective metadata and a loaded model, checking every grant
    /// against the metadata it must name.
    ///
    /// # Errors
    ///
    /// [`PermErrors`] listing every inconsistency: a grant on an unknown DocType, a field
    /// grant on an unknown field or an undeclared group.
    pub fn build(meta: &EffectiveMeta, model: &Model) -> Result<Snapshot, PermErrors> {
        Snapshot::build_with_apps(meta, model, &AppPermissionCatalogue::new(), &[])
    }

    /// As [`Snapshot::build`], with the app permissions the site's installed apps declare
    /// (`catalogue`) and the roles' grants of them. `build` means "empty catalogue": every
    /// app permission is unknown.
    ///
    /// A grant naming a permission the catalogue does not declare is **not** an error (the
    /// app may be uninstalled or not yet installed): it is kept out of the effective sets,
    /// never allows anything, and is listed by [`Snapshot::orphan_app_grants`].
    ///
    /// # Errors
    ///
    /// As [`Snapshot::build`].
    pub fn build_with_apps(
        meta: &EffectiveMeta,
        model: &Model,
        catalogue: &AppPermissionCatalogue,
        app_grants: &[AppPermissionGrant],
    ) -> Result<Snapshot, PermErrors> {
        let mut errors: Vec<PermError> = Vec::new();
        let mut facts = BTreeMap::new();
        for view in meta.doctypes() {
            facts.insert(
                view.name.as_str().to_owned(),
                Arc::new(DoctypeFacts::of(view)),
            );
        }
        for grant in &model.grants {
            if !facts.contains_key(&grant.doctype) {
                errors.push(PermError::Inconsistent {
                    at: format!("grants: {}", grant.role.as_str()),
                    detail: format!("grant names unknown doctype `{}`", grant.doctype),
                });
            }
        }
        for fg in &model.field_grants {
            let at = format!("field_grants: {}", fg.role.as_str());
            let Some(doctype) = facts.get(&fg.doctype) else {
                errors.push(PermError::Inconsistent {
                    at,
                    detail: format!("field grant names unknown doctype `{}`", fg.doctype),
                });
                continue;
            };
            match &fg.target {
                FieldTarget::Field(path) => {
                    if doctype.field(path).is_none() {
                        errors.push(PermError::Inconsistent {
                            at,
                            detail: format!("`{}` has no field `{path}`", fg.doctype),
                        });
                    }
                }
                FieldTarget::Group(group) => {
                    if !model.groups.iter().any(|g| &g.name == group) {
                        errors.push(PermError::Inconsistent {
                            at,
                            detail: format!("protection group `{}` is not declared", group.0),
                        });
                    }
                }
            }
        }
        if errors.is_empty() {
            Ok(Snapshot {
                facts,
                grants: model.grants.clone(),
                field_grants: model.field_grants.clone(),
                groups: model.groups.clone(),
                shares: model.shares.clone(),
                apps: AppIndex::new(catalogue, app_grants),
            })
        } else {
            Err(PermErrors(errors))
        }
    }

    /// A snapshot of metadata only: no grants, no groups, no shares.
    ///
    /// # Errors
    ///
    /// Same as [`Snapshot::build`]; an empty model never fails it.
    pub fn empty(meta: &EffectiveMeta) -> Result<Snapshot, PermErrors> {
        Snapshot::build(meta, &Model::default())
    }

    /// Replaces one DocType's field facts (groups, tags, masks), checking that every field
    /// exists and every group is declared.
    ///
    /// # Errors
    ///
    /// [`PermErrors`] for an unknown DocType, an unknown field, or an undeclared group.
    pub fn with_field_facts(
        mut self,
        doctype: &str,
        fields: Vec<FieldFacts>,
    ) -> Result<Snapshot, PermErrors> {
        let at = format!("field facts: {doctype}");
        let mut errors = Vec::new();
        let Some(current) = self.facts.get(doctype) else {
            return Err(PermErrors(vec![PermError::Inconsistent {
                at,
                detail: format!("unknown doctype `{doctype}`"),
            }]));
        };
        let mut merged = current.fields.clone();
        for supplied in fields {
            for group in &supplied.groups {
                if !self.groups.iter().any(|g| &g.name == group) {
                    errors.push(PermError::Inconsistent {
                        at: at.clone(),
                        detail: format!(
                            "`{}` is in protection group `{}`, which is not declared",
                            supplied.path, group.0
                        ),
                    });
                }
            }
            match merged.iter_mut().find(|f| f.path == supplied.path) {
                Some(slot) => *slot = supplied,
                None => errors.push(PermError::Inconsistent {
                    at: at.clone(),
                    detail: format!("`{doctype}` has no field `{}`", supplied.path),
                }),
            }
        }
        if !errors.is_empty() {
            return Err(PermErrors(errors));
        }
        let mut facts = (**current).clone();
        facts.fields = merged;
        self.facts.insert(doctype.to_owned(), Arc::new(facts));
        Ok(self)
    }

    /// The facts of one DocType, when the metadata knows it.
    #[must_use]
    pub fn facts(&self, doctype: &str) -> Option<Arc<DoctypeFacts>> {
        self.facts.get(doctype).cloned()
    }

    /// Every DocType grant, in load order (the loader sorts it).
    #[must_use]
    pub fn grants(&self) -> &[RoleGrant] {
        &self.grants
    }

    /// Every field grant, in load order.
    #[must_use]
    pub fn field_grants(&self) -> &[FieldGrant] {
        &self.field_grants
    }

    /// Whether a protection group is declared.
    #[must_use]
    pub fn has_group(&self, name: &GroupName) -> bool {
        self.groups.iter().any(|g| &g.name == name)
    }

    /// Whether a group asks for a field access trace.
    #[must_use]
    pub fn traces(&self, name: &GroupName) -> bool {
        self.groups.iter().any(|g| &g.name == name && g.trace)
    }

    /// Every share, in load order.
    #[must_use]
    pub fn shares(&self) -> &[Share] {
        &self.shares
    }

    /// Whether an installed app declares this permission.
    #[must_use]
    pub fn declares_app_permission(&self, name: &AppPermissionName) -> bool {
        self.apps.catalogue().contains(name)
    }

    /// Whether `role` holds the **declared** permission `name`. A grant of an undeclared
    /// name is never visible here.
    #[must_use]
    pub fn role_holds_app_permission(&self, role: &Role, name: &AppPermissionName) -> bool {
        self.apps.role_holds(role, name)
    }

    /// The app permissions the installed apps declare.
    #[must_use]
    pub fn app_catalogue(&self) -> &AppPermissionCatalogue {
        self.apps.catalogue()
    }

    /// The grants that name a permission no installed app declares, sorted by role then
    /// name. They allow nothing; the access review shows them.
    #[must_use]
    pub fn orphan_app_grants(&self) -> &[(Role, AppPermissionName)] {
        self.apps.orphans()
    }

    pub(crate) fn app_index(&self) -> &AppIndex {
        &self.apps
    }

    /// The DocType names this snapshot knows, in name order.
    pub fn doctypes(&self) -> impl Iterator<Item = &str> {
        self.facts.keys().map(String::as_str)
    }
}

/// The engine's read and link checks see one document as this plain value bag: the engine is
/// a pure evaluator over callers' structures, not over storage.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct DocumentData {
    /// Field values by name, in the caller's encoding (`Value::Id` for links).
    pub fields: BTreeMap<String, spot_app_api::prelude::Value>,
    /// Child-table rows: their own field values by table name.
    pub tables: BTreeMap<String, Vec<BTreeMap<String, spot_app_api::prelude::Value>>>,
    /// The document's own record id, when it has one (a draft in creation does not yet).
    pub id: Option<spot_app_api::prelude::RecordId>,
    /// The DocType a dynamic link names per value, by the type field's name.
    pub dynamic_targets: BTreeMap<String, String>,
}
