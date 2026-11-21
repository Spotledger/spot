//! The Document handler through the real engine: registered in the handler registry, driven by a
//! compiled Process, with the Case file written through the in-memory document engine.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::sync::Arc;

use common::{block_on, engine, site};
use spot_bpm_documents::CaseFileStore;
use spot_bpm_documents::{DocumentNodeHandler, EngineDocumentOps, MemoryCaseFile};
use spot_bpm_engine::{CaseId, HandlerRegistry, Harness};
use spot_bpm_model::from_text;

/// A process whose only work node creates a bare note.
const PROCESS: &str = r#"
name = "order_process"
app = "bpm_documents_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "create_note"
kind = "document"
action = "create"
doctype = "note"

[[nodes]]
id = "done"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "create_note"

[[edges]]
from = "create_note"
to = "done"
"#;

#[test]
fn a_document_node_registers_and_runs_through_the_engine() {
    let document_engine = engine();
    let case_file = Arc::new(MemoryCaseFile::new());
    let ops = Arc::new(EngineDocumentOps::new(
        document_engine.clone(),
        site(),
        case_file.clone(),
    ));
    let mut handlers = HandlerRegistry::logic_default();
    DocumentNodeHandler::register(&mut handlers, ops);
    let harness = Harness::new(handlers);

    let process = from_text(PROCESS).unwrap();
    let case = CaseId::new("case_note").unwrap();
    block_on(harness.start(&process, case.clone(), "note-key")).unwrap();
    let record = block_on(harness.drive(&case, 50)).unwrap();
    assert!(record.run.is_finished(), "the Case finished");

    let entries = case_file.entries(&case).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].node.as_str(), "create_note");
    assert_eq!(entries[0].doctype.as_str(), "note");
    assert!(
        block_on(document_engine.peek("note", entries[0].document))
            .unwrap()
            .is_some(),
        "the note exists"
    );
}
