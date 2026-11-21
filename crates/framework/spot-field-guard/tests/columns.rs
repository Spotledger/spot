//! Columns for list, export, print and report, and the no-inference rule for user queries.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_field_guard::{Channel, QueryField, QueryUse};

fn cols(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn a_reporter_gets_allowed_and_masked_columns() {
    let fx = fixture();
    let requested = cols(&["number", "card_number", "salary", "items.qty", "items.iban"]);
    let analyst = fx
        .guard
        .columns(Channel::Export, &principal("analyst"), DOCTYPE, &requested)
        .unwrap();
    assert_eq!(analyst.allowed, requested);
    assert!(analyst.masked.is_empty() && analyst.refused.is_empty());

    let masked = fx
        .guard
        .columns(
            Channel::ListColumn,
            &principal("masked_reader"),
            DOCTYPE,
            &requested,
        )
        .unwrap();
    assert_eq!(masked.allowed, cols(&["number", "items.qty"]));
    assert_eq!(
        masked.masked,
        cols(&["card_number", "salary", "items.iban"])
    );
}

#[test]
fn a_refused_column_names_its_path_and_is_never_silently_dropped() {
    let fx = fixture();
    let requested = cols(&["number", "card_number", "items.iban"]);
    for channel in [
        Channel::Export,
        Channel::Report,
        Channel::ListColumn,
        Channel::Search,
    ] {
        let err = fx
            .guard
            .columns(channel, &principal("read_no_report"), DOCTYPE, &requested)
            .unwrap_err();
        assert_eq!(err.code(), "permission.field_denied");
        assert_eq!(err.paths(), requested.as_slice(), "{channel:?}");
        let app = err.to_app_error();
        assert_eq!(app.code, "permission.field_denied");
        assert_eq!(app.fields.len(), 3);
        assert_eq!(app.fields[1].field.to_string(), "card_number");
    }
    // A column that does not resolve (an unknown table) is refused, not passed.
    let err = fx
        .guard
        .columns(
            Channel::Export,
            &principal("analyst"),
            DOCTYPE,
            &cols(&["ghost.col"]),
        )
        .unwrap_err();
    assert_eq!(err.paths(), ["ghost.col"]);
}

#[test]
fn print_prints_the_masked_form_instead_of_refusing() {
    let fx = fixture();
    let requested = cols(&["number", "card_number", "annual_cost", "items.iban"]);
    let c = fx
        .guard
        .columns(
            Channel::Print,
            &principal("read_no_report"),
            DOCTYPE,
            &requested,
        )
        .unwrap();
    // `read` without `report`: protected fields print masked; a field with no read at all
    // (`annual_cost`, `items.iban`) is left off and listed, not an error.
    assert_eq!(c.masked, cols(&["number", "card_number"]));
    assert_eq!(c.refused, cols(&["annual_cost", "items.iban"]));
    assert!(c.allowed.is_empty());
}

#[test]
fn a_names_only_channel_has_no_columns() {
    let fx = fixture();
    let err = fx
        .guard
        .columns(
            Channel::Notification,
            &principal("admin"),
            DOCTYPE,
            &cols(&["number"]),
        )
        .unwrap_err();
    assert_eq!(err.code(), "permission.field_denied");
}

fn q(path: &str, usage: QueryUse) -> QueryField {
    QueryField::new(path, usage)
}

#[test]
fn a_filter_on_a_field_read_only_masked_is_refused() {
    let fx = fixture();
    let masked = principal("masked_reader");
    let err = fx
        .guard
        .check_query_fields(
            &masked,
            DOCTYPE,
            &[
                q("number", QueryUse::Filter),
                q("card_number", QueryUse::Filter),
            ],
        )
        .unwrap_err();
    assert_eq!(err.code(), "permission.field_denied");
    assert_eq!(err.paths(), ["card_number"]);
    // The same query is fine for a reader who may read the field unmasked.
    fx.guard
        .check_query_fields(
            &principal("analyst"),
            DOCTYPE,
            &[
                q("card_number", QueryUse::Filter),
                q("items.iban", QueryUse::Sort),
            ],
        )
        .unwrap();
}

#[test]
fn an_aggregate_over_a_hidden_field_is_refused() {
    let fx = fixture();
    let err = fx
        .guard
        .check_query_fields(
            &principal("clerk"),
            DOCTYPE,
            &[
                q("salary", QueryUse::Aggregate),
                q("salary", QueryUse::Group),
                q("number", QueryUse::Sort),
            ],
        )
        .unwrap_err();
    // One path, named once, however many ways the query used it. A clerk may read `number`
    // but not report it, so sorting by it is refused too (WP-SEC-04.T13).
    assert_eq!(err.paths(), ["number", "salary"]);
}

#[test]
fn sorting_and_grouping_follow_the_same_rule() {
    let fx = fixture();
    let masked = principal("masked_reader");
    for usage in [QueryUse::Sort, QueryUse::Group, QueryUse::Aggregate] {
        let err = fx
            .guard
            .check_query_fields(&masked, DOCTYPE, &[q("items.iban", usage)])
            .unwrap_err();
        assert_eq!(err.paths(), ["items.iban"], "{usage:?}");
    }
    let err = fx
        .guard
        .check_query_fields(
            &principal("analyst"),
            DOCTYPE,
            &[q("ghost.col", QueryUse::Filter)],
        )
        .unwrap_err();
    assert_eq!(err.paths(), ["ghost.col"]);
}

#[test]
fn an_unknown_doctype_releases_nothing() {
    let fx = fixture();
    let err = fx
        .guard
        .columns(Channel::Export, &principal("admin"), "ghost", &cols(&["a"]))
        .unwrap_err();
    assert_eq!(err.code(), "fieldguard.unknown_doctype");
}
