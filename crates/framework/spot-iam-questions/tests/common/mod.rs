//! The seeded access model the question tests evaluate: 40 users, 12 roles, scoped
//! assignments across 4 companies, shares, an expiring assignment, a field grant with a deny
//! — plus an independently coded brute-force oracle over the same model.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spot_iam_governance::{ChangeActor, ChangeEvent, ChangeOp};
use spot_iam_questions::{AccessModel, Change, DocumentRecord, ShareRecord};
use spot_iam_roles::{
    Assignment, Duty, DutyName, DutyRef, Origin, Registry, Role, RoleName, Source, Version,
};
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, Registry as MetaRegistry};
use spot_permissions::{
    AppliesTo, FieldGrant, FieldRight, FieldTarget, GroupName, ProtectionGroup, Right, Rights,
    Role as GrantRole, RoleGrant, ShareRight, UserPermission,
};
use spot_sod::RuleSet;
use spot_storage_contract::{RecordId, Value};
use time::OffsetDateTime;

/// The users of the seeded matrix.
pub const USERS: &[&str] = &[
    "u00", "u01", "u02", "u03", "u04", "u05", "u06", "u07", "u08", "u09", "u10", "u11", "u12",
    "u13", "u14", "u15", "u16", "u17", "u18", "u19", "u20", "u21", "u22", "u23", "u24", "u25",
    "u26", "u27", "u28", "u29", "u30", "u31", "u32", "u33", "u34", "u35", "u36", "u37", "u38",
    "u39",
];

/// The four companies, each with a stable record id.
pub fn company(n: u8) -> RecordId {
    record_id(u64::from(n) + 100)
}

/// A deterministic UUIDv7 record id from a counter (tests never read the wall clock).
pub fn record_id(n: u64) -> RecordId {
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&0x0000_0190_0000_0000u64.to_be_bytes()[2..]);
    b[6..8].copy_from_slice(&((n >> 32) as u16).to_be_bytes());
    b[8..16].copy_from_slice(&n.to_be_bytes());
    b[6] = (b[6] & 0x0f) | 0x70;
    b[8] = (b[8] & 0x3f) | 0x80;
    RecordId::from_bytes(b).expect("a hand-built uuidv7 is valid")
}

/// An instant from integer epoch seconds.
pub fn t(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(secs).expect("a valid instant")
}

/// The seeded read model.
pub struct Fixture {
    pub registry: Registry,
    pub assignments: Vec<Assignment>,
    pub field_grants_vec: Vec<FieldGrant>,
    pub groups: Vec<ProtectionGroup>,
    pub shares: Vec<ShareRecord>,
    pub rules: RuleSet,
    pub history: Vec<ChangeEvent>,
    pub meta: Arc<EffectiveMeta>,
    pub field_facts_by_doctype: BTreeMap<String, Vec<spot_permissions::FieldFacts>>,
    pub documents: Vec<DocumentRecord>,
    pub definitions: Vec<Definition>,
    pub portal_user_ids: Vec<String>,
}

impl Fixture {
    /// Marks `users` as portal users.
    pub fn set_portal_users(&mut self, users: &[&str]) {
        self.portal_user_ids = users.iter().map(|u| (*u).to_owned()).collect();
    }
}

impl AccessModel for Fixture {
    fn registry(&self) -> &Registry {
        &self.registry
    }
    fn assignments(&self) -> &[Assignment] {
        &self.assignments
    }
    fn portal_users(&self) -> &[String] {
        &self.portal_user_ids
    }
    fn field_grants(&self) -> &[FieldGrant] {
        &self.field_grants_vec
    }
    fn groups(&self) -> &[ProtectionGroup] {
        &self.groups
    }
    fn shares(&self) -> &[ShareRecord] {
        &self.shares
    }
    fn rules(&self) -> &RuleSet {
        &self.rules
    }
    fn history(&self) -> &[ChangeEvent] {
        &self.history
    }
    fn meta(&self) -> &EffectiveMeta {
        &self.meta
    }
    fn field_facts(&self, doctype: &str) -> Vec<spot_permissions::FieldFacts> {
        self.field_facts_by_doctype
            .get(doctype)
            .cloned()
            .unwrap_or_default()
    }
    fn document(&self, doctype: &str, name: &str) -> Option<&DocumentRecord> {
        self.documents
            .iter()
            .find(|d| d.doctype == doctype && d.name == name)
    }
    fn documents(&self) -> &[DocumentRecord] {
        &self.documents
    }
}

