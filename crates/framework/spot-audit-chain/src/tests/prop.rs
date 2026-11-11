//! Property: flipping any single bit anywhere in a stored entry is detected.

use futures::executor::block_on;
use proptest::prelude::*;

use super::*;
use crate::{PayloadValue, Verifier};

fn flip_byte(bytes: &mut [u8], index: usize, bit: u8) {
    let i = index % bytes.len();
    bytes[i] ^= 1 << (bit % 8);
}

/// Flips a bit of an ASCII string without leaving ASCII (so it stays valid UTF-8).
fn flip_ascii(s: &mut String, index: usize, bit: u8) {
    let mut bytes = std::mem::take(s).into_bytes();
    let i = index % bytes.len();
    bytes[i] ^= 1 << (bit % 7);
    *s = String::from_utf8(bytes).unwrap();
}

proptest! {
    #[test]
    fn any_single_bit_flip_in_an_entry_is_detected(
        which in 0usize..5,
        field in 0u8..12,
        index in 0usize..64,
        bit in 0u8..64,
    ) {
        let (chain, ring) = built(5, 0);
        mutate(&chain, |entries| {
            let e = &mut entries[which];
            match field {
                0 => e.seq ^= 1 << bit,
                1 => flip_byte(&mut e.prev_hash, index, bit),
                2 => flip_byte(&mut e.hash, index, bit),
                3 => e.record.at ^= 1 << bit,
                4 => {
                    if let Some(PayloadValue::Int(n)) = e.record.payload.get_mut("n") {
                        *n ^= 1 << bit;
                    }
                }
                5 => {
                    if let Some(PayloadValue::Text(t)) = e.record.payload.get_mut("note") {
                        flip_ascii(t, index, bit);
                    }
                }
                6 => e.record.masked = !e.record.masked,
                7 => flip_ascii(&mut e.record.kind, index, bit),
                8 => flip_ascii(&mut e.record.actor, index, bit),
                9 => {
                    if let Some(s) = e.record.subject.as_mut() {
                        flip_ascii(&mut s.id, index, bit);
                    }
                }
                10 => {
                    if let Some(a) = e.record.on_behalf_of.as_mut() {
                        flip_ascii(a, index, bit);
                    }
                }
                _ => {
                    if let Some(PayloadValue::Bool(b)) = e.record.payload.get_mut("ok") {
                        *b = !*b;
                    }
                }
            }
        });
        let report = block_on(Verifier::new(&ring).verify_range(chain.store(), SITE, 1, u64::MAX)).unwrap();
        prop_assert!(!report.is_ok(), "undetected: which={which} field={field} index={index} bit={bit}");
    }
}
