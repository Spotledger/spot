//! Approvals are counted through `human_approvers` only (WP-BPM-05.T5): an SLA auto-approval is
//! never a person, and never satisfies an n-approver or distinct-approver rule.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::now;
use spot_bpm_entry::{AUTO_APPROVED_TEXT, ApprovalDisplay, ApprovalRequirement, ApprovalStanding};
use spot_bpm_tasks::{Decision, SLA_ACTOR, TaskAction, TaskEvent, TaskId};

fn task() -> TaskId {
    TaskId::new("t1").expect("id")
}

fn person(name: &str) -> TaskEvent {
    let mut event = TaskEvent::new(&task(), TaskAction::Completed, name, now());
    event.choice = Some(Decision::Approve);
    event
}

fn by_sla() -> TaskEvent {
    let mut event =
        TaskEvent::new(&task(), TaskAction::Completed, SLA_ACTOR, now()).automatic("p1");
    event.choice = Some(Decision::Approve);
    event
}

#[test]
fn two_approver_node_after_an_auto_approved_node_still_needs_two_people() {
    let standing = ApprovalStanding::from_events(&[by_sla()]);
    assert!(standing.people().is_empty());
    assert!(!standing.satisfies(&ApprovalRequirement::at_least(2)));
    assert!(!standing.satisfies(&ApprovalRequirement::at_least(1)));
    let one = ApprovalStanding::from_events(&[by_sla(), person("ann")]);
    assert!(!one.satisfies(&ApprovalRequirement::at_least(2)));
    let two = ApprovalStanding::from_events(&[by_sla(), person("ann"), person("bob")]);
    assert!(two.satisfies(&ApprovalRequirement::at_least(2)));
}

#[test]
fn only_automatic_approval_does_not_satisfy_a_distinct_rule() {
    let standing = ApprovalStanding::from_events(&[by_sla()]);
    let rule = ApprovalRequirement::at_least(1)
        .distinct_from("ann")
        .distinct_from("creator");
    assert!(!standing.satisfies(&rule));
    // An event that only claims to be automatic by its flag is treated the same.
    let mut flagged = person("someone");
    flagged.auto = true;
    assert!(!ApprovalStanding::from_events(&[flagged]).satisfies(&rule));
}

#[test]
fn a_mixed_history_counts_only_the_people() {
    let standing = ApprovalStanding::from_events(&[person("ann"), by_sla(), person("bob")]);
    assert_eq!(standing.people().len(), 2);
    assert!(!standing.people().contains(SLA_ACTOR));
    assert!(standing.satisfies(&ApprovalRequirement::at_least(2)));
    // Distinct from the previous approver: ann is excluded, bob still counts.
    let rule = ApprovalRequirement::at_least(1).distinct_from("ann");
    assert_eq!(standing.counted(&rule).len(), 1);
    assert!(standing.satisfies(&rule));
    assert!(!standing.satisfies(&ApprovalRequirement::at_least(2).distinct_from("ann")));
}

#[test]
fn an_automatic_approval_is_shown_as_the_clock_not_a_name() {
    let standing = ApprovalStanding::from_events(&[by_sla()]);
    let display = standing.display();
    assert_eq!(
        display,
        ApprovalDisplay::AutomaticBySla {
            policy: Some("p1".to_owned())
        }
    );
    assert_eq!(display.text(), AUTO_APPROVED_TEXT);
    assert!(!display.text().contains(SLA_ACTOR));
    let mixed = ApprovalStanding::from_events(&[by_sla(), person("ann")]).display();
    assert_eq!(mixed.text(), "ann");
    assert_eq!(
        ApprovalStanding::from_events(&[]).display(),
        ApprovalDisplay::None
    );
}

