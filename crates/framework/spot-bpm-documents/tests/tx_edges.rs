//! The Case-file edge written in the document's own transaction, on the fake backend (the
//! PostgreSQL twin is `tx_edges_pg.rs`): success leaves the document and the edge, a failure on
//! either side leaves neither, a replay is a no-op, a cancel marks the edge in the cancelling
//! transaction.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::replay::Replaying;
use common::{block_on, meta, n, now, site};
use spot_app_api::prelude::{RecordId, SiteId, Value};
use spot_app_api::{AppError, AppResult, BoxFuture, ErrorClass};
use spot_bpm_documents::case_file::CaseFile;
use spot_bpm_documents::{
    CaseFileEdgeStore, CaseFileEdges, CaseFileError, DocumentOps, DocumentRequest, EdgeIntent,
    EngineDocumentOps, MemoryCaseFileEdges, Operation, Settled, TxCaseFile, codes,
};
use spot_bpm_engine::CaseId;
use spot_core::ManualClock;
use spot_dag::NodeId;
use spot_document::engine::{Engine, TxParticipant, V7Ids, WriteRecord};
use spot_edge_sync::{
    Edge, EdgeFuture, EdgeWriter, EdgeWriterFor, MemoryEdgeStore, MemoryEdgeWriter,
};
use spot_seams::RegistryBuilder;
use spot_storage_fake::{FakeBackend, FakeTx};

/// An edge factory that fails every write, standing in for a crash between the document write and
/// the edge write.
struct FailingEdges;

struct FailingWriter;

impl EdgeWriter for FailingWriter {
    fn add<'a>(&'a mut self, _edge: &'a Edge) -> EdgeFuture<'a, ()> {
        Box::pin(async {
            Err(spot_edge_sync::EdgeSyncError::WriteFailed {
                message: "injected".to_owned(),
            })
        })
    }
    fn remove<'a>(&'a mut self, _edge: &'a Edge) -> EdgeFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    fn update<'a>(&'a mut self, _edge: &'a Edge) -> EdgeFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    fn scan_by_source<'a>(
        &'a mut self,
        _source: &'a spot_storage_contract::NodeRef,
    ) -> EdgeFuture<'a, Vec<Edge>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

impl EdgeWriterFor<FakeTx> for FailingEdges {
    fn writer<'a>(&'a self, _tx: &'a mut FakeTx, _site: &'a SiteId) -> Box<dyn EdgeWriter + 'a> {
        Box::new(FailingWriter)
    }
}

/// A participant that refuses every write of `sales_order`, run after the Case-file one.
struct RefuseAfter;

impl TxParticipant<FakeTx> for RefuseAfter {
    fn after_write<'a>(
        &'a self,
        _tx: &'a mut FakeTx,
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

struct World {
    engine: Arc<Engine<FakeBackend>>,
    edges: MemoryEdgeStore,
    ops: EngineDocumentOps<Engine<FakeBackend>>,
}

fn world(
    install: impl FnOnce(&TxCaseFile, &MemoryEdgeStore) -> Vec<Arc<dyn TxParticipant<FakeTx>>>,
) -> World {
    let seams = Arc::new(RegistryBuilder::new().build().unwrap());
    let backend = FakeBackend::new();
    let edges = MemoryEdgeStore::new();
    let tx_file = TxCaseFile::new(Arc::new(MemoryCaseFileEdges::new(
        edges.clone(),
        backend.clone(),
        site(),
    )));
    let engine = Engine::new(
        backend,
        meta(),
        seams,
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap();
    block_on(engine.prepare()).unwrap();
    engine.set_participants(install(&tx_file, &edges));
    let engine = Arc::new(engine);
    let ops = EngineDocumentOps::in_transaction(engine.clone(), site(), Arc::new(tx_file));
    World { engine, edges, ops }
}

fn standard(tx_file: &TxCaseFile, edges: &MemoryEdgeStore) -> Vec<Arc<dyn TxParticipant<FakeTx>>> {
    vec![Arc::new(tx_file.participant::<FakeTx, _>(edges.clone()))]
}

fn request(
    operation: Operation,
    doctype: &str,
    document: Option<RecordId>,
    node: &str,
    attempt: u32,
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
        key: format!("{case}:{node}:{attempt}"),
        now: now(),
    }
}

fn committed_entries(w: &World, case: &str) -> Vec<spot_bpm_documents::Entry> {
    block_on(async {
        let mut writer: MemoryEdgeWriter = w.edges.committed(w.engine.backend()).await.unwrap();
        CaseFile::entries(&mut writer, &CaseId::new(case).unwrap())
            .await
            .unwrap()
    })
}

fn edge_count(w: &World) -> usize {
    block_on(w.edges.all(w.engine.backend())).unwrap().len()
}

#[test]
fn success_leaves_the_document_and_its_edge() {
    let w = world(standard);
    let receipt = w
        .ops
        .execute(&request(
            Operation::CreateFrom,
            "sales_order",
            None,
            "order",
            1,
            &[("title", "SO-1")],
        ))
        .unwrap();
    assert!(
        block_on(w.engine.peek("sales_order", receipt.document()))
            .unwrap()
            .is_some()
    );
    let entries = committed_entries(&w, "case_order");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].document, receipt.document());
    assert_eq!(entries[0].operation, Operation::CreateFrom);
}

