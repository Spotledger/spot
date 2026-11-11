//! The authz suite's world: a small DocType set, a permission model, the principals the matrix
//! names, and the decision core they are judged against (WP-PLT-03.T2).
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_app_api::context::{Actor, Invocation};
use spot_app_api::prelude::{CommandId, Name, OffsetDateTime, RecordId, SiteId, Value};
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, Registry};
use spot_permissions::{
    AppliesTo, AssignmentContext, FieldFacts, GroupName, MaskFormat, Model, ModelTexts,
    PermissionEngine, Principal, Role, Snapshot, TENANT_ADMINISTRATOR, UserPermission, load,
};

/// The DocTypes the matrix covers.
pub const DOCTYPES: [&str; 3] = ["customer", "sales_invoice", "note"];

/// The operations the matrix covers.
pub const OPERATIONS: [&str; 6] = ["read", "create", "update", "delete", "post", "cancel"];

/// A value the scoped principal may use (it is never the document's region).
pub const ALLOWED_REGION: &str = "01890000-0000-7000-8000-000000000001";
/// The region every matrix document holds: outside the scoped principal's allowed list.
pub const OTHER_REGION: &str = "01890000-0000-7000-8000-000000000002";

/// The DocType definitions: a customer, a posted invoice with an item table, a note and the
/// region they link to. Protection comes from the field facts, not the metadata.
pub fn definitions() -> Vec<Definition> {
    let mut region = Definition::new("region");
    region.fields = vec![FieldSpec::new("name", FieldType::Text)];

    let mut customer = Definition::new("customer");
    let mut region_link = FieldSpec::new("region", FieldType::Link);
    region_link.options = Some("region".into());
    let mut salary = FieldSpec::new("salary", FieldType::Decimal);
    salary.scale = Some(2);
    customer.fields = vec![
        FieldSpec::new("name", FieldType::Text),
        region_link,
        FieldSpec::new("secret_note", FieldType::LongText),
        salary,
    ];

    let mut invoice = Definition::new("sales_invoice");
    invoice.is_posted = true;
    let mut customer_link = FieldSpec::new("customer", FieldType::Link);
    customer_link.options = Some("customer".into());
    let mut region_link = FieldSpec::new("region", FieldType::Link);
    region_link.options = Some("region".into());
    let mut total = FieldSpec::new("total", FieldType::Decimal);
    total.scale = Some(2);
    let mut items = FieldSpec::new("items", FieldType::Table);
    items.options = Some("sales_invoice_item".into());
    invoice.fields = vec![
        FieldSpec::new("number", FieldType::Text),
        customer_link,
        region_link,
        total,
        items,
        FieldSpec::new("secret_note", FieldType::LongText),
    ];

    let mut item = Definition::new("sales_invoice_item");
    item.is_child = true;
    let mut qty = FieldSpec::new("qty", FieldType::Decimal);
    qty.scale = Some(3);
    let mut cost = FieldSpec::new("cost", FieldType::Decimal);
    cost.scale = Some(2);
    item.fields = vec![FieldSpec::new("item", FieldType::Text), qty, cost];

    let mut note = Definition::new("note");
    let mut region_link = FieldSpec::new("region", FieldType::Link);
    region_link.options = Some("region".into());
    note.fields = vec![
        FieldSpec::new("text", FieldType::Text),
        region_link,
        FieldSpec::new("secret_note", FieldType::LongText),
    ];

    vec![region, customer, invoice, item, note]
}

/// The effective metadata.
pub fn meta() -> Arc<EffectiveMeta> {
    Registry::new(definitions()).unwrap().effective()
}

/// The permission model: three ordinary roles (`editor`, `reader`, `owner_reader`), one writer
/// (`writer`), the two protection groups and the field grants over them.
pub fn model_texts() -> ModelTexts {
    ModelTexts {
        grants: GRANTS.to_owned(),
        field_grants: FIELD_GRANTS.to_owned(),
        groups: GROUPS.to_owned(),
        shares: String::new(),
    }
}

