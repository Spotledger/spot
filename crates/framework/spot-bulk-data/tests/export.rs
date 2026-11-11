//! Export: equivalence to the list, field guarding, formatting, keyset paging, resumable
//! cursors, concurrency and child parts.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{
    CHILD, DOCTYPE, TestAuthority, build, build_same_modified, field_set, name, principal,
    small_limits,
};
use spot_bulk_data::{
    BulkLimits, Cursor, CursorKey, ExportPlan, ExportSpec, ExportStream, FieldSet, PageReader,
    Part, StreamContext, format,
};
use spot_storage_contract::{
    CmpOp, DocumentData, DocumentStore, Expr, Fields, Isolation, Query, RecordId, RowLock, Rows,
    StorageBackend, StorageResult, Transaction, TxOptions, Value,
};
use spot_storage_fake::FakeBackend;

#[tokio::test]
async fn export_equals_the_list_the_principal_sees() {
    let world = build(30).await;
    for user in ["exporter", "masked", "analyst", "system"] {
        let spec = ExportSpec::csv(DOCTYPE);
        let ids = world.export_ids(&spec, user).await;
        let unique: BTreeSet<RecordId> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len(), "{user}: a row id repeated");
        assert_eq!(
            unique,
            world.allowed(&principal(user)),
            "{user}: the export does not equal the list"
        );
    }
    // An owner-only principal has no document when the guard classifies columns, so its
    // owner grants do not count and every column is refused (a documented limitation).
    let owner_refused = ExportPlan::build(
        &ExportSpec::csv(DOCTYPE).with_columns(&["id"]),
        &principal("owner"),
        &world.guard,
        &TestAuthority,
        &field_set(),
        &*world.protection,
    )
    .unwrap_err();
    assert_eq!(owner_refused.code(), "permission.field_denied");
    // A principal without the export right is refused even though it may read.
    let world2 = build(3).await;
    let denied = ExportPlan::build(
        &ExportSpec::csv(DOCTYPE),
        &principal("clerk"),
        &world2.guard,
        &TestAuthority,
        &field_set(),
        &*world2.protection,
    )
    .unwrap_err();
    assert_eq!(denied.code(), spot_bulk_data::EXPORT_DENIED);
}

#[tokio::test]
async fn masked_columns_are_masked_and_refused_columns_error() {
    let world = build(3).await;
    // The masked reader may report the protected columns but only in masked form.
    let spec = ExportSpec::csv(DOCTYPE).with_columns(&["id", "card_number", "salary"]);
    let plan = world.plan(&spec, &principal("masked"));
    assert_eq!(plan.columns[1].path, "card_number");
    assert_eq!(
        plan.columns[1].mask,
        Some(spot_permissions::MaskFormat::LastChars(4))
    );
    assert_eq!(
        plan.columns[2].mask,
        Some(spot_permissions::MaskFormat::Placeholder)
    );
    let (bytes, _) = world.export_bytes(&spec, "masked").await;
    let text = String::from_utf8(bytes).unwrap();
    assert!(
        text.contains("***************0000"),
        "the card number must be masked: {text}"
    );
    assert!(!text.contains("4111-1111-1111-0000"));

    // The reader may read the DocType but has no report on the protected group: the column is
    // refused by name, never silently dropped.
    let refused = ExportPlan::build(
        &ExportSpec::csv(DOCTYPE).with_columns(&["id", "card_number"]),
        &principal("exporter"),
        &world.guard,
        &TestAuthority,
        &field_set(),
        &*world.protection,
    )
    .unwrap_err();
    assert_eq!(refused.code(), "permission.field_denied");
    assert!(refused.message().contains("card_number"));
}

