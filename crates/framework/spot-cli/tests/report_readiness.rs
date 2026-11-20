//! WP-PLT-14.T7: the readiness detail of the reports, `in flight n/max`, read off the site's real
//! [`ReportGate`]. `n` grows and shrinks with the runs, a refused run does not change it, it never
//! passes `max`, and the text carries two numbers and nothing else. The readiness check object
//! (`ReportReadiness`) prints exactly `ReportGate::status`; building it needs a whole site, which
//! the l3 cases cover.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_cli::services::report::ReportGate;

#[test]
fn the_detail_is_two_numbers_and_follows_the_permits() {
    let gate = ReportGate::new(3, 3);
    assert_eq!(gate.status(), "in flight 0/3");
    let a = gate.try_acquire("u1").expect("a place");
    assert_eq!(gate.status(), "in flight 1/3");
    let b = gate.try_acquire("u2").expect("a place");
    assert_eq!(gate.status(), "in flight 2/3");
    drop(a);
    assert_eq!(gate.status(), "in flight 1/3");
    drop(b);
    assert_eq!(gate.status(), "in flight 0/3");
}

#[test]
fn a_refused_run_does_not_change_the_count_and_the_count_never_passes_max() {
    let gate = ReportGate::new(2, 2);
    let a = gate.try_acquire("u1").expect("a place");
    let b = gate.try_acquire("u2").expect("a place");
    for user in ["u1", "u2", "u3"] {
        assert!(
            gate.try_acquire(user).is_err(),
            "{user} is refused at the cap"
        );
        assert_eq!(gate.status(), "in flight 2/2", "a refusal changes nothing");
    }
    drop(a);
    let c = gate.try_acquire("u3").expect("a place frees up");
    assert_eq!(gate.in_flight(), 2);
    drop((b, c));
    assert_eq!(gate.status(), "in flight 0/2");
}

#[test]
fn a_per_user_refusal_does_not_change_the_count_either() {
    let gate = ReportGate::new(4, 1);
    let held = gate.try_acquire("u1").expect("a place");
    assert!(gate.try_acquire("u1").is_err());
    assert_eq!(gate.status(), "in flight 1/4");
    drop(held);
}

#[test]
fn the_detail_names_no_user_report_or_site() {
    let gate = ReportGate::new(2, 2);
    let _permit = gate.try_acquire("alice@example.invalid").expect("a place");
    let text = gate.status();
    assert!(!text.contains("alice"), "{text}");
    assert!(
        text.chars()
            .all(|c| c.is_ascii_digit() || " /inflight".contains(c)),
        "only the words and two numbers: {text}"
    );
}
