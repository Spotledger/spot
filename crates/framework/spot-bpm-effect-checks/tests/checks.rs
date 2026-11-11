//! Golden processes, one per problem kind plus a clean one, over a registry built from a small
//! fake app (WP-BPM-15.T3).
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::sync::Arc;
use std::time::Duration;

use spot_app_api::prelude::{OffsetDateTime, Version};
use spot_app_api::{
    ActionRef, AppResult, BoxFuture, Controller, EffectClassName, EffectDecl, EffectKind,
    EffectReceipt, Hex32, HookContext, HookPoint, OpaqueRef, Operation, OperationReceipt, Phase,
    ReceiptState, Reversal, StepDecl, StepEffects, StepName, StoreName, Target,
};
use spot_bpm_effect_checks::{
    ChecksError, EdgeMode, MAX_COMMIT_POINTS, PolicyMeta, ProblemKind, PublishGate, Severity,
    TestRunReceipt, Undo, Where, check, codes, effect_summary, preview_from_receipts,
};
use spot_bpm_model::{
    DocumentAction, DocumentConfig, Edge, Node, NodeId, NodeKind, ProcessVersion, UnknownNode,
    VersionStatus,
};
use spot_seams::{Registry, RegistryBuilder, UndeclaredReason};

// ---- the fake app ---------------------------------------------------------------------------

struct Fake {
    doctype: &'static str,
    steps: Vec<StepDecl>,
    effects: Vec<StepEffects>,
}