#[test]
fn formula_injection_and_csv_quoting() {
    let columns = vec!["v".to_owned()];
    let text = |s: &str| {
        format::row(
            &columns,
            &[Value::Text(s.to_owned())],
            format::Format::Csv,
            true,
        )
    };
    for dangerous in ["=1+1", "+cmd", "-2", "@x", "\ttab", "\rcr"] {
        let out = String::from_utf8(text(dangerous)).unwrap();
        assert!(
            out.starts_with('\''),
            "`{dangerous}` must be hardened: {out:?}"
        );
    }
    // Hardening off leaves the text alone.
    let plain = format::row(
        &columns,
        &[Value::Text("=1+1".into())],
        format::Format::Csv,
        false,
    );
    assert_eq!(String::from_utf8(plain).unwrap(), "=1+1\n");
    // A field of only commas, newlines and quotes survives quoting.
    let out = String::from_utf8(format::row(
        &columns,
        &[Value::Text(",,,\n\"\"".to_owned())],
        format::Format::Csv,
        false,
    ))
    .unwrap();
    assert_eq!(out, "\",,,\n\"\"\"\"\"\n");
    // Numbers and dates are not hardened, even when they start with `-`.
    let out = String::from_utf8(format::row(
        &columns,
        &[Value::Int(-5)],
        format::Format::Csv,
        true,
    ))
    .unwrap();
    assert_eq!(out, "-5\n");
    // NDJSON never hardens; a null is `null`.
    let out = String::from_utf8(format::row(
        &columns,
        &[Value::Text("=1+1".into())],
        format::Format::Ndjson,
        true,
    ))
    .unwrap();
    assert_eq!(out, "{\"v\":\"=1+1\"}\n");
    let out = String::from_utf8(format::row(
        &columns,
        &[Value::Null],
        format::Format::Ndjson,
        true,
    ))
    .unwrap();
    assert_eq!(out, "{\"v\":null}\n");
}

#[tokio::test]
async fn keyset_paging_is_correct_with_10_000_shared_modified() {
    let world = build_same_modified(10_000).await;
    let spec = ExportSpec::csv(DOCTYPE);
    let plan = Arc::new(world.plan(&spec, &principal("analyst")));
    let limits = BulkLimits {
        rows_per_chunk: 1_000,
        bytes_per_chunk: 64 * 1024 * 1024,
        ..BulkLimits::default()
    };
    let mut stream = ExportStream::start(plan, world.ctx("analyst", limits)).unwrap();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next_chunk().await.unwrap() {
        bytes.extend_from_slice(&chunk.bytes);
    }
    let mut ids = Vec::new();
    for line in String::from_utf8_lossy(&bytes).lines().skip(1) {
        if let Some(first) = line.split(',').next()
            && let Ok(u) = uuid::Uuid::parse_str(first)
            && let Ok(id) = RecordId::from_bytes(*u.as_bytes())
        {
            ids.push(id);
        }
    }
    let unique: BTreeSet<RecordId> = ids.iter().copied().collect();
    assert_eq!(ids.len(), 10_000);
    assert_eq!(
        unique.len(),
        10_000,
        "a row id repeated across keyset pages"
    );
}

#[tokio::test]
async fn resume_at_every_chunk_boundary_is_identical() {
    let world = build(30).await;
    let spec = ExportSpec::csv(DOCTYPE).with_columns(&["id", "title", "amount"]);
    let plan = Arc::new(world.plan(&spec, &principal("analyst")));

    let mut full = Vec::new();
    let mut boundaries = 0usize;
    let mut stream =
        ExportStream::start(plan.clone(), world.ctx("analyst", small_limits())).unwrap();
    while let Some(chunk) = stream.next_chunk().await.unwrap() {
        full.extend_from_slice(&chunk.bytes);
        boundaries += 1;
    }
    assert!(boundaries > 3, "the export must cross several chunks");

    for k in 0..=boundaries {
        let mut first =
            ExportStream::start(plan.clone(), world.ctx("analyst", small_limits())).unwrap();
        let mut bytes = Vec::new();
        for _ in 0..k {
            if let Some(chunk) = first.next_chunk().await.unwrap() {
                bytes.extend_from_slice(&chunk.bytes);
            }
        }
        let cursor = first.cursor().unwrap();
        drop(first);
        let mut resumed =
            ExportStream::resume(plan.clone(), world.ctx("analyst", small_limits()), &cursor)
                .unwrap();
        while let Some(chunk) = resumed.next_chunk().await.unwrap() {
            bytes.extend_from_slice(&chunk.bytes);
        }
        assert_eq!(bytes, full, "resume after {k} chunks differed");
    }
}

