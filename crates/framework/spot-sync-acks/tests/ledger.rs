//! The ledger with a virtual clock. Default settings: window 24h, amber at 12h, overdue at 24h,
//! red at 30h (grace is 25% of the window).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_conditions::{AuditEvent, MemorySink, Severity};
use spot_sync_acks::{
    AckError, AckLedger, LevelChange, MemoryStore, OBLIGATION_DUE_SOON, OBLIGATION_OVERDUE,
    OBLIGATION_RED, StreamHead,
};
use spot_sync_obligations::{Level, MandatoryEvent, ObligationSettings, Recipient, ReportStatus};

const T: &str = "t1";
const H: i64 = 3_600_000;
/// A hub head above every position these tests use.
const HEAD: StreamHead = StreamHead::new(100);

type Ledger = AckLedger<MemoryStore, MemorySink>;

fn event(position: u64, published: i64) -> MandatoryEvent {
    MandatoryEvent {
        position,
        published_at_millis: published,
        effective_from_millis: None,
        record: format!("tax_rate/{position}"),
    }
}

fn ledger() -> Ledger {
    AckLedger::new(
        MemoryStore::new(),
        MemorySink::new(),
        defs(),
        ObligationSettings::default(),
    )
    .unwrap()
}

fn open_ids(l: &Ledger) -> Vec<String> {
    l.conditions()
        .open()
        .iter()
        .map(|o| o.id.as_str().to_owned())
        .collect()
}

#[test]
fn applying_inside_the_window_notifies_nobody() {
    let mut l = ledger();
    l.register_node("n1", T, "1.0", 0).unwrap();
    assert!(l.register_mandatory(T, event(5, 0)));
    let out = l.on_ack("n1", T, 5, HEAD, H).unwrap();
    let s = out.satisfied.unwrap();
    assert_eq!((s.through_position, s.count), (5, 1));
    assert_eq!(s.received_at_observed_millis, H);
    let report = l.tick(25 * H).unwrap();
    assert!(report.notifications.is_empty());
    assert!(open_ids(&l).is_empty());
    assert!(l.conditions().sink().is_empty());
    assert_eq!(l.level("n1", T), Some(Level::Ok));
}

#[test]
fn a_missed_event_steps_the_ladder_at_the_right_instants() {
    let mut l = ledger();
    l.register_node("n1", T, "1.0", 0).unwrap();
    l.register_mandatory(T, event(5, 0));

    assert!(l.tick(12 * H - 1).unwrap().notifications.is_empty());
    assert!(open_ids(&l).is_empty());

    // Amber at half the window: information, nobody is paged.
    let r = l.tick(12 * H).unwrap();
    assert!(r.notifications.is_empty());
    assert_eq!(
        r.level_changes,
        vec![LevelChange {
            node: "n1".into(),
            tenant: T.into(),
            level: Level::Amber
        }]
    );
    assert_eq!(open_ids(&l), [OBLIGATION_DUE_SOON]);

    // Overdue at the deadline: the node contact.
    let r = l.tick(24 * H).unwrap();
    assert_eq!(r.notifications.len(), 1);
    assert_eq!(r.notifications[0].notify.who, Recipient::NodeContact);
    assert_eq!(r.notifications[0].notify.node, "n1");
    assert_eq!(r.notifications[0].notify.obligations, vec![5]);
    assert_eq!(open_ids(&l), [OBLIGATION_DUE_SOON, OBLIGATION_OVERDUE]);

    assert!(l.tick(30 * H - 1).unwrap().notifications.is_empty());

    // Red once the grace has elapsed: sync_admin.
    let r = l.tick(30 * H).unwrap();
    assert_eq!(r.notifications.len(), 1);
    assert_eq!(r.notifications[0].notify.who, Recipient::SyncAdmin);
    assert_eq!(l.level("n1", T), Some(Level::Red));

    // A repeated tick changes and emits nothing; each step was exactly one audit event.
    let again = l.tick(31 * H).unwrap();
    assert!(again.notifications.is_empty() && again.level_changes.is_empty());
    let events = l.conditions().sink().events();
    assert_eq!(events.len(), 3);
    let severities: Vec<Severity> = events.iter().map(AuditEvent::severity).collect();
    assert_eq!(severities, [Severity::Info, Severity::Amber, Severity::Red]);
    for e in events {
        assert_eq!(e.subject().as_str(), "node:n1@tenant:t1");
        assert_eq!(e.params().get("node"), Some("n1"));
        assert_eq!(e.params().get("tenant"), Some("t1"));
        assert_eq!(e.params().get("positions"), Some("5"));
        assert!(e.params().get("level").is_some());
        // No record key or payload anywhere in the event.
        assert!(!format!("{e:?}").contains("tax_rate"));
    }
}

