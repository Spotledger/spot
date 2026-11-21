//! Property tests: the envelope decoder must never panic on hostile input.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;
use spot_agent_core::CommandEnvelope;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..1024)) {
        // The result does not matter; the decoder must return it, never unwind.
        let _ = CommandEnvelope::parse(&bytes);
    }

    #[test]
    fn arbitrary_text_never_panics(text in "\\PC{0,1024}") {
        let _ = CommandEnvelope::parse_str(&text);
    }

    #[test]
    fn json_like_text_is_refused_or_yields_a_canonical_command(text in "[ -~\\n]{0,1024}") {
        // Any JSON-ish text that is not exactly the envelope shape is a refusal, not a
        // panic; if it parses, its command must itself be canonical.
        if let Ok(envelope) = CommandEnvelope::parse_str(&text) {
            prop_assert!(spot_signed_command::Command::from_json_str(envelope.command_json()).is_ok());
        }
    }
}