const GRANTS: &str = r#"
# A link is only valid to a record the writer may read (a forbidden target reads as a missing
# one, WP-DOC-06.T2), so the roles that write documents linking to `region` may read it.
[[entries]]
doctype = "region"
role = "editor"
rights = ["read"]

[[entries]]
doctype = "region"
role = "writer"
rights = ["read"]

[[entries]]
doctype = "customer"
role = "editor"
rights = ["read", "write", "create", "delete", "submit", "cancel", "correct"]

[[entries]]
doctype = "sales_invoice"
role = "editor"
rights = ["read", "write", "create", "delete", "submit", "cancel", "correct"]

[[entries]]
doctype = "note"
role = "editor"
rights = ["read", "write", "create", "delete", "submit", "cancel", "correct"]

[[entries]]
doctype = "customer"
role = "writer"
rights = ["read", "write", "create"]

[[entries]]
doctype = "sales_invoice"
role = "writer"
rights = ["read", "write", "create"]

[[entries]]
doctype = "note"
role = "writer"
rights = ["read", "write", "create"]

[[entries]]
doctype = "customer"
role = "reader"
rights = ["read"]

[[entries]]
doctype = "sales_invoice"
role = "reader"
rights = ["read"]

[[entries]]
doctype = "note"
role = "reader"
rights = ["read"]

[[entries]]
doctype = "customer"
role = "owner_reader"
rights = ["read"]
if_owner = true

[[entries]]
doctype = "sales_invoice"
role = "owner_reader"
rights = ["read"]
if_owner = true

[[entries]]
doctype = "note"
role = "owner_reader"
rights = ["read"]
if_owner = true

[[entries]]
doctype = "sales_invoice"
role = "cancel_only"
rights = ["cancel"]

[[entries]]
doctype = "sales_invoice"
role = "create_only"
rights = ["create"]

[[entries]]
doctype = "sales_invoice_item"
role = "editor"
rights = ["read", "write", "create"]

[[entries]]
doctype = "sales_invoice_item"
role = "writer"
rights = ["read", "write", "create"]

[[entries]]
doctype = "customer"
role = "masked_creator"
rights = ["read", "create"]

[[entries]]
doctype = "customer"
role = "customer_creator"
rights = ["read", "write", "create"]
"#;

const FIELD_GRANTS: &str = r#"
[[entries]]
doctype = "customer"
role = "editor"
target = { group = "confidential" }
access = ["read", "create", "update"]

[[entries]]
doctype = "customer"
role = "editor"
target = { group = "financial" }
access = ["read", "create", "update", "report"]

[[entries]]
doctype = "sales_invoice"
role = "editor"
target = { group = "confidential" }
access = ["read", "create", "update"]

[[entries]]
doctype = "sales_invoice"
role = "editor"
target = { group = "financial" }
access = ["read", "create", "update", "report"]

[[entries]]
doctype = "note"
role = "editor"
target = { group = "confidential" }
access = ["read", "create", "update"]

[[entries]]
doctype = "customer"
role = "writer"
target = { group = "confidential" }
access = ["read", "create", "update"]

[[entries]]
doctype = "sales_invoice"
role = "writer"
target = { group = "confidential" }
access = ["read", "create", "update"]

[[entries]]
doctype = "note"
role = "writer"
target = { group = "confidential" }
access = ["read", "create", "update"]

[[entries]]
doctype = "customer"
role = "reader"
target = { group = "confidential" }
access = ["read_masked"]

[[entries]]
doctype = "sales_invoice"
role = "reader"
target = { group = "confidential" }
access = ["read_masked"]

[[entries]]
doctype = "note"
role = "reader"
target = { group = "confidential" }
access = ["read_masked"]

[[entries]]
doctype = "customer"
role = "shared_reader"
target = { group = "confidential" }
access = ["read"]