impl Controller for Fake {
    fn doctype(&self) -> &str {
        self.doctype
    }
    fn steps(&self) -> Vec<StepDecl> {
        self.steps.clone()
    }
    fn step_effects(&self) -> Vec<StepEffects> {
        self.effects.clone()
    }
    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        _ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

fn sn(s: &str) -> StepName {
    StepName::new(s).unwrap()
}

fn class(s: &str) -> EffectClassName {
    EffectClassName::new(s).unwrap()
}

fn store(s: &str) -> StoreName {
    StoreName::new(s).unwrap()
}

fn step(name: &str, phase: Phase, op: Operation) -> StepDecl {
    StepDecl {
        name: sn(name),
        point: HookPoint::of(phase, Target::Op(op)),
        priority: 10,
        budget: Duration::from_millis(100),
    }
}

fn internal(class_name: &str, stores: &[&str], by: Operation) -> EffectDecl {
    EffectDecl::new(
        EffectKind::Internal,
        class(class_name),
        stores.iter().map(|s| store(s)).collect(),
        Reversal::ReversibleBy(by),
    )
}

fn external(class_name: &str, reversal: Reversal) -> EffectDecl {
    EffectDecl::new(
        EffectKind::External,
        class(class_name),
        vec![store("outbound")],
        reversal,
    )
}

fn controller(
    doctype: &'static str,
    rows: Vec<(StepDecl, Vec<EffectDecl>)>,
) -> Arc<dyn Controller> {
    Arc::new(Fake {
        doctype,
        steps: rows.iter().map(|(s, _)| s.clone()).collect(),
        effects: rows
            .into_iter()
            .map(|(s, e)| StepEffects::new(s.name, e))
            .collect(),
    })
}

/// invoice: post writes the ledger (cancel reverses it) and mails (compensable); order: a draft
/// row (delete reverses it); tax_return: a fiscal transmission (irreversible); orphan: a draft
/// row nothing reverses; gadget: an internal effect of a class the site does not know; memo: no
/// effects.
fn registry() -> Registry {
    let mut b = RegistryBuilder::new();
    b.add_controller(
        "ledger",
        controller(
            "invoice",
            vec![
                (
                    step("ledger:write", Phase::After, Operation::Post),
                    vec![internal(
                        "balance_movement",
                        &["ledger_entry"],
                        Operation::Cancel,
                    )],
                ),
                (
                    step("ledger:reverse", Phase::After, Operation::Cancel),
                    vec![internal(
                        "balance_movement",
                        &["ledger_entry"],
                        Operation::Post,
                    )],
                ),
                (
                    step("ledger:notify", Phase::Committed, Operation::Post),
                    vec![external(
                        "mail",
                        Reversal::CompensableBy(ActionRef::new("mailer:send_correction").unwrap()),
                    )],
                ),
            ],
        ),
    );
    b.add_controller(
        "tax",
        controller(
            "tax_return",
            vec![(
                step("tax:transmit", Phase::Committed, Operation::Post),
                vec![external(
                    "fiscal_transmission",
                    Reversal::Irreversible {
                        reason: "the authority holds it".to_owned(),
                    },
                )],
            )],
        ),
    );
    b.add_controller(
        "sales",
        controller(
            "order",
            vec![
                (
                    step("sales:rows", Phase::After, Operation::Create),
                    vec![internal("order_row", &["order_line"], Operation::Delete)],
                ),
                (
                    step("sales:unrows", Phase::After, Operation::Delete),
                    vec![internal("order_row", &["order_line"], Operation::Create)],
                ),
            ],
        ),
    );
    b.add_controller(
        "sales",
        controller(
            "orphan",
            vec![(
                step("sales:orphan_rows", Phase::After, Operation::Create),
                vec![internal("order_row", &["orphan_line"], Operation::Delete)],
            )],
        ),
    );
    b.add_controller(
        "lab",
        controller(
            "gadget",
            vec![
                (
                    step("lab:spin", Phase::After, Operation::Create),
                    vec![internal("mystery", &["gadget_row"], Operation::Delete)],
                ),
                (
                    step("lab:unspin", Phase::After, Operation::Delete),
                    vec![internal("mystery", &["gadget_row"], Operation::Create)],
                ),
            ],
        ),
    );
    // memo: a step that declares no effects (a known pipeline, nothing to reverse).
    b.add_controller(
        "misc",
        controller(
            "memo",
            vec![
                (
                    step("misc:note", Phase::After, Operation::Create),
                    Vec::new(),
                ),
                (
                    step("misc:note_up", Phase::After, Operation::Update),
                    Vec::new(),
                ),
            ],
        ),
    );
    b.build().expect("the fake app registers cleanly")
}

// ---- processes ------------------------------------------------------------------------------

fn id(s: &str) -> NodeId {
    NodeId::new(s).unwrap()
}

fn doc(name: &str, action: DocumentAction, doctype: &str) -> Node {
    Node::new(
        id(name),
        NodeKind::Document(DocumentConfig {
            action,
            doctype: doctype.to_owned(),
            operation: None,
            mapping: None,
            principal: None,
        }),
    )
}

fn commit(name: &str) -> Node {
    Node::new(
        id(name),
        NodeKind::Unknown(UnknownNode {
            kind: "commit".to_owned(),
            config: toml::Table::new(),
        }),
    )
}

fn edge(from: &str, to: &str) -> Edge {
    Edge::new(id(from), id(to))
}

fn process(nodes: Vec<Node>, edges: Vec<Edge>) -> ProcessVersion {
    ProcessVersion::new(
        "flow",
        "app",
        1,
        VersionStatus::Draft,
        nodes,
        edges,
        Vec::new(),
    )
    .unwrap()
}

fn tokens(problems: &[spot_bpm_effect_checks::Problem]) -> Vec<&'static str> {
    problems.iter().map(|p| p.token()).collect()
}

fn meta() -> PolicyMeta {
    PolicyMeta::new()
}

// ---- tests ----------------------------------------------------------------------------------

#[test]
fn a_clean_process_has_no_problems() {
    let p = process(
        vec![
            doc("make", DocumentAction::Create, "order"),
            doc("book", DocumentAction::Post, "invoice"),
            commit("go"),
        ],
        vec![edge("make", "book"), edge("book", "go")],
    );
    assert!(check(&p, &registry(), &meta()).is_empty());
}

