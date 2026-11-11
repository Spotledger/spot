//! Undo/redo correctness: a property test over arbitrary op sequences.

use proptest::prelude::*;
use spot_meta::FieldType;
use spot_nocode_model::{Draft, DraftEditor, DraftOp, SampleStats};

/// A deterministic op from a command byte and its position in the sequence.
fn op_for(byte: u8, index: usize) -> DraftOp {
    let name = format!("f{}", index % 5);
    match byte % 8 {
        0 => DraftOp::AddField {
            child: None,
            name: format!("f{index}"),
            label: None,
            ty: FieldType::Text,
            insert_after: None,
        },
        1 => DraftOp::SetRequired {
            child: None,
            name,
            required: true,
        },
        2 => DraftOp::SetDefault {
            child: None,
            name,
            default: Some("x".to_owned()),
        },
        3 => DraftOp::AddFormSection {
            child: None,
            id: format!("s{index}"),
            title: None,
            columns: None,
            tab: None,
        },
        4 => DraftOp::MoveFieldInLayout {
            child: None,
            field: name,
            section: "s0".to_owned(),
            after: None,
        },
        5 => DraftOp::ReorderField {
            child: None,
            name,
            after: None,
        },
        6 => DraftOp::SetOptions {
            child: None,
            name,
            options: vec!["A".to_owned(), "B".to_owned()],
        },
        _ => DraftOp::ChangeType {
            child: None,
            name,
            ty: FieldType::Text,
            stats: SampleStats::empty(),
        },
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn undo_and_redo_are_exact(commands in proptest::collection::vec(any::<u8>(), 0..40)) {
        let mut editor = DraftEditor::new(Draft::new("d", "ann", "vehicle", "Vehicle"));
        let mut states = vec![editor.current().clone()];
        for (index, byte) in commands.iter().enumerate() {
            if editor.apply(op_for(*byte, index)).is_ok() {
                states.push(editor.current().clone());
                // Replaying the log reaches exactly the state the editor holds.
                let replayed = editor.replayed().unwrap_or_else(|e| panic!("{e}"));
                prop_assert_eq!(&replayed, editor.current());
            }
        }
        prop_assert_eq!(editor.len() + 1, states.len());

        // Undo back to the start, checking each state.
        let mut at = states.len() - 1;
        while editor.can_undo() {
            prop_assert!(editor.undo());
            at -= 1;
            prop_assert_eq!(editor.current(), &states[at]);
        }
        prop_assert_eq!(at, 0);
        prop_assert_eq!(editor.current(), &states[0]);

        // Redo forward to the end, checking each state.
        while editor.can_redo() {
            prop_assert!(editor.redo());
            at += 1;
            prop_assert_eq!(editor.current(), &states[at]);
        }
        prop_assert_eq!(at, states.len() - 1);
        prop_assert_eq!(editor.current(), states.last().unwrap_or_else(|| unreachable!()));
    }
}

#[test]
fn a_new_op_after_undo_drops_the_redo_future() {
    let mut editor = DraftEditor::new(Draft::new("d", "ann", "vehicle", "Vehicle"));
    let add = |name: &str| DraftOp::AddField {
        child: None,
        name: name.to_owned(),
        label: None,
        ty: FieldType::Text,
        insert_after: None,
    };
    editor.apply(add("a")).unwrap_or_else(|e| panic!("{e}"));
    editor.apply(add("b")).unwrap_or_else(|e| panic!("{e}"));
    assert!(editor.undo());
    assert!(editor.can_redo());
    editor.apply(add("c")).unwrap_or_else(|e| panic!("{e}"));
    assert!(!editor.can_redo());
    assert!(editor.current().field(None, "c").is_some());
    assert!(editor.current().field(None, "b").is_none());
}
