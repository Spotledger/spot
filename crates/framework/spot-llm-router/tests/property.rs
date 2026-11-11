//! Property tests: the integer cost is exact round-half-up over arbitrary shapes, and the
//! estimate model's guarantees hold for arbitrary requests.

use proptest::prelude::*;

use spot_llm::Message;
use spot_llm::Request;
use spot_llm_router::Price;
use spot_llm_router::cost;
use spot_llm_router::estimate_input_tokens;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    /// `cost` is the exact round-half-up of the token-weighted price, pointwise.
    #[test]
    fn cost_matches_the_exact_rational_round_half_up(
        input in 0u64..=1_000_000,
        output in 0u64..=1_000_000,
        per_million_input in 0u64..=10_000,
        per_million_output in 0u64..=50_000,
    ) {
        let price = Price::new(per_million_input, per_million_output);
        // The expected value: one round-half-up on the COMBINED product, the semantics
        // the crate documents — (in*pi + out*po + 500_000) / 1_000_000, all in u128.
        let expected =
            (u128::from(input) * u128::from(per_million_input)
                + u128::from(output) * u128::from(per_million_output)
                + 500_000)
                / 1_000_000;
        prop_assert_eq!(cost(input, output, &price), expected);
    }

    /// `cost` is monotone: more tokens never cost less, at any price.
    #[test]
    fn cost_is_monotone_in_tokens(
        input in 0u64..=500_000,
        extra in 0u64..=50_000,
        per_million_input in 0u64..=10_000,
        per_million_output in 0u64..=10_000,
    ) {
        let price = Price::new(per_million_input, per_million_output);
        prop_assert!(cost(input + extra, 0, &price) >= cost(input, 0, &price));
    }

    /// `cost` with a zero price is zero, whatever the token counts.
    #[test]
    fn a_zero_price_costs_nothing(input in 0u64..=1_000_000, output in 0u64..=1_000_000) {
        prop_assert_eq!(cost(input, output, &Price::new(0, 0)), 0);
    }

    /// The input estimate is at least one token and grows with the request's text.
    #[test]
    fn the_input_estimate_is_positive_and_text_driven(
        a in "[a-z]{0,40}",
        b in "[a-z]{0,40}",
    ) {
        let small = Request::new("m", "t:chat:s").with_message(Message::user(&a));
        let big = Request::new("m", "t:chat:b")
            .with_message(Message::user(&a))
            .with_message(Message::user(&b));
        prop_assert!(estimate_input_tokens(&small) >= 1);
        prop_assert!(estimate_input_tokens(&big) >= estimate_input_tokens(&small));
    }
}