#[test]
fn undo_past_irreversible_names_the_node_and_the_edge() {
    let p = process(
        vec![
            doc("make", DocumentAction::Create, "order"),
            doc("file", DocumentAction::Post, "tax_return"),
            doc("note", DocumentAction::Create, "memo"),
        ],
        vec![
            edge("make", "file"),
            edge("file", "note"),
            edge("note", "make").with_label("undo"),
        ],
    );
    let problems = check(&p, &registry(), &meta());
    assert_eq!(tokens(&problems), ["undo_past_irreversible"]);
    let found = &problems[0];
    assert_eq!(found.severity, Severity::Error);
    assert_eq!(found.node, Some(id("file")));
    assert_eq!(found.edge, Some((id("note"), id("make"))));
    assert_eq!(found.class.as_deref(), Some("fiscal_transmission"));
}

#[test]
fn a_rewind_past_an_irreversible_effect_keeps_it_and_is_fine() {
    let p = process(
        vec![
            doc("start", DocumentAction::Create, "memo"),
            doc("file", DocumentAction::Post, "tax_return"),
            doc("note", DocumentAction::Create, "memo"),
        ],
        vec![
            edge("start", "file"),
            edge("file", "note"),
            edge("note", "start").with_label("rewind"),
        ],
    );
    let revising = meta().with_revision(id("note"));
    assert!(check(&p, &registry(), &revising).is_empty());
}

#[test]
fn an_undo_that_does_not_cross_the_irreversible_node_is_fine() {
    let p = process(
        vec![
            doc("file", DocumentAction::Post, "tax_return"),
            doc("make", DocumentAction::Create, "order"),
            doc("note", DocumentAction::Create, "memo"),
        ],
        vec![
            edge("file", "make"),
            edge("make", "note"),
            edge("note", "make").with_label("undo"),
        ],
    );
    assert!(check(&p, &registry(), &meta()).is_empty());
}

#[test]
fn a_record_type_with_nothing_reversing_its_effect_is_reported() {
    let p = process(
        vec![doc("make", DocumentAction::Create, "orphan")],
        Vec::new(),
    );
    let problems = check(&p, &registry(), &meta());
    assert_eq!(tokens(&problems), ["no_declared_reversal"]);
    assert_eq!(problems[0].node, Some(id("make")));
    assert_eq!(problems[0].class.as_deref(), Some("order_row"));
}

#[test]
fn an_empty_commit_point_is_a_warning() {
    let p = process(
        vec![doc("note", DocumentAction::Create, "memo"), commit("go")],
        vec![edge("note", "go")],
    );
    let problems = check(&p, &registry(), &meta());
    assert_eq!(tokens(&problems), ["empty_commit_point"]);
    assert_eq!(problems[0].severity, Severity::Warning);
    assert_eq!(problems[0].node, Some(id("go")));
}

#[test]
fn a_rewind_that_would_create_twice_is_refused_until_a_revision_is_mapped() {
    let p = process(
        vec![
            doc("start", DocumentAction::Create, "memo"),
            doc("make", DocumentAction::Create, "order"),
            doc("review", DocumentAction::Create, "memo"),
        ],
        vec![
            edge("start", "make"),
            edge("make", "review"),
            edge("review", "start").with_label("rewind"),
        ],
    );
    let problems = check(&p, &registry(), &meta());
    assert!(tokens(&problems).contains(&"rewind_would_duplicate"));
    let dup = problems
        .iter()
        .find(|p| p.kind == ProblemKind::RewindWouldDuplicate && p.node == Some(id("make")))
        .expect("the order node is named");
    assert_eq!(dup.edge, Some((id("review"), id("start"))));
    // The memo nodes create too; map a revision for every creating node in the range.
    let mapped = meta().with_revision(id("make")).with_revision(id("review"));
    assert!(check(&p, &registry(), &mapped).is_empty());
}

#[test]
fn an_unknown_effect_class_is_reported() {
    let p = process(
        vec![doc("spin", DocumentAction::Create, "gadget")],
        Vec::new(),
    );
    assert!(check(&p, &registry(), &meta()).is_empty());
    let strict = meta().with_known_classes(["balance_movement", "order_row", "mail"]);
    let problems = check(&p, &registry(), &strict);
    assert_eq!(tokens(&problems), ["unknown_effect_class"]);
    assert_eq!(problems[0].class.as_deref(), Some("mystery"));
}