#[tokio::test]
async fn forged_and_cross_plan_cursors_are_refused() {
    let world = build(9).await;
    let spec = ExportSpec::csv(DOCTYPE).with_columns(&["id", "title"]);
    let plan = Arc::new(world.plan(&spec, &principal("analyst")));
    let mut stream =
        ExportStream::start(plan.clone(), world.ctx("analyst", small_limits())).unwrap();
    stream.next_chunk().await.unwrap();
    let cursor = stream.cursor().unwrap();

    // A tampered token.
    let mut tampered = cursor.as_str().to_owned();
    tampered.push('X');
    let forged = Cursor::from_token(tampered);
    assert_eq!(
        ExportStream::resume(plan.clone(), world.ctx("analyst", small_limits()), &forged)
            .unwrap_err()
            .code(),
        spot_bulk_data::CURSOR_INVALID
    );

    // The right token under the wrong key.
    let wrong_key = StreamContext {
        key: CursorKey::new(b"another-key".to_vec()),
        ..world.ctx("analyst", small_limits())
    };
    assert_eq!(
        ExportStream::resume(plan.clone(), wrong_key, &cursor)
            .unwrap_err()
            .code(),
        spot_bulk_data::CURSOR_INVALID
    );

    // The right token on another plan.
    let other_spec = ExportSpec::csv(DOCTYPE).with_columns(&["id", "amount"]);
    let other_plan = Arc::new(world.plan(&other_spec, &principal("analyst")));
    assert_eq!(
        ExportStream::resume(other_plan, world.ctx("analyst", small_limits()), &cursor)
            .unwrap_err()
            .code(),
        spot_bulk_data::CURSOR_INVALID
    );

    // The right token for another principal.
    assert_eq!(
        ExportStream::resume(plan, world.ctx("analyst-2", small_limits()), &cursor)
            .unwrap_err()
            .code(),
        spot_bulk_data::CURSOR_INVALID
    );
}

#[tokio::test]
async fn children_are_separate_parts_keyed_by_parent() {
    let world = build(6).await;
    let spec = ExportSpec::csv(DOCTYPE).with_children();
    let plan = Arc::new(world.plan(&spec, &principal("analyst")));
    let mut stream = ExportStream::start(plan, world.ctx("analyst", small_limits())).unwrap();
    let mut child_rows = 0usize;
    let mut parent_rows = 0usize;
    let mut parent_bytes = Vec::new();
    let mut child_header_checked = false;
    while let Some(chunk) = stream.next_chunk().await.unwrap() {
        match &chunk.part {
            Part::Parent => {
                parent_bytes.extend_from_slice(&chunk.bytes);
                parent_rows += chunk.rows;
            }
            Part::Child(table) => {
                assert_eq!(table, "lines");
                let text = String::from_utf8_lossy(&chunk.bytes);
                if !child_header_checked {
                    let header = text.lines().next().unwrap();
                    assert!(header.starts_with("parent,idx,"), "header: {header}");
                    child_header_checked = true;
                }
                child_rows += chunk.rows;
            }
        }
    }
    assert!(parent_bytes.starts_with(b"id,"));
    assert_eq!(parent_rows, 6);
    assert_eq!(child_rows, 12);
}

#[tokio::test]
async fn children_follow_the_callers_row_filter() {
    let world = build(12).await;
    let filter = Expr::Cmp(
        Box::new(Expr::field(
            &name(spot_bulk_data::EXPORT_ALIAS),
            &name("title"),
        )),
        CmpOp::Eq,
        Box::new(Expr::Value(Value::Text("doc 3".into()))),
    );
    let spec = ExportSpec::csv(DOCTYPE).with_filter(filter).with_children();
    let plan = Arc::new(world.plan(&spec, &principal("analyst")));
    let mut stream = ExportStream::start(plan, world.ctx("analyst", small_limits())).unwrap();
    let (mut parent_rows, mut child_rows) = (0usize, 0usize);
    while let Some(chunk) = stream.next_chunk().await.unwrap() {
        match chunk.part {
            Part::Parent => parent_rows += chunk.rows,
            Part::Child(_) => child_rows += chunk.rows,
        }
    }
    assert_eq!(parent_rows, 1);
    assert_eq!(
        child_rows, 2,
        "children of filtered-out parents were exported"
    );
}

