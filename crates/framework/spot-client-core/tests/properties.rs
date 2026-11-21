//! Property tests over random input sequences, with a fixed generator so failures reproduce.
#![cfg(not(feature = "storage"))]
#![allow(missing_docs)]

use std::collections::BTreeSet;

use serde_json::{Value, json};
use spot_client_core::{Core, Input, Output};

/// A small deterministic generator (xorshift); the crate itself has no random source.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn random_input(rng: &mut Rng, now: &mut u64) -> Value {
    match rng.below(9) {
        0 => {
            json!({"type":"request","op":"sign_in","req_id":rng.below(50),"user":"u","password":"p"})
        }
        1 => {
            json!({"type":"request","op":"resource_get","req_id":rng.below(50),"doctype":"Invoice","id":"I1"})
        }
        2 => {
            json!({"type":"request","op":"call","req_id":rng.below(50),"method":"PATCH","path":"/api/v1/resource/Invoice/I1","body":"{}","if_match":1,"idempotency_key":"k"})
        }
        3 => json!({"type":"request","op":"subscribe","room":"doc:Invoice:I1"}),
        4 => {
            json!({"type":"request","op":"realtime","kind":"event","room":"doc:Invoice:I1","seq":rng.below(6),"event":"doc_updated","payload":{"doctype":"Invoice","id":"I1","version":rng.below(5)}})
        }
        5 => {
            *now += rng.below(5000);
            json!({"type":"tick","now_ms":*now})
        }
        6 => json!({"type":"transport_error","id":rng.below(12)}),
        _ => {
            let status =
                [200, 304, 401, 412, 429, 503, 500][usize::try_from(rng.below(7)).unwrap_or(0)];
            let body = if status == 200 {
                r#"{"data":{"token":"t","expires_at":"x","user":{"id":"u"}}}"#
            } else {
                r#"{"error":{"code":"x.y","message":"m","retryable":true,"request_id":"r"}}"#
            };
            json!({"type":"transport_reply","id":rng.below(12),"reply":{"status":status,"headers":[["Retry-After","1"]],"body":body}})
        }
    }
}

#[test]
fn deterministic_and_no_hidden_clock() -> Result<(), String> {
    for seed in 1..200_u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let (mut a, mut b) = (Core::new(), Core::new());
        let mut now = 0_u64;
        let mut timers: Vec<u64> = Vec::new();
        let mut inflight: BTreeSet<u64> = BTreeSet::new();
        for _ in 0..60 {
            let raw = random_input(&mut rng, &mut now);
            let make = || serde_json::from_value::<Input>(raw.clone()).map_err(|e| e.to_string());
            let (first, second) = (a.handle(make()?), b.handle(make()?));
            assert_eq!(
                format!("{first:?}"),
                format!("{second:?}"),
                "same inputs, same outputs (seed {seed})"
            );

            match raw["type"].as_str() {
                Some("tick") => {
                    let due = timers.iter().any(|at| *at <= now);
                    let sends = first.iter().any(|o| matches!(o, Output::SendHttp { .. }));
                    assert!(
                        due || !sends,
                        "a Tick sent a request with no timer due (seed {seed})"
                    );
                    assert!(
                        due || first.is_empty(),
                        "a Tick with no timer due produced output (seed {seed})"
                    );
                    timers.retain(|at| *at > now);
                }
                Some("transport_reply" | "transport_error") => {
                    let id = raw["id"].as_u64().unwrap_or(u64::MAX);
                    if !inflight.remove(&id) {
                        assert!(
                            first.is_empty(),
                            "a reply to an id never sent produced output (seed {seed})"
                        );
                    }
                }
                _ => {}
            }
            for output in &first {
                match output {
                    Output::SendHttp { id, .. } => {
                        assert!(inflight.insert(*id), "ids are unique (seed {seed})");
                    }
                    Output::SetTimer { at_ms } => timers.push(*at_ms),
                    Output::Emit(_) => {}
                }
            }
        }
    }
    Ok(())
}
