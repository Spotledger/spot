//! `prepare`: the schema it builds, idempotence, additive changes and what it refuses.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod documents_support;

use std::collections::BTreeSet;
use std::sync::Arc;

use documents_support::*;
use spot_storage_contract::{
    Catalog, Collation, DocumentStore, EntityDef, EntityKind, ErrorKind, FieldDef, FieldType,
    IndexDef, LinkCheck, LinkTarget, Name, RecordId, StorageBackend, StorageError, Transaction,
    Value,
};
use tokio_postgres::Client;

async fn value<T>(client: &Client, sql: &str) -> T
where
    T: for<'a> tokio_postgres::types::FromSql<'a>,
{
    client.query_one(sql, &[]).await.unwrap().get(0)
}

/// The base catalog's entities, for tests to change.
fn entities() -> Vec<EntityDef> {
    catalog().entities().cloned().collect()
}

fn catalog_v(version: u64, entities: Vec<EntityDef>) -> Arc<Catalog> {
    Arc::new(Catalog::new(version, Collation::new("en").unwrap(), entities).unwrap())
}

fn entity_mut<'a>(entities: &'a mut [EntityDef], name: &str) -> &'a mut EntityDef {
    entities
        .iter_mut()
        .find(|e| e.name.as_str() == name)
        .unwrap()
}

fn field_mut<'a>(entities: &'a mut [EntityDef], entity: &str, field: &str) -> &'a mut FieldDef {
    entity_mut(entities, entity)
        .fields
        .iter_mut()
        .find(|f| f.name.as_str() == field)
        .unwrap()
}

fn field(name: &str, ty: FieldType, nullable: bool) -> FieldDef {
    FieldDef {
        name: n(name),
        ty,
        nullable,
    }
}

/// Every object `prepare` could create, as sorted oids: running it again must change none.
async fn objects(client: &Client) -> BTreeSet<(String, u32)> {
    let rows = client
        .query(
            "SELECT 'class', oid FROM pg_class WHERE relnamespace = 'public'::regnamespace \
             UNION ALL SELECT 'constraint', oid FROM pg_constraint \
                 WHERE connamespace = 'public'::regnamespace \
             UNION ALL SELECT 'trigger', oid FROM pg_trigger WHERE NOT tgisinternal \
             UNION ALL SELECT 'proc', oid FROM pg_proc WHERE pronamespace = 'public'::regnamespace \
             UNION ALL SELECT 'collation', oid FROM pg_collation WHERE collname LIKE 'spot\\_%'",
            &[],
        )
        .await
        .unwrap();
    rows.iter().map(|r| (r.get(0), r.get(1))).collect()
}

fn refusal(error: &StorageError, needles: &[&str]) {
    assert_eq!(error.kind, ErrorKind::InvalidSchema, "{error}");
    for needle in needles {
        assert!(
            error.explanation.contains(needle),
            "`{needle}` missing from: {error}"
        );
    }
}