[[entries]]
doctype = "customer"
role = "shared_reader"
target = { group = "financial" }
access = ["read", "report"]

[[entries]]
doctype = "customer"
role = "masked_creator"
target = { group = "confidential" }
access = ["read_masked", "create"]

[[entries]]
doctype = "sales_invoice_item"
role = "writer"
target = { group = "confidential" }
access = ["read", "create", "update"]

[[entries]]
doctype = "sales_invoice_item"
role = "editor"
target = { group = "confidential" }
access = ["read", "create", "update"]

[[entries]]
doctype = "sales_invoice_item"
role = "editor"
target = { group = "financial" }
access = ["read", "create", "update"]
"#;

const GROUPS: &str = r#"
[[entries]]
name = "confidential"

[[entries]]
name = "financial"
"#;

/// The loaded model.
pub fn model() -> Model {
    load(&model_texts()).unwrap_or_else(|errors| panic!("the test model loads: {errors}"))
}

/// The field facts the metadata does not carry: `secret_note` is confidential (masked) and
/// `salary`/`total`/`cost` are financial.
fn field_facts() -> Vec<(&'static str, Vec<FieldFacts>)> {
    let confidential = |path: &str| FieldFacts {
        path: path.to_owned(),
        groups: vec![GroupName("confidential".to_owned())],
        mask: Some(MaskFormat::Placeholder),
        ..FieldFacts::default()
    };
    let financial = |path: &str, mask: MaskFormat| FieldFacts {
        path: path.to_owned(),
        groups: vec![GroupName("financial".to_owned())],
        mask: Some(mask),
        ..FieldFacts::default()
    };
    vec![
        (
            "customer",
            vec![
                confidential("secret_note"),
                financial("salary", MaskFormat::LastChars(2)),
            ],
        ),
        (
            "sales_invoice",
            vec![
                confidential("secret_note"),
                financial("total", MaskFormat::LastChars(2)),
            ],
        ),
        ("note", vec![confidential("secret_note")]),
        (
            "sales_invoice_item",
            vec![financial("cost", MaskFormat::LastChars(2))],
        ),
    ]
}

/// The decision core over the world's model and field facts.
pub fn decision_engine() -> Arc<PermissionEngine> {
    let mut snapshot = Snapshot::build(&meta(), &model()).unwrap();
    for (doctype, facts) in field_facts() {
        snapshot = snapshot.with_field_facts(doctype, facts).unwrap();
    }
    Arc::new(PermissionEngine::new(Arc::new(snapshot)))
}

/// A second decision core that grants `reader` the `create` right on `note` (a permission
/// change): the same grants, with that one role's rights widened.
pub fn decision_engine_with_extra_grant() -> Arc<PermissionEngine> {
    let mut texts = model_texts();
    texts.grants = texts.grants.replace(
        "doctype = \"note\"\nrole = \"reader\"\nrights = [\"read\"]",
        "doctype = \"note\"\nrole = \"reader\"\nrights = [\"read\", \"create\"]",
    );
    let model = load(&texts).unwrap();
    let mut snapshot = Snapshot::build(&meta(), &model).unwrap();
    for (doctype, facts) in field_facts() {
        snapshot = snapshot.with_field_facts(doctype, facts).unwrap();
    }
    Arc::new(PermissionEngine::new(Arc::new(snapshot)))
}

