//! Document writes against PostgreSQL: full checked writes, child replacement, versions, links.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod documents_support;

use documents_support::*;
use rust_decimal::Decimal;
use spot_storage_contract::{
    DocumentStore, ErrorKind, Name, RecordId, RowLock, Transaction, Value, Version, Wait,
};

#[tokio::test]
async fn a_decimal_with_too_many_digits_is_refused_never_rounded() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    let mut p = party("Acme", 0);
    // 0.005 would round to 0.01 (or 0.00) if PostgreSQL were allowed to coerce it.
    p.fields
        .insert(n("credit_limit"), Value::Decimal(Decimal::new(5, 3)));
    let mut tx = begin(&env, &cat).await;
    let e = expect_kind(tx.insert(&p).await, ErrorKind::InvalidValue);
    assert_eq!(e.field, Some(n("credit_limit")));
    assert_eq!(e.entity, Some(n("party")));
    // Validation happens before anything is sent, so the transaction is still usable.
    let ok = party("Fine", 100);
    tx.insert(&ok).await.unwrap();
    tx.commit().await.unwrap();
    assert!(load(&env, &cat, "party", p.id).await.is_none());
    assert!(load(&env, &cat, "party", ok.id).await.is_some());

    // Trailing zeros are the same number and are fine.
    let mut q = party("Zeros", 0);
    q.fields
        .insert(n("credit_limit"), Value::Decimal(Decimal::new(12_500, 3)));
    seed(&env, &cat, &[&q]).await;
    let (stored, _) = load(&env, &cat, "party", q.id).await.unwrap();
    assert_eq!(stored.fields[&n("credit_limit")], money(1_250));
}

#[tokio::test]
async fn child_rows_are_reordered_inserted_and_deleted_on_update() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    let p = party("Acme", 0);
    let (a, b, c) = (
        line("a", 1_000, 100),
        line("b", 1_000, 200),
        line("c", 1_000, 300),
    );
    let mut inv = invoice("INV-1", p.id, vec![a.clone(), b, c.clone()]);
    seed(&env, &cat, &[&p, &inv]).await;

    // Reorder, change c, drop b, add d.
    let mut c2 = c.clone();
    c2.fields.insert(n("amount"), money(333));
    let d = line("d", 1_000, 400);
    inv.children
        .insert(n("lines"), vec![c2.clone(), a.clone(), d.clone()]);
    let mut tx = begin(&env, &cat).await;
    assert_eq!(tx.update(&inv, Version::FIRST).await.unwrap(), Version(2));
    tx.commit().await.unwrap();
    let (stored, version) = load(&env, &cat, "invoice", inv.id).await.unwrap();
    assert_eq!(version, Version(2));
    assert_eq!(lines_of(&stored), &vec![c2, a.clone(), d]);

    // The kept rows kept their ids and the table holds exactly those three, with idx 1..=3.
    let client = env.db.raw_client().await;
    let rows = client
        .query(
            "SELECT id, idx, parent_field FROM invoice_line ORDER BY idx",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
    let idx: Vec<i32> = rows.iter().map(|r| r.get(1)).collect();
    assert_eq!(idx, [1, 2, 3]);
    let first: uuid::Uuid = rows[0].get(0);
    assert_eq!(first.as_bytes(), c.id.as_bytes());
    assert!(rows.iter().all(|r| r.get::<_, String>(2) == "lines"));

    // An update that drops every line, by leaving the table out.
    inv.children.clear();
    let mut tx = begin(&env, &cat).await;
    tx.update(&inv, Version(2)).await.unwrap();
    tx.commit().await.unwrap();
    let (stored, version) = load(&env, &cat, "invoice", inv.id).await.unwrap();
    assert_eq!(version, Version(3));
    assert!(lines_of(&stored).is_empty());
    let count: i64 = client
        .query_one("SELECT count(*) FROM invoice_line", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 0);
}

#[tokio::test]
async fn a_child_row_id_belongs_to_one_document() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    let p = party("Acme", 0);
    let one = invoice("INV-1", p.id, vec![line("a", 1_000, 1)]);
    let mut two = invoice("INV-2", p.id, vec![]);
    seed(&env, &cat, &[&p, &one, &two]).await;
    // Moving the row to another document by update is the same refusal as by insert.
    two.children.insert(n("lines"), lines_of(&one).clone());
    let mut tx = begin(&env, &cat).await;
    let e = expect_kind(
        tx.update(&two, Version::FIRST).await,
        ErrorKind::AlreadyExists,
    );
    assert_eq!(e.entity, Some(n("invoice_line")));
    // The same id twice in one document is refused before anything is sent.
    let mut dup = invoice("INV-3", p.id, vec![]);
    let row = line("x", 1_000, 1);
    dup.children.insert(n("lines"), vec![row.clone(), row]);
    let mut tx = begin(&env, &cat).await;
    expect_kind(tx.insert(&dup).await, ErrorKind::InvalidValue);
}

