//! The leak matrix: 6 principals x 10 channels over a DocType with a plain field, group
//! protected fields, masked fields, a derived field that inherits a protected group, a
//! trace group and a protected child-table column. The leak harness must be clean on every
//! cell, and must be able to see a value where the principal may.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::*;
use spot_field_guard::leakcheck::{self, Output, assert_no_leak, find_leaks};
use spot_field_guard::{Channel, Need, RevealService};
use spot_permissions::MaskFormat;

fn visible_labels(principal: &str, channel: Channel) -> Vec<&'static str> {
    let e = expect(principal);
    let groups = match channel.need() {
        Need::Read => e.read_groups,
        Need::Report => e.report_groups,
        Need::NamesOnly => &[],
    };
    SECRET_GROUPS
        .iter()
        .filter(|(_, needs)| needs.iter().all(|g| groups.contains(g)))
        .map(|(label, _)| *label)
        .collect()
}

#[test]
fn no_principal_leaks_on_any_channel() {
    let fx = fixture();
    let corpus = corpus();
    let mut cells = 0;
    for name in PRINCIPALS {
        let principal = principal(name);
        for channel in Channel::ALL {
            let mut doc = invoice(&corpus);
            fx.guard
                .redact(channel, &principal, DOCTYPE, &doc_facts(), &mut doc)
                .unwrap_or_else(|e| panic!("{name} on {channel:?}: {e}"));
            let out = outputs(&doc);
            let visible = visible_labels(name, channel);

            // Nothing outside the visible set appears in any trivial encoding.
            let forbidden = corpus.without(&visible);
            assert_no_leak(&forbidden, &out);

            // The harness can see what is allowed: every visible secret IS in the output
            // (otherwise "clean" would prove nothing).
            let seen: Vec<String> = find_leaks(&corpus.only(&visible), &out)
                .into_iter()
                .map(|l| l.secret)
                .collect();
            for label in &visible {
                assert!(
                    seen.iter().any(|s| s == label),
                    "{name} on {channel:?}: `{label}` should be visible but is not"
                );
            }

            // Plain fields follow the DocType right; names-only channels carry nothing.
            let e = expect(name);
            let plain_expected = match channel.need() {
                Need::Read => e.plain_read,
                Need::Report => e.plain_report,
                Need::NamesOnly => false,
            };
            let text = to_json(&doc);
            assert_eq!(
                text.contains(PLAIN_NUMBER),
                plain_expected,
                "{name} on {channel:?}: plain `number`"
            );
            if !channel.carries_values() {
                assert!(doc.fields().is_empty() && doc.tables().is_empty());
            }
            cells += 1;
        }
    }
    assert_eq!(cells, 60);
}

#[test]
fn a_derived_field_needs_every_inherited_group() {
    // `annual_cost` is `finance` itself plus `hr` inherited from `salary`: read_no_report
    // holds `hr` but not `finance`, so it sees the salary and not the annual cost.
    let fx = fixture();
    let corpus = corpus();
    let p = principal("read_no_report");
    let mut doc = invoice(&corpus);
    fx.guard
        .redact(Channel::ApiScim, &p, DOCTYPE, &doc_facts(), &mut doc)
        .unwrap();
    assert!(doc.fields().contains_key(&name("salary")));
    assert!(!doc.fields().contains_key(&name("annual_cost")));
    // The metadata adapter really did add the inherited group.
    let prot = spot_field_guard::ProtectionSource::protection(&*fx.protection, DOCTYPE).unwrap();
    let groups: Vec<_> = prot
        .field("annual_cost")
        .groups
        .iter()
        .map(|g| g.0.clone())
        .collect();
    assert_eq!(groups, vec!["finance".to_owned(), "hr".to_owned()]);
}