#[tokio::test]
async fn a_scoped_principal_sees_only_its_region_and_children_join() {
    let world = build(10).await;
    // Equivalence: the export equals the decision core's list.
    let ids = world.export_ids(&ExportSpec::csv(DOCTYPE), "scoped").await;
    let allowed = world.allowed(&principal("scoped"));
    assert_eq!(ids.iter().copied().collect::<BTreeSet<_>>(), allowed);
    assert!(
        !allowed.is_empty() && allowed.len() < 10,
        "the scope must bite"
    );

    // Children: only for the parents the scoped principal may read (the join path).
    let spec = ExportSpec::csv(DOCTYPE)
        .with_columns(&["id", "region", "title"])
        .with_children();
    let plan = Arc::new(world.plan(&spec, &principal("scoped")));
    let mut stream = ExportStream::start(plan, world.ctx("scoped", small_limits())).unwrap();
    let mut parent_bytes = Vec::new();
    let mut child_parents = BTreeSet::new();
    let mut child_header_seen = false;
    while let Some(chunk) = stream.next_chunk().await.unwrap() {
        match &chunk.part {
            Part::Parent => parent_bytes.extend_from_slice(&chunk.bytes),
            Part::Child(_) => {
                let text = String::from_utf8_lossy(&chunk.bytes).into_owned();
                for line in text.lines() {
                    if !child_header_seen {
                        child_header_seen = true;
                        continue;
                    }
                    if let Some(first) = line.split(',').next()
                        && let Ok(u) = uuid::Uuid::parse_str(first)
                        && let Ok(id) = RecordId::from_bytes(*u.as_bytes())
                    {
                        child_parents.insert(id);
                    }
                }
            }
        }
    }
    let parent_ids: BTreeSet<RecordId> = String::from_utf8_lossy(&parent_bytes)
        .lines()
        .skip(1)
        .filter_map(|l| uuid::Uuid::parse_str(l.split(',').next().unwrap_or("")).ok())
        .filter_map(|u| RecordId::from_bytes(*u.as_bytes()).ok())
        .collect();
    assert_eq!(parent_ids, allowed);
    assert_eq!(child_parents, parent_ids, "a child row escaped the join");
}

/// A reader that performs one concurrent mutation before its second page, then delegates.
struct MutatingReader {
    inner: Arc<dyn PageReader>,
    backend: FakeBackend,
    catalog: Arc<spot_storage_contract::Catalog>,
    calls: AtomicUsize,
    behind: RecordId,
    delete_ahead: RecordId,
    update_ahead: RecordId,
    new_title: String,
}

impl MutatingReader {
    async fn mutate(&self) -> StorageResult<()> {
        let mut tx = self
            .backend
            .begin(
                self.catalog.clone(),
                TxOptions::new(name("mutate"), Isolation::ReadCommitted),
            )
            .await?;
        let mut fields = Fields::new();
        fields.insert(name("modified"), Value::DateTime(world_base().unwrap()));
        fields.insert(name("title"), Value::Text("inserted behind".into()));
        fields.insert(name("owner"), Value::Text("analyst".into()));
        fields.insert(name("region"), Value::Text("north".into()));
        fields.insert(
            name("amount"),
            Value::Decimal(rust_decimal::Decimal::new(1, 2)),
        );
        fields.insert(name("note"), Value::Text("behind".into()));
        fields.insert(name("card_number"), Value::Text("x".into()));
        fields.insert(name("salary"), Value::Text("x".into()));
        tx.insert(&DocumentData {
            entity: name(DOCTYPE),
            id: self.behind,
            fields,
            children: std::collections::BTreeMap::new(),
        })
        .await?;
        if let Some(doc) = tx
            .load(&name(DOCTYPE), self.delete_ahead, RowLock::None)
            .await?
        {
            tx.delete(&name(DOCTYPE), self.delete_ahead, doc.version)
                .await?;
        }
        if let Some(doc) = tx
            .load(&name(DOCTYPE), self.update_ahead, RowLock::None)
            .await?
        {
            let mut data = doc.data;
            data.fields
                .insert(name("title"), Value::Text(self.new_title.clone()));
            tx.update(&data, doc.version).await?;
        }
        tx.commit().await
    }
}

fn world_base() -> StorageResult<time::OffsetDateTime> {
    time::OffsetDateTime::from_unix_timestamp(1_700_000_000).map_err(|e| {
        spot_storage_contract::StorageError::new(
            spot_storage_contract::ErrorKind::Internal,
            e.to_string(),
        )
    })
}