/// The principals the matrix names, keyed by their name.
pub fn principals() -> BTreeMap<String, Principal> {
    let mut out = BTreeMap::new();
    out.insert(
        "admin".to_owned(),
        Principal::with_roles("admin", &[TENANT_ADMINISTRATOR]),
    );
    out.insert(
        "editor".to_owned(),
        Principal::with_roles("editor", &["editor"]),
    );
    out.insert(
        "reader".to_owned(),
        Principal::with_roles("reader", &["reader"]),
    );
    out.insert(
        "owner_reader_other".to_owned(),
        Principal::with_roles("owner_reader_other", &["owner_reader"]),
    );
    out.insert(
        "owner_reader_own".to_owned(),
        Principal::with_roles("owner_reader_own", &["owner_reader"]),
    );
    out.insert("scoped_out".to_owned(), scoped("scoped_out"));
    out.insert("no_role".to_owned(), Principal::user("no_role", Vec::new()));
    out.insert("system".to_owned(), Principal::system());
    out.insert(
        "writer".to_owned(),
        Principal::with_roles("writer", &["writer"]),
    );
    out.insert(
        "cancel_only".to_owned(),
        Principal::with_roles("cancel_only", &["cancel_only"]),
    );
    out.insert(
        "create_only".to_owned(),
        Principal::with_roles("create_only", &["create_only"]),
    );
    out.insert(
        "masked_creator".to_owned(),
        Principal::with_roles("masked_creator", &["masked_creator"]),
    );
    out.insert(
        "customer_creator".to_owned(),
        Principal::with_roles("customer_creator", &["customer_creator"]),
    );
    out.insert("scoped_writer".to_owned(), scoped("scoped_writer"));
    out
}

/// A writer whose every assignment is scope-blocked: it may use only [`ALLOWED_REGION`], and
/// the documents hold [`OTHER_REGION`].
fn scoped(user: &str) -> Principal {
    let scope = |doctype: &str| UserPermission {
        user: user.to_owned(),
        doctype: None,
        field_or_link_doctype: format!("{doctype}.region"),
        allowed: vec![ALLOWED_REGION.to_owned()],
        applies_to: AppliesTo::AllDoctypes,
        include_descendants: false,
    };
    Principal::user(
        user,
        vec![AssignmentContext {
            id: "default".to_owned(),
            roles: vec![Role("writer".to_owned())],
            scope: vec![scope("customer"), scope("sales_invoice"), scope("note")],
        }],
    )
}

/// The world the engine tests run against: the decision core, the principals, and one actor
/// record id per principal.
pub struct World {
    pub engine: Arc<PermissionEngine>,
    pub principals: BTreeMap<String, Principal>,
    pub ids: BTreeMap<String, RecordId>,
}

impl World {
    /// Builds the world.
    pub fn new() -> World {
        let principals = principals();
        let ids = principals
            .keys()
            .map(|name| (name.clone(), RecordId::new_v7()))
            .collect();
        World {
            engine: decision_engine(),
            principals,
            ids,
        }
    }

    /// The principal named by a matrix row.
    pub fn principal(&self, name: &str) -> &Principal {
        self.principals
            .get(name)
            .unwrap_or_else(|| panic!("no principal named `{name}`"))
    }

    /// The actor record id of a principal.
    pub fn id(&self, name: &str) -> RecordId {
        *self
            .ids
            .get(name)
            .unwrap_or_else(|| panic!("no principal named `{name}`"))
    }

    /// An invocation as the named principal.
    pub fn inv(&self, name: &str) -> Invocation {
        Invocation {
            site: SiteId::new("authz.test").unwrap(),
            actor: Actor::User {
                id: self.id(name),
                via: None,
            },
            command_id: CommandId::new("cmd-authz").ok(),
            language: None,
            now: now(),
        }
    }

    /// An invocation as the platform.
    pub fn system_inv(&self) -> Invocation {
        Invocation {
            site: SiteId::new("authz.test").unwrap(),
            actor: Actor::System {
                component: Name::new("authz_tests").unwrap(),
            },
            command_id: CommandId::new("cmd-authz").ok(),
            language: None,
            now: now(),
        }
    }
}

/// A fixed instant.
pub fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap()
}

/// The name helper.
pub fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

/// A `region` link value.
pub fn region(value: &str) -> Value {
    Value::Id(spot_permissions::ids::parse_record_id(value).unwrap())
}
