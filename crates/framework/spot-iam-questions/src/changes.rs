//! What changed in access between two dates: the fourth answerable question, read from the
//! permission-change audit of T1 (ADR-070 section 8, rules 4 and 5).
//!
//! A [`Change`] is flattened from a [`ChangeEvent`](spot_iam_governance::ChangeEvent) and
//! deliberately drops the before/after snapshots: an audit answer must not echo a value a
//! permission hides. The window is inclusive at both ends and the rows are sorted on
//! `(at, subject, kind, op)`, so the same window always reads the same.

use crate::model::Change;

/// The changes whose `occurred_at` falls in `[from, to]`.
///
/// `from` after `to` gives an empty answer rather than an error: a window with no rows is a
/// legitimate answer, not a malformed question.
#[must_use]
pub fn changes(history: &[spot_iam_governance::ChangeEvent], from: i64, to: i64) -> Vec<Change> {
    if from > to {
        return Vec::new();
    }
    let mut out: Vec<Change> = history
        .iter()
        .filter(|e| e.occurred_at >= from && e.occurred_at <= to)
        .map(Change::of)
        .collect();
    out.sort_by(|a, b| {
        (&a.at, &a.subject, &a.kind, &a.op).cmp(&(&b.at, &b.subject, &b.kind, &b.op))
    });
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use spot_iam_governance::{ChangeActor, ChangeEvent, ChangeOp};
    use spot_iam_roles::Source;

    fn history() -> Vec<ChangeEvent> {
        vec![
            ChangeEvent::grant(
                "grant:1",
                "ben",
                "Finance Approver",
                Source::Manual,
                ChangeOp::Created,
                ChangeActor::user("ana"),
                10,
            ),
            ChangeEvent::grant(
                "grant:2",
                "ben",
                "Accounts Payable Clerk",
                Source::Emergency,
                ChangeOp::Ended,
                ChangeActor::user("ana"),
                20,
            )
            .with_cause("expired"),
            ChangeEvent::role(
                "Finance Approver",
                ChangeOp::Updated,
                "reference",
                ChangeActor::system("migration"),
                30,
            ),
        ]
    }

    #[test]
    fn the_window_is_inclusive_and_the_rows_are_sorted() {
        let all = changes(&history(), 0, 100);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].subject, "grant:1");
        assert_eq!(all[2].actor, "system:migration");
        assert_eq!(changes(&history(), 0, 15).len(), 1);
        assert_eq!(changes(&history(), 30, 30).len(), 1);
        assert!(changes(&history(), 5, 4).is_empty());
    }

    #[test]
    fn a_change_drops_the_before_and_after_snapshots() {
        let event = ChangeEvent::role(
            "Finance Approver",
            ChangeOp::Updated,
            "reference",
            ChangeActor::user("ana"),
            5,
        )
        .with_after(spot_iam_governance::Snapshot::from_value(
            serde_json::json!({"secret": "value"}),
        ));
        let rows = changes(&[event], 0, 10);
        let text = serde_json::to_string(&rows).expect("serialisable");
        assert!(!text.contains("secret"), "the snapshot leaked: {text}");
    }
}