#[test]
fn masked_forms_never_reveal_more_than_the_core_mask() {
    let fx = fixture();
    let corpus = corpus();
    let p = principal("masked_reader");
    for channel in [
        Channel::Version,
        Channel::LinkTitle,
        Channel::ApiScim,
        Channel::Export,
    ] {
        let mut doc = invoice(&corpus);
        fx.guard
            .redact(channel, &p, DOCTYPE, &doc_facts(), &mut doc)
            .unwrap();
        let card = corpus.get("card").unwrap().value();
        let masked = match doc.get("card_number") {
            spot_app_api::prelude::Value::Text(t) => t.clone(),
            other => panic!("{channel:?}: card_number is {other:?}"),
        };
        leakcheck::check_mask(card, &MaskFormat::LastChars(4), &masked).unwrap();
        assert!(masked.ends_with("1111") && masked.starts_with('*'));
        // The placeholder fields show only stars; a decimal has no text form and is nulled.
        assert_eq!(
            doc.get("salary"),
            &spot_app_api::prelude::Value::Text("***".into())
        );
        assert_eq!(doc.get("bonus"), &spot_app_api::prelude::Value::Null);
        let row = &doc.rows("items")[0];
        let iban = corpus.get("iban").unwrap().value();
        match row.fields.get(&name("iban")) {
            Some(spot_app_api::prelude::Value::Text(t)) => {
                leakcheck::check_mask(iban, &MaskFormat::LastChars(4), t).unwrap();
            }
            other => panic!("{channel:?}: iban is {other:?}"),
        }
    }
}

#[test]
fn print_degrades_to_the_masked_form_instead_of_refusing() {
    let fx = fixture();
    let corpus = corpus();
    // read_no_report may read the card but not report it: Print masks it, Export drops it.
    let p = principal("read_no_report");
    let mut printed = invoice(&corpus);
    fx.guard
        .redact(Channel::Print, &p, DOCTYPE, &doc_facts(), &mut printed)
        .unwrap();
    assert_no_leak(&corpus, &outputs(&printed));
    assert!(printed.fields().contains_key(&name("card_number")));
    assert_eq!(
        printed.get("card_number"),
        &spot_app_api::prelude::Value::Text("***************1111".into())
    );
    let mut exported = invoice(&corpus);
    fx.guard
        .redact(Channel::Export, &p, DOCTYPE, &doc_facts(), &mut exported)
        .unwrap();
    assert!(!exported.fields().contains_key(&name("card_number")));
}

#[test]
fn names_only_channels_say_names_and_never_values() {
    let fx = fixture();
    let names: Vec<String> = [
        "number",
        "notes",
        "card_number",
        "salary",
        "annual_cost",
        "items",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let ask = |who: &str| {
        fx.guard
            .visible_names(&principal(who), DOCTYPE, &doc_facts(), &names)
            .unwrap()
    };
    assert!(ask("nobody").is_empty());
    assert_eq!(ask("clerk"), vec!["number", "notes", "items"]);
    // A masked reader may know a protected field exists; its value still does not travel.
    assert_eq!(ask("masked_reader").len(), 6);
    assert_eq!(
        ask("read_no_report"),
        vec!["number", "notes", "card_number", "salary", "items"]
    );
}

#[test]
fn an_administrator_sees_values_only_through_the_reveal_service() {
    let fx = fixture();
    let corpus = corpus();
    let admin = principal("admin");
    let id = "doc-1";
    let values = Arc::new(MapValues::default());
    values.put(
        DOCTYPE,
        id,
        "card_number",
        spot_app_api::prelude::Value::Text(corpus.get("card").unwrap().value().to_owned()),
    );
    let service = RevealService::new(Arc::new(fx.guard.clone()), values);

    // On the channels that carry no values at all, the administrator gets nothing...
    for channel in [Channel::Notification, Channel::Journal] {
        let mut doc = invoice(&corpus);
        fx.guard
            .redact(channel, &admin, DOCTYPE, &doc_facts(), &mut doc)
            .unwrap();
        assert_no_leak(&corpus, &outputs(&doc));
        assert!(doc.fields().is_empty());
    }
    // ...and the value comes through the audited reveal, with a reason, one field at a time.
    let revealed = service
        .reveal(&admin, DOCTYPE, id, "card_number", "chargeback case 4471")
        .unwrap();
    assert_eq!(
        revealed,
        spot_app_api::prelude::Value::Text("4111 1111 1111 1111".into())
    );
    assert_eq!(fx.audit.len(), 1);
}

#[test]
fn a_byte_output_form_is_checked_too() {
    let fx = fixture();
    let corpus = corpus();
    let mut doc = invoice(&corpus);
    fx.guard
        .redact(
            Channel::Export,
            &principal("clerk"),
            DOCTYPE,
            &doc_facts(),
            &mut doc,
        )
        .unwrap();
    assert_no_leak(&corpus, &[Output::Bytes(to_json(&doc).into_bytes())]);
}