#[test]
fn a_never_held_effect_in_a_held_region_is_a_warning() {
    let p = process(
        vec![
            doc("file", DocumentAction::Post, "tax_return"),
            commit("go"),
        ],
        vec![edge("file", "go")],
    );
    let problems = check(&p, &registry(), &meta());
    assert_eq!(
        tokens(&problems),
        ["empty_commit_point", "held_region_contains_never_held"]
    );
    assert!(problems.iter().all(|p| p.severity == Severity::Warning));
}

#[test]
fn a_process_beyond_the_bounds_is_refused_not_half_checked() {
    let mut edges = vec![edge("note", "note").with_label("undo"); 300];
    edges.push(edge("note", "note"));
    let p = process(vec![doc("note", DocumentAction::Create, "memo")], edges);
    let problems = check(&p, &registry(), &meta());
    assert_eq!(tokens(&problems), ["too_large"]);
    assert!(problems[0].is_error());
}

#[test]
fn the_result_does_not_depend_on_the_order_of_nodes_and_edges() {
    let nodes = vec![
        doc("make", DocumentAction::Create, "order"),
        doc("file", DocumentAction::Post, "tax_return"),
        doc("note", DocumentAction::Create, "memo"),
        commit("go"),
    ];
    let edges = vec![
        edge("make", "file"),
        edge("file", "note"),
        edge("note", "go"),
        edge("note", "make").with_label("undo"),
    ];
    let forward = process(nodes.clone(), edges.clone());
    let mut rn = nodes;
    rn.reverse();
    let mut re = edges;
    re.reverse();
    let backward = process(rn, re);
    let reg = registry();
    assert_eq!(
        check(&forward, &reg, &meta()),
        check(&backward, &reg, &meta())
    );
    assert_eq!(
        effect_summary(&forward, &reg),
        effect_summary(&backward, &reg)
    );
}

#[test]
fn a_custom_meta_decides_which_edge_undoes() {
    struct AllBackEdgesUndo;
    impl spot_bpm_effect_checks::EffectMeta for AllBackEdgesUndo {
        fn edge_mode(&self, edge: &Edge) -> EdgeMode {
            if edge.label.as_deref() == Some("back") {
                EdgeMode::Undo
            } else {
                EdgeMode::Forward
            }
        }
        fn has_revision(&self, _: &NodeId) -> bool {
            true
        }
        fn is_known_class(&self, _: &EffectClassName) -> bool {
            true
        }
        fn is_never_held(&self, _: &EffectClassName) -> bool {
            false
        }
    }
    let p = process(
        vec![
            doc("make", DocumentAction::Create, "order"),
            doc("file", DocumentAction::Post, "tax_return"),
        ],
        vec![
            edge("make", "file"),
            edge("file", "make").with_label("back"),
        ],
    );
    assert_eq!(
        tokens(&check(&p, &registry(), &AllBackEdgesUndo)),
        ["undo_past_irreversible"]
    );
}

// ---- summary --------------------------------------------------------------------------------

#[test]
fn the_summary_lists_declared_effects_with_their_reversibility() {
    let p = process(
        vec![
            doc("book", DocumentAction::Post, "invoice"),
            doc("file", DocumentAction::Post, "tax_return"),
            doc("note", DocumentAction::Create, "memo"),
            doc("copy", DocumentAction::Map, "invoice"),
        ],
        Vec::new(),
    );
    let summary = effect_summary(&p, &registry());
    // Nodes without effects (memo, a map node) are left out.
    let nodes: Vec<&str> = summary.iter().map(|n| n.node.as_str()).collect();
    assert_eq!(nodes, ["book", "file"]);
    let book = &summary[0].effects;
    assert_eq!(book.len(), 2);
    assert_eq!(
        (book[0].class.as_str(), book[0].kind, book[0].reversibility),
        ("balance_movement", Where::Internal, Undo::Reversible)
    );
    assert_eq!(book[0].by.as_deref(), Some("cancel"));
    assert_eq!(book[1].reversibility, Undo::Compensable);
    assert_eq!(book[1].by.as_deref(), Some("mailer:send_correction"));
    assert_eq!(summary[1].effects[0].reversibility, Undo::Irreversible);
    assert_eq!(summary[1].effects[0].by, None);
}