/// A DocType grant for a role or duty.
pub fn grant(doctype: &str, role: &str, rights: &[Right], if_owner: bool) -> RoleGrant {
    RoleGrant {
        doctype: doctype.to_owned(),
        role: GrantRole(role.to_owned()),
        rights: Rights::new(rights.iter().copied()),
        if_owner,
    }
}

/// A duty with DocType grants.
pub fn duty(name: &str, grants: Vec<RoleGrant>) -> Duty {
    Duty {
        name: DutyName::from(name),
        version: Version(1),
        grants,
        field_grants: Vec::new(),
        app_permissions: Vec::new(),
        outside_exposed: false,
    }
}

pub fn role(name: &str, duties: Vec<&str>, direct: Vec<RoleGrant>) -> Role {
    Role {
        name: RoleName::from(name),
        origin: Origin::Custom { copied_from: None },
        duties: duties
            .into_iter()
            .map(|d| DutyRef {
                name: DutyName::from(d),
                version: None,
            })
            .collect(),
        direct_grants: direct,
        direct_field_grants: Vec::new(),
        direct_app_permissions: Vec::new(),
        parents: Vec::new(),
        kind: Default::default(),
    }
}

pub fn duty_with_field(name: &str, field_grants: Vec<FieldGrant>) -> Duty {
    let mut d = duty(name, Vec::new());
    d.field_grants = field_grants;
    d
}

fn field_group(doctype: &str, role: &str, group: &str, access: &[FieldRight]) -> FieldGrant {
    FieldGrant {
        doctype: doctype.to_owned(),
        role: GrantRole(role.to_owned()),
        target: FieldTarget::Group(GroupName(group.to_owned())),
        access: access.iter().copied().collect(),
        deny: false,
        condition: None,
    }
}

fn field_deny(doctype: &str, role: &str, group: &str) -> FieldGrant {
    FieldGrant {
        doctype: doctype.to_owned(),
        role: GrantRole(role.to_owned()),
        target: FieldTarget::Group(GroupName(group.to_owned())),
        access: BTreeSet::new(),
        deny: true,
        condition: None,
    }
}

/// A scoped assignment: `company` is a link value the scope is restricted to.
pub fn assignment(
    id: &str,
    user: &str,
    role_name: &str,
    source: Source,
    scope_company: Option<u8>,
    valid_from: Option<i64>,
    valid_to: Option<i64>,
) -> Assignment {
    let mut a = Assignment::new(id, user, role_name);
    a.source = source;
    if let Some(c) = scope_company {
        a.scope.restrictions = vec![UserPermission {
            user: String::new(),
            doctype: None,
            field_or_link_doctype: "company".to_owned(),
            allowed: vec![company(c).to_string()],
            applies_to: AppliesTo::AllDoctypes,
            include_descendants: false,
        }];
    }
    a.valid_from = valid_from.map(t);
    a.valid_to = valid_to.map(t);
    a
}

