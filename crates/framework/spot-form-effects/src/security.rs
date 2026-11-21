//! The three authorities an effect is judged by: the form's metadata, the triggering user's
//! field rights, and the form's registered dialogs.
//!
//! A form script runs with the triggering user's rights, never more. The runner asks these
//! traits, so the host wires them to the same effective metadata and permission snapshot the
//! form-event endpoint already uses; tests and the conformance corpus use the small
//! [`StaticSecurity`] and [`MapMeta`] implementations below.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spot_app_api::meta::DocTypeDef;
use spot_permissions::{DocFacts, FieldFacts, FieldLevel, PermissionEngine, Principal, Snapshot};

/// The form's effective metadata, as the runner needs it: a DocType by storage name, child
/// DocTypes included.
pub trait FormMeta: Send + Sync {
    /// The definition of `doctype`, if the site has one.
    fn doctype(&self, doctype: &str) -> Option<Arc<DocTypeDef>>;
}

/// A metadata map: the corpus's and tests' implementation, and a host's if it already holds one.
#[derive(Clone, Debug, Default)]
pub struct MapMeta {
    doctypes: BTreeMap<String, Arc<DocTypeDef>>,
}

impl MapMeta {
    /// An empty map.
    #[must_use]
    pub fn new() -> MapMeta {
        MapMeta::default()
    }

    /// Adds `def` under its own name.
    pub fn insert(&mut self, def: DocTypeDef) {
        self.doctypes
            .insert(def.name.as_str().to_owned(), Arc::new(def));
    }

    /// Adds an already-shared definition.
    pub fn insert_shared(&mut self, def: Arc<DocTypeDef>) {
        self.doctypes.insert(def.name.as_str().to_owned(), def);
    }
}

impl FormMeta for MapMeta {
    fn doctype(&self, doctype: &str) -> Option<Arc<DocTypeDef>> {
        self.doctypes.get(doctype).cloned()
    }
}

/// What the triggering user may do with one field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldAccess {
    /// The field is visible (not hidden).
    pub visible: bool,
    /// The user may change the field in this operation.
    pub writable: bool,
}

impl FieldAccess {
    /// A visible, writable field.
    #[must_use]
    pub fn open() -> FieldAccess {
        FieldAccess {
            visible: true,
            writable: true,
        }
    }

    /// A visible, read-only field.
    #[must_use]
    pub fn read_only() -> FieldAccess {
        FieldAccess {
            visible: true,
            writable: false,
        }
    }

    /// A field the user cannot see.
    #[must_use]
    pub fn hidden() -> FieldAccess {
        FieldAccess {
            visible: false,
            writable: false,
        }
    }
}

/// The triggering user's field rights, as the permission decision core answers them.
pub trait FormSecurity: Send + Sync {
    /// How `principal` sees and writes `field` of `doctype` on a document with `facts`.
    fn access(
        &self,
        doctype: &str,
        field: &str,
        principal: &Principal,
        facts: &DocFacts,
    ) -> FieldAccess;
}

/// The production implementation over the permission decision core.
///
/// It asks exactly what the API's `AccessControl` asks: an unprotected field is
/// [`FieldFacts::plain`], a protected one comes from the snapshot, and the write flag is the
/// union of `can_create` and `can_update` (a form event does not know yet whether the draft is a
/// create or an update; either right is enough for an effect to be applied, and the save itself
/// still decides).
pub struct PermissionSecurity {
    engine: Arc<PermissionEngine>,
}

impl PermissionSecurity {
    /// A view over `engine`.
    #[must_use]
    pub fn new(engine: Arc<PermissionEngine>) -> PermissionSecurity {
        PermissionSecurity { engine }
    }

    fn facts_for(&self, doctype: &str, field: &str) -> FieldFacts {
        field_facts(self.engine.snapshot().as_ref(), doctype, field)
    }
}

/// The protection facts of one field, `plain` when the snapshot has none.
#[must_use]
pub fn field_facts(snapshot: &Snapshot, doctype: &str, field: &str) -> FieldFacts {
    snapshot
        .facts(doctype)
        .as_deref()
        .and_then(|facts| facts.field(field))
        .cloned()
        .unwrap_or_else(|| FieldFacts::plain(field))
}