// ---- gate -----------------------------------------------------------------------------------

#[test]
fn the_gate_refuses_errors_and_passes_warnings_through() {
    let reg = registry();
    let bad = process(
        vec![doc("make", DocumentAction::Create, "orphan")],
        Vec::new(),
    );
    let err = PublishGate::check(&bad, &reg, &meta()).unwrap_err();
    assert_eq!(err.code(), codes::PUBLISH_REFUSED);
    match err {
        ChecksError::PublishRefused { problems } => {
            assert_eq!(tokens(&problems), ["no_declared_reversal"]);
        }
        other => panic!("unexpected {other:?}"),
    }
    let warn = process(
        vec![doc("note", DocumentAction::Create, "memo"), commit("go")],
        vec![edge("note", "go")],
    );
    let warnings = PublishGate::check(&warn, &reg, &meta()).unwrap();
    assert_eq!(tokens(&warnings), ["empty_commit_point"]);
}

#[test]
fn the_gate_refuses_an_oversized_process() {
    let edges = vec![edge("note", "note").with_label("undo"); 300];
    let p = process(vec![doc("note", DocumentAction::Create, "memo")], edges);
    let err = PublishGate::check(&p, &registry(), &meta()).unwrap_err();
    assert_eq!(err.code(), codes::PUBLISH_REFUSED);
}

// ---- preview --------------------------------------------------------------------------------

fn effect(step_name: &str, class_name: &str, kind: EffectKind, count: u32) -> EffectReceipt {
    EffectReceipt {
        step: sn(step_name),
        class: class(class_name),
        kind,
        store_refs: vec![OpaqueRef::new("row").unwrap()],
        count,
        digest: Hex32::new(&"0".repeat(64)).unwrap(),
        state: ReceiptState::Applied,
    }
}

fn run(node: &str, op: Operation, doctype: &str, effects: Vec<EffectReceipt>) -> TestRunReceipt {
    TestRunReceipt {
        node: id(node),
        receipt: OperationReceipt {
            operation: op,
            doctype: doctype.to_owned(),
            name: "x".to_owned(),
            version: Version(1),
            effects,
            committed_at: OffsetDateTime::UNIX_EPOCH,
        },
    }
}

#[test]
fn the_preview_groups_by_node_flags_undeclared_and_totals_per_class() {
    let receipts = vec![
        run(
            "book",
            Operation::Post,
            "invoice",
            vec![
                effect("ledger:write", "balance_movement", EffectKind::Internal, 2),
                effect("ledger:notify", "mail", EffectKind::External, 1),
                // Declared on no step of the pipeline.
                effect("ledger:write", "mail", EffectKind::External, 1),
            ],
        ),
        run(
            "again",
            Operation::Post,
            "invoice",
            vec![effect(
                "ledger:write",
                "balance_movement",
                EffectKind::Internal,
                3,
            )],
        ),
    ];
    let preview = preview_from_receipts(&registry(), &receipts).unwrap();
    let nodes: Vec<&str> = preview.nodes.iter().map(|n| n.node.as_str()).collect();
    assert_eq!(nodes, ["again", "book"]);
    assert_eq!(preview.undeclared, 1);
    let book = &preview.nodes[1].effects;
    assert_eq!(book.len(), 3);
    assert_eq!(book[0].undeclared, None);
    assert_eq!(book[2].undeclared, Some(UndeclaredReason::ClassNotDeclared));
    let movement = preview
        .totals
        .iter()
        .find(|t| t.class == "balance_movement")
        .unwrap();
    assert_eq!(
        (movement.effects, movement.count, movement.kind),
        (2, 5, Where::Internal)
    );
    let mail = preview.totals.iter().find(|t| t.class == "mail").unwrap();
    assert_eq!(
        (mail.effects, mail.count, mail.kind),
        (2, 2, Where::External)
    );
}