impl PageReader for MutatingReader {
    fn read<'a>(&'a self, query: &'a Query) -> spot_bulk_data::BoxFuture<'a, StorageResult<Rows>> {
        Box::pin(async move {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
                self.mutate().await?;
            }
            self.inner.read(query).await
        })
    }
}

#[tokio::test]
async fn concurrent_insert_update_delete_during_export() {
    let world = build(30).await;
    let behind = RecordId::new_v7();
    // `docs` is ordered by insertion; the export orders by (modified, id). Delete and update
    // documents that sort well after the first page.
    let delete_ahead = world.docs[28].id;
    let update_ahead = world.docs[29].id;
    let new_title = "changed ahead".to_owned();

    let spec = ExportSpec::csv(DOCTYPE).with_columns(&["id", "title"]);
    let plan = Arc::new(world.plan(&spec, &principal("analyst")));
    let inner: Arc<dyn PageReader> = Arc::new(spot_bulk_data::BackendPageReader::new(
        world.backend.clone(),
        world.catalog.clone(),
    ));
    let reader: Arc<dyn PageReader> = Arc::new(MutatingReader {
        inner,
        backend: world.backend.clone(),
        catalog: world.catalog.clone(),
        calls: AtomicUsize::new(0),
        behind,
        delete_ahead,
        update_ahead,
        new_title: new_title.clone(),
    });
    let ctx = StreamContext {
        reader,
        ..world.ctx("analyst", small_limits())
    };
    let mut stream = ExportStream::start(plan, ctx).unwrap();
    let mut text = String::new();
    while let Some(chunk) = stream.next_chunk().await.unwrap() {
        text.push_str(&String::from_utf8_lossy(&chunk.bytes));
    }
    let ids: Vec<&str> = text
        .lines()
        .skip(1)
        .filter_map(|l| l.split(',').next())
        .collect();
    assert!(
        !ids.contains(&behind.to_string().as_str()),
        "a row inserted behind was delivered"
    );
    assert!(
        !ids.contains(&delete_ahead.to_string().as_str()),
        "a row deleted ahead was delivered"
    );
    assert!(
        ids.contains(&update_ahead.to_string().as_str()),
        "a row changed ahead was not delivered"
    );
    assert!(
        text.contains(&new_title),
        "the changed row was not delivered in its current state"
    );
    // No id repeated.
    let set: BTreeSet<&str> = ids.iter().copied().collect();
    assert_eq!(set.len(), ids.len());
}

#[tokio::test]
async fn a_cmp_filter_is_applied() {
    let world = build(12).await;
    let filter = Expr::Cmp(
        Box::new(Expr::field(
            &name(spot_bulk_data::EXPORT_ALIAS),
            &name("title"),
        )),
        CmpOp::Eq,
        Box::new(Expr::Value(Value::Text("doc 3".into()))),
    );
    let spec = ExportSpec::csv(DOCTYPE).with_filter(filter);
    let ids = world.export_ids(&spec, "analyst").await;
    assert_eq!(ids.len(), 1);
    assert_eq!(ids[0], world.docs[3].id);
}

#[tokio::test]
async fn the_field_set_type_is_usable() {
    // A tiny guard that the public `FieldSet` is exported and usable.
    let fs: FieldSet = field_set();
    assert_eq!(fs.doctype, DOCTYPE);
    assert_eq!(fs.children[0].doctype, CHILD);
}

/// A coordinator that refuses every export.
struct DenyingCoordinator;

impl spot_bulk_data::BulkCoordinator for DenyingCoordinator {
    fn acquire(
        &self,
        _tenant: &str,
        _user: &str,
        _limits: &BulkLimits,
    ) -> spot_bulk_data::BulkResult<Box<dyn spot_bulk_data::ExportLease>> {
        Err(spot_bulk_data::BulkError::limit("too many exports"))
    }
}

/// A coordinator whose lease refuses the row budget.
struct RowLimitedCoordinator;

impl spot_bulk_data::BulkCoordinator for RowLimitedCoordinator {
    fn acquire(
        &self,
        _tenant: &str,
        _user: &str,
        _limits: &BulkLimits,
    ) -> spot_bulk_data::BulkResult<Box<dyn spot_bulk_data::ExportLease>> {
        struct Lease;
        impl spot_bulk_data::ExportLease for Lease {
            fn take_rows(&self, _rows: u64) -> spot_bulk_data::BulkResult<()> {
                Err(spot_bulk_data::BulkError::limit("rows per second"))
            }
        }
        Ok(Box::new(Lease))
    }
}