#[test]
fn a_failure_between_the_document_write_and_the_edge_write_leaves_neither() {
    let w = world(|tx_file, _| vec![Arc::new(tx_file.participant::<FakeTx, _>(FailingEdges))]);
    let failure = w
        .ops
        .execute(&request(
            Operation::CreateFrom,
            "sales_order",
            None,
            "order",
            1,
            &[("title", "SO-1")],
        ))
        .unwrap_err();
    assert_eq!(failure.code(), codes::CASE_FILE);
    let id = spot_bpm_documents::ids::record_id_for("case_order:order:1:document");
    assert!(
        block_on(w.engine.peek("sales_order", id))
            .unwrap()
            .is_none(),
        "the document rolled back with the edge"
    );
    assert_eq!(edge_count(&w), 0);
}

#[test]
fn a_failure_after_the_edge_write_rolls_the_edge_back_with_the_document() {
    let w = world(|tx_file, edges| {
        vec![
            Arc::new(tx_file.participant::<FakeTx, _>(edges.clone())),
            Arc::new(RefuseAfter),
        ]
    });
    let failure = w
        .ops
        .execute(&request(
            Operation::CreateFrom,
            "sales_order",
            None,
            "order",
            1,
            &[("title", "SO-1")],
        ))
        .unwrap_err();
    assert_eq!(failure.code(), "test.refused");
    let id = spot_bpm_documents::ids::record_id_for("case_order:order:1:document");
    assert!(
        block_on(w.engine.peek("sales_order", id))
            .unwrap()
            .is_none()
    );
    assert_eq!(edge_count(&w), 0, "no edge without its document");
    assert!(committed_entries(&w, "case_order").is_empty());
}

#[test]
fn a_replay_of_the_same_key_is_a_no_op() {
    let w = world(standard);
    let create = request(
        Operation::CreateFrom,
        "sales_order",
        None,
        "order",
        1,
        &[("title", "SO-1")],
    );
    let first = w.ops.execute(&create).unwrap();
    let second = w.ops.execute(&create).unwrap();
    assert_eq!(first, second);
    assert_eq!(edge_count(&w), 1);
}

#[test]
fn a_second_node_on_the_same_document_updates_the_one_edge() {
    let w = world(standard);
    let created = w
        .ops
        .execute(&request(
            Operation::CreateFrom,
            "sales_order",
            None,
            "order",
            1,
            &[("title", "SO-1")],
        ))
        .unwrap();
    let updated = w
        .ops
        .execute(&request(
            Operation::Update,
            "sales_order",
            Some(created.document()),
            "revise",
            1,
            &[("note", "n")],
        ))
        .unwrap();
    assert_eq!(edge_count(&w), 1, "one edge per (case, document)");
    let entries = committed_entries(&w, "case_order");
    assert_eq!(entries[0].node.as_str(), "revise");
    assert_eq!(entries[0].version, updated.version());
}