#[test]
fn a_blank_actor_is_not_an_approver() {
    for actor in ["", " ", "   ", "\t", "\n", " \t\n "] {
        let standing = ApprovalStanding::from_events(&[person(actor)]);
        assert!(standing.people().is_empty(), "actor {actor:?} counted");
        assert!(
            !standing.satisfies(&ApprovalRequirement::at_least(1)),
            "actor {actor:?} satisfied at_least(1)"
        );
        assert!(
            standing
                .counted(&ApprovalRequirement::at_least(1))
                .is_empty()
        );
        assert_eq!(standing.display(), ApprovalDisplay::None);
        assert_eq!(standing.display().text(), "");
    }
}

#[test]
fn a_system_actor_is_not_an_approver_even_without_the_auto_flag() {
    for actor in [
        SLA_ACTOR,
        "system:other",
        "system:",
        "system:import",
        " system:sla",
        "System:SLA",
        "SYSTEM:x",
    ] {
        let standing = ApprovalStanding::from_events(&[person(actor)]);
        assert!(standing.people().is_empty(), "actor {actor:?} counted");
        assert!(
            !standing.satisfies(&ApprovalRequirement::at_least(1)),
            "actor {actor:?} satisfied at_least(1)"
        );
        // A system actor is never shown as a person's name.
        assert!(!standing.display().text().to_lowercase().contains("system"));
    }
}

#[test]
fn invisible_and_unicode_whitespace_actors_are_not_approvers() {
    for actor in [
        "\u{00A0}",
        "\u{2028}",
        "\u{3000}",
        "\u{200B}",
        "\u{FEFF}",
        " \u{200B}\u{200D} ",
        "\u{0}",
        "\u{00A0}system:sla",
        "SyStEm:sla",
    ] {
        let standing = ApprovalStanding::from_events(&[person(actor)]);
        assert!(standing.people().is_empty(), "actor {actor:?} counted");
    }
    // Not system: a space inside the prefix, or a longer name that merely contains it.
    for actor in ["system :x", "my-system:x", "systemx", "sys"] {
        let standing = ApprovalStanding::from_events(&[person(actor)]);
        assert!(
            standing.satisfies(&ApprovalRequirement::at_least(1)),
            "{actor:?}"
        );
    }
}

#[test]
fn a_very_long_actor_is_one_person() {
    let long = "a".repeat(100_000);
    let standing = ApprovalStanding::from_events(&[person(&long), person(&format!(" {long} "))]);
    assert_eq!(standing.people().len(), 1);
}

#[test]
fn the_same_person_with_different_padding_counts_once_and_stays_excluded() {
    let standing = ApprovalStanding::from_events(&[person("ann"), person(" ann\u{00A0}")]);
    assert_eq!(standing.people().len(), 1);
    assert!(!standing.satisfies(&ApprovalRequirement::at_least(2)));
    // A padded approver cannot slip past "distinct from ann", in either direction.
    let padded = ApprovalStanding::from_events(&[person(" ann ")]);
    assert!(!padded.satisfies(&ApprovalRequirement::at_least(1).distinct_from("ann")));
    let plain = ApprovalStanding::from_events(&[person("ann")]);
    assert!(!plain.satisfies(&ApprovalRequirement::at_least(1).distinct_from(" ann ")));
}

#[test]
fn a_blank_does_not_fill_the_second_seat_of_a_two_approver_rule() {
    let standing = ApprovalStanding::from_events(&[person("ann"), person(""), person("  ")]);
    assert_eq!(standing.people().len(), 1);
    assert!(standing.satisfies(&ApprovalRequirement::at_least(1)));
    assert!(!standing.satisfies(&ApprovalRequirement::at_least(2)));
}

#[test]
fn a_mixed_history_of_a_blank_an_sla_event_and_two_people_counts_exactly_two() {
    let standing = ApprovalStanding::from_events(&[
        person(""),
        by_sla(),
        person("ann"),
        person("   "),
        person("bob"),
        person("system:batch"),
    ]);
    assert_eq!(standing.people().len(), 2);
    assert!(standing.people().contains("ann"));
    assert!(standing.people().contains("bob"));
    assert!(standing.satisfies(&ApprovalRequirement::at_least(2)));
    assert!(!standing.satisfies(&ApprovalRequirement::at_least(3)));
    assert_eq!(standing.display().text(), "ann, bob");
}

