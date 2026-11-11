//! A seeded world: three DocTypes, 500 documents, twelve principals.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spot_app_api::prelude::{RecordId, Value};
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, Registry};
use spot_perm_query::ReadTarget;
use spot_permissions::{
    AppliesTo, AssignmentContext, DocFacts, Model, PermissionEngine, Principal, Right, Rights,
    Role, RoleGrant, Share, ShareRight, Snapshot, TENANT_ADMINISTRATOR, UserPermission,
};
use spot_storage_contract::{
    Catalog, Collation, DocumentData, Expr, Fields, Isolation, Name, Query, QueryExecutor,
    QueryPath, Selection, StorageBackend, Transaction, TxOptions,
};
use spot_storage_fake::FakeBackend;

pub fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

/// A small deterministic generator.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1))
    }
    pub fn below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        usize::try_from(self.0 >> 33).unwrap() % n
    }
}

pub fn meta() -> Arc<EffectiveMeta> {
    let text = |name: &str| FieldSpec::new(name, FieldType::Text);
    let mut company = Definition::new("company");
    company.fields = vec![text("title"), text("owner")];
    let mut defs = vec![company];
    for name in ["project", "task"] {
        let mut d = Definition::new(name);
        let mut link = FieldSpec::new("company", FieldType::Link);
        link.options = Some("company".into());
        let mut region = FieldSpec::new("region", FieldType::Select);
        region.options = Some("north\nsouth\neast".into());
        d.fields = vec![text("title"), link, region, text("owner")];
        defs.push(d);
    }
    Registry::new(defs).unwrap().effective()
}

pub const DOCTYPES: [&str; 3] = ["company", "project", "task"];

pub fn target() -> ReadTarget {
    ReadTarget {
        alias: n("d"),
        owner_field: n("owner"),
    }
}

/// One stored document and the facts the decision core is asked about.
pub struct Doc {
    pub doctype: &'static str,
    pub id: RecordId,
    pub facts: DocFacts,
}

pub struct World {
    pub backend: FakeBackend,
    pub catalog: Arc<Catalog>,
    pub engine: PermissionEngine,
    pub docs: Vec<Doc>,
    pub principals: Vec<Principal>,
}

const USERS: [&str; 6] = ["alice", "dave", "erin", "ivan", "ken", "zed"];
const REGIONS: [&str; 3] = ["north", "south", "east"];

fn grant(doctype: &str, role: &str, rights: &[Right], if_owner: bool) -> RoleGrant {
    RoleGrant {
        doctype: doctype.into(),
        role: Role(role.into()),
        rights: Rights::new(rights.iter().copied()),
        if_owner,
    }
}

fn scope(path: &str, allowed: &[String], applies_to: AppliesTo) -> UserPermission {
    UserPermission {
        user: String::new(),
        doctype: None,
        field_or_link_doctype: path.into(),
        allowed: allowed.to_vec(),
        applies_to,
        include_descendants: false,
    }
}

fn assignment(id: &str, roles: &[&str], scope: Vec<UserPermission>) -> AssignmentContext {
    AssignmentContext {
        id: id.into(),
        roles: roles.iter().map(|r| Role((*r).into())).collect(),
        scope,
    }
}