#[tokio::test]
async fn a_denied_coordinator_raises_bulk_limit() {
    let world = build(3).await;
    let plan = Arc::new(world.plan(&ExportSpec::csv(DOCTYPE), &principal("analyst")));
    let ctx = StreamContext {
        coordinator: Arc::new(DenyingCoordinator),
        ..world.ctx("analyst", small_limits())
    };
    let err = ExportStream::start(plan, ctx).unwrap_err();
    assert_eq!(err.code(), spot_bulk_data::LIMIT);
}

#[tokio::test]
async fn a_rate_budget_denial_raises_bulk_limit() {
    let world = build(3).await;
    let plan = Arc::new(world.plan(&ExportSpec::csv(DOCTYPE), &principal("analyst")));
    let ctx = StreamContext {
        coordinator: Arc::new(RowLimitedCoordinator),
        ..world.ctx("analyst", small_limits())
    };
    let mut stream = ExportStream::start(plan, ctx).unwrap();
    let err = stream.next_chunk().await.unwrap_err();
    assert_eq!(err.code(), spot_bulk_data::LIMIT);
}

#[tokio::test]
async fn the_max_rows_cap_is_enforced() {
    let world = build(10).await;
    let spec = ExportSpec::csv(DOCTYPE).with_max_rows(4);
    let plan = Arc::new(world.plan(&spec, &principal("analyst")));
    let mut stream = ExportStream::start(plan, world.ctx("analyst", small_limits())).unwrap();
    let mut error = None;
    loop {
        match stream.next_chunk().await {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e) => {
                error = Some(e);
                break;
            }
        }
    }
    assert_eq!(error.unwrap().code(), spot_bulk_data::LIMIT);
}

/// An audit sink that records the events, for the start/finish/cancel test.
#[derive(Default)]
struct RecordingAudit {
    events: std::sync::Mutex<Vec<spot_bulk_data::ExportAuditEvent>>,
}

impl spot_bulk_data::ExportAuditSink for RecordingAudit {
    fn record(&self, event: &spot_bulk_data::ExportAuditEvent) {
        self.events.lock().unwrap().push(event.clone());
    }
}

#[tokio::test]
async fn audit_records_start_finish_and_cancel_without_values() {
    let world = build(6).await;
    let spec = ExportSpec::csv(DOCTYPE).with_columns(&["id", "title"]);
    let plan = Arc::new(world.plan(&spec, &principal("analyst")));

    // A completed export: start then finish.
    let audit = Arc::new(RecordingAudit::default());
    let ctx = StreamContext {
        audit: audit.clone(),
        ..world.ctx("analyst", small_limits())
    };
    let mut stream = ExportStream::start(plan.clone(), ctx).unwrap();
    while stream.next_chunk().await.unwrap().is_some() {}
    drop(stream);
    {
        let events = audit.events.lock().unwrap();
        let phases: Vec<_> = events.iter().map(|e| e.phase).collect();
        assert_eq!(
            phases,
            vec![
                spot_bulk_data::ExportPhase::Start,
                spot_bulk_data::ExportPhase::Finish
            ]
        );
        for e in events.iter() {
            assert_eq!(e.doctype, DOCTYPE);
            assert!(!e.filter_digest.is_empty());
            assert_eq!(e.columns, vec!["id".to_owned(), "title".to_owned()]);
        }
    }

    // A dropped export: start then cancel.
    let audit = Arc::new(RecordingAudit::default());
    let ctx = StreamContext {
        audit: audit.clone(),
        ..world.ctx("analyst", small_limits())
    };
    let mut stream = ExportStream::start(plan, ctx).unwrap();
    stream.next_chunk().await.unwrap();
    drop(stream);
    let events = audit.events.lock().unwrap();
    let phases: Vec<_> = events.iter().map(|e| e.phase).collect();
    assert_eq!(
        phases,
        vec![
            spot_bulk_data::ExportPhase::Start,
            spot_bulk_data::ExportPhase::Cancel
        ]
    );
}