#[tokio::test]
async fn prepare_builds_tables_types_collation_links_and_indexes() {
    let Some((env, _)) = prepared().await else {
        return;
    };
    let c = env.db.raw_client().await;

    let ty = |table: &str, column: &str| {
        let sql = format!(
            "SELECT format_type(atttypid, atttypmod) FROM pg_attribute \
             WHERE attrelid = '{table}'::regclass AND attname = '{column}'"
        );
        let c = &c;
        async move { value::<String>(c, &sql).await }
    };
    assert_eq!(ty("party", "credit_limit").await, "numeric(18,2)");
    assert_eq!(ty("party", "name").await, "text");
    assert_eq!(ty("party", "seen_at").await, "timestamp with time zone");
    assert_eq!(ty("party", "profile").await, "jsonb");
    assert_eq!(ty("party", "parent_party").await, "uuid");
    assert_eq!(ty("party", "id").await, "uuid");
    assert_eq!(ty("party", "version").await, "bigint");
    assert_eq!(ty("invoice_line", "qty").await, "numeric(18,3)");
    assert_eq!(ty("invoice_line", "parent").await, "uuid");
    assert_eq!(ty("invoice_line", "parent_field").await, "text");
    assert_eq!(ty("invoice_line", "idx").await, "integer");

    // A child row has no version column of its own.
    let columns: i64 = value(
        &c,
        "SELECT count(*) FROM pg_attribute WHERE attrelid = 'invoice_line'::regclass \
         AND attname = 'version'",
    )
    .await;
    assert_eq!(columns, 0);

    // Text columns are byte-wise (`C`), so equality and unique indexes are exact.
    let collation: String = value(
        &c,
        "SELECT collname FROM pg_attribute a JOIN pg_collation c ON c.oid = a.attcollation \
         WHERE attrelid = 'party'::regclass AND attname = 'name'",
    )
    .await;
    assert_eq!(collation, "C");

    // The tenant's ICU collation exists, is deterministic, and orders by locale rules (a < B),
    // where the byte-wise `C` order puts every capital first.
    let icu = c
        .query_one(
            "SELECT collprovider::text, collisdeterministic, \
                    ('a' < 'B' COLLATE spot_en), ('a' < 'B' COLLATE \"C\") \
             FROM pg_collation WHERE collname = 'spot_en'",
            &[],
        )
        .await
        .unwrap();
    let icu: (String, bool, bool, bool) = (icu.get(0), icu.get(1), icu.get(2), icu.get(3));
    assert_eq!(icu, ("i".to_owned(), true, true, false));

    // Database links and every child's parent are deferred foreign keys that never cascade;
    // an engine-only link has none.
    for name in [
        "invoice_party_fkey",
        "party_parent_party_fkey",
        "invoice_line_parent_fkey",
    ] {
        let row = c
            .query_one(
                "SELECT contype::text, condeferrable, condeferred, confdeltype::text, confupdtype::text \
                 FROM pg_constraint WHERE conname = $1",
                &[&name],
            )
            .await
            .unwrap();
        let got: (String, bool, bool, String, String) =
            (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4));
        assert_eq!(
            got,
            ("f".to_owned(), true, true, "a".to_owned(), "a".to_owned()),
            "{name}"
        );
    }
    let engine_only: i64 = value(
        &c,
        "SELECT count(*) FROM pg_constraint WHERE conname = 'invoice_ref_id_fkey'",
    )
    .await;
    assert_eq!(engine_only, 0);

    // Declared indexes, unique or not; the child's `(parent, idx)` index is the declared one.
    for (name, unique, columns) in [
        ("party_name", true, "name"),
        ("invoice_number", true, "number"),
        ("invoice_party", false, "party"),
        ("invoice_line_parent", false, "parent,idx"),
    ] {
        let row = c
            .query_one(
                "SELECT i.indisunique, (SELECT string_agg(a.attname, ',' ORDER BY k.ord) \
                 FROM unnest(i.indkey::int2[]) WITH ORDINALITY k(n, ord) \
                 JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.n) \
                 FROM pg_index i WHERE i.indexrelid = $1::text::regclass",
                &[&name],
            )
            .await
            .unwrap();
        assert_eq!(
            (row.get::<_, bool>(0), row.get::<_, String>(1)),
            (unique, columns.to_owned()),
            "{name}"
        );
    }

    // A text limit is enforced by the database as well.
    let too_long = c
        .execute(
            "INSERT INTO party (id, version, name, credit_limit) VALUES (gen_random_uuid(), 1, repeat('x', 141), 0)",
            &[],
        )
        .await;
    assert!(too_long.is_err());

    // The schema's own bookkeeping.
    let meta: Vec<(String, String)> = c
        .query("SELECT key, value FROM spot_meta ORDER BY key", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    assert_eq!(
        meta,
        [
            ("catalog_version".to_owned(), "1".to_owned()),
            ("collation".to_owned(), "en".to_owned()),
            // The outbox and change feed (WP-STO-08.T1).
            ("feed_generation".to_owned(), "1".to_owned()),
            ("feed_schema".to_owned(), "1".to_owned()),
        ]
    );
}

#[tokio::test]
async fn a_child_table_without_a_declared_parent_index_gets_one() {
    let Some(env) = common::env().await else {
        return;
    };
    let mut es = entities();
    entity_mut(&mut es, "invoice_line").indexes.clear();
    let cat = catalog_v(1, es);
    env.backend.prepare(&cat).await.unwrap();
    let c = env.db.raw_client().await;
    let columns: String = value(
        &c,
        "SELECT string_agg(a.attname, ',' ORDER BY k.ord) \
         FROM pg_index i, unnest(i.indkey::int2[]) WITH ORDINALITY k(n, ord), pg_attribute a \
         WHERE i.indexrelid = 'invoice_line_parent_idx'::regclass \
           AND a.attrelid = i.indrelid AND a.attnum = k.n",
    )
    .await;
    assert_eq!(columns, "parent,idx");
}

#[tokio::test]
async fn preparing_again_with_the_same_catalog_changes_nothing() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    let c = env.db.raw_client().await;
    let before = objects(&c).await;
    assert!(before.len() > 20, "the schema should hold many objects");
    env.backend.prepare(&cat).await.unwrap();
    env.backend.prepare(&cat).await.unwrap();
    assert_eq!(objects(&c).await, before);
}

#[tokio::test]
async fn concurrent_prepares_queue_instead_of_racing() {
    let Some(env) = common::env().await else {
        return;
    };
    let cat = catalog();
    let (a, b, c) = tokio::join!(
        env.backend.prepare(&cat),
        env.backend.prepare(&cat),
        env.backend.prepare(&cat)
    );
    a.unwrap();
    b.unwrap();
    c.unwrap();
}