#[tokio::test]
async fn versions_count_by_one_and_missing_documents_are_not_found() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    let mut p = party("Acme", 0);
    seed(&env, &cat, &[&p]).await;
    for expected in 1..=3 {
        p.fields.insert(n("credit_limit"), money(expected * 100));
        let mut tx = begin(&env, &cat).await;
        let v = tx.update(&p, Version(expected as u64)).await.unwrap();
        assert_eq!(v, Version(expected as u64 + 1));
        tx.commit().await.unwrap();
    }
    let (stored, version) = load(&env, &cat, "party", p.id).await.unwrap();
    assert_eq!(version, Version(4));
    assert_eq!(stored.fields[&n("credit_limit")], money(300));

    let mut tx = begin(&env, &cat).await;
    expect_kind(
        tx.delete(&n("party"), RecordId::new_v7(), Version::FIRST)
            .await,
        ErrorKind::NotFound,
    );
    // A version above what a bigint holds can never match.
    let mut tx = begin(&env, &cat).await;
    expect_kind(
        tx.update(&p, Version(u64::MAX)).await,
        ErrorKind::InvalidValue,
    );
}

#[tokio::test]
async fn a_child_entity_cannot_be_written_or_read_directly() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    let mut tx = begin(&env, &cat).await;
    let e = expect_kind(
        tx.load(&n("invoice_line"), RecordId::new_v7(), RowLock::None)
            .await,
        ErrorKind::UnknownEntity,
    );
    assert!(e.explanation.contains("invoice"), "{e}");
    expect_kind(
        tx.delete(&n("invoice_line"), RecordId::new_v7(), Version::FIRST)
            .await,
        ErrorKind::UnknownEntity,
    );
    expect_kind(
        tx.load(&n("nonexistent"), RecordId::new_v7(), RowLock::None)
            .await,
        ErrorKind::UnknownEntity,
    );
    // Writes naming a child table the entity does not have.
    let p = party("Acme", 0);
    let mut inv = invoice("I", p.id, vec![]);
    inv.children.insert(n("ghost"), vec![]);
    let mut tx = begin(&env, &cat).await;
    expect_kind(tx.insert(&inv).await, ErrorKind::UnknownField);
}