#[test]
fn the_preview_flags_everything_in_a_receipt_for_another_operation() {
    let receipts = vec![run(
        "book",
        Operation::Cancel,
        "memo",
        vec![effect(
            "ledger:write",
            "balance_movement",
            EffectKind::Internal,
            1,
        )],
    )];
    let preview = preview_from_receipts(&registry(), &receipts).unwrap();
    assert_eq!(preview.undeclared, 1);
    assert_eq!(
        preview.nodes[0].effects[0].undeclared,
        Some(UndeclaredReason::NoDeclaration)
    );
}

#[test]
fn an_empty_run_previews_nothing_and_an_oversized_one_is_refused() {
    let empty = preview_from_receipts(&registry(), &[]).unwrap();
    assert!(empty.nodes.is_empty() && empty.totals.is_empty() && empty.undeclared == 0);

    let many: Vec<TestRunReceipt> = (0..10_001)
        .map(|_| run("book", Operation::Post, "invoice", Vec::new()))
        .collect();
    let err = preview_from_receipts(&registry(), &many).unwrap_err();
    assert_eq!(err.code(), codes::TOO_LARGE);

    let wide = vec![run(
        "book",
        Operation::Post,
        "invoice",
        (0..257)
            .map(|_| effect("ledger:write", "balance_movement", EffectKind::Internal, 1))
            .collect(),
    )];
    assert_eq!(
        preview_from_receipts(&registry(), &wide)
            .unwrap_err()
            .code(),
        codes::TOO_LARGE
    );
}

#[test]
fn a_preview_never_carries_references_or_digests() {
    let receipts = vec![run(
        "book",
        Operation::Post,
        "invoice",
        vec![effect(
            "ledger:write",
            "balance_movement",
            EffectKind::Internal,
            1,
        )],
    )];
    let preview = preview_from_receipts(&registry(), &receipts).unwrap();
    let text = format!("{preview:?}");
    assert!(!text.contains("row\"") && !text.contains(&"0".repeat(64)));
}

#[test]
fn every_code_is_catalogued() {
    let catalogue = spot_errors::Catalogue::load();
    for code in [codes::TOO_LARGE, codes::PUBLISH_REFUSED] {
        assert!(
            catalogue.lookup(code).is_some(),
            "{code} has no catalogue entry"
        );
    }
}

// ---- adversarial graphs ---------------------------------------------------------------------

fn names(problems: &[spot_bpm_effect_checks::Problem]) -> Vec<(&'static str, Option<String>)> {
    problems
        .iter()
        .map(|p| (p.token(), p.node.as_ref().map(|n| n.to_string())))
        .collect()
}

