//! Round-trip tests over the authored corpus (WP-INT-06.T1, brief items 1 and 3).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_edi::{Limits, Syntax, check_envelope, parse_str, write};

const CORPUS: &[(&str, &str)] = &[
    ("x12_850", include_str!("corpus/x12_850.edi")),
    ("x12_810", include_str!("corpus/x12_810.edi")),
    ("x12_856", include_str!("corpus/x12_856.edi")),
    ("x12_997", include_str!("corpus/x12_997.edi")),
    ("edifact_orders", include_str!("corpus/edifact_orders.edi")),
    ("edifact_invoic", include_str!("corpus/edifact_invoic.edi")),
    ("edifact_desadv", include_str!("corpus/edifact_desadv.edi")),
    ("edifact_contrl", include_str!("corpus/edifact_contrl.edi")),
];

#[test]
fn every_corpus_document_round_trips_byte_for_byte() {
    for (name, text) in CORPUS {
        let interchange = parse_str(text, Limits::default())
            .unwrap_or_else(|e| panic!("{name} did not parse: {e}"));
        let written = write(&interchange).unwrap_or_else(|e| panic!("{name} did not write: {e}"));
        assert_eq!(written, *text, "{name} did not round-trip");
        assert!(
            check_envelope(&interchange).is_empty(),
            "{name} has envelope violations: {:?}",
            check_envelope(&interchange)
        );
    }
}

#[test]
fn parsing_twice_gives_the_same_model() {
    for (name, text) in CORPUS {
        let first = parse_str(text, Limits::default()).unwrap();
        let second = parse_str(text, Limits::default()).unwrap();
        assert_eq!(first, second, "{name} parsed differently twice");
    }
}

#[test]
fn a_pretty_printed_document_normalises_to_the_canonical_form() {
    let pretty = include_str!("corpus/x12_850.edi").replace("~", "~\n");
    let canonical = include_str!("corpus/x12_850.edi");
    let interchange = parse_str(&pretty, Limits::default()).unwrap();
    assert_eq!(write(&interchange).unwrap(), canonical);
}

#[test]
fn trailing_empty_elements_are_trimmed_and_inner_ones_kept() {
    let isa = "ISA*00*          *00*          *ZZ*SENDER         *ZZ*RECEIVER       *240101*1200*^*00401*000000001*0*P*>~";
    let tail = "GS*PO*SENDER*RECEIVER*20240101*1200*1*X*004010~ST*850*0001~BEG*00*NE*PO1**20240101~PO1*1*10*EA*12.50**BP*X~SE*4*0001~GE*1*1~IEA*1*000000001~";
    let canonical = format!("{isa}{tail}");
    let interchange = parse_str(&canonical, Limits::default()).unwrap();
    assert_eq!(write(&interchange).unwrap(), canonical);
    assert!(canonical.contains("**20240101"), "an inner empty element");
    assert!(canonical.contains("**BP*X"), "an inner empty element");

    // A trailing empty element is trimmed on write, normalising to the canonical form.
    let with_trailing = canonical.replace("**20240101~", "**20240101*~");
    assert_ne!(with_trailing, canonical);
    let interchange = parse_str(&with_trailing, Limits::default()).unwrap();
    assert_eq!(write(&interchange).unwrap(), canonical);
}

#[test]
fn composites_and_repetitions_survive() {
    let text = include_str!("corpus/x12_810.edi");
    let interchange = parse_str(text, Limits::default()).unwrap();
    let it1 = interchange
        .segments
        .iter()
        .find(|s| s.tag == "IT1")
        .expect("IT1 present");
    let composite = it1.element(6).expect("element 7 present");
    assert_eq!(composite.components(), vec!["WIDGET", "1"]);

    let text = include_str!("corpus/x12_856.edi");
    let interchange = parse_str(text, Limits::default()).unwrap();
    let reference = interchange
        .segments
        .iter()
        .find(|s| s.tag == "REF")
        .expect("REF present");
    assert_eq!(reference.element(1).unwrap().components(), vec!["A1", "A2"]);
    assert_eq!(write(&interchange).unwrap(), text);
}

