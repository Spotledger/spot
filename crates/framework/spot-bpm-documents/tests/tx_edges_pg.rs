//! The Case-file edge written in the document's own transaction on the real PostgreSQL backend
//! (the fake-backend twin is `tx_edges.rs`): success leaves the document and the edge, a failure
//! after the document write leaves neither, a replay is a no-op, a cancel marks the edge, and the
//! edge set equals the link set (`check_invariant`) with the Case-file edges present.
//!
//! Each test makes its own scratch database. Without a server the tests print `SKIPPED:` and
//! return; `SPOT_REQUIRE_PG=1` turns that into a failure.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::replay::Replaying;
use common::{block_on, meta, n, now, site};
use spot_app_api::prelude::{RecordId, SiteId, Value};
use spot_app_api::{AppError, AppResult, BoxFuture, ErrorClass};
use spot_bpm_documents::case_file::CaseFile;
use spot_bpm_documents::{
    CaseFileEdgeStore, CaseFileError, DocumentOps, DocumentRequest, EngineDocumentOps, Entry,
    Operation, TxCaseFile,
};
use spot_bpm_engine::CaseId;
use spot_core::ManualClock;
use spot_dag::NodeId;
use spot_document::engine::{Engine, TxParticipant, V7Ids, WriteRecord};
use spot_edge_pg::{PgEdgeWriter, PgEdgeWriters};
use spot_edge_sync::{EdgeSchema, check_invariant};
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgTransaction};
use spot_seams::RegistryBuilder;
use spot_storage_contract::{Isolation, NodeRef, StorageBackend, Transaction, TxOptions};

type PgEngine = Engine<PgBackend>;

/// Refuses every write of a `sales_order`, run after the Case-file participant.
struct RefuseAfter;

impl TxParticipant<PgTransaction> for RefuseAfter {
    fn after_write<'a>(
        &'a self,
        _tx: &'a mut PgTransaction,
        write: &'a WriteRecord<'a>,
    ) -> BoxFuture<'a, AppResult<()>> {
        let refuse = write.doctype == "sales_order";
        Box::pin(async move {
            if refuse {
                return Err(AppError::new(
                    ErrorClass::Internal,
                    "test.refused",
                    "refused",
                ));
            }
            Ok(())
        })
    }
}

/// The durable edge read on its own connection to the same database (a test-local twin of
/// `PgCaseFileEdgeStore`, which `tx_edges_pg_store.rs` covers under feature `pg`).
struct OwnConnection {
    backend: PgBackend,
    catalog: Arc<spot_storage_contract::Catalog>,
}

impl CaseFileEdgeStore for OwnConnection {
    fn edge_exists(
        &self,
        site_id: &SiteId,
        case: &CaseId,
        document: RecordId,
        node: &NodeId,
        operation: Operation,
    ) -> Result<bool, CaseFileError> {
        if site_id != &site() {
            return Ok(false);
        }
        let unreadable = || CaseFileError::new("the Case-file edge could not be read");
        block_on(async {
            let options = TxOptions::new(n("case_file_read"), Isolation::ReadCommitted);
            let mut tx = self
                .backend
                .begin(self.catalog.clone(), options)
                .await
                .map_err(|_| unreadable())?;
            let read = {
                let mut writer = PgEdgeWriter::new(&mut tx);
                CaseFile::entries(&mut writer, case).await
            };
            tx.rollback().await.map_err(|_| unreadable())?;
            let entries = read.map_err(|_| unreadable())?;
            Ok(entries
                .iter()
                .any(|e| e.document == document && &e.node == node && e.operation == operation))
        })
    }
}

async fn store(db: &TestDb) -> Arc<OwnConnection> {
    let backend = PgBackend::connect(db.config()).await.expect("connect");
    let catalog = Arc::new(
        meta()
            .catalog(spot_storage_contract::Collation::new("und").unwrap())
            .unwrap(),
    );
    Arc::new(OwnConnection { backend, catalog })
}