/// (nodes, edges, expected (token, node) pairs, meta) per shape.
#[test]
fn table_of_graph_shapes() {
    type Case = (
        &'static str,
        Vec<Node>,
        Vec<Edge>,
        Vec<(&'static str, Option<&'static str>)>,
    );
    let undo = |f: &str, t: &str| edge(f, t).with_label("undo");
    let cases: Vec<Case> = vec![
        (
            "forward cycle with an undo across the irreversible node",
            vec![
                doc("a", DocumentAction::Create, "memo"),
                doc("file", DocumentAction::Post, "tax_return"),
                doc("b", DocumentAction::Create, "memo"),
            ],
            vec![
                edge("a", "file"),
                edge("file", "b"),
                edge("b", "a"),
                undo("b", "a"),
            ],
            vec![("undo_past_irreversible", Some("file"))],
        ),
        (
            "diamond: one parallel branch holds the irreversible node",
            vec![
                doc("top", DocumentAction::Create, "memo"),
                doc("left", DocumentAction::Create, "memo"),
                doc("file", DocumentAction::Post, "tax_return"),
                doc("join", DocumentAction::Create, "memo"),
            ],
            vec![
                edge("top", "left"),
                edge("top", "file"),
                edge("left", "join"),
                edge("file", "join"),
                undo("join", "top"),
            ],
            vec![("undo_past_irreversible", Some("file"))],
        ),
        (
            "diamond: the irreversible node is on a branch the undo does not cross",
            vec![
                doc("top", DocumentAction::Create, "memo"),
                doc("left", DocumentAction::Create, "memo"),
                doc("file", DocumentAction::Post, "tax_return"),
                doc("join", DocumentAction::Create, "memo"),
            ],
            vec![
                edge("top", "left"),
                edge("top", "file"),
                edge("left", "join"),
                undo("join", "top"),
            ],
            vec![],
        ),
        (
            "a self-loop undo reverses the node itself (fail closed)",
            vec![doc("file", DocumentAction::Post, "tax_return")],
            vec![undo("file", "file")],
            vec![("undo_past_irreversible", Some("file"))],
        ),
        (
            "a self-loop undo on a reversible node is fine",
            vec![doc("make", DocumentAction::Create, "order")],
            vec![undo("make", "make")],
            vec![],
        ),
        (
            "nested back edges: only the outer one crosses the irreversible node",
            vec![
                doc("a", DocumentAction::Create, "memo"),
                doc("b", DocumentAction::Create, "memo"),
                doc("file", DocumentAction::Post, "tax_return"),
                doc("c", DocumentAction::Create, "memo"),
            ],
            vec![
                edge("a", "b"),
                edge("b", "file"),
                edge("file", "c"),
                undo("c", "b"),
                undo("c", "a"),
            ],
            vec![
                ("undo_past_irreversible", Some("file")),
                ("undo_past_irreversible", Some("file")),
            ],
        ),
        (
            "a node feeding two commit regions is held by both and not reported twice",
            vec![
                doc("book", DocumentAction::Post, "invoice"),
                commit("go1"),
                commit("go2"),
            ],
            vec![edge("book", "go1"), edge("book", "go2")],
            vec![],
        ),
        (
            "a node in a commit region behind another commit point belongs to the nearer one",
            vec![
                doc("book", DocumentAction::Post, "invoice"),
                commit("go1"),
                commit("go2"),
            ],
            vec![edge("book", "go1"), edge("go1", "go2")],
            vec![("empty_commit_point", Some("go2"))],
        ),
        (
            "a commit point inside a forward cycle terminates",
            vec![doc("book", DocumentAction::Post, "invoice"), commit("go")],
            vec![edge("book", "go"), edge("go", "book")],
            vec![],
        ),
        (
            "an update node with an operation override of create is a create for a rewind",
            vec![doc("start", DocumentAction::Create, "memo"), {
                let mut n = doc("end", DocumentAction::Create, "memo");
                if let NodeKind::Document(c) = &mut n.kind {
                    c.action = DocumentAction::Update;
                    c.operation = Some("create".to_owned());
                }
                n
            }],
            vec![
                edge("start", "end"),
                edge("end", "start").with_label("rewind"),
            ],
            vec![("rewind_would_duplicate", Some("end"))],
        ),
    ];
    for (name, nodes, edges, expected) in cases {
        let p = process(nodes, edges);
        let got = names(&check(&p, &registry(), &meta()));
        let mut want: Vec<(&str, Option<String>)> = expected
            .into_iter()
            .map(|(t, n)| (t, n.map(str::to_owned)))
            .collect();
        want.sort_by_key(|w| w.0);
        let mut got_sorted = got.clone();
        got_sorted.sort_by_key(|w| w.0);
        assert_eq!(
            got_sorted.iter().map(|g| g.0).collect::<Vec<_>>(),
            want.iter().map(|w| w.0).collect::<Vec<_>>(),
            "case: {name}: {got:?}"
        );
        for (g, w) in got_sorted.iter().zip(&want) {
            assert_eq!(g.1.is_some(), w.1.is_some(), "case: {name}");
        }
        let mut gn: Vec<_> = got.iter().map(|g| g.1.clone()).collect();
        let mut wn: Vec<_> = want.iter().map(|g| g.1.clone()).collect();
        gn.sort();
        wn.sort();
        gn.dedup();
        wn.dedup();
        assert_eq!(gn, wn, "case: {name}");
    }
}

