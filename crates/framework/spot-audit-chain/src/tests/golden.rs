//! Golden vectors: the canonical bytes and hashes are pinned so the encoding cannot drift.

use super::*;
use crate::{ChainEntry, genesis_hash};

fn records() -> [AuditRecord; 3] {
    // 1: every field set. 2: minimal (system actor, no subject, empty payload). 3: negative
    // timestamp, a text with multibyte characters, unmasked.
    let mut p3 = Payload::new();
    p3.insert("zeta".to_owned(), PayloadValue::Bool(true));
    p3.insert(
        "alpha".to_owned(),
        PayloadValue::Text("caf\u{e9}".to_owned()),
    );
    p3.insert("min".to_owned(), PayloadValue::Int(i64::MIN));
    [
        record(1),
        AuditRecord {
            kind: "agent.decision".to_owned(),
            actor: "system".to_owned(),
            on_behalf_of: None,
            subject: None,
            at: 0,
            payload: Payload::new(),
            masked: false,
        },
        AuditRecord {
            kind: "doc.delete".to_owned(),
            actor: "agent:a7".to_owned(),
            on_behalf_of: None,
            subject: Some(Subject {
                doctype: "Item".to_owned(),
                id: "X".to_owned(),
            }),
            at: -5,
            payload: p3,
            masked: false,
        },
    ]
}

#[test]
fn canonical_bytes_of_the_minimal_record_are_pinned() {
    let mut expected: Vec<u8> = Vec::new();
    expected.extend_from_slice(&[0, 0, 0, 14]);
    expected.extend_from_slice(b"agent.decision");
    expected.extend_from_slice(&[0, 0, 0, 6]);
    expected.extend_from_slice(b"system");
    expected.push(0); // no on_behalf_of
    expected.push(0); // no subject
    expected.extend_from_slice(&[0; 8]); // at = 0
    expected.push(0); // masked = false
    expected.extend_from_slice(&[0, 0, 0, 0]); // empty payload
    assert_eq!(records()[1].canonical_bytes(), expected);
}

#[test]
fn canonical_bytes_of_a_payload_value_of_each_type_are_pinned() {
    let mut r = records()[1].clone();
    r.payload.insert("i".to_owned(), PayloadValue::Int(-2));
    r.payload
        .insert("t".to_owned(), PayloadValue::Text("hi".to_owned()));
    r.payload.insert("b".to_owned(), PayloadValue::Bool(true));
    let bytes = r.canonical_bytes();
    let mut expected: Vec<u8> = vec![0, 0, 0, 3];
    expected.extend_from_slice(&[0, 0, 0, 1, b'b', 3, 1]);
    expected.extend_from_slice(&[0, 0, 0, 1, b'i', 2]);
    expected.extend_from_slice(&(-2_i64).to_be_bytes());
    expected.extend_from_slice(&[0, 0, 0, 1, b't', 1, 0, 0, 0, 2, b'h', b'i']);
    assert_eq!(&bytes[bytes.len() - expected.len()..], &expected[..]);
}

#[test]
fn genesis_hash_is_pinned() {
    assert_eq!(
        hex(&genesis_hash(SITE)),
        "09dbc56701efc432d4ee5a4930e77acd3f3b252d25c16abad4d378867685cdd8"
    );
}

#[test]
fn three_record_chain_hashes_are_pinned() {
    let mut prev = genesis_hash(SITE);
    let mut hashes = Vec::new();
    for (i, r) in records().into_iter().enumerate() {
        let e = ChainEntry::new(i as u64 + 1, prev, r);
        prev = e.hash;
        hashes.push(hex(&e.hash));
    }
    assert_eq!(
        hashes,
        vec![
            "1885612148b367499793c128056890aec3c620e2d1e98c45d9f7bdbb9230b140",
            "b0baaa1eef257d153bac51d1ab363943fa84c61c31ea3dbda84eb788e9a59e81",
            "26fc3e288883f7049b90e3aa7c2ff1f3059e1363c0e2fad85e99fe529a9584ab"
        ]
    );
}

#[test]
fn payload_keys_are_encoded_in_sorted_order_regardless_of_insertion() {
    let mut a = Payload::new();
    a.insert("b".to_owned(), PayloadValue::Int(1));
    a.insert("a".to_owned(), PayloadValue::Int(2));
    let mut b = Payload::new();
    b.insert("a".to_owned(), PayloadValue::Int(2));
    b.insert("b".to_owned(), PayloadValue::Int(1));
    let mut ra = record(1);
    let mut rb = record(1);
    ra.payload = a;
    rb.payload = b;
    assert_eq!(ra.canonical_bytes(), rb.canonical_bytes());
}