struct Fx {
    /// Kept alive so the scratch database lives as long as the fixture.
    db: TestDb,
    engine: Arc<PgEngine>,
    ops: Arc<EngineDocumentOps<PgEngine>>,
}

/// `None` without a server. `refuse_after` adds the participant that fails after the edge write.
async fn fx(refuse_after: bool) -> Option<Fx> {
    let db = TestDb::create().await?;
    let backend = PgBackend::connect(db.config()).await.expect("connect");
    let engine = Engine::new(
        backend,
        meta(),
        Arc::new(RegistryBuilder::new().build().unwrap()),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap();
    engine.prepare().await.expect("prepare documents");
    {
        let mut client = db.raw_client().await;
        spot_edge_pg::schema::prepare(&mut client).await.unwrap();
    }
    let tx_file = TxCaseFile::new(store(&db).await);
    let mut participants: Vec<Arc<dyn TxParticipant<PgTransaction>>> = vec![Arc::new(
        tx_file.participant::<PgTransaction, _>(PgEdgeWriters),
    )];
    if refuse_after {
        participants.push(Arc::new(RefuseAfter));
    }
    engine.set_participants(participants);
    let engine = Arc::new(engine);
    let ops = Arc::new(EngineDocumentOps::in_transaction(
        engine.clone(),
        site(),
        Arc::new(tx_file),
    ));
    Some(Fx { db, engine, ops })
}

fn request(
    operation: Operation,
    doctype: &str,
    document: Option<RecordId>,
    node: &str,
    fields: &[(&str, &str)],
) -> DocumentRequest {
    let case = CaseId::new("case_order").unwrap();
    DocumentRequest {
        operation,
        doctype: n(doctype),
        document,
        fields: fields
            .iter()
            .map(|(k, v)| (n(k), Value::Text((*v).to_owned())))
            .collect::<BTreeMap<_, _>>(),
        expected: None,
        compensation: None,
        principal: None,
        case: case.clone(),
        node: NodeId::new(node).unwrap(),
        key: format!("{case}:{node}:1"),
        now: now(),
    }
}

impl Fx {
    /// Runs one operation on a blocking thread: the port bridges the engine's async API with a
    /// polling loop, which must not occupy a runtime worker.
    async fn execute(
        &self,
        request: DocumentRequest,
    ) -> Result<spot_bpm_documents::DocumentReceipt, spot_bpm_documents::DocumentFailure> {
        let ops = self.ops.clone();
        tokio::task::spawn_blocking(move || ops.execute(&request))
            .await
            .unwrap()
    }

    /// The Case-file entries of `case`, read through a fresh transaction.
    async fn entries(&self, case: &str) -> Vec<Entry> {
        let catalog = Arc::new(
            meta()
                .catalog(spot_storage_contract::Collation::new("und").unwrap())
                .unwrap(),
        );
        let options = TxOptions::new(n("case_file_read"), Isolation::ReadCommitted);
        let mut tx = self.engine.backend().begin(catalog, options).await.unwrap();
        let entries = {
            let mut writer = PgEdgeWriter::new(&mut tx);
            CaseFile::entries(&mut writer, &CaseId::new(case).unwrap())
                .await
                .unwrap()
        };
        tx.rollback().await.unwrap();
        entries
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn success_leaves_the_document_and_its_edge() {
    let Some(f) = fx(false).await else {
        return;
    };
    let receipt = f
        .execute(request(
            Operation::CreateFrom,
            "sales_order",
            None,
            "order",
            &[("title", "SO-1")],
        ))
        .await
        .unwrap();
    assert!(
        f.engine
            .peek("sales_order", receipt.document())
            .await
            .unwrap()
            .is_some()
    );
    let entries = f.entries("case_order").await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].document, receipt.document());
    assert_eq!(f.entries("case_order").await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failure_after_the_edge_write_leaves_neither_the_edge_nor_the_document() {
    let Some(f) = fx(true).await else {
        return;
    };
    let failure = f
        .execute(request(
            Operation::CreateFrom,
            "sales_order",
            None,
            "order",
            &[("title", "SO-1")],
        ))
        .await
        .unwrap_err();
    assert_eq!(failure.code(), "test.refused");
    let id = spot_bpm_documents::ids::record_id_for("case_order:order:1:document");
    assert!(f.engine.peek("sales_order", id).await.unwrap().is_none());
    assert_eq!(f.entries("case_order").await.len(), 0);
    assert!(f.entries("case_order").await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replay_is_a_no_op_and_a_cancel_marks_the_edge() {
    let Some(f) = fx(false).await else {
        return;
    };
    let create = request(
        Operation::CreateFrom,
        "sales_order",
        None,
        "order",
        &[("title", "SO-1")],
    );
    let first = f.execute(create.clone()).await.unwrap();
    let second = f.execute(create).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(f.entries("case_order").await.len(), 1);

    let id = first.document();
    f.execute(request(
        Operation::Submit,
        "sales_order",
        Some(id),
        "post",
        &[],
    ))
    .await
    .unwrap();
    assert_eq!(
        f.entries("case_order").await[0].operation,
        Operation::Submit
    );
    let cancelled = f
        .execute(request(
            Operation::Cancel,
            "sales_order",
            Some(id),
            "undo",
            &[],
        ))
        .await
        .unwrap();
    let entries = f.entries("case_order").await;
    assert_eq!(entries.len(), 1, "one edge per (case, document)");
    assert_eq!(entries[0].operation, Operation::Cancel);
    assert_eq!(entries[0].version, cancelled.version());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_edge_set_still_equals_the_link_set() {
    let Some(f) = fx(false).await else {
        return;
    };
    let order = f
        .execute(request(
            Operation::CreateFrom,
            "sales_order",
            None,
            "order",
            &[("title", "SO-1")],
        ))
        .await
        .unwrap();
    assert_eq!(f.entries("case_order").await.len(), 1);
    // The Case-file edge is not a link-derived edge: the link invariant over the live document
    // reports no drift once the Case-file edge type is left out of the stored set.
    let doc = f
        .engine
        .peek("sales_order", order.document())
        .await
        .unwrap()
        .unwrap();
    let schema = EdgeSchema::derive(&meta(), &[]);
    let owner = NodeRef {
        entity: n("sales_order"),
        id: order.document(),
    };
    let catalog = Arc::new(
        meta()
            .catalog(spot_storage_contract::Collation::new("und").unwrap())
            .unwrap(),
    );
    let mut tx = f
        .engine
        .backend()
        .begin(
            catalog,
            TxOptions::new(n("verify"), Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    let stored = PgEdgeWriter::new(&mut tx)
        .edges_owned_by(&owner)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert!(check_invariant(&schema, [&doc], stored).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repeated_command_id_after_a_restart_settles_from_the_committed_edge() {
    let Some(f) = fx(false).await else {
        return;
    };
    let create = request(
        Operation::CreateFrom,
        "sales_order",
        None,
        "order",
        &[("title", "SO-1")],
    );
    let first = f.execute(create.clone()).await.unwrap();
    // A new process: new intents, a new participant and port over the same database. The
    // engine answers the repeat as a replay and does not run the participant.
    let tx_file = TxCaseFile::new(store(&f.db).await);
    f.engine.set_participants(vec![Arc::new(
        tx_file.participant::<PgTransaction, _>(PgEdgeWriters),
    )]);
    let ledger = Arc::new(Replaying::seeded(f.engine.clone(), &["case_order:order:1"]));
    let ops = Arc::new(EngineDocumentOps::in_transaction(
        ledger,
        site(),
        Arc::new(tx_file),
    ));
    let again = tokio::task::spawn_blocking(move || ops.execute(&create))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.document(), first.document());
    assert_eq!(f.entries("case_order").await.len(), 1, "no duplicate edge");
}
