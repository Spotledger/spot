//! The `channel` and `os_user` fields of the identity audit record (WP-PRD-01.T55): they
//! round-trip, they are skipped when absent so a record (and a chain) written before they existed
//! is byte for byte what it was, the channel is a closed set, and the OS account is bounded and
//! redacted like a sign-in name.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_cli::audit::{ATTEMPT_NAME_MAX, AuditRecord, CHANNELS};

fn record() -> AuditRecord {
    AuditRecord {
        at: 1_800_000_000,
        site: "site".into(),
        kind: "console_create".into(),
        user: Some("u-1".into()),
        attempted: None,
        source: None,
        reason: Some("label".into()),
        session: None,
        count: None,
        until: None,
        key_id: None,
        path: None,
        channel: None,
        os_user: None,
    }
}

/// A record of the shape written before this task: no `channel`, no `os_user`.
const PRE_CHANGE_LINE: &str = "{\"at\":1800000000,\"site\":\"site\",\"kind\":\"console_create\",\"user\":\"u-1\",\"reason\":\"label\"}";

#[test]
fn a_record_without_the_new_fields_serialises_exactly_as_before() {
    // The chain hashes the stored line: any difference here would break every existing chain.
    assert_eq!(serde_json::to_string(&record()).unwrap(), PRE_CHANGE_LINE);
}

#[test]
fn a_record_with_the_fields_carries_them_and_nothing_else_changes() {
    let mut with = record();
    assert!(with.set_channel("console", Some("ops")));
    let text = serde_json::to_string(&with).unwrap();
    assert!(text.contains("\"channel\":\"console\""), "{text}");
    assert!(text.contains("\"os_user\":\"ops\""), "{text}");
    assert!(!text.contains("\"path\""), "{text}");
    assert!(!text.contains("\"attempted\""), "{text}");
    // The shared prefix is the pre-change line: the new fields are appended, nothing moves.
    let prefix = PRE_CHANGE_LINE.trim_end_matches('}');
    assert!(text.starts_with(prefix), "{text}");
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["channel"], "console");
    assert_eq!(value["os_user"], "ops");
}

#[test]
fn the_channel_is_a_closed_set() {
    for channel in CHANNELS {
        let mut r = record();
        assert!(r.set_channel(channel, None));
        assert_eq!(r.channel.as_deref(), Some(channel));
        assert_eq!(r.os_user, None);
    }
    let mut r = record();
    assert!(!r.set_channel("somewhere-else", Some("ops")));
    assert_eq!(r.channel, None);
    assert_eq!(r.os_user, None, "a refused channel changes nothing");
}

#[test]
fn the_os_user_is_redacted_and_bounded_like_a_sign_in_name() {
    let mut r = record();
    // A token-shaped value is never kept.
    r.set_channel("console", Some(&"a".repeat(64)));
    assert_eq!(r.os_user.as_deref(), Some("[redacted]"));
    // A long account name is cut to the bound.
    let long = format!("DOMAIN\\{}", "u ".repeat(200));
    r.set_channel("console", Some(&long));
    assert!(r.os_user.as_ref().unwrap().chars().count() <= ATTEMPT_NAME_MAX);
    // An ordinary name is kept as it is.
    r.set_channel("cli", Some("deploy"));
    assert_eq!(r.os_user.as_deref(), Some("deploy"));
    assert_eq!(r.channel.as_deref(), Some("cli"));
}