impl FormSecurity for PermissionSecurity {
    fn access(
        &self,
        doctype: &str,
        field: &str,
        principal: &Principal,
        facts: &DocFacts,
    ) -> FieldAccess {
        let decision =
            self.engine
                .field_access(principal, doctype, &self.facts_for(doctype, field), facts);
        FieldAccess {
            visible: decision.level != FieldLevel::Hidden,
            writable: decision.can_create || decision.can_update,
        }
    }
}

/// A fixed table of hidden and read-only fields: the conformance corpus's implementation.
///
/// A key is either `field` (any DocType) or `doctype.field` (exactly that DocType). Everything
/// else is visible and writable.
#[derive(Clone, Debug, Default)]
pub struct StaticSecurity {
    hidden: BTreeSet<String>,
    read_only: BTreeSet<String>,
}

impl StaticSecurity {
    /// Nothing hidden, nothing read-only.
    #[must_use]
    pub fn new() -> StaticSecurity {
        StaticSecurity::default()
    }

    /// Hides `field`.
    pub fn hide(&mut self, field: &str) {
        self.hidden.insert(field.to_owned());
    }

    /// Makes `field` read-only.
    pub fn read_only(&mut self, field: &str) {
        self.read_only.insert(field.to_owned());
    }

    /// Hides every name in `fields`.
    pub fn hide_all(&mut self, fields: impl IntoIterator<Item = String>) {
        self.hidden.extend(fields);
    }

    /// Makes every name in `fields` read-only.
    pub fn read_only_all(&mut self, fields: impl IntoIterator<Item = String>) {
        self.read_only.extend(fields);
    }

    fn names(&self, set: &BTreeSet<String>, doctype: &str, field: &str) -> bool {
        set.contains(field) || set.contains(&format!("{doctype}.{field}"))
    }
}

impl FormSecurity for StaticSecurity {
    fn access(
        &self,
        doctype: &str,
        field: &str,
        _principal: &Principal,
        _facts: &DocFacts,
    ) -> FieldAccess {
        if self.names(&self.hidden, doctype, field) {
            return FieldAccess::hidden();
        }
        if self.names(&self.read_only, doctype, field) {
            return FieldAccess::read_only();
        }
        FieldAccess::open()
    }
}

/// The dialogs a form may open. `open_dialog` names one of these; anything else is dropped.
pub trait DialogRegistry: Send + Sync {
    /// Whether `dialog_id` is registered for the form being served.
    fn is_registered(&self, dialog_id: &str) -> bool;
}

/// A fixed set of dialog ids: the corpus's and a host's manifest-derived registry.
#[derive(Clone, Debug, Default)]
pub struct StaticDialogs {
    ids: BTreeSet<String>,
}

impl StaticDialogs {
    /// A registry holding `ids`.
    #[must_use]
    pub fn new(ids: impl IntoIterator<Item = String>) -> StaticDialogs {
        StaticDialogs {
            ids: ids.into_iter().collect(),
        }
    }

    /// A registry with no dialogs: every `open_dialog` is dropped.
    #[must_use]
    pub fn none() -> StaticDialogs {
        StaticDialogs::default()
    }

    /// Adds one dialog id.
    pub fn insert(&mut self, id: &str) {
        self.ids.insert(id.to_owned());
    }
}

impl DialogRegistry for StaticDialogs {
    fn is_registered(&self, dialog_id: &str) -> bool {
        self.ids.contains(dialog_id)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use spot_permissions::DocFacts;

    #[test]
    fn the_static_security_reads_bare_and_qualified_names() {
        let mut security = StaticSecurity::new();
        security.hide("secret");
        security.read_only("order.total");
        let principal = Principal::system();
        let facts = DocFacts::default();
        assert_eq!(
            security.access("order", "secret", &principal, &facts),
            FieldAccess::hidden()
        );
        assert_eq!(
            security.access("order", "total", &principal, &facts),
            FieldAccess::read_only()
        );
        assert_eq!(
            security.access("invoice", "total", &principal, &facts),
            FieldAccess::open()
        );
    }

    #[test]
    fn a_dialog_registry_only_knows_what_it_registered() {
        let dialogs = StaticDialogs::new(["pick_customer".to_owned()]);
        assert!(dialogs.is_registered("pick_customer"));
        assert!(!dialogs.is_registered("pick_item"));
    }
}