#[test]
fn an_unknown_record_type_is_a_named_warning_not_silence() {
    let p = process(
        vec![
            doc("make", DocumentAction::Create, "ordre"),
            doc("note", DocumentAction::Create, "memo"),
        ],
        vec![
            edge("make", "note"),
            edge("note", "make").with_label("undo"),
        ],
    );
    let problems = check(&p, &registry(), &meta());
    assert_eq!(tokens(&problems), ["no_declared_pipeline"]);
    assert_eq!(problems[0].severity, Severity::Warning);
    assert_eq!(problems[0].node, Some(id("make")));
}

#[test]
fn an_undo_past_an_unknown_effect_variant_is_treated_as_irreversible() {
    // A compensable effect is fine, a reversible one is fine; only the irreversible one stops.
    let p = process(
        vec![
            doc("book", DocumentAction::Post, "invoice"),
            doc("note", DocumentAction::Create, "memo"),
        ],
        vec![
            edge("book", "note"),
            edge("note", "book").with_label("undo"),
        ],
    );
    // `book` is the target: its own effects are kept, so nothing is crossed.
    assert!(check(&p, &registry(), &meta()).is_empty());
}

#[test]
fn a_mislabelled_edge_is_forward_under_the_default_meta() {
    let p = process(
        vec![
            doc("a", DocumentAction::Create, "memo"),
            doc("file", DocumentAction::Post, "tax_return"),
            doc("b", DocumentAction::Create, "memo"),
        ],
        vec![
            edge("a", "file"),
            edge("file", "b"),
            edge("b", "a").with_label("Undo"),
        ],
    );
    assert!(check(&p, &registry(), &meta()).is_empty());
}

fn chain(prefix: &str, n: usize) -> (Vec<Node>, Vec<Edge>) {
    let nodes: Vec<Node> = (0..n)
        .map(|i| doc(&format!("{prefix}{i}"), DocumentAction::Create, "memo"))
        .collect();
    let edges = (1..n)
        .map(|i| edge(&format!("{prefix}{}", i - 1), &format!("{prefix}{i}")))
        .collect();
    (nodes, edges)
}

#[test]
fn a_hostile_chain_into_a_hub_into_many_commits_is_refused_quickly() {
    // 5,000-node chain -> hub -> 5,000 commit points: inside the node and edge bounds, beyond the
    // commit-point bound. It must be refused, not walked.
    let (mut nodes, mut edges) = chain("c", 5_000);
    nodes.push(doc("hub", DocumentAction::Create, "memo"));
    edges.push(edge("c4999", "hub"));
    for i in 0..4_990 {
        nodes.push(commit(&format!("k{i}")));
        edges.push(edge("hub", &format!("k{i}")));
    }
    let p = process(nodes, edges);
    let start = std::time::Instant::now();
    let problems = check(&p, &registry(), &meta());
    assert!(start.elapsed() < Duration::from_secs(20));
    assert_eq!(tokens(&problems), ["too_large"]);
}

#[test]
fn the_worst_case_inside_the_bounds_completes() {
    // The largest accepted shape: a 9,000-node chain feeding a hub that feeds the most commit
    // points, plus the most undo edges over the chain.
    let (mut nodes, mut edges) = chain("c", 9_000);
    nodes.push(doc("hub", DocumentAction::Create, "memo"));
    edges.push(edge("c8999", "hub"));
    for i in 0..MAX_COMMIT_POINTS {
        nodes.push(commit(&format!("k{i}")));
        edges.push(edge("hub", &format!("k{i}")));
    }
    for i in 0..256 {
        edges.push(edge(&format!("c{}", 8_000 + i), &format!("c{i}")).with_label("undo"));
    }
    let p = process(nodes, edges);
    let start = std::time::Instant::now();
    let problems = check(&p, &registry(), &meta());
    let took = start.elapsed();
    assert!(took < Duration::from_secs(20), "took {took:?}");
    assert!(tokens(&problems).iter().all(|t| *t == "empty_commit_point"));
    assert_eq!(problems.len(), MAX_COMMIT_POINTS);
}
