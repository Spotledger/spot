//! A real field guard over real metadata for the names-only projection tests: one DocType with a
//! plain field, two protected fields in two groups and a child table with a protected column.

use std::sync::Arc;

use serde_json::json;
use spot_core::ManualClock;
use spot_field_guard::{
    AuditRecord, BoundedQueue, FieldAccessRecord, FieldGuard, MetaProtection, ProtectionSource,
};
use spot_meta::{
    BuildOptions, Definition, FieldSpec, FieldType, MaskSpec, ProtectionGroupSpec, Registry,
};
use spot_permissions::{Model, ModelTexts, PermissionEngine, Principal, Snapshot, load};

pub(crate) const DOCTYPE: &str = "employee_file";
pub(crate) const CHILD: &str = "employee_file_line";

/// Planted values: none of these may ever appear in a view, an error or a debug text.
pub(crate) const SALARY: &str = "SALARY-4815162342";
pub(crate) const CARD: &str = "4111111111111111";
pub(crate) const IBAN: &str = "PK36SCBL0000001123456702";
pub(crate) const TITLE: &str = "quarterly review";

fn grouped(name: &str, group: &str) -> FieldSpec {
    let mut f = FieldSpec::new(name, FieldType::Text);
    f.protection_group = Some(group.to_owned());
    f.masking = Some(MaskSpec::placeholder());
    f
}

fn definitions() -> Vec<Definition> {
    let mut file = Definition::new(DOCTYPE);
    let mut lines = FieldSpec::new("lines", FieldType::Table);
    lines.options = Some(CHILD.into());
    file.fields = vec![
        FieldSpec::new("title", FieldType::Text),
        grouped("salary", "hr"),
        grouped("card_number", "pci"),
        lines,
    ];
    file.protection_groups = vec![
        ProtectionGroupSpec::new("hr", "Compensation"),
        ProtectionGroupSpec::new("pci", "Payment cards"),
    ];
    let mut line = Definition::new(CHILD);
    line.is_child = true;
    line.fields = vec![
        FieldSpec::new("memo", FieldType::Text),
        grouped("iban", "bank"),
    ];
    line.protection_groups = vec![ProtectionGroupSpec::new("bank", "Bank")];
    vec![file, line]
}

fn model() -> Model {
    let mut grants = String::new();
    let mut field_grants = String::new();
    for role in ["Clerk", "Masked", "Analyst"] {
        for doctype in [DOCTYPE, CHILD] {
            grants.push_str(&format!(
                "[[entries]]\nrole = \"{role}\"\ndoctype = \"{doctype}\"\nrights = [\"read\"]\n"
            ));
        }
    }
    for (role, access) in [("Masked", r#"["read_masked"]"#), ("Analyst", r#"["read"]"#)] {
        for (doctype, group) in [(DOCTYPE, "hr"), (DOCTYPE, "pci"), (CHILD, "bank")] {
            field_grants.push_str(&format!(
                "[[entries]]\nrole = \"{role}\"\ndoctype = \"{doctype}\"\ntarget = {{ group = \"{group}\" }}\naccess = {access}\ndeny = false\n"
            ));
        }
    }
    load(&ModelTexts {
        grants,
        field_grants,
        groups: "[[entries]]\nname = \"hr\"\n[[entries]]\nname = \"pci\"\n[[entries]]\nname = \"bank\"\n"
            .to_owned(),
        shares: String::new(),
    })
    .unwrap_or_else(|e| panic!("fixture model: {:?}", e.as_slice()))
}

/// The guard and the protection facts it answers from.
pub(crate) struct World {
    pub(crate) guard: FieldGuard,
    pub(crate) source: Arc<MetaProtection>,
}

pub(crate) fn world() -> World {
    let meta = Registry::with_options(definitions(), BuildOptions::new())
        .unwrap_or_else(|e| panic!("fixture meta: {e:?}"))
        .effective();
    let source = Arc::new(MetaProtection::new(&meta));
    let mut snapshot =
        Snapshot::build(&meta, &model()).unwrap_or_else(|e| panic!("{:?}", e.as_slice()));
    for doctype in [DOCTYPE, CHILD] {
        let prot = source.protection(doctype).expect("doctype facts");
        snapshot = snapshot
            .with_field_facts(doctype, prot.fields.clone())
            .unwrap_or_else(|e| panic!("{:?}", e.as_slice()));
    }
    let guard = FieldGuard::new(
        Arc::new(PermissionEngine::new(Arc::new(snapshot))),
        source.clone(),
        Arc::new(BoundedQueue::<FieldAccessRecord>::new(64)),
        Arc::new(BoundedQueue::<AuditRecord>::new(64)),
        Arc::new(ManualClock::new(
            spot_app_api::prelude::OffsetDateTime::UNIX_EPOCH,
        )),
    );
    World { guard, source }
}

pub(crate) fn clerk() -> Principal {
    Principal::with_roles("clerk", &["Clerk"])
}

pub(crate) fn masked() -> Principal {
    Principal::with_roles("masked", &["Masked"])
}

pub(crate) fn analyst() -> Principal {
    Principal::with_roles("analyst", &["Analyst"])
}

/// One `doc_state` envelope line of `entity` with every planted value in the image.
pub(crate) fn state_line(entity: &str, seq: u64, op: &str) -> Vec<u8> {
    let image = json!({
        "id": "0192f1c0-0000-7000-8000-000000000001",
        "title": TITLE,
        "salary": SALARY,
        "card_number": CARD,
        "lines": [
            { "memo": "first", "iban": IBAN },
            { "memo": "second", "iban": IBAN }
        ],
    });
    let envelope = json!({
        "event_id": "0192f1c0-0000-7000-8000-0000000000e1",
        "topic": "doc_state",
        "schema_version": 1,
        "source_id": "acme-site",
        "position": { "generation": 1, "seq": seq },
        "transaction": "0192f1c0-0000-7000-8000-0000000000f1",
        "ordinal": 0,
        "subject": { "entity": entity, "id": "0192f1c0-0000-7000-8000-000000000001" },
        "subject_version": 3,
        "meta": {
            "occurred_at": "2026-11-02T10:00:00Z",
            "actor": { "kind": "system", "component": "capture" },
            "correlation_id": "0192f1c0-0000-7000-8000-0000000000e1"
        },
        "data": { "op": op, "entity_schema": 1, "image": image }
    });
    envelope.to_string().into_bytes()
}
