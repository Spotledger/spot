//! Undo and redo: a property test over arbitrary op sequences, plus the command log's
//! contract (a failed op changes nothing, undo and redo move the same cursor).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;
use spot_nocode_builders::{
    Aggregate, Compare, Direction, DraftColumn, DraftFilter, DraftParameter, FilterTarget,
    FilterValue, ReportDraft, ReportEditor, ReportOp, apply_op,
};

fn draft() -> ReportDraft {
    let mut draft = ReportDraft::new("draft-h", "gina", "history", "History", "journal");
    draft.columns = vec![DraftColumn::new("title")];
    draft.roles = vec!["reader".to_owned()];
    draft
}

fn op_for(byte: u8, index: usize) -> ReportOp {
    match byte % 10 {
        0 => ReportOp::AddColumn {
            field: format!("f{index}"),
            label: None,
            aggregate: None,
            format: None,
        },
        1 => ReportOp::AddColumn {
            field: format!("n{index}"),
            label: Some("Number".to_owned()),
            aggregate: Some(Aggregate::Sum),
            format: Some("money".to_owned()),
        },
        2 => ReportOp::SetLabel {
            label: format!("Report {index}"),
        },
        3 => ReportOp::AddParameter {
            parameter: DraftParameter::date(&format!("p{index}"), "From"),
        },
        4 => ReportOp::AddFilter {
            filter: DraftFilter::new(
                "date",
                Compare::Ge,
                FilterTarget::value(FilterValue::Date {
                    text: "2026-01-01".to_owned(),
                }),
            ),
        },
        5 => ReportOp::SortBy {
            field: "title".to_owned(),
            direction: Direction::Desc,
        },
        6 => ReportOp::GroupBy {
            field: "title".to_owned(),
        },
        7 => ReportOp::SetLimit {
            limit: Some(10 + u32::try_from(index).unwrap_or(0)),
            max_rows: None,
        },
        8 => ReportOp::SetRoles {
            roles: vec!["reader".to_owned(), format!("role{index}")],
        },
        _ => ReportOp::ClearSort,
    }
}

proptest! {
    #[test]
    fn undo_and_redo_reach_the_same_states(bytes in prop::collection::vec(any::<u8>(), 0..40)) {
        let initial = draft();
        let mut editor = ReportEditor::new(initial.clone());
        for (index, byte) in bytes.iter().enumerate() {
            // A refused op changes nothing; the property holds for whatever was applied.
            let _ = editor.apply(op_for(*byte, index));
        }
        let depth = editor.len();
        let current = editor.current().clone();
        prop_assert_eq!(editor.replayed().unwrap(), current.clone());

        while editor.undo() {}
        prop_assert_eq!(editor.current(), &initial);
        prop_assert!(editor.is_empty());

        while editor.redo() {}
        prop_assert_eq!(editor.current(), &current);
        prop_assert_eq!(editor.len(), depth);
    }
}

#[test]
fn a_refused_op_changes_nothing_and_keeps_the_log() {
    let draft = draft();
    let mut editor = ReportEditor::new(draft.clone());
    editor
        .apply(ReportOp::AddColumn {
            field: "debit".to_owned(),
            label: None,
            aggregate: None,
            format: None,
        })
        .expect("a new column");
    let before = editor.current().clone();
    let err = editor
        .apply(ReportOp::RemoveColumn {
            field: "missing".to_owned(),
        })
        .expect_err("no such column");
    assert_eq!(err.code(), "nocode_builders.unknown_column");
    assert_eq!(editor.current(), &before);
    assert_eq!(editor.len(), 1);
}

#[test]
fn apply_op_is_pure() {
    let draft = draft();
    let before = draft.clone();
    let next = apply_op(
        &draft,
        &ReportOp::AddColumn {
            field: "debit".to_owned(),
            label: None,
            aggregate: Some(Aggregate::Sum),
            format: None,
        },
    )
    .expect("adds");
    assert_eq!(draft, before, "the input draft is untouched");
    assert_ne!(next, draft);
}

#[test]
fn removing_a_link_removes_the_columns_that_used_it() {
    let mut editor = ReportEditor::new(draft());
    editor
        .apply(ReportOp::AddJoin {
            alias: "acct".to_owned(),
            doctype: "account".to_owned(),
            on: "account".to_owned(),
        })
        .expect("the link");
    editor
        .apply(ReportOp::AddColumn {
            field: "acct.title".to_owned(),
            label: None,
            aggregate: None,
            format: None,
        })
        .expect("a linked column");
    editor
        .apply(ReportOp::RemoveJoin {
            alias: "acct".to_owned(),
        })
        .expect("removes");
    assert!(editor.current().column("acct.title").is_none());
    assert!(editor.current().joins.is_empty());
}