pub async fn build(seed: u64) -> World {
    let meta = meta();
    let catalog = Arc::new(meta.catalog(Collation::new("und").unwrap()).unwrap());
    let backend = FakeBackend::new();
    backend.prepare(&catalog).await.unwrap();
    let mut rng = Rng::new(seed);
    let mut docs: Vec<Doc> = Vec::new();
    let mut tx = backend
        .begin(
            catalog.clone(),
            TxOptions::new(n("seed_world"), Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    let mut companies = Vec::new();
    for i in 0..3 {
        let id = RecordId::new_v7();
        companies.push(id);
        let mut fields = Fields::new();
        fields.insert(n("title"), Value::Text(format!("company {i}")));
        fields.insert(n("owner"), Value::Text("zed".into()));
        insert(&mut tx, "company", id, fields).await;
        docs.push(Doc {
            doctype: "company",
            id,
            facts: facts("company", id, "zed", BTreeMap::new()),
        });
    }
    for i in 0..497 {
        let doctype = if i % 5 < 2 { "project" } else { "task" };
        let id = RecordId::new_v7();
        let owner = USERS[rng.below(USERS.len())];
        let company = (rng.below(5) != 0).then(|| companies[rng.below(3)]);
        let region = (rng.below(5) != 0).then(|| REGIONS[rng.below(3)]);
        let mut fields = Fields::new();
        fields.insert(n("title"), Value::Text(format!("{doctype} {i}")));
        fields.insert(n("owner"), Value::Text(owner.into()));
        fields.insert(n("company"), company.map_or(Value::Null, Value::Id));
        fields.insert(
            n("region"),
            region.map_or(Value::Null, |r| Value::Text(r.into())),
        );
        insert(&mut tx, doctype, id, fields).await;
        let mut values = BTreeMap::new();
        if let Some(c) = company {
            values.insert("company".to_owned(), Value::Id(c));
        }
        if let Some(r) = region {
            values.insert("region".to_owned(), Value::Text(r.into()));
        }
        docs.push(Doc {
            doctype: if doctype == "project" {
                "project"
            } else {
                "task"
            },
            id,
            facts: facts(doctype, id, owner, values),
        });
    }
    tx.commit().await.unwrap();

    let ids =
        |set: &[usize]| -> Vec<String> { set.iter().map(|i| companies[*i].to_string()).collect() };
    let mut grants = Vec::new();
    for dt in DOCTYPES {
        grants.push(grant(dt, "reader", &[Right::Read], false));
        grants.push(grant(dt, "owner_reader", &[Right::Read], true));
        grants.push(grant(dt, "writer", &[Right::Write], false));
    }
    let mut shares = Vec::new();
    for user in ["frank", "judy", "ken", "bob"] {
        for _ in 0..25 {
            let doc = &docs[rng.below(docs.len())];
            let rights = if rng.below(4) == 0 {
                vec![ShareRight::Write]
            } else {
                vec![ShareRight::Read]
            };
            shares.push(Share {
                doc: doc.id,
                user: user.into(),
                rights,
            });
        }
    }
    let model = Model {
        grants,
        shares,
        ..Model::default()
    };
    let snapshot = Snapshot::build(&meta, &model).unwrap();
    let all = AppliesTo::AllDoctypes;
    let principals = vec![
        Principal::user("alice", vec![assignment("a1", &["reader"], vec![])]),
        Principal::user(
            "bob",
            vec![assignment(
                "a1",
                &["reader"],
                vec![scope("company", &ids(&[0]), all.clone())],
            )],
        ),
        Principal::user(
            "carol",
            vec![
                assignment(
                    "a1",
                    &["reader"],
                    vec![scope("company", &ids(&[0]), all.clone())],
                ),
                assignment(
                    "a2",
                    &["reader"],
                    vec![scope("project.region", &["south".into()], all.clone())],
                ),
            ],
        ),
        Principal::user("dave", vec![assignment("a1", &["owner_reader"], vec![])]),
        Principal::user(
            "erin",
            vec![assignment(
                "a1",
                &["owner_reader"],
                vec![scope("company", &ids(&[1]), all.clone())],
            )],
        ),
        Principal::user("frank", vec![]),
        Principal::user("grace", vec![assignment("a1", &[], vec![])]),
        Principal::system(),
        Principal::user(
            "heidi",
            vec![assignment(
                TENANT_ADMINISTRATOR,
                &[TENANT_ADMINISTRATOR],
                vec![scope("task.region", &["north".into()], all.clone())],
            )],
        ),
        Principal::user(
            "ivan",
            vec![
                assignment(
                    "a1",
                    &["reader"],
                    vec![scope("company", &ids(&[0]), all.clone())],
                ),
                assignment("a2", &["owner_reader"], vec![]),
            ],
        ),
        Principal::user(
            "judy",
            vec![assignment(
                "a1",
                &["writer"],
                vec![scope("company", &ids(&[2]), all.clone())],
            )],
        ),
        Principal::user(
            "ken",
            vec![assignment(
                "a1",
                &["reader"],
                vec![
                    scope("company", &ids(&[2, 0]), all.clone()),
                    scope("company", &ids(&[2, 1]), all),
                    scope(
                        "project.region",
                        &["south".into(), "east".into()],
                        AppliesTo::Only(vec!["project".into()]),
                    ),
                ],
            )],
        ),
    ];
    World {
        backend,
        catalog,
        engine: PermissionEngine::new(Arc::new(snapshot)),
        docs,
        principals,
    }
}

fn facts(doctype: &str, id: RecordId, owner: &str, values: BTreeMap<String, Value>) -> DocFacts {
    DocFacts {
        doctype: doctype.into(),
        id: Some(id.to_string()),
        created_by: Some(owner.into()),
        state: None,
        values,
    }
}

async fn insert<T: Transaction>(tx: &mut T, doctype: &str, id: RecordId, fields: Fields) {
    tx.insert(&DocumentData {
        entity: n(doctype),
        id,
        fields,
        children: BTreeMap::new(),
    })
    .await
    .unwrap();
}

impl World {
    /// The ids of `doctype` rows for which `filter` holds (all rows when `None`).
    pub async fn run(&self, doctype: &str, filter: Option<Expr>) -> BTreeSet<RecordId> {
        let mut q = Query::from(&n(doctype), &n("d"), QueryPath::Transactional);
        q.select = vec![Selection {
            expr: Expr::field(&n("d"), &n("id")),
            alias: n("id"),
        }];
        q.filter = filter;
        let mut tx = self
            .backend
            .begin(
                self.catalog.clone(),
                TxOptions::new(n("list"), Isolation::ReadCommitted).read_only(),
            )
            .await
            .unwrap();
        let rows = tx.query(&q).await.unwrap();
        rows.rows
            .into_iter()
            .map(|r| match &r[0] {
                Value::Id(id) => *id,
                other => panic!("not an id: {other:?}"),
            })
            .collect()
    }

    /// The ids of `doctype` documents the decision core lets the principal read.
    pub fn allowed(&self, principal: &Principal, doctype: &str) -> BTreeSet<RecordId> {
        self.docs
            .iter()
            .filter(|d| d.doctype == doctype)
            .filter(|d| {
                self.engine
                    .can(principal, spot_permissions::Action::Read, &d.facts)
                    .allowed
            })
            .map(|d| d.id)
            .collect()
    }
}