#[tokio::test]
async fn link_violations_name_the_linking_entity_and_field() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    // A link to a missing record, found at commit.
    let orphan = invoice("INV-1", RecordId::new_v7(), vec![]);
    let mut tx = begin(&env, &cat).await;
    tx.insert(&orphan).await.unwrap();
    let e = expect_kind(tx.commit().await, ErrorKind::ReferenceViolation);
    assert_eq!((e.entity, e.field), (Some(n("invoice")), Some(n("party"))));
    assert_eq!(e.constraint, Some(n("invoice_party_fkey")));

    // A self link on the same table is named for the field, not the table alone.
    let mut child = party("Child", 0);
    child
        .fields
        .insert(n("parent_party"), Value::Id(RecordId::new_v7()));
    let mut tx = begin(&env, &cat).await;
    tx.insert(&child).await.unwrap();
    let e = expect_kind(tx.commit().await, ErrorKind::ReferenceViolation);
    assert_eq!(
        (e.entity, e.field),
        (Some(n("party")), Some(n("parent_party")))
    );

    // A child row whose parent does not exist: the `parent` column of the child table.
    let mut tx = begin(&env, &cat).await;
    let bogus = RecordId::new_v7();
    tx.raw_execute(
        "INSERT INTO invoice_line (id, parent, parent_field, idx, item, qty, amount) \
         VALUES ($1, $2, 'lines', 1, 'x', 1, 1)",
        &[
            &uuid::Uuid::from_bytes(*RecordId::new_v7().as_bytes()),
            &uuid::Uuid::from_bytes(*bogus.as_bytes()),
        ],
    )
    .await
    .unwrap();
    let e = expect_kind(tx.commit().await, ErrorKind::ReferenceViolation);
    assert_eq!(
        (e.entity, e.field),
        (Some(n("invoice_line")), Some(n("parent")))
    );

    // Deleting a linked party names the *linking* side, and nothing cascades.
    let p = party("Acme", 0);
    let inv = invoice("INV-2", p.id, vec![line("a", 1_000, 1)]);
    seed(&env, &cat, &[&p, &inv]).await;
    let mut tx = begin(&env, &cat).await;
    tx.delete(&n("party"), p.id, Version::FIRST).await.unwrap();
    let e = expect_kind(tx.commit().await, ErrorKind::ReferenceViolation);
    assert_eq!((e.entity, e.field), (Some(n("invoice")), Some(n("party"))));
    assert!(load(&env, &cat, "invoice", inv.id).await.is_some());
    assert!(load(&env, &cat, "party", p.id).await.is_some());

    // Re-creating the target in the same transaction satisfies the link again.
    let mut tx = begin(&env, &cat).await;
    tx.delete(&n("party"), p.id, Version::FIRST).await.unwrap();
    let mut again = party("Acme again", 0);
    again.id = p.id;
    tx.insert(&again).await.unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn an_update_that_links_to_a_missing_record_fails_at_commit() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    // A document that was valid is refused at commit once an update points its link nowhere.
    let p = party("Acme", 0);
    let mut inv = invoice("INV-1", p.id, vec![]);
    seed(&env, &cat, &[&p, &inv]).await;
    inv.fields.insert(n("party"), Value::Id(RecordId::new_v7()));
    let mut tx = begin(&env, &cat).await;
    tx.update(&inv, Version::FIRST).await.unwrap();
    let e = expect_kind(tx.commit().await, ErrorKind::ReferenceViolation);
    assert_eq!(e.field, Some(n("party")));
}

#[tokio::test]
async fn row_locks_lock_the_parent_row() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    let p = party("Acme", 0);
    seed(&env, &cat, &[&p]).await;
    let mut holder = begin(&env, &cat).await;
    assert!(
        holder
            .load(&n("party"), p.id, RowLock::Update(Wait::Block))
            .await
            .unwrap()
            .is_some()
    );
    let mut other = begin(&env, &cat).await;
    let sp = other.savepoint().await.unwrap();
    expect_kind(
        other
            .load(&n("party"), p.id, RowLock::Update(Wait::NoWait))
            .await,
        ErrorKind::LockNotAvailable,
    );
    // A refused NOWAIT is recoverable: after rolling back to the savepoint the transaction goes on.
    other.rollback_to(sp).await.unwrap();
    other.insert(&party("Other", 0)).await.unwrap();
    let mut third = begin(&env, &cat).await;
    let skipped = third
        .load(&n("party"), p.id, RowLock::Share(Wait::SkipLocked))
        .await
        .unwrap();
    assert!(skipped.is_none(), "a skipped row loads as None");
    holder.rollback().await.unwrap();
    // Once released it can be locked again.
    let mut again = begin(&env, &cat).await;
    assert!(
        again
            .load(&n("party"), p.id, RowLock::Share(Wait::NoWait))
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn text_and_json_that_postgres_cannot_hold_are_refused_with_the_field_named() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    for (field, value) in [
        ("note", Value::Text("a\0b".into())),
        ("profile", Value::Json("{\"a\": \"\\u0000\"}".into())),
        ("profile", Value::Json("[1, 2".into())),
    ] {
        let mut p = party("Acme", 0);
        p.fields.insert(Name::new(field).unwrap(), value);
        let mut tx = begin(&env, &cat).await;
        let e = expect_kind(tx.insert(&p).await, ErrorKind::InvalidValue);
        assert_eq!(e.field, Some(Name::new(field).unwrap()));
    }
    // Empty text and null stay distinct, and JSON comes back equivalent.
    let mut p = party("Acme", 0);
    p.fields.insert(n("note"), Value::Text(String::new()));
    p.fields.insert(
        n("profile"),
        Value::Json("{\"a\":[1,2,{\"b\":null}]}".into()),
    );
    seed(&env, &cat, &[&p]).await;
    let (stored, _) = load(&env, &cat, "party", p.id).await.unwrap();
    assert_eq!(stored.fields[&n("note")], Value::Text(String::new()));
    assert_eq!(stored.fields[&n("visits")], Value::Null);
    assert!(matches!(&stored.fields[&n("profile")], Value::Json(j) if j.contains("\"b\": null")));
}
