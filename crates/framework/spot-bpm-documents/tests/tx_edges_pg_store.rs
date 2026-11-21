//! `PgCaseFileEdgeStore`, the durable `edge_exists` read, on the real PostgreSQL backend. Needs
//! feature `pg` (`cargo test -p spot-bpm-documents --features pg`); without a server the test
//! prints `SKIPPED:` and returns, and `SPOT_REQUIRE_PG=1` turns that into a failure.
#![cfg(feature = "pg")]
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{meta, n, now, site};
use spot_app_api::prelude::{SiteId, Value};
use spot_bpm_documents::{
    CaseFileEdgeStore, DocumentOps, DocumentRequest, EngineDocumentOps, Operation,
    PgCaseFileEdgeStore, TxCaseFile,
};
use spot_bpm_engine::CaseId;
use spot_core::ManualClock;
use spot_dag::NodeId;
use spot_document::engine::{Engine, TxParticipant, V7Ids};
use spot_edge_pg::PgEdgeWriters;
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgTransaction};
use spot_seams::RegistryBuilder;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn edge_exists_reads_the_committed_edge_for_its_own_site_only() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let catalog = Arc::new(
        meta()
            .catalog(spot_storage_contract::Collation::new("und").unwrap())
            .unwrap(),
    );
    let reader = Arc::new(PgCaseFileEdgeStore::new(
        PgBackend::connect(db.config()).await.expect("connect"),
        catalog,
        site(),
    ));
    let engine = Engine::new(
        PgBackend::connect(db.config()).await.expect("connect"),
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
    let tx_file = TxCaseFile::new(reader.clone());
    let participants: Vec<Arc<dyn TxParticipant<PgTransaction>>> = vec![Arc::new(
        tx_file.participant::<PgTransaction, _>(PgEdgeWriters),
    )];
    engine.set_participants(participants);
    let ops = Arc::new(EngineDocumentOps::in_transaction(
        Arc::new(engine),
        site(),
        Arc::new(tx_file),
    ));
    let case = CaseId::new("case_order").unwrap();
    let node = NodeId::new("order").unwrap();
    let request = DocumentRequest {
        operation: Operation::CreateFrom,
        doctype: n("sales_order"),
        document: None,
        fields: BTreeMap::from([(n("title"), Value::Text("SO-1".to_owned()))]),
        expected: None,
        compensation: None,
        principal: None,
        case: case.clone(),
        node: node.clone(),
        key: "case_order:order:1".to_owned(),
        now: now(),
    };
    let receipt = tokio::task::spawn_blocking(move || ops.execute(&request))
        .await
        .unwrap()
        .unwrap();
    let document = receipt.document();
    let other = SiteId::new("bpm.other").unwrap();
    let wrong_node = NodeId::new("elsewhere").unwrap();
    let r = reader.clone();
    let answers = tokio::task::spawn_blocking(move || {
        (
            r.edge_exists(&site(), &case, document, &node, Operation::CreateFrom)
                .unwrap(),
            r.edge_exists(&other, &case, document, &node, Operation::CreateFrom)
                .unwrap(),
            r.edge_exists(&site(), &case, document, &wrong_node, Operation::CreateFrom)
                .unwrap(),
            r.edge_exists(&site(), &case, document, &node, Operation::Submit)
                .unwrap(),
        )
    })
    .await
    .unwrap();
    assert_eq!(answers, (true, false, false, false));
}
