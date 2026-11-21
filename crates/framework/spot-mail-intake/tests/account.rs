//! The account model's validation table.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::time::Duration;

use spot_mail_intake::Account;
use support::account;

fn broken(change: impl FnOnce(&mut Account)) -> Option<&'static str> {
    let mut a = account();
    change(&mut a);
    a.validate().err().map(|e| e.field)
}

#[test]
fn a_good_account_validates() {
    assert!(account().validate().is_ok());
}

#[test]
fn every_rule_names_its_field() {
    assert_eq!(broken(|a| a.name = "Bad-Name".into()), Some("name"));
    assert_eq!(broken(|a| a.name = String::new()), Some("name"));
    assert_eq!(broken(|a| a.name = "a".repeat(64)), Some("name"));
    assert_eq!(broken(|a| a.imap.host = " ".into()), Some("imap.host"));
    assert_eq!(broken(|a| a.imap.port = 0), Some("imap.port"));
    assert_eq!(
        broken(|a| a.imap.username = String::new()),
        Some("imap.username")
    );
    assert_eq!(
        broken(|a| a.imap.timeout = Duration::ZERO),
        Some("imap.timeout")
    );
    assert_eq!(
        broken(|a| a.credential_ref = String::new()),
        Some("credential_ref")
    );
    assert_eq!(broken(|a| a.mailbox = String::new()), Some("mailbox"));
    assert_eq!(broken(|a| a.address = "nobody".into()), Some("address"));
}

#[test]
fn the_interval_is_between_thirty_seconds_and_a_day() {
    assert_eq!(
        broken(|a| a.poll_interval = Duration::from_secs(29)),
        Some("poll_interval")
    );
    assert_eq!(broken(|a| a.poll_interval = Duration::from_secs(30)), None);
    assert_eq!(
        broken(|a| a.poll_interval = Duration::from_secs(86_400)),
        None
    );
    assert_eq!(
        broken(|a| a.poll_interval = Duration::from_secs(86_401)),
        Some("poll_interval")
    );
}

#[test]
fn the_limits_have_bounds() {
    assert_eq!(
        broken(|a| a.limits.max_messages_per_poll = 0),
        Some("limits.max_messages_per_poll")
    );
    assert_eq!(
        broken(|a| a.limits.max_messages_per_poll = 201),
        Some("limits.max_messages_per_poll")
    );
    assert_eq!(broken(|a| a.limits.max_messages_per_poll = 200), None);
    assert_eq!(
        broken(|a| a.limits.max_message_bytes = 0),
        Some("limits.max_message_bytes")
    );
    assert_eq!(
        broken(|a| a.limits.max_message_bytes = 25 * 1024 * 1024 + 1),
        Some("limits.max_message_bytes")
    );
    assert_eq!(
        broken(|a| a.limits.max_messages_per_day = 0),
        Some("limits.max_messages_per_day")
    );
    assert_eq!(
        broken(|a| a.limits.max_messages_per_day = 100_001),
        Some("limits.max_messages_per_day")
    );
}

#[test]
fn the_error_text_names_the_field_and_no_value() {
    let mut a = account();
    a.address = "secret-value".into();
    let text = a.validate().unwrap_err().to_string();
    assert!(text.contains("address"));
    assert!(!text.contains("secret-value"));
}
