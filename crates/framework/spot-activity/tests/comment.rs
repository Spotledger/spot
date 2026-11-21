//! Comments: mention parsing edge cases, edit rules, soft delete (WP-DOC-07.T1).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_activity::comment::{Comment, EditCheck, can_edit, parse_mentions};
use std::time::Duration;

#[test]
fn a_plain_mention_is_found() {
    assert_eq!(parse_mentions("ping @bob please"), vec!["bob"]);
}

#[test]
fn an_email_address_is_not_a_mention() {
    assert!(parse_mentions("write to name@example.com today").is_empty());
    // Even at the text start the e-mail shape is one word; the @ is mid-word.
    assert!(parse_mentions("name@example.com").is_empty());
}

#[test]
fn punctuation_ends_a_mention() {
    assert_eq!(parse_mentions("thanks @bob, well done"), vec!["bob"]);
    assert_eq!(parse_mentions("(see @bob)"), vec!["bob"]);
    assert_eq!(parse_mentions("@bob."), vec!["bob"]);
}

#[test]
fn a_mention_at_text_start_and_after_space() {
    assert_eq!(parse_mentions("@bob hi"), vec!["bob"]);
    assert_eq!(parse_mentions("hi @bob"), vec!["bob"]);
    assert_eq!(parse_mentions("hi\t@bob"), vec!["bob"]);
    assert_eq!(parse_mentions("hi\n@bob"), vec!["bob"]);
}

#[test]
fn word_characters_before_at_block_a_mention() {
    assert!(parse_mentions("user@host").is_empty());
    assert!(parse_mentions("a@b").is_empty());
    // Digits count as word characters.
    assert!(parse_mentions("x2@y").is_empty());
}

#[test]
fn capitals_and_hyphens_are_not_name_characters() {
    // The scan stops at the first non-name byte; capital letters simply make it stop early.
    assert!(parse_mentions("@Bob").is_empty());
    assert_eq!(
        parse_mentions("@bob-x"),
        vec!["bob"],
        "the hyphen ends the token"
    );
    // Digits and underscores belong to the name.
    assert_eq!(parse_mentions("@user_2x"), vec!["user_2x"]);
}

#[test]
fn mentions_are_unique_in_first_seen_order() {
    assert_eq!(
        parse_mentions("@bob and @amy and @bob again"),
        vec!["bob", "amy"]
    );
}

#[test]
fn an_empty_or_atless_body_has_no_mentions() {
    assert!(parse_mentions("").is_empty());
    assert!(parse_mentions("no mentions here").is_empty());
    assert!(parse_mentions("@").is_empty());
    assert!(parse_mentions("@@").is_empty());
    assert!(parse_mentions("@ ").is_empty());
}

#[test]
fn a_63_byte_name_is_a_mention_and_longer_is_not() {
    let ok = "a".repeat(63);
    assert_eq!(parse_mentions(&format!("@{ok}")), vec![ok.as_str()]);
    let too_long = "a".repeat(64);
    assert!(parse_mentions(&format!("@{too_long}")).is_empty());
}

#[test]
fn comment_new_parses_mentions_once() {
    let c = Comment::new(uuid7(1, 1), uuid7(1, 2), "amy", at(0), "@bob look");
    assert_eq!(c.mentions, vec!["bob"]);
    assert_eq!(c.body, "@bob look");
    assert!(!c.deleted);
    assert_eq!(c.edited_at, None);
}

#[test]
fn soft_delete_blanks_the_rendered_body_but_keeps_storage() {
    let mut c = Comment::new(uuid7(1, 1), uuid7(1, 2), "amy", at(0), "secret words");
    assert_eq!(c.rendered_body(), "secret words");
    c.deleted = true;
    assert_eq!(
        c.rendered_body(),
        "",
        "the row stays, the rendered body blanks"
    );
    assert_eq!(c.body, "secret words", "storage keeps the body until purge");
    assert!(!c.is_present());
}

#[test]
fn only_the_author_edits() {
    let c = Comment::new(uuid7(1, 1), uuid7(1, 2), "amy", at(0), "hi");
    assert!(can_edit(&c, "amy", at(10), Duration::from_secs(60)).is_allowed());
    assert_eq!(
        can_edit(&c, "bob", at(10), Duration::from_secs(60)),
        EditCheck::NotAuthor
    );
}

#[test]
fn edits_close_on_the_window_boundary() {
    let c = Comment::new(uuid7(1, 1), uuid7(1, 2), "amy", at(0), "hi");
    // Window of 60s: at exactly +60s the edit is still in (the deadline is inclusive).
    assert!(can_edit(&c, "amy", at(60), Duration::from_secs(60)).is_allowed());
    // One second later it is not.
    assert_eq!(
        can_edit(&c, "amy", at(61), Duration::from_secs(60)),
        EditCheck::WindowClosed
    );
}

#[test]
fn a_deleted_comment_never_edits() {
    let mut c = Comment::new(uuid7(1, 1), uuid7(1, 2), "amy", at(0), "hi");
    c.deleted = true;
    assert_eq!(
        can_edit(&c, "amy", at(0), Duration::from_secs(60)),
        EditCheck::Deleted
    );
}

#[test]
fn the_window_is_whatever_the_caller_passes() {
    let c = Comment::new(uuid7(1, 1), uuid7(1, 2), "amy", at(0), "hi");
    // No window: closed immediately after creation.
    assert_eq!(
        can_edit(&c, "amy", at(1), Duration::ZERO),
        EditCheck::WindowClosed
    );
    assert!(can_edit(&c, "amy", at(0), Duration::ZERO).is_allowed());
    // A day.
    assert!(can_edit(&c, "amy", at(86_400), Duration::from_secs(86_400)).is_allowed());
}

#[test]
fn comment_serde_round_trip() {
    let mut c = Comment::new(uuid7(1, 1), uuid7(1, 2), "amy", at(0), "hi @bob");
    c.edited_at = Some(at(5));
    let json = serde_json::to_string(&c).unwrap();
    let back: Comment = serde_json::from_str(&json).unwrap();
    assert_eq!(back, c);
    // Stable names: id, doc, author, at, body, mentions, edited_at, deleted.
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    for key in [
        "id",
        "doc",
        "author",
        "at",
        "body",
        "mentions",
        "edited_at",
        "deleted",
    ] {
        assert!(parsed.get(key).is_some(), "missing {key} in {json}");
    }
}
