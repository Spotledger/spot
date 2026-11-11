//! The typed explanation lines of a plan.

use super::{at, cat, ranges, snapshot};
use crate::{ExplainLine, Target, explain, plan};

fn keys(lines: &[ExplainLine]) -> Vec<&'static str> {
    lines.iter().map(|l| l.key).collect()
}

#[test]
fn a_rebuild_to_the_end_explains_base_steps_and_the_kept_generation() {
    let catalog = cat(
        vec![snapshot(10, 1, true)],
        ranges(1, &[(1, 10), (11, 20)]),
        vec![],
    );
    let plan = plan(&catalog, None, Target::End).expect("complete");
    let lines = explain(&plan);
    assert_eq!(
        keys(&lines),
        vec![
            "journal-rebuild-base-snapshot",
            "journal-rebuild-apply-segment",
            "journal-rebuild-ends-at",
            "journal-rebuild-keeps-generation",
            "journal-rebuild-positions-preserved",
            "journal-rebuild-projectors",
            "journal-rebuild-invariant-suite",
        ]
    );
    assert_eq!(
        lines[0].params,
        vec![
            ("generation", "1".to_owned()),
            ("position", "10".to_owned())
        ]
    );
    assert_eq!(
        lines[1].params,
        vec![
            ("first_seq", "11".to_owned()),
            ("generation", "1".to_owned()),
            ("last_seq", "20".to_owned())
        ]
    );
    assert_eq!(lines[1].param("last_seq"), Some("20"));
    assert_eq!(lines[1].param("no_such_param"), None);
    assert_eq!(lines[2].param("position"), Some("20"));
}

#[test]
fn a_fork_explains_the_cut_and_the_resync() {
    let catalog = cat(vec![], ranges(1, &[(1, 10), (11, 20)]), vec![]);
    let plan = plan(&catalog, None, at(15)).expect("15 is live");
    let lines = explain(&plan);
    assert_eq!(
        keys(&lines),
        vec![
            "journal-rebuild-base-empty",
            "journal-rebuild-apply-segment",
            "journal-rebuild-apply-segment",
            "journal-rebuild-ends-at",
            "journal-rebuild-forks",
            "journal-rebuild-consumers-resync",
            "journal-rebuild-projectors",
            "journal-rebuild-invariant-suite",
        ],
        "a forking plan says nothing about keeping the generation"
    );
    let forks = &lines[4];
    assert_eq!(forks.param("from_position"), Some("15"));
    assert_eq!(forks.param("abandoned_generation"), Some("1"));
    assert_eq!(forks.param("new_generation"), Some("2"));
    assert_eq!(lines[5].param("abandoned_generation"), Some("1"));
    assert_eq!(lines[0].params, vec![], "an empty base carries no params");
}

#[test]
fn a_backup_base_is_explained_with_its_position() {
    let catalog = cat(
        vec![snapshot(10, 1, true)],
        ranges(1, &[(1, 10), (11, 20)]),
        vec![],
    );
    let plan = plan(&catalog, Some(&crate::Backup { position: 15 }), Target::End)
        .expect("the backup is covered");
    let lines = explain(&plan);
    assert_eq!(lines[0].key, "journal-rebuild-base-backup");
    assert_eq!(lines[0].param("position"), Some("15"));
}

#[test]
fn the_explanation_is_deterministic() {
    let catalog = cat(
        vec![snapshot(10, 1, true)],
        ranges(1, &[(1, 10), (11, 20)]),
        vec![],
    );
    let plan = plan(&catalog, None, Target::End).expect("complete");
    assert_eq!(explain(&plan), explain(&plan));
    assert!(!explain(&plan).is_empty());
}