#[test]
fn a_cancel_marks_the_edge_in_the_cancelling_transaction() {
    let w = world(standard);
    let created = w
        .ops
        .execute(&request(
            Operation::CreateFrom,
            "sales_order",
            None,
            "order",
            1,
            &[("title", "SO-1")],
        ))
        .unwrap();
    let id = created.document();
    w.ops
        .execute(&request(
            Operation::Submit,
            "sales_order",
            Some(id),
            "post",
            1,
            &[],
        ))
        .unwrap();
    assert_eq!(
        committed_entries(&w, "case_order")[0].operation,
        Operation::Submit
    );
    let cancelled = w
        .ops
        .execute(&request(
            Operation::Cancel,
            "sales_order",
            Some(id),
            "undo",
            1,
            &[],
        ))
        .unwrap();
    let entries = committed_entries(&w, "case_order");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].operation, Operation::Cancel);
    assert_eq!(entries[0].version, cancelled.version());
}

#[test]
fn a_participant_that_is_not_installed_is_reported_not_ignored() {
    let w = world(|_, _| Vec::new());
    let failure = w
        .ops
        .execute(&request(
            Operation::CreateFrom,
            "sales_order",
            None,
            "order",
            1,
            &[("title", "SO-1")],
        ))
        .unwrap_err();
    assert_eq!(failure.code(), codes::CASE_FILE);
    assert!(failure.reason().contains("participant"));
}

/// Counts the writes the engine hands its participants.
struct Counting(Arc<AtomicUsize>);

impl TxParticipant<FakeTx> for Counting {
    fn after_write<'a>(
        &'a self,
        _tx: &'a mut FakeTx,
        _write: &'a WriteRecord<'a>,
    ) -> BoxFuture<'a, AppResult<()>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

/// A durable read that answers from a list of (site) it knows an edge for, or fails.
struct Stub {
    knows: Vec<SiteId>,
    fail: bool,
}

impl CaseFileEdgeStore for Stub {
    fn edge_exists(
        &self,
        site: &SiteId,
        _case: &CaseId,
        _document: RecordId,
        _node: &NodeId,
        _operation: Operation,
    ) -> Result<bool, CaseFileError> {
        if self.fail {
            return Err(CaseFileError::new("injected read failure"));
        }
        Ok(self.knows.contains(site))
    }
}

type Restarted = EngineDocumentOps<Replaying<Engine<FakeBackend>>>;

fn restarted(w: &World, store: Arc<dyn CaseFileEdgeStore>) -> (Restarted, Arc<AtomicUsize>) {
    // A new process: new intents, a new participant, a new port, the same engine and store.
    let tx_file = TxCaseFile::new(store);
    let calls = Arc::new(AtomicUsize::new(0));
    w.engine.set_participants(vec![
        Arc::new(tx_file.participant::<FakeTx, _>(w.edges.clone())),
        Arc::new(Counting(calls.clone())),
    ]);
    // The command ledger remembers the first run's command id across the restart.
    let ledger = Arc::new(Replaying::seeded(w.engine.clone(), &["case_order:order:1"]));
    let ops = EngineDocumentOps::in_transaction(ledger, site(), Arc::new(tx_file));
    (ops, calls)
}

fn create_order() -> DocumentRequest {
    request(
        Operation::CreateFrom,
        "sales_order",
        None,
        "order",
        1,
        &[("title", "SO-1")],
    )
}

fn durable(w: &World) -> Arc<dyn CaseFileEdgeStore> {
    Arc::new(MemoryCaseFileEdges::new(
        w.edges.clone(),
        w.engine.backend().clone(),
        site(),
    ))
}