/// Builds the seeded fixture.
#[must_use]
pub fn seeded() -> Fixture {
    // Metadata.
    let mut company_def = Definition::new("company");
    company_def.fields = vec![FieldSpec::new("name", FieldType::Text)];
    let mut customer = Definition::new("customer");
    let mut clink = FieldSpec::new("company", FieldType::Link);
    clink.options = Some("company".into());
    customer.fields = vec![FieldSpec::new("name", FieldType::Text), clink];
    let mut invoice = Definition::new("sales_invoice");
    invoice.is_posted = true;
    let mut ilink = FieldSpec::new("company", FieldType::Link);
    ilink.options = Some("company".into());
    let mut cust = FieldSpec::new("customer", FieldType::Link);
    cust.options = Some("customer".into());
    invoice.fields = vec![
        FieldSpec::new("number", FieldType::Text),
        ilink,
        cust,
        FieldSpec::new("cost", FieldType::Text),
    ];
    invoice.protection_groups = vec![spot_meta::ProtectionGroupSpec::new("cost", "Cost")];
    let mut project = Definition::new("project");
    let mut plink = FieldSpec::new("company", FieldType::Link);
    plink.options = Some("company".into());
    project.fields = vec![FieldSpec::new("name", FieldType::Text), plink];
    let definitions = vec![company_def, customer, invoice, project];
    let meta = MetaRegistry::new(definitions.clone())
        .expect("metadata is valid")
        .effective();

    // Duties.
    let duties = vec![
        duty(
            "Read Customers",
            vec![grant("customer", "Read Customers", &[Right::Read], false)],
        ),
        duty(
            "Edit Customers",
            vec![grant(
                "customer",
                "Edit Customers",
                &[Right::Write, Right::Create],
                false,
            )],
        ),
        duty(
            "Read Invoices",
            vec![grant(
                "sales_invoice",
                "Read Invoices",
                &[Right::Read],
                false,
            )],
        ),
        duty(
            "Post Invoices",
            vec![grant(
                "sales_invoice",
                "Post Invoices",
                &[Right::Submit, Right::Cancel],
                false,
            )],
        ),
        duty(
            "Manage Projects",
            vec![grant(
                "project",
                "Manage Projects",
                &[Right::Read, Right::Write, Right::Create],
                false,
            )],
        ),
        duty("Access Governance", Vec::new()),
        duty_with_field(
            "See Cost",
            vec![field_group(
                "sales_invoice",
                "See Cost",
                "cost",
                &[FieldRight::Read],
            )],
        ),
    ];

    // Roles.
    let roles = vec![
        role("Customer Reader", vec!["Read Customers"], vec![]),
        role(
            "Customer Clerk",
            vec!["Read Customers", "Edit Customers"],
            vec![],
        ),
        role(
            "Invoice Clerk",
            vec!["Read Invoices"],
            vec![grant(
                "sales_invoice",
                "Invoice Clerk",
                &[Right::Create],
                false,
            )],
        ),
        role(
            "Invoice Poster",
            vec!["Read Invoices", "Post Invoices"],
            vec![],
        ),
        role("Project Manager", vec!["Manage Projects"], vec![]),
        role("Finance Approver", vec!["Post Invoices"], vec![]),
        role("Access Governance", vec!["Access Governance"], vec![]),
        {
            let mut r = role(
                "Auditor",
                vec![],
                vec![
                    grant("customer", "Auditor", &[Right::Read], false),
                    grant("sales_invoice", "Auditor", &[Right::Read], false),
                    grant("project", "Auditor", &[Right::Read], false),
                ],
            );
            r.direct_field_grants = vec![field_deny("sales_invoice", "Auditor", "cost")];
            r
        },
        role("Cost Viewer", vec!["Read Invoices", "See Cost"], vec![]),
        role("Tenant Administrator", vec![], vec![]),
        role(
            "Company Viewer",
            vec![],
            vec![grant("customer", "Company Viewer", &[Right::Read], false)],
        ),
        role(
            "Emergency Admin",
            vec![],
            vec![
                grant(
                    "customer",
                    "Emergency Admin",
                    &[Right::Read, Right::Write, Right::Create, Right::Delete],
                    false,
                ),
                grant(
                    "sales_invoice",
                    "Emergency Admin",
                    &[Right::Read, Right::Write, Right::Create, Right::Delete],
                    false,
                ),
                grant(
                    "project",
                    "Emergency Admin",
                    &[Right::Read, Right::Write, Right::Create, Right::Delete],
                    false,
                ),
            ],
        ),
    ];
    let registry = Registry::new(duties, roles).expect("the registry is valid");

    // Assignments: the explicit rows, then a deterministic spread for the rest.
    let mut assignments = vec![
        assignment(
            "a-u00-1",
            "u00",
            "Customer Clerk",
            Source::Manual,
            Some(1),
            None,
            None,
        ),
        assignment(
            "a-u00-2",
            "u00",
            "Customer Reader",
            Source::Manual,
            None,
            None,
            None,
        ),
        assignment(
            "a-u01",
            "u01",
            "Customer Clerk",
            Source::Manual,
            Some(2),
            None,
            None,
        ),
        assignment(
            "a-u02",
            "u02",
            "Invoice Clerk",
            Source::Manual,
            Some(1),
            None,
            None,
        ),
        assignment(
            "a-u03",
            "u03",
            "Invoice Poster",
            Source::Manual,
            Some(1),
            None,
            None,
        ),
        assignment(
            "a-u04",
            "u04",
            "Project Manager",
            Source::Manual,
            Some(1),
            None,
            None,
        ),
        assignment(
            "a-u05",
            "u05",
            "Finance Approver",
            Source::Manual,
            Some(2),
            None,
            None,
        ),
        assignment(
            "a-u06",
            "u06",
            "Access Governance",
            Source::Manual,
            None,
            None,
            None,
        ),
        assignment(
            "a-u07",
            "u07",
            "Auditor",
            Source::Manual,
            Some(1),
            None,
            None,
        ),
        assignment(
            "a-u08",
            "u08",
            "Cost Viewer",
            Source::Manual,
            Some(1),
            None,
            None,
        ),
        assignment(
            "a-u09",
            "u09",
            "Tenant Administrator",
            Source::Manual,
            None,
            None,
            None,
        ),
        assignment(
            "a-u10",
            "u10",
            "Customer Reader",
            Source::Manual,
            Some(3),
            None,
            None,
        ),
        assignment(
            "a-u11",
            "u11",
            "Company Viewer",
            Source::Manual,
            Some(1),
            None,
            None,
        ),
        assignment(
            "a-u12",
            "u12",
            "Invoice Poster",
            Source::Manual,
            Some(1),
            None,
            Some(100),
        ),
        assignment(
            "a-u22-1",
            "u22",
            "Cost Viewer",
            Source::Manual,
            Some(1),
            None,
            None,
        ),
        assignment(
            "a-u22-2",
            "u22",
            "Auditor",
            Source::Manual,
            Some(1),
            None,
            None,
        ),
        assignment(
            "a-u20",
            "u20",
            "Customer Reader",
            Source::Manual,
            None,
            None,
            None,
        ),
    ];
    let names = [
        "Customer Reader",
        "Customer Clerk",
        "Invoice Clerk",
        "Invoice Poster",
        "Project Manager",
        "Finance Approver",
        "Access Governance",
        "Auditor",
        "Cost Viewer",
        "Tenant Administrator",
        "Company Viewer",
        "Emergency Admin",
    ];
    let covered: BTreeSet<String> = assignments.iter().map(|a| a.user.clone()).collect();
    for (i, user) in USERS.iter().enumerate() {
        if covered.contains(*user) {
            continue;
        }
        let role_name = names[i % names.len()];
        let company = u8::try_from((i % 4) + 1).expect("small");
        assignments.push(assignment(
            &format!("a-{user}"),
            user,
            role_name,
            Source::Rule,
            Some(company),
            None,
            None,
        ));
    }

    // Shares: one invoice shared with u20.
    let shares = vec![ShareRecord::read(record_id(1), "u20")];

    // Documents.
    let documents = vec![
        DocumentRecord::named("customer", "customer-1")
            .with_id(record_id(10))
            .with_value("company", Value::Id(company(1)))
            .created_by("u00"),
        DocumentRecord::named("sales_invoice", "invoice-1")
            .with_id(record_id(1))
            .with_value("company", Value::Id(company(1)))
            .with_value("cost", Value::Text("SECRET-COST".to_owned()))
            .created_by("u02"),
        DocumentRecord::named("sales_invoice", "invoice-2")
            .with_id(record_id(2))
            .with_value("company", Value::Id(company(2)))
            .created_by("u05"),
        DocumentRecord::named("project", "project-1")
            .with_id(record_id(11))
            .with_value("company", Value::Id(company(1)))
            .created_by("u04"),
    ];

    // Field facts: sales_invoice.cost is protected by group `cost`.
    let mut field_facts_by_doctype: BTreeMap<String, Vec<spot_permissions::FieldFacts>> =
        BTreeMap::new();
    field_facts_by_doctype.insert(
        "sales_invoice".to_owned(),
        vec![spot_permissions::FieldFacts {
            path: "cost".to_owned(),
            groups: vec![GroupName("cost".to_owned())],
            tags: Vec::new(),
            mask: None,
        }],
    );

    // History.
    let history = vec![
        ChangeEvent::grant(
            "grant:old",
            "u01",
            "Customer Reader",
            Source::Manual,
            ChangeOp::Created,
            ChangeActor::user("u06"),
            10,
        )
        .with_reason("onboarding"),
        ChangeEvent::grant(
            "grant:old",
            "u01",
            "Customer Reader",
            Source::Manual,
            ChangeOp::Ended,
            ChangeActor::user("u06"),
            50,
        )
        .with_cause("removed"),
        ChangeEvent::role(
            "Customer Clerk",
            ChangeOp::Updated,
            "custom",
            ChangeActor::system("migration"),
            70,
        ),
    ];

    Fixture {
        registry,
        assignments,
        field_grants_vec: Vec::new(),
        groups: vec![ProtectionGroup {
            name: GroupName("cost".to_owned()),
            trace: true,
        }],
        shares,
        rules: RuleSet::default(),
        history,
        meta,
        field_facts_by_doctype,
        documents,
        definitions,
        portal_user_ids: Vec::new(),
    }
}