#[tokio::test]
async fn additive_changes_are_applied_and_data_survives() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    let p = party("Acme", 100);
    seed(&env, &cat, &[&p]).await;

    // A new optional field, a new index, a new entity (with a link into an old one) and a new
    // version number.
    let mut es = entities();
    entity_mut(&mut es, "party").fields.push(field(
        "nickname",
        FieldType::Text {
            max_chars: Some(20),
        },
        true,
    ));
    entity_mut(&mut es, "party").indexes.push(IndexDef {
        name: n("party_nickname"),
        fields: vec![n("nickname")],
        unique: false,
        method: spot_storage_contract::IndexMethod::BTree,
        opclass: None,
        include: vec![],
    });
    es.push(EntityDef {
        name: n("memo"),
        kind: EntityKind::Document,
        fields: vec![
            field(
                "about",
                FieldType::Link {
                    target: LinkTarget::Entity(n("party")),
                    check: LinkCheck::Database,
                },
                false,
            ),
            field("body", FieldType::Json, true),
        ],
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    });
    let v2 = catalog_v(2, es);
    env.backend.prepare(&v2).await.unwrap();

    let c = env.db.raw_client().await;
    let version: String = value(
        &c,
        "SELECT value FROM spot_meta WHERE key = 'catalog_version'",
    )
    .await;
    assert_eq!(version, "2");
    let names: i64 = value(
        &c,
        "SELECT count(*) FROM pg_attribute WHERE attrelid = 'party'::regclass AND attname = 'nickname'",
    )
    .await;
    assert_eq!(names, 1);
    // The new limit exists too.
    let limit = c
        .execute("UPDATE party SET nickname = repeat('x', 21)", &[])
        .await;
    assert!(limit.is_err());

    // The old document reads back with the new field null, and the new entity works.
    let (stored, _) = load(&env, &v2, "party", p.id).await.unwrap();
    assert_eq!(stored.fields[&n("nickname")], Value::Null);
    let mut memo = spot_storage_contract::DocumentData {
        entity: n("memo"),
        id: RecordId::new_v7(),
        fields: Default::default(),
        children: Default::default(),
    };
    memo.fields.insert(n("about"), Value::Id(p.id));
    memo.fields.insert(n("body"), Value::Null);
    let mut tx = begin(&env, &v2).await;
    tx.insert(&memo).await.unwrap();
    tx.commit().await.unwrap();

    // Going back to the older catalog would drop a column, which is refused.
    let refused = env.backend.prepare(&cat).await.unwrap_err();
    refusal(&refused, &["nickname", "dropping a column"]);
}

#[tokio::test]
async fn a_required_column_cannot_be_added_to_a_table_that_holds_records() {
    let Some((env, _)) = prepared().await else {
        return;
    };
    let mut es = entities();
    entity_mut(&mut es, "party")
        .fields
        .push(field("rank", FieldType::Int, false));
    let with_rank = catalog_v(2, es);
    // Empty table: fine.
    env.backend.prepare(&with_rank).await.unwrap();

    let mut es = entities();
    entity_mut(&mut es, "party")
        .fields
        .push(field("rank", FieldType::Int, false));
    entity_mut(&mut es, "party")
        .fields
        .push(field("grade", FieldType::Int, false));
    let more = catalog_v(3, es);
    let mut p = party("Acme", 0);
    p.fields.insert(n("rank"), Value::Int(1));
    seed(&env, &with_rank, &[&p]).await;
    let refused = env.backend.prepare(&more).await.unwrap_err();
    refusal(&refused, &["party.grade", "already holds records"]);
    assert_eq!(refused.field, Some(n("grade")));
}