#[test]
fn a_repeated_command_id_after_a_restart_settles_from_the_durable_edge() {
    let w = world(standard);
    let first = w.ops.execute(&create_order()).unwrap();
    let (ops, calls) = restarted(&w, durable(&w));
    let again = ops.execute(&create_order()).unwrap();
    assert_eq!(again.document(), first.document());
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the engine answers the repeat as a replay and does not run the participants"
    );
    assert_eq!(edge_count(&w), 1, "no duplicate edge");
}

#[test]
fn a_repeated_command_id_whose_edge_was_never_written_is_not_run() {
    let w = world(|_, _| Vec::new());
    let tx_file = TxCaseFile::new(durable(&w));
    let document = spot_bpm_documents::ids::record_id_for("case_order:order:1:document");
    tx_file
        .expect(EdgeIntent {
            site: site(),
            key: "case_order:order:1".to_owned(),
            case: CaseId::new("case_order").unwrap(),
            node: NodeId::new("order").unwrap(),
            operation: Operation::CreateFrom,
            doctype: n("sales_order"),
            document,
            compensation: None,
        })
        .unwrap();
    assert_eq!(
        tx_file.settle(&site(), "case_order:order:1").unwrap(),
        Settled::NotRun
    );
}

#[test]
fn a_failing_durable_read_fails_the_node_and_never_reports_the_edge() {
    let w = world(standard);
    w.ops.execute(&create_order()).unwrap();
    let (ops, _) = restarted(
        &w,
        Arc::new(Stub {
            knows: vec![site()],
            fail: true,
        }),
    );
    let failure = ops.execute(&create_order()).unwrap_err();
    assert_eq!(failure.code(), codes::CASE_FILE);
    assert!(!failure.reason().contains("participant did not write"));
}

fn intent(site: SiteId, key: &str) -> EdgeIntent {
    EdgeIntent {
        site,
        key: key.to_owned(),
        case: CaseId::new("case_order").unwrap(),
        node: NodeId::new("order").unwrap(),
        operation: Operation::CreateFrom,
        doctype: n("sales_order"),
        document: spot_bpm_documents::ids::record_id_for("d"),
        compensation: None,
    }
}

#[test]
fn two_sites_with_one_command_id_never_share_a_slot_or_an_edge() {
    let a = site();
    let b = SiteId::new("bpm.other").unwrap();
    let tx_file = TxCaseFile::new(Arc::new(Stub {
        knows: vec![a.clone()],
        fail: false,
    }));
    tx_file.expect(intent(a.clone(), "k:1")).unwrap();
    tx_file
        .expect(intent(b.clone(), "k:1"))
        .expect("the same command id at another site is a different slot");
    assert_eq!(tx_file.settle(&b, "k:1").unwrap(), Settled::NotRun);
    assert_eq!(tx_file.settle(&a, "k:1").unwrap(), Settled::Written);
    assert_eq!(
        tx_file.settle(&a, "k:1").unwrap(),
        Settled::NotRun,
        "settled once"
    );
}

#[test]
fn the_participant_of_one_site_finds_no_slot_of_another() {
    // The slot is declared for another site, so a write at this site's invocation is not a
    // process write and the slot stays unwritten.
    let w = world(|_, _| Vec::new());
    let other = SiteId::new("bpm.other").unwrap();
    let tx_file = TxCaseFile::new(durable(&w));
    let calls = Arc::new(AtomicUsize::new(0));
    w.engine.set_participants(vec![
        Arc::new(tx_file.participant::<FakeTx, _>(w.edges.clone())),
        Arc::new(Counting(calls.clone())),
    ]);
    let request = create_order();
    let mut foreign = intent(other.clone(), &request.key);
    foreign.document = spot_bpm_documents::ids::record_id_for(&format!("{}:document", request.key));
    tx_file.expect(foreign).unwrap();
    let ops = EngineDocumentOps::in_transaction(w.engine.clone(), site(), Arc::new(tx_file));
    // This site's port declares its own slot; the other site's stays untouched.
    ops.execute(&request).unwrap();
    assert_eq!(edge_count(&w), 1);
    assert!(calls.load(Ordering::SeqCst) > 0);
}
