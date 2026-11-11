//! Mapping tests (WP-INT-06.T1, brief item 5).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use serde_json::json;
use spot_connectors::{Coercion, MappingRule, MappingSpec};
use spot_edi::{EdiMapping, Limits, codes, parse_str};

fn rule(target: &str, source: &str) -> MappingRule {
    MappingRule {
        target: target.to_owned(),
        source: source.to_owned(),
        expression: String::new(),
        coerce: Coercion::Text,
    }
}

fn mapping() -> EdiMapping {
    EdiMapping::new(MappingSpec {
        rules: vec![
            rule("order.number", "segments.BEG.s0.e2"),
            rule("order.date", "segments.BEG.s0.e4"),
            rule("order.message", "message.id"),
        ],
    })
}

#[test]
fn a_message_maps_to_a_document() {
    let interchange = parse_str(include_str!("corpus/x12_850.edi"), Limits::default()).unwrap();
    let message = interchange.messages().into_iter().next().unwrap();
    let document = mapping().to_document(&message, 0).unwrap();
    assert_eq!(
        document,
        json!({"order": {"number": "PO12345", "date": "20240101", "message": "850"}})
    );
}

#[test]
fn a_document_maps_back_onto_the_message() {
    let interchange = parse_str(include_str!("corpus/x12_850.edi"), Limits::default()).unwrap();
    let message = interchange.messages().into_iter().next().unwrap();
    let document = json!({"order": {"number": "PO99999", "date": "20240202"}});
    let rebuilt = mapping().from_document(&document, &message).unwrap();
    let beg = rebuilt.segments.iter().find(|s| s.tag == "BEG").unwrap();
    assert_eq!(beg.text(2), "PO99999");
    assert_eq!(beg.text(4), "20240202");
    // The trailer's count and control are recomputed.
    let trailer = rebuilt.trailer.as_ref().unwrap();
    assert_eq!(trailer.text(0), rebuilt.all_segments().len().to_string());
    assert_eq!(trailer.text(1), "0001");
}

#[test]
fn the_view_addresses_segments_by_tag_and_occurrence() {
    let interchange = parse_str(include_str!("corpus/x12_850.edi"), Limits::default()).unwrap();
    let message = interchange.messages().into_iter().next().unwrap();
    let view = spot_edi::mapping::message_view(&message);
    assert_eq!(view["segments"]["N1"]["s0"]["e0"], json!("ST"));
    assert_eq!(view["segments"]["N1"]["s1"]["e0"], json!("BT"));
    assert_eq!(view["message"]["control"], json!("0001"));
}

#[test]
fn composite_components_map_individually() {
    let interchange = parse_str(include_str!("corpus/x12_810.edi"), Limits::default()).unwrap();
    let message = interchange.messages().into_iter().next().unwrap();
    let spec = EdiMapping::new(MappingSpec {
        rules: vec![
            rule("line.item.code", "segments.IT1.s0.e6.c0"),
            rule("line.item.version", "segments.IT1.s0.e6.c1"),
        ],
    });
    let document = spec.to_document(&message, 0).unwrap();
    assert_eq!(
        document,
        json!({"line": {"item": {"code": "WIDGET", "version": "1"}}})
    );
    let rebuilt = spec
        .from_document(
            &json!({"line": {"item": {"code": "GADGET", "version": "9"}}}),
            &message,
        )
        .unwrap();
    let it1 = rebuilt.segments.iter().find(|s| s.tag == "IT1").unwrap();
    assert_eq!(it1.element(6).unwrap().components(), vec!["GADGET", "9"]);
}

#[test]
fn a_bad_source_path_is_a_mapping_error() {
    let interchange = parse_str(include_str!("corpus/x12_850.edi"), Limits::default()).unwrap();
    let message = interchange.messages().into_iter().next().unwrap();
    let spec = EdiMapping::new(MappingSpec {
        rules: vec![rule("order.number", "not.a.path")],
    });
    let error = spec
        .from_document(&json!({"order": {"number": "X"}}), &message)
        .unwrap_err();
    assert_eq!(error.code(), codes::MAPPING);
}

#[test]
fn a_missing_target_is_left_empty() {
    let interchange = parse_str(include_str!("corpus/x12_850.edi"), Limits::default()).unwrap();
    let message = interchange.messages().into_iter().next().unwrap();
    let rebuilt = mapping().from_document(&json!({}), &message).unwrap();
    let beg = rebuilt.segments.iter().find(|s| s.tag == "BEG").unwrap();
    assert_eq!(beg.text(2), "PO12345");
}
