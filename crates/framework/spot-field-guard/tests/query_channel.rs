//! The channel-aware query check (WP-SEC-04.T13): a field is usable in a query on a channel
//! only if the principal may use it unmasked and holds the channel's right.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_field_guard::{Channel, QueryField, QueryUse};

const USES: [QueryUse; 4] = [
    QueryUse::Filter,
    QueryUse::Sort,
    QueryUse::Group,
    QueryUse::Aggregate,
];

/// Fields of the fixture: a plain one, a pci field, an hr field and a child-table column.
const PATHS: [&str; 5] = ["number", "card_number", "salary", "items.qty", "items.iban"];

fn allowed(fx: &Fixture, channel: Channel, who: &str, path: &str, usage: QueryUse) -> bool {
    fx.guard
        .check_query_fields_for(
            channel,
            &principal(who),
            DOCTYPE,
            &[QueryField::new(path, usage)],
        )
        .is_ok()
}

#[test]
fn read_without_report_is_refused_for_report_and_export_but_not_a_read_channel() {
    let fx = fixture();
    for usage in USES {
        for channel in [Channel::Report, Channel::Export] {
            assert!(
                !allowed(&fx, channel, "read_no_report", "card_number", usage),
                "{channel:?} {usage:?}"
            );
            assert!(!allowed(&fx, channel, "read_no_report", "salary", usage));
        }
        // A channel that needs only `read` accepts a field readable unmasked.
        assert!(allowed(
            &fx,
            Channel::Version,
            "read_no_report",
            "card_number",
            usage
        ));
        // A channel that never carries a value refuses everything.
        assert!(!allowed(&fx, Channel::Journal, "analyst", "number", usage));
    }
}

#[test]
fn both_rights_pass_and_a_masked_field_stays_refused() {
    let fx = fixture();
    for usage in USES {
        for channel in [Channel::Report, Channel::Export] {
            for path in PATHS {
                assert!(allowed(&fx, channel, "analyst", path, usage), "{path}");
            }
            // report right, masked form only: still refused.
            assert!(!allowed(
                &fx,
                channel,
                "masked_reader",
                "card_number",
                usage
            ));
            assert!(!allowed(&fx, channel, "masked_reader", "items.iban", usage));
            // a plain field the principal can read but not report.
            assert!(!allowed(&fx, channel, "clerk", "number", usage));
            assert!(allowed(&fx, channel, "masked_reader", "number", usage));
        }
    }
}

#[test]
fn every_refused_path_is_named_once_sorted_and_an_unknown_doctype_fails() {
    let fx = fixture();
    let err = fx
        .guard
        .check_query_fields_for(
            Channel::Export,
            &principal("read_no_report"),
            DOCTYPE,
            &[
                QueryField::new("salary", QueryUse::Sort),
                QueryField::new("items.iban", QueryUse::Filter),
                QueryField::new("salary", QueryUse::Group),
                QueryField::new("ghost.col", QueryUse::Filter),
            ],
        )
        .unwrap_err();
    assert_eq!(err.code(), "permission.field_denied");
    assert_eq!(err.paths(), ["ghost.col", "items.iban", "salary"]);
    let err = fx
        .guard
        .check_query_fields_for(Channel::Report, &principal("admin"), "ghost", &[])
        .unwrap_err();
    assert_eq!(err.code(), "fieldguard.unknown_doctype");
}

/// Never looser than `columns`: a query may use a path only if the channel would also show
/// its value unmasked.
#[test]
fn the_query_check_is_never_looser_than_the_column_check() {
    let fx = fixture();
    for channel in Channel::ALL {
        for who in PRINCIPALS {
            for path in PATHS {
                let shown = fx
                    .guard
                    .columns(channel, &principal(who), DOCTYPE, &[path.to_owned()])
                    .map(|c| c.allowed.iter().any(|p| p == path))
                    .unwrap_or(false);
                if channel.need() == spot_field_guard::Need::Report {
                    for usage in USES {
                        if allowed(&fx, channel, who, path, usage) {
                            assert!(shown, "{channel:?} {who} {path}: query looser than columns");
                        }
                    }
                }
            }
        }
    }
}
