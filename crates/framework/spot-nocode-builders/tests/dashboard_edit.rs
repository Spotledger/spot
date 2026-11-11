//! The dashboard op log (WP-DX-07.T3): the golden JSON the client checks against, replay,
//! atomicity of refused ops, undo and redo, and the validation SetTile and SetRoles apply.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use proptest::prelude::*;
use spot_nocode_builders::{
    DashboardDraft, DashboardEditor, DashboardOp, DraftParamType, Grid, OpError, Place,
    RefreshPolicy, SharedFilter, Tile, apply_dashboard_op, error_codes,
};

fn empty() -> DashboardDraft {
    let mut draft = DashboardDraft::new("dash-e", "gina", "overview", "Overview");
    draft.grid = Grid {
        columns: 12,
        rows: 6,
    };
    draft
}

fn number(id: &str, place: Place) -> Tile {
    Tile::number(id, "Sales", "sales", "debit", place)
}

fn with_tile(id: &str, place: Place) -> DashboardDraft {
    let mut draft = empty();
    draft.add_tile(number(id, place)).expect("fits");
    draft
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/dashboard_ops")
}

fn golden(name: &str) -> (String, DashboardOp) {
    let text = std::fs::read_to_string(golden_dir().join(format!("{name}.json"))).expect("golden");
    let op = serde_json::from_str(&text).expect("the golden file is a DashboardOp");
    (text, op)
}

/// Every op name, so a new variant without a golden file fails here.
const NAMES: [&str; 8] = [
    "add_filter",
    "add_tile",
    "move_tile",
    "remove_filter",
    "remove_tile",
    "set_refresh",
    "set_roles",
    "set_tile",
];

/// The draft each golden op is applied to.
fn base_for(name: &str) -> DashboardDraft {
    let mut draft = empty();
    match name {
        "move_tile" | "remove_tile" | "set_tile" => {
            draft
                .add_tile(number("number_1", Place::new(1, 1, 3, 2)))
                .unwrap();
        }
        "remove_filter" => {
            draft
                .add_filter(SharedFilter::new("year", "Year", DraftParamType::Int))
                .unwrap();
        }
        _ => {}
    }
    draft
}

#[test]
fn golden_files_are_exactly_one_per_op() {
    let mut stems = BTreeSet::new();
    for entry in std::fs::read_dir(golden_dir()).expect("dir") {
        let path = entry.expect("entry").path();
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("json"));
        stems.insert(path.file_stem().unwrap().to_str().unwrap().to_owned());
    }
    let expected: BTreeSet<String> = NAMES.iter().map(|n| (*n).to_owned()).collect();
    assert_eq!(stems, expected);
}

#[test]
fn every_golden_file_loads_roundtrips_and_applies() {
    for name in NAMES {
        let (text, op) = golden(name);
        assert_eq!(op.name(), name, "the file name is the op's wire name");
        // Serialising the op gives the same JSON the client writes.
        let written: serde_json::Value = serde_json::to_value(&op).unwrap();
        let on_disk: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(written, on_disk, "{name} round-trips");
        let base = base_for(name);
        let next = apply_dashboard_op(&base, &op).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_ne!(next, base, "{name} changes the draft");
    }
}

#[test]
fn golden_results_are_the_expected_drafts() {
    let (_, op) = golden("add_tile");
    let next = apply_dashboard_op(&base_for("add_tile"), &op).unwrap();
    let tile = next.tile("number_1").expect("added");
    assert_eq!(tile.place, Place::new(1, 1, 3, 2));
    assert_eq!(tile.title, "Sales");
    assert!(tile.uses_filters);

    let (_, op) = golden("move_tile");
    let next = apply_dashboard_op(&base_for("move_tile"), &op).unwrap();
    assert_eq!(next.tile("number_1").unwrap().place, Place::new(2, 3, 3, 2));

    let (_, op) = golden("remove_tile");
    assert!(
        apply_dashboard_op(&base_for("remove_tile"), &op)
            .unwrap()
            .tiles
            .is_empty()
    );

    let (_, op) = golden("set_tile");
    let tile = apply_dashboard_op(&base_for("set_tile"), &op)
        .unwrap()
        .tile("number_1")
        .unwrap()
        .clone();
    assert_eq!(tile.title, "Revenue");
    assert!(!tile.uses_filters);

    let (_, op) = golden("add_filter");
    let next = apply_dashboard_op(&base_for("add_filter"), &op).unwrap();
    assert_eq!(next.filters.len(), 1);
    assert_eq!(next.filters[0].name, "year");

    let (_, op) = golden("remove_filter");
    assert!(
        apply_dashboard_op(&base_for("remove_filter"), &op)
            .unwrap()
            .filters
            .is_empty()
    );

    let (_, op) = golden("set_refresh");
    let next = apply_dashboard_op(&base_for("set_refresh"), &op).unwrap();
    assert_eq!(next.refresh, RefreshPolicy::Every { seconds: 300 });

    let (_, op) = golden("set_roles");
    let next = apply_dashboard_op(&base_for("set_roles"), &op).unwrap();
    assert_eq!(next.roles, vec!["Accounts Manager".to_owned()]);
}