#[test]
fn a_jump_to_red_notifies_both_recipients() {
    let mut l = ledger();
    l.register_node("n1", T, "1.0", 0).unwrap();
    l.register_mandatory(T, event(5, 0));
    let r = l.tick(40 * H).unwrap();
    let who: Vec<Recipient> = r.notifications.iter().map(|n| n.notify.who).collect();
    assert_eq!(who, [Recipient::NodeContact, Recipient::SyncAdmin]);
}

#[test]
fn an_effective_date_makes_the_node_red_at_once() {
    let mut l = ledger();
    l.register_node("n1", T, "1.0", 0).unwrap();
    l.register_mandatory(
        T,
        MandatoryEvent {
            effective_from_millis: Some(30 * H),
            ..event(5, 0)
        },
    );
    // 24h before the effective date.
    let r = l.tick(6 * H).unwrap();
    assert_eq!(l.level("n1", T), Some(Level::Red));
    assert!(
        r.notifications
            .iter()
            .any(|n| n.notify.who == Recipient::SyncAdmin)
    );
    assert!(open_ids(&l).contains(&OBLIGATION_RED.to_owned()));
}

#[test]
fn an_offline_node_escalates_and_a_late_ack_clears_and_records_satisfaction() {
    let mut l = ledger();
    l.register_node("n1", T, "1.0", 0).unwrap();
    l.register_mandatory(T, event(5, 0));
    l.tick(40 * H).unwrap();
    assert_eq!(open_ids(&l).len(), 3);

    let out = l.on_ack("n1", T, 5, HEAD, 41 * H).unwrap();
    assert_eq!(out.satisfied.unwrap().count, 1);
    assert!(out.notifications.is_empty());
    assert!(open_ids(&l).is_empty());
    assert_eq!(l.level("n1", T), Some(Level::Ok));

    // The clears are committed once the debounce window closes.
    let r = l.tick(41 * H + 60_000).unwrap();
    assert_eq!(r.conditions.cleared, 3);
    let kinds: Vec<&str> = l
        .conditions()
        .sink()
        .events()
        .iter()
        .map(|e| e.transition().as_str())
        .collect();
    assert_eq!(
        kinds,
        [
            "raised", "raised", "raised", "cleared", "cleared", "cleared"
        ]
    );

    let row = l.evidence(T, 5, "n1").unwrap();
    assert_eq!(row.status, ReportStatus::AppliedAt(41 * H));
    assert!(l.who_is_missing(T, 5).unwrap().is_empty());
}

#[test]
fn a_regressed_prefix_is_refused_and_changes_nothing() {
    let mut l = ledger();
    l.register_node("n1", T, "1.0", 0).unwrap();
    l.register_mandatory(T, event(3, 0));
    l.register_mandatory(T, event(5, 0));
    l.on_ack("n1", T, 5, HEAD, H).unwrap();
    let before = l.store().clone();
    let err = l.on_ack("n1", T, 3, HEAD, 2 * H).unwrap_err();
    assert_eq!(err.code(), "sync_acks.prefix_regressed");
    assert_eq!(
        err,
        AckError::PrefixRegressed {
            node: "n1".into(),
            tenant: T.into(),
            before: 5,
            after: 3
        }
    );
    // Nothing else raised: state, audit and conditions are as they were.
    assert_eq!(format!("{:?}", l.store()), format!("{before:?}"));
    assert!(l.conditions().sink().is_empty());
    // An equal prefix is a plain check-in.
    assert!(
        l.on_ack("n1", T, 5, HEAD, 3 * H)
            .unwrap()
            .satisfied
            .is_none()
    );
}