// -------------------------------------------------------------------------------------------
// The brute-force oracle: an independent implementation over the same model.
// -------------------------------------------------------------------------------------------

/// Whether an assignment is in force at `at`.
fn active(a: &Assignment, at: i64) -> bool {
    a.valid_from.is_none_or(|f| f.unix_timestamp() <= at)
        && a.valid_to.is_none_or(|to| at < to.unix_timestamp())
}

/// The link fields of a DocType, from the raw definitions: `(field, target)`.
pub fn link_fields(definitions: &[Definition], doctype: &str) -> Vec<(String, String)> {
    definitions
        .iter()
        .find(|d| d.name == doctype)
        .map(|d| {
            d.fields
                .iter()
                .filter_map(|f| match f.ty {
                    FieldType::Link => {
                        Some((f.name.clone(), f.options.clone().unwrap_or_default()))
                    }
                    FieldType::Select => Some((f.name.clone(), String::new())),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn value_text(v: &Value) -> String {
    match v {
        Value::Id(id) => id.to_string(),
        Value::Text(t) => t.clone(),
        other => format!("{other:?}"),
    }
}

/// Whether one user permission restricts a link field of the document.
fn restricts(up: &UserPermission, doctype: &str, field: &str, target: &str) -> bool {
    let path = format!("{doctype}.{field}");
    let matches_field = up.field_or_link_doctype == path;
    let matches_target = !target.is_empty() && up.field_or_link_doctype == target;
    let applies = match &up.applies_to {
        AppliesTo::AllDoctypes => true,
        AppliesTo::Only(list) => list.iter().any(|d| d == doctype),
    };
    (matches_field || matches_target) && applies
}

/// Whether the assignment's scope covers the document's values.
pub fn scope_covers(
    definitions: &[Definition],
    a: &Assignment,
    doctype: &str,
    values: &BTreeMap<String, Value>,
) -> bool {
    let links = link_fields(definitions, doctype);
    a.scope.restrictions.iter().all(|up| {
        for (field, target) in &links {
            if !restricts(up, doctype, field, target) {
                continue;
            }
            if let Some(v) = values.get(field)
                && !up.allowed.iter().any(|x| x == &value_text(v))
            {
                return false;
            }
        }
        true
    })
}

/// The rights one role reaches on a DocType, with whether each is owner-only.
pub fn role_rights(registry: &Registry, role_name: &str, doctype: &str) -> Vec<(Right, bool)> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    collect_role_rights(registry, role_name, doctype, &mut seen, &mut out);
    out.sort_by_key(|(r, owner)| (r.as_str(), *owner));
    out.dedup();
    out
}

fn collect_role_rights(
    registry: &Registry,
    role_name: &str,
    doctype: &str,
    seen: &mut BTreeSet<RoleName>,
    out: &mut Vec<(Right, bool)>,
) {
    if !seen.insert(RoleName::from(role_name)) {
        return;
    }
    if role_name == spot_permissions::TENANT_ADMINISTRATOR {
        out.extend(Right::all().into_iter().map(|r| (r, false)));
        return;
    }
    let Some(role) = registry.role(&RoleName::from(role_name)) else {
        return;
    };
    for used in &role.duties {
        if let Some(duty) = registry.duty(&used.name) {
            for g in &duty.grants {
                if g.doctype == doctype {
                    for r in g.rights.as_slice() {
                        out.push((*r, g.if_owner));
                    }
                }
            }
        }
    }
    for g in &role.direct_grants {
        if g.doctype == doctype {
            for r in g.rights.as_slice() {
                out.push((*r, g.if_owner));
            }
        }
    }
    for parent in &role.parents {
        collect_role_rights(registry, parent.as_str(), doctype, seen, out);
    }
}

/// Whether `user` holds `right` on `doctype` in `scope` (a document with these values), by
/// brute force over the model. `created_by` is the document's creator.
pub fn oracle_can(
    f: &Fixture,
    user: &str,
    doctype: &str,
    right: Right,
    at: i64,
    values: &BTreeMap<String, Value>,
    created_by: Option<&str>,
) -> bool {
    for a in f
        .assignments
        .iter()
        .filter(|a| a.user == user && active(a, at))
    {
        if !scope_covers(&f.definitions, a, doctype, values) {
            continue;
        }
        for (r, owner) in role_rights(&f.registry, a.role.as_str(), doctype) {
            if r != right {
                continue;
            }
            if !owner {
                return true;
            }
            if created_by == Some(user) {
                return true;
            }
        }
    }
    false
}

/// The set of `(doctype, right)` a user holds, brute force.
pub fn oracle_rights(f: &Fixture, user: &str, at: i64) -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    for a in f
        .assignments
        .iter()
        .filter(|a| a.user == user && active(a, at))
    {
        for def in &f.definitions {
            for (r, _owner) in role_rights(&f.registry, a.role.as_str(), &def.name) {
                out.insert((def.name.clone(), r.as_str().to_owned()));
            }
        }
    }
    out
}

/// Whether `user` can read the document, brute force.
///
/// Mirrors the decision core's rule that a scope blocking **every** assignment is a final
/// denial a share cannot lift, while a share does lift a read when at least one assignment
/// covers the document (or the user has none at all).
pub fn oracle_can_read(f: &Fixture, user: &str, doc: &DocumentRecord, at: i64) -> bool {
    let assignments: Vec<&Assignment> = f
        .assignments
        .iter()
        .filter(|a| a.user == user && active(a, at))
        .collect();
    let covers = |a: &&Assignment| scope_covers(&f.definitions, a, &doc.doctype, &doc.values);
    let any_covers = assignments.is_empty() || assignments.iter().any(covers);
    for a in assignments.iter().filter(|a| covers(a)) {
        for (r, owner) in role_rights(&f.registry, a.role.as_str(), &doc.doctype) {
            if r == Right::Read && (!owner || doc.created_by.as_deref() == Some(user)) {
                return true;
            }
        }
    }
    any_covers
        && f.shares.iter().any(|s| {
            s.user == user && Some(s.doc) == doc.id && s.rights.contains(&ShareRight::Read)
        })
}

/// The users who can see the document, brute force.
pub fn oracle_viewers(f: &Fixture, doc: &DocumentRecord, at: i64) -> Vec<String> {
    let mut users: BTreeSet<String> = f.assignments.iter().map(|a| a.user.clone()).collect();
    for s in &f.shares {
        users.insert(s.user.clone());
    }
    if let Some(c) = &doc.created_by {
        users.insert(c.clone());
    }
    users
        .into_iter()
        .filter(|u| oracle_can_read(f, u, doc, at))
        .collect()
}

/// The oracle's change rows for a window.
pub fn oracle_changes(history: &[ChangeEvent], from: i64, to: i64) -> Vec<Change> {
    if from > to {
        return Vec::new();
    }
    let mut out: Vec<Change> = history
        .iter()
        .filter(|e| e.occurred_at >= from && e.occurred_at <= to)
        .map(Change::of)
        .collect();
    out.sort_by(|a, b| {
        (&a.at, &a.subject, &a.kind, &a.op).cmp(&(&b.at, &b.subject, &b.kind, &b.op))
    });
    out
}

/// The three DocTypes a random model uses: `customer`, `sales_invoice` and `project`, each
/// with a `company` link.
pub fn random_definitions() -> Vec<Definition> {
    let mut company_def = Definition::new("company");
    company_def.fields = vec![FieldSpec::new("name", FieldType::Text)];
    let mut customer = Definition::new("customer");
    let mut clink = FieldSpec::new("company", FieldType::Link);
    clink.options = Some("company".into());
    customer.fields = vec![FieldSpec::new("name", FieldType::Text), clink];
    let mut invoice = Definition::new("sales_invoice");
    let mut ilink = FieldSpec::new("company", FieldType::Link);
    ilink.options = Some("company".into());
    invoice.fields = vec![FieldSpec::new("number", FieldType::Text), ilink];
    let mut project = Definition::new("project");
    let mut plink = FieldSpec::new("company", FieldType::Link);
    plink.options = Some("company".into());
    project.fields = vec![FieldSpec::new("name", FieldType::Text), plink];
    vec![company_def, customer, invoice, project]
}

/// Builds a fixture from a registry, assignments, shares and documents over the random model's
/// DocTypes, with no field protection and no history.
#[must_use]
pub fn fixture_from(
    duties: Vec<Duty>,
    roles: Vec<Role>,
    assignments: Vec<Assignment>,
    shares: Vec<ShareRecord>,
    documents: Vec<DocumentRecord>,
) -> Fixture {
    let definitions = random_definitions();
    let meta = MetaRegistry::new(definitions.clone())
        .expect("the random model's metadata is valid")
        .effective();
    let registry = Registry::new(duties, roles).expect("the random registry is valid");
    Fixture {
        registry,
        assignments,
        field_grants_vec: Vec::new(),
        groups: Vec::new(),
        shares,
        rules: RuleSet::default(),
        history: Vec::new(),
        meta,
        field_facts_by_doctype: BTreeMap::new(),
        documents,
        definitions,
        portal_user_ids: Vec::new(),
    }
}