#[test]
fn an_unknown_field_or_op_in_json_is_refused() {
    assert!(serde_json::from_str::<DashboardOp>(r#"{"op":"explode"}"#).is_err());
    assert!(
        serde_json::from_str::<DashboardOp>(r#"{"op":"remove_tile","id":"a","extra":1}"#).is_err()
    );
}

// ---- refused ops change nothing -------------------------------------------------------

/// Applies `op` through an editor holding `draft` and checks nothing moved.
fn assert_refused(draft: DashboardDraft, op: DashboardOp) -> OpError {
    let mut editor = DashboardEditor::new(draft.clone());
    // One accepted op first, so the log is not trivially empty.
    editor
        .apply(DashboardOp::SetRoles {
            roles: vec!["reader".to_owned()],
        })
        .unwrap();
    let before = editor.current().clone();
    let before_log = editor.log().to_vec();
    let before_ops = editor.ops().to_vec();

    let pure = apply_dashboard_op(&before, &op);
    let err = pure.expect_err("the op is refused");
    let editor_err = editor.apply(op).expect_err("the editor refuses it too");
    assert_eq!(err, editor_err);

    assert_eq!(editor.current(), &before);
    assert_eq!(editor.log(), before_log.as_slice());
    assert_eq!(editor.ops(), before_ops.as_slice());
    assert_eq!(editor.len(), before_log.len());
    assert!(editor.can_undo());
    assert!(!editor.can_redo());
    assert_eq!(editor.replayed().unwrap(), before);
    err
}

#[test]
fn refused_ops_leave_the_draft_and_log_untouched() {
    let mut draft = with_tile("a", Place::new(1, 1, 3, 2));
    draft.add_tile(number("b", Place::new(1, 4, 3, 2))).unwrap();
    draft
        .add_filter(SharedFilter::new("year", "Year", DraftParamType::Int))
        .unwrap();

    // Overlap on add and on move.
    let err = assert_refused(
        draft.clone(),
        DashboardOp::AddTile {
            tile: number("c", Place::new(1, 3, 2, 2)),
        },
    );
    assert_eq!(err.code(), error_codes::BAD_LAYOUT);
    let err = assert_refused(
        draft.clone(),
        DashboardOp::MoveTile {
            id: "b".to_owned(),
            place: Place::new(2, 2, 3, 2),
        },
    );
    assert_eq!(err.code(), error_codes::BAD_LAYOUT);
    // Off the grid.
    assert_refused(
        draft.clone(),
        DashboardOp::MoveTile {
            id: "b".to_owned(),
            place: Place::new(1, 11, 3, 2),
        },
    );
    // Duplicate tile and filter.
    let err = assert_refused(
        draft.clone(),
        DashboardOp::AddTile {
            tile: number("a", Place::new(5, 1, 1, 1)),
        },
    );
    assert_eq!(err.code(), error_codes::DUPLICATE);
    let err = assert_refused(
        draft.clone(),
        DashboardOp::AddFilter {
            filter: SharedFilter::new("year", "Year", DraftParamType::Int),
        },
    );
    assert_eq!(err.code(), error_codes::DUPLICATE);
    // Unknown tile and filter.
    for op in [
        DashboardOp::RemoveTile {
            id: "zzz".to_owned(),
        },
        DashboardOp::MoveTile {
            id: "zzz".to_owned(),
            place: Place::new(5, 1, 1, 1),
        },
        DashboardOp::SetTile {
            id: "zzz".to_owned(),
            title: "T".to_owned(),
            uses_filters: true,
        },
        DashboardOp::RemoveFilter {
            name: "nope".to_owned(),
        },
    ] {
        assert_refused(draft.clone(), op);
    }
    // A zero refresh interval keeps the earlier policy.
    let mut every = draft.clone();
    every.refresh = RefreshPolicy::Every { seconds: 60 };
    let err = assert_refused(
        every,
        DashboardOp::SetRefresh {
            refresh: RefreshPolicy::Every { seconds: 0 },
        },
    );
    assert_eq!(err.code(), error_codes::BAD_REFRESH);
}

#[test]
fn a_refused_op_does_not_mutate_the_input_draft() {
    let draft = with_tile("a", Place::new(1, 1, 3, 2));
    let copy = draft.clone();
    let _ = apply_dashboard_op(
        &draft,
        &DashboardOp::AddTile {
            tile: number("b", Place::new(1, 1, 1, 1)),
        },
    )
    .expect_err("overlap");
    assert_eq!(draft, copy);
    let _ = apply_dashboard_op(
        &draft,
        &DashboardOp::SetRefresh {
            refresh: RefreshPolicy::Every { seconds: 0 },
        },
    )
    .expect_err("zero");
    assert_eq!(draft, copy);
}

// ---- validation of SetTile and SetRoles -----------------------------------------------

#[test]
fn set_tile_refuses_a_blank_title_and_keeps_the_tile() {
    let draft = with_tile("a", Place::new(1, 1, 3, 2));
    for title in ["", "   ", "\t\n"] {
        let err = assert_refused(
            draft.clone(),
            DashboardOp::SetTile {
                id: "a".to_owned(),
                title: title.to_owned(),
                uses_filters: false,
            },
        );
        assert_eq!(err.code(), error_codes::BAD_VALUE);
    }
    // A readable title is accepted, and surrounding text is kept as typed.
    let next = apply_dashboard_op(
        &draft,
        &DashboardOp::SetTile {
            id: "a".to_owned(),
            title: "Net sales".to_owned(),
            uses_filters: false,
        },
    )
    .unwrap();
    assert_eq!(next.tile("a").unwrap().title, "Net sales");
    assert!(!next.tile("a").unwrap().uses_filters);
}

#[test]
fn set_roles_refuses_blank_names_but_accepts_clearing_the_list() {
    let draft = empty();
    for roles in [
        vec![String::new()],
        vec!["reader".to_owned(), "  ".to_owned()],
    ] {
        let err = assert_refused(draft.clone(), DashboardOp::SetRoles { roles });
        assert_eq!(err.code(), error_codes::BAD_VALUE);
    }
    // An empty list is the "nobody yet" state; publish validation warns about it (a
    // not-ready warning), so the op itself allows it, as the report draft's SetRoles does.
    let mut with_roles = empty();
    with_roles.roles = vec!["reader".to_owned()];
    let next = apply_dashboard_op(&with_roles, &DashboardOp::SetRoles { roles: vec![] }).unwrap();
    assert!(next.roles.is_empty());
    // Role names are stored as given (no registry of roles is known to this crate).
    let next = apply_dashboard_op(
        &draft,
        &DashboardOp::SetRoles {
            roles: vec!["Accounts Manager".to_owned(), "reader".to_owned()],
        },
    )
    .unwrap();
    assert_eq!(next.roles, vec!["Accounts Manager", "reader"]);
}

// ---- undo and redo --------------------------------------------------------------------

#[test]
fn undo_then_redo_restores_each_step() {
    let mut editor = DashboardEditor::new(empty());
    let ops = [
        DashboardOp::AddTile {
            tile: number("a", Place::new(1, 1, 3, 2)),
        },
        DashboardOp::MoveTile {
            id: "a".to_owned(),
            place: Place::new(3, 1, 3, 2),
        },
        DashboardOp::SetRefresh {
            refresh: RefreshPolicy::OnOpen,
        },
    ];
    let mut states = vec![editor.current().clone()];
    for op in ops {
        editor.apply(op).unwrap();
        states.push(editor.current().clone());
    }
    assert_eq!(editor.len(), 3);
    assert!(editor.can_undo() && !editor.can_redo());
    for expected in states.iter().rev().skip(1) {
        assert!(editor.undo());
        assert_eq!(editor.current(), expected);
    }
    assert!(editor.is_empty() && !editor.can_undo() && !editor.undo());
    for expected in states.iter().skip(1) {
        assert!(editor.redo());
        assert_eq!(editor.current(), expected);
    }
    assert!(!editor.can_redo() && !editor.redo());
    assert_eq!(editor.replayed().unwrap(), *editor.current());
}

#[test]
fn a_new_op_after_undo_drops_the_redo_future() {
    let mut editor = DashboardEditor::new(empty());
    editor
        .apply(DashboardOp::AddTile {
            tile: number("a", Place::new(1, 1, 3, 2)),
        })
        .unwrap();
    editor
        .apply(DashboardOp::AddTile {
            tile: number("b", Place::new(1, 4, 3, 2)),
        })
        .unwrap();
    assert!(editor.undo());
    assert!(editor.can_redo());

    let other = DashboardOp::SetRoles {
        roles: vec!["reader".to_owned()],
    };
    editor.apply(other.clone()).unwrap();
    assert!(!editor.can_redo(), "the redoable add of b is gone");
    assert!(!editor.redo());
    assert_eq!(editor.ops().len(), 2);
    assert_eq!(editor.log().last(), Some(&other));
    assert!(editor.current().tile("b").is_none());
    assert_eq!(editor.replayed().unwrap(), *editor.current());
}

#[test]
fn a_refused_op_after_undo_keeps_the_redo_future() {
    let mut editor = DashboardEditor::new(empty());
    editor
        .apply(DashboardOp::AddTile {
            tile: number("a", Place::new(1, 1, 3, 2)),
        })
        .unwrap();
    assert!(editor.undo());
    editor
        .apply(DashboardOp::RemoveTile { id: "a".to_owned() })
        .expect_err("a is not in the draft after undo");
    assert!(editor.can_redo(), "a refused op must not drop the future");
    assert!(editor.redo());
    assert!(editor.current().tile("a").is_some());
}

// ---- replay property ------------------------------------------------------------------

/// A deterministic op from a byte, drawn so that both accepted and refused ops occur.
fn op_for(byte: u8, index: usize) -> DashboardOp {
    let id = format!("t{}", byte % 5);
    let row = u32::from(byte / 5 % 6) + 1;
    let column = byte / 30 % 12 + 1;
    match byte % 9 {
        0 => DashboardOp::AddTile {
            tile: number(&id, Place::new(row, column, 2, 1)),
        },
        1 => DashboardOp::RemoveTile { id },
        2 => DashboardOp::MoveTile {
            id,
            place: Place::new(row, column, 2, 1),
        },
        3 => DashboardOp::SetTile {
            id,
            title: format!("Title {index}"),
            uses_filters: byte.is_multiple_of(2),
        },
        4 => DashboardOp::AddFilter {
            filter: SharedFilter::new(&format!("f{}", byte % 4), "Filter", DraftParamType::Int),
        },
        5 => DashboardOp::RemoveFilter {
            name: format!("f{}", byte % 4),
        },
        6 => DashboardOp::SetRefresh {
            refresh: match byte % 4 {
                0 => RefreshPolicy::Manual,
                1 => RefreshPolicy::OnOpen,
                2 => RefreshPolicy::Every {
                    seconds: u32::from(byte % 7),
                },
                _ => RefreshPolicy::Every { seconds: 60 },
            },
        },
        7 => DashboardOp::SetRoles {
            roles: vec![format!("role{}", byte % 3)],
        },
        _ => DashboardOp::SetTile {
            id,
            title: " ".to_owned(),
            uses_filters: true,
        },
    }
}

proptest! {
    #[test]
    fn replaying_the_log_reproduces_the_draft(
        steps in prop::collection::vec((any::<u8>(), 0u8..4), 0..60),
    ) {
        let initial = empty();
        let mut editor = DashboardEditor::new(initial.clone());
        for (index, (byte, action)) in steps.iter().enumerate() {
            match action {
                // Mostly ops, sometimes undo or redo, so the cursor moves around.
                0 => { editor.undo(); }
                1 => { editor.redo(); }
                _ => {
                    let before = editor.current().clone();
                    let log_len = editor.log().len();
                    let ops_len = editor.ops().len();
                    if editor.apply(op_for(*byte, index)).is_err() {
                        prop_assert_eq!(editor.current(), &before);
                        prop_assert_eq!(editor.log().len(), log_len);
                        prop_assert_eq!(editor.ops().len(), ops_len);
                    }
                }
            }
            prop_assert_eq!(editor.replayed().unwrap(), editor.current().clone());
        }
        // Walk to the tip of the log (redo may still be pending), then all the way back
        // and forth again.
        while editor.redo() {}
        let depth = editor.len();
        let current = editor.current().clone();
        prop_assert_eq!(editor.replayed().unwrap(), current.clone());

        while editor.undo() {}
        prop_assert_eq!(editor.current(), &initial);
        prop_assert!(editor.is_empty());
        while editor.redo() {}
        prop_assert_eq!(editor.current(), &current);
        prop_assert_eq!(editor.len(), depth);

        // Replaying the logged ops from an empty draft by hand reproduces the draft too.
        let mut manual = initial;
        for op in editor.log() {
            manual = apply_dashboard_op(&manual, op).unwrap();
        }
        prop_assert_eq!(manual, current);
    }

    #[test]
    fn the_log_serialises_and_replays_from_json(
        bytes in prop::collection::vec(any::<u8>(), 0..40),
    ) {
        let mut editor = DashboardEditor::new(empty());
        for (index, byte) in bytes.iter().enumerate() {
            let _ = editor.apply(op_for(*byte, index));
        }
        let json = serde_json::to_string(editor.log()).unwrap();
        let ops: Vec<DashboardOp> = serde_json::from_str(&json).unwrap();
        let mut draft = empty();
        for op in &ops {
            draft = apply_dashboard_op(&draft, op).unwrap();
        }
        prop_assert_eq!(&draft, editor.current());
    }
}