#[test]
fn rebuilt_from_stored_state_gives_the_same_levels() {
    let mut l = ledger();
    l.register_node("n1", T, "1.0", 0).unwrap();
    l.register_node("n2", T, "1.0", 0).unwrap();
    l.register_mandatory(T, event(5, 0));
    l.register_mandatory(T, event(6, 10 * H));
    l.on_ack("n2", T, 5, HEAD, H).unwrap();
    l.tick(26 * H).unwrap();
    let levels = (l.level("n1", T), l.level("n2", T));
    assert_eq!(levels, (Some(Level::Overdue), Some(Level::Amber)));

    let mut rebuilt: Ledger = AckLedger::new(
        l.store().clone(),
        MemorySink::new(),
        defs(),
        ObligationSettings::default(),
    )
    .unwrap();
    let r = rebuilt.tick(26 * H).unwrap();
    assert_eq!((rebuilt.level("n1", T), rebuilt.level("n2", T)), levels);
    // The stored level is the last notified one: nobody is notified twice after a restart.
    assert!(r.notifications.is_empty() && r.level_changes.is_empty());
}

#[test]
fn one_nodes_ack_never_clears_another_nodes_obligation() {
    let mut l = ledger();
    l.register_node("n1", T, "1.0", 0).unwrap();
    l.register_node("n2", T, "1.0", 0).unwrap();
    l.register_mandatory(T, event(5, 0));
    l.on_ack("n1", T, 5, HEAD, H).unwrap();
    l.tick(30 * H).unwrap();
    assert_eq!(l.level("n1", T), Some(Level::Ok));
    assert_eq!(l.level("n2", T), Some(Level::Red));
    let rows = l.who_is_missing(T, 5).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].node, "n2");
    // An ack under an id nobody registered is refused.
    let err = l.on_ack("n3", T, 5, HEAD, 31 * H).unwrap_err();
    assert_eq!(err.code(), "sync_acks.unknown_node");
}

#[test]
fn who_is_missing_is_as_of_the_last_report() {
    let mut l = ledger();
    l.register_node("n1", T, "2.1", 0).unwrap();
    l.register_mandatory(T, event(5, 0));
    l.on_checkin("n1", T, 7 * H).unwrap();
    let rows = l.who_is_missing(T, 5).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].last_contact_millis, 7 * H);
    assert_eq!(rows[0].version, "2.1");
    assert_eq!(rows[0].applied_prefix, 0);
    assert!(!rows[0].held);
    let row = l.evidence(T, 5, "n1").unwrap();
    assert_eq!(
        row.status,
        ReportStatus::OutstandingSince {
            since_millis: 0,
            last_seen_millis: 7 * H
        }
    );
    assert_eq!(
        l.who_is_missing(T, 9).unwrap_err().code(),
        "sync_acks.unknown_event"
    );
}

#[test]
fn a_held_record_stays_outstanding_until_released() {
    let mut l = ledger();
    l.register_node("n1", T, "1.0", 0).unwrap();
    l.register_mandatory(T, event(5, 0));
    assert_eq!(
        l.set_holds("n1", T, vec![9], HEAD, 0).unwrap_err().code(),
        "sync_acks.hold_unknown"
    );
    let out = l.on_ack("n1", T, 8, HEAD, H).unwrap();
    assert!(out.satisfied.is_some());
    l.set_holds("n1", T, vec![5], HEAD, H).unwrap();
    l.tick(25 * H).unwrap();
    assert_eq!(l.level("n1", T), Some(Level::Overdue));
    assert!(l.who_is_missing(T, 5).unwrap()[0].held);
    let out = l.set_holds("n1", T, vec![], HEAD, 26 * H).unwrap();
    assert_eq!(out.satisfied.unwrap().count, 1);
    assert_eq!(l.level("n1", T), Some(Level::Ok));
}

#[test]
fn registration_is_idempotent_and_definitions_are_checked() {
    let mut l = ledger();
    assert!(l.register_mandatory(T, event(5, 0)));
    assert!(!l.register_mandatory(T, event(5, 99)));
    assert_eq!(
        spot_sync_acks::NodeStateStore::events(l.store(), T).len(),
        1
    );
    assert!(l.register_node("n1", T, "1", 0).unwrap());
    assert!(!l.register_node("n1", T, "2", 5).unwrap());

    let mut short = defs();
    short.pop();
    let err = AckLedger::new(
        MemoryStore::new(),
        MemorySink::new(),
        short,
        ObligationSettings::default(),
    )
    .unwrap_err();
    assert_eq!(err.code(), "sync_acks.condition_missing");
    let bad = ObligationSettings {
        delivery_window_millis: 0,
        ..ObligationSettings::default()
    };
    let err = AckLedger::new(MemoryStore::new(), MemorySink::new(), defs(), bad).unwrap_err();
    assert_eq!(err.code(), "sync_obligations.settings_invalid");
}