#[test]
fn edifact_release_sequences_are_unescaped_and_re_escaped() {
    let text = include_str!("corpus/edifact_orders.edi");
    let interchange = parse_str(text, Limits::default()).unwrap();
    let ftx = interchange
        .segments
        .iter()
        .find(|s| s.tag == "FTX")
        .expect("FTX present");
    assert_eq!(ftx.text(3), "Handle with care+fragile");
    assert_eq!(write(&interchange).unwrap(), text);
}

#[test]
fn the_syntax_is_detected_from_the_header() {
    let x12 = parse_str(include_str!("corpus/x12_850.edi"), Limits::default()).unwrap();
    assert_eq!(x12.syntax, Syntax::X12);
    assert_eq!(x12.sender.id, "SENDER");
    assert_eq!(x12.receiver.id, "RECEIVER");
    assert_eq!(x12.control, "000000001");

    let edifact = parse_str(include_str!("corpus/edifact_orders.edi"), Limits::default()).unwrap();
    assert_eq!(edifact.syntax, Syntax::Edifact);
    assert_eq!(edifact.sender.id, "SENDER");
    assert_eq!(edifact.sender.qualifier.as_deref(), Some("ZZ"));
    assert_eq!(edifact.control, "1");
}

#[test]
fn messages_and_groups_are_visible() {
    let x12 = parse_str(include_str!("corpus/x12_850.edi"), Limits::default()).unwrap();
    let messages = x12.messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].id(), "850");
    assert_eq!(messages[0].control(), "0001");
    assert_eq!(x12.groups().len(), 1);
    assert_eq!(x12.groups()[0].messages.len(), 1);

    let edifact = parse_str(include_str!("corpus/edifact_orders.edi"), Limits::default()).unwrap();
    let messages = edifact.messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].id(), "ORDERS");
    assert_eq!(messages[0].control(), "1");
}

#[test]
fn the_streaming_scanner_matches_the_slice_parser() {
    use spot_edi::{SegmentScanner, discover};
    for (_, text) in CORPUS {
        let bytes = text.as_bytes();
        let (_, delimiters) = discover(bytes).unwrap();
        let mut scanner = SegmentScanner::new(delimiters, Limits::default());
        let mut streamed = Vec::new();
        for &byte in bytes {
            if let Some(segment) = scanner.push(byte).unwrap() {
                streamed.push(segment);
            }
        }
        if let Some(segment) = scanner.finish().unwrap() {
            streamed.push(segment);
        }
        let interchange = parse_str(text, Limits::default()).unwrap();
        let expected: Vec<_> = interchange.all_segments().into_iter().cloned().collect();
        assert_eq!(streamed, expected);
    }
}

#[test]
fn parse_reader_agrees_with_parse() {
    use std::io::Cursor;
    for (name, text) in CORPUS {
        let mut cursor = Cursor::new(text.as_bytes());
        let from_reader = spot_edi::parse_reader(&mut cursor, Limits::default()).unwrap();
        let from_slice = parse_str(text, Limits::default()).unwrap();
        assert_eq!(
            from_reader, from_slice,
            "{name} differed between reader and slice"
        );
    }
}

#[test]
fn limits_are_enforced() {
    let text = include_str!("corpus/x12_850.edi");
    let too_small = Limits {
        max_bytes: 10,
        ..Limits::default()
    };
    assert_eq!(
        parse_str(text, too_small).unwrap_err().code(),
        spot_edi::codes::TOO_LARGE
    );
    let few_segments = Limits {
        max_segments: 3,
        ..Limits::default()
    };
    assert_eq!(
        parse_str(text, few_segments).unwrap_err().code(),
        spot_edi::codes::TOO_MANY_SEGMENTS
    );
}