#[tokio::test]
async fn changes_that_are_not_additive_are_refused_by_name_and_change_nothing() {
    let Some((env, _)) = prepared().await else {
        return;
    };
    type Change = Box<dyn Fn(&mut Vec<EntityDef>)>;
    let cases: Vec<(&str, Change, Vec<&str>)> = vec![
        (
            "type change",
            Box::new(|es| field_mut(es, "party", "credit_limit").ty = FieldType::Int),
            vec![
                "party.credit_limit",
                "type changed",
                "numeric(18,2)",
                "bigint",
            ],
        ),
        (
            "decimal scale change",
            Box::new(|es| {
                field_mut(es, "party", "credit_limit").ty = FieldType::Decimal {
                    precision: 18,
                    scale: 3,
                }
            }),
            vec!["party.credit_limit", "type changed"],
        ),
        (
            "dropped column",
            Box::new(|es| {
                entity_mut(es, "party")
                    .fields
                    .retain(|f| f.name.as_str() != "note")
            }),
            vec!["party.note", "dropping a column"],
        ),
        (
            "nullability change",
            Box::new(|es| field_mut(es, "party", "note").nullable = false),
            vec!["party.note", "nullability"],
        ),
        (
            "text limit change",
            Box::new(|es| {
                field_mut(es, "party", "name").ty = FieldType::Text {
                    max_chars: Some(200),
                }
            }),
            vec!["party.name", "length limit"],
        ),
        (
            "link no longer checked",
            Box::new(|es| {
                field_mut(es, "invoice", "party").ty = FieldType::Link {
                    target: LinkTarget::Entity(n("party")),
                    check: LinkCheck::EngineOnly {
                        reason: "hot".into(),
                    },
                }
            }),
            vec!["invoice.party", "no longer checked"],
        ),
        (
            "index made non-unique",
            Box::new(|es| entity_mut(es, "party").indexes[0].unique = false),
            vec!["party_name", "different definition"],
        ),
        (
            "index on other columns",
            Box::new(|es| entity_mut(es, "party").indexes[0].fields = vec![n("note")]),
            vec!["party_name", "different definition"],
        ),
    ];
    let c = env.db.raw_client().await;
    let before = objects(&c).await;
    for (what, change, needles) in cases {
        let mut es = entities();
        change(&mut es);
        // Also ask for something additive in the same run: a refusal must roll it back too.
        entity_mut(&mut es, "invoice")
            .fields
            .push(field("extra", FieldType::Int, true));
        let cat = catalog_v(2, es);
        let refused = env.backend.prepare(&cat).await.unwrap_err();
        eprintln!("{what}: {refused}");
        refusal(&refused, &needles);
        assert_eq!(objects(&c).await, before, "{what} left changes behind");
        let extra: i64 = value(
            &c,
            "SELECT count(*) FROM pg_attribute WHERE attrelid = 'invoice'::regclass AND attname = 'extra'",
        )
        .await;
        assert_eq!(extra, 0, "{what}: the additive part must roll back too");
    }
}

#[tokio::test]
async fn a_different_collation_is_refused() {
    let Some((env, _)) = prepared().await else {
        return;
    };
    let other = Arc::new(Catalog::new(2, Collation::new("ur-PK").unwrap(), entities()).unwrap());
    let refused = env.backend.prepare(&other).await.unwrap_err();
    refusal(&refused, &["collation", "`en`", "`ur-PK`"]);
    let c = env.db.raw_client().await;
    let made: i64 = value(
        &c,
        "SELECT count(*) FROM pg_collation WHERE collname = 'spot_ur_pk'",
    )
    .await;
    assert_eq!(made, 0, "nothing was created for the refused collation");
}

#[tokio::test]
async fn the_bookkeeping_table_name_is_reserved() {
    let Some(env) = common::env().await else {
        return;
    };
    let mut es = entities();
    es.push(EntityDef {
        name: n("spot_meta"),
        kind: EntityKind::Document,
        fields: vec![],
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    });
    let refused = env.backend.prepare(&catalog_v(1, es)).await.unwrap_err();
    refusal(&refused, &["spot_meta"]);
}

#[tokio::test]
async fn long_names_stay_within_postgres_limits_and_still_map_back() {
    let Some(env) = common::env().await else {
        return;
    };
    let entity = format!("e{}", "n".repeat(50));
    let link = format!("l{}", "k".repeat(50));
    let long = EntityDef {
        name: n(&entity),
        kind: EntityKind::Document,
        fields: vec![field(
            &link,
            FieldType::Link {
                target: LinkTarget::Entity(n(&entity)),
                check: LinkCheck::Database,
            },
            true,
        )],
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    };
    let cat = catalog_v(1, vec![long]);
    env.backend.prepare(&cat).await.unwrap();
    env.backend.prepare(&cat).await.unwrap();

    let mut doc = spot_storage_contract::DocumentData {
        entity: n(&entity),
        id: RecordId::new_v7(),
        fields: Default::default(),
        children: Default::default(),
    };
    doc.fields
        .insert(Name::new(&link).unwrap(), Value::Id(RecordId::new_v7()));
    let mut tx = env
        .backend
        .begin(
            cat.clone(),
            spot_storage_contract::TxOptions::new(
                n("long"),
                spot_storage_contract::Isolation::ReadCommitted,
            ),
        )
        .await
        .unwrap();
    tx.insert(&doc).await.unwrap();
    let e = expect_kind(tx.commit().await, ErrorKind::ReferenceViolation);
    assert_eq!(e.entity, Some(n(&entity)));
    assert_eq!(e.field, Some(n(&link)));
    assert!(e.constraint.is_some_and(|c| c.as_str().len() <= 63));
}