/// The three obligation definitions, taken from the catalogue.
fn defs() -> Vec<spot_conditions::ConditionDef> {
    [
        "OBLIGATION_DUE_SOON",
        "OBLIGATION_OVERDUE",
        "OBLIGATION_RED",
    ]
    .into_iter()
    .map(|id| spot_sync_conditions::entry(id).unwrap().def)
    .collect()
}

#[test]
fn the_ledger_uses_the_catalogue_definitions() {
    let l = ledger();
    let registry = l.conditions().registry();
    for id in [OBLIGATION_DUE_SOON, OBLIGATION_OVERDUE, OBLIGATION_RED] {
        let def = registry.get(id).unwrap();
        // The message key is the catalogue's, not a private one.
        assert_eq!(
            def.explanation_key,
            format!("sync.condition.{}.explanation", id.to_ascii_lowercase())
        );
        // The parameters are exactly the ones the ledger raises with.
        assert_eq!(def.params, ["node", "level", "positions", "count"]);
        assert_eq!(def, &spot_sync_conditions::entry(id).unwrap().def);
    }
}

#[test]
fn with_catalogue_builds_the_same_ledger() {
    let l = AckLedger::with_catalogue(
        MemoryStore::new(),
        MemorySink::new(),
        ObligationSettings::default(),
    )
    .unwrap();
    assert!(l.conditions().registry().get(OBLIGATION_RED).is_some());
    let bad = ObligationSettings {
        delivery_window_millis: 0,
        ..ObligationSettings::default()
    };
    let err = AckLedger::with_catalogue(MemoryStore::new(), MemorySink::new(), bad).unwrap_err();
    assert_eq!(err.code(), "sync_obligations.settings_invalid");
}

fn actions(r: &spot_sync_acks::TickReport) -> Vec<(Level, &str)> {
    r.actions
        .iter()
        .map(|a| (a.level, a.action.as_str()))
        .collect()
}

#[test]
fn the_catalogue_response_action_ids_come_out_per_level() {
    let mut l = ledger();
    l.register_node("n1", T, "1.0", 0).unwrap();
    l.register_mandatory(T, event(5, 0));

    // Amber has no response; the first overdue tick puts the mandatory lane first, then notifies.
    assert!(l.tick(12 * H).unwrap().actions.is_empty());
    let r = l.tick(24 * H).unwrap();
    assert_eq!(
        actions(&r),
        [
            (Level::Overdue, "mandatory_lane_first"),
            (Level::Overdue, "notify_contact"),
        ]
    );
    assert!(
        r.actions
            .iter()
            .all(|a| a.node == "n1" && a.tenant == T && a.limit == 1)
    );
    // Red notifies the role.
    assert_eq!(
        actions(&l.tick(30 * H).unwrap()),
        [(Level::Red, "notify_role")]
    );
    // Nothing is repeated at the same level.
    assert!(l.tick(31 * H).unwrap().actions.is_empty());
}

#[test]
fn a_jump_to_red_reports_the_actions_of_both_rungs() {
    let mut l = ledger();
    l.register_node("n1", T, "1.0", 0).unwrap();
    l.register_mandatory(T, event(5, 0));
    let r = l.tick(40 * H).unwrap();
    assert_eq!(
        actions(&r),
        [
            (Level::Overdue, "mandatory_lane_first"),
            (Level::Overdue, "notify_contact"),
            (Level::Red, "notify_role"),
        ]
    );
}

#[test]
fn a_custom_list_still_reports_a_missing_condition() {
    let mut custom = defs();
    custom.retain(|d| d.id != OBLIGATION_RED);
    let err = AckLedger::new(
        MemoryStore::new(),
        MemorySink::new(),
        custom,
        ObligationSettings::default(),
    )
    .unwrap_err();
    assert_eq!(err.code(), "sync_acks.condition_missing");
}
