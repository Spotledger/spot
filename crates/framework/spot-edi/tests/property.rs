//! Property tests (WP-INT-06.T1, brief item 7): random bytes never panic, random valid messages
//! round-trip, and control-number mismatches are detected.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;
use spot_edi::{
    Delimiters, Interchange, Limits, Segment, Syntax, check_envelope, codes, has_code, parse,
    parse_str, write,
};

fn arb_value() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            Just('A'),
            Just('Z'),
            Just('7'),
            Just('+'),
            Just(':'),
            Just('\''),
            Just('?'),
            Just(' '),
        ],
        0..8,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

fn arb_segment() -> impl Strategy<Value = Segment> {
    (0..4usize, proptest::collection::vec(arb_value(), 0..5)).prop_map(|(tag_index, values)| {
        let tags = ["FTX", "RFF", "NAD", "LIN"];
        Segment::simple(tags[tag_index], values)
    })
}

fn edifact_delimiters() -> Delimiters {
    Delimiters {
        element: '+',
        component: ':',
        repetition: None,
        release: Some('?'),
        terminator: '\'',
        decimal: Some('.'),
        una: true,
    }
}

fn wrap(body: Vec<Segment>) -> Interchange {
    let delimiters = edifact_delimiters();
    let mut unb = Segment::new("UNB");
    unb.push("UNOA:1");
    unb.push("SENDER:ZZ");
    unb.push("RECEIVER:ZZ");
    unb.push("20240101:1200");
    unb.push("1");
    let mut unz = Segment::new("UNZ");
    unz.push("1");
    unz.push("1");
    Interchange::new(Syntax::Edifact, delimiters, unb, unz, body)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn random_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        // Parsing arbitrary bytes must return Ok or Err, never panic.
        let _ = parse(&bytes, Limits::small());
    }

    #[test]
    fn random_valid_messages_round_trip(segments in proptest::collection::vec(arb_segment(), 0..8)) {
        let mut body = Vec::new();
        body.push(Segment::simple("UNH", vec!["1".into(), "ORDERS:D:96A:UN".into()]));
        body.extend(segments);
        let count = (body.len() + 1).to_string();
        body.push(Segment::simple("UNT", vec![count, "1".into()]));
        let interchange = wrap(body);

        let first = write(&interchange).unwrap();
        let parsed = parse_str(&first, Limits::default()).unwrap();
        let second = write(&parsed).unwrap();
        prop_assert_eq!(&first, &second);
        // Writing is idempotent: parsing the canonical text and writing it again is stable.
        prop_assert_eq!(write(&parse_str(&second, Limits::default()).unwrap()).unwrap(), second);
    }

    #[test]
    fn control_number_mismatches_are_detected(a in "[0-9]{1,6}", b in "[0-9]{1,6}") {
        prop_assume!(a != b);
        let text = format!(
            "UNA:+.? 'UNB+UNOA:1+SENDER:ZZ+RECEIVER:ZZ+20240101:1200+{a}'UNH+1+ORDERS:D:96A:UN'UNT+2+1'UNZ+1+{b}'"
        );
        let interchange = parse_str(&text, Limits::default()).unwrap();
        let violations = check_envelope(&interchange);
        prop_assert!(has_code(&violations, codes::ENVELOPE_CONTROL), "{violations:?}");
    }

    #[test]
    fn a_consistent_envelope_has_no_violations(control in "[0-9]{1,6}") {
        let text = format!(
            "UNA:+.? 'UNB+UNOA:1+SENDER:ZZ+RECEIVER:ZZ+20240101:1200+{control}'UNH+1+ORDERS:D:96A:UN'UNT+2+1'UNZ+1+{control}'"
        );
        let interchange = parse_str(&text, Limits::default()).unwrap();
        prop_assert!(check_envelope(&interchange).is_empty());
    }
}