#[test]
fn the_same_person_twice_counts_once() {
    let standing = ApprovalStanding::from_events(&[person("ann"), person("ann")]);
    assert_eq!(standing.people().len(), 1);
    assert!(!standing.satisfies(&ApprovalRequirement::at_least(2)));
}

#[test]
fn distinct_from_ignores_a_blank() {
    let standing = ApprovalStanding::from_events(&[person("ann")]);
    // Excluding a blank excludes nobody, and a blank approver never appears to be excluded.
    assert!(standing.satisfies(&ApprovalRequirement::at_least(1).distinct_from("")));
    assert!(standing.satisfies(&ApprovalRequirement::at_least(1).distinct_from("   ")));
    let only_blank = ApprovalStanding::from_events(&[person("")]);
    assert!(!only_blank.satisfies(&ApprovalRequirement::at_least(1).distinct_from("ann")));
    assert!(!only_blank.satisfies(&ApprovalRequirement::at_least(1).distinct_from("")));
    // A zero-person requirement still needs one real approver.
    assert!(!only_blank.satisfies(&ApprovalRequirement::at_least(0)));
    assert!(!ApprovalStanding::from_events(&[]).satisfies(&ApprovalRequirement::at_least(0)));
}

#[test]
fn satisfies_is_the_decision_and_agrees_with_counted() {
    // The doc-stated rule: enough approvals means `satisfies`, which is `counted` against the
    // requirement; a caller never compares `people().len()` itself.
    let standing = ApprovalStanding::from_events(&[person("ann"), person(""), person("bob")]);
    for needed in 0..=3_usize {
        let rule = ApprovalRequirement::at_least(needed).distinct_from("ann");
        assert_eq!(
            standing.satisfies(&rule),
            standing.counted(&rule).len() >= needed.max(1)
        );
    }
    // people() holds 2 but one is excluded, so a 2-person rule that excludes ann is not met.
    assert_eq!(standing.people().len(), 2);
    assert!(!standing.satisfies(&ApprovalRequirement::at_least(2).distinct_from("ann")));
}

#[test]
fn a_blank_actor_event_is_not_an_automatic_approval_either() {
    let standing = ApprovalStanding::from_events(&[person(""), person("ann")]);
    assert_eq!(
        standing.display(),
        ApprovalDisplay::People(vec!["ann".to_owned()])
    );
}

#[test]
fn no_node_in_the_crate_decides_sufficiency_on_its_own() {
    // Every "enough approvals" decision goes through `ApprovalStanding::satisfies`. A node that
    // takes the length of the approver set itself would count people the requirement excludes.
    let needle = ["people()", ".len()"].concat();
    let needle_compact = ["people().", "len()"].concat();
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut seen = 0;
    let mut stack = vec![src];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read src") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                seen += 1;
                let text: String = std::fs::read_to_string(&path)
                    .expect("read")
                    .split_whitespace()
                    .collect();
                assert!(
                    !text.contains(&needle_compact) && !text.contains(&needle.replace(' ', "")),
                    "{} counts approvers itself instead of calling ApprovalStanding::satisfies",
                    path.display()
                );
            }
        }
    }
    assert!(seen > 0, "no sources scanned");
}

#[test]
fn entry_and_tasks_share_one_is_person() {
    for actor in [
        "",
        " ",
        "\u{00A0}",
        "\u{2028}",
        "\u{200B}",
        "system:sla",
        "System:SLA",
        " system:x",
        "ann",
        " ann ",
        "Ann",
    ] {
        assert_eq!(
            spot_bpm_entry::is_person(actor),
            spot_bpm_tasks::is_person(actor),
            "{actor:?}"
        );
    }
    let same: fn(&str) -> bool = spot_bpm_entry::is_person;
    assert!(same("ann") && !same("system:sla"));
}
