//! Properties every chain must have, over generated images.

use proptest::prelude::*;
use serde_json::{Value, json};

use super::img;
use crate::steps::{add_field_with_default, drop_field, rename_field, retype_field};
use crate::{Registry, Step, Upcaster, upcast};

/// A whole number stored as a string becomes a number.
fn as_number(value: &Value) -> Result<Value, String> {
    match value.as_str() {
        Some(text) => text
            .parse::<u64>()
            .map(|number| json!(number))
            .map_err(|_| "is not a whole number".to_owned()),
        None => Err("is not a whole number".to_owned()),
    }
}

fn register(registry: &mut Registry, from: u64, to: u64, f: Step) {
    registry
        .register(Upcaster::new("party", from, to, f))
        .expect("registered");
}

/// Four steps for `party`: schema 1 -> 5.
fn chain() -> Registry {
    let mut registry = Registry::new();
    register(&mut registry, 1, 2, rename_field("legacy_name", "name"));
    register(
        &mut registry,
        2,
        3,
        add_field_with_default("status", json!("draft")),
    );
    register(&mut registry, 3, 4, retype_field("credit_limit", as_number));
    register(&mut registry, 4, 5, drop_field("legacy_flag"));
    registry
}

/// The field names the chain rewrites, so a generated body can carry them.
static KEYS: [&str; 5] = ["legacy_name", "name", "credit_limit", "legacy_flag", "code"];

fn body() -> impl Strategy<Value = Value> {
    proptest::collection::btree_map(
        proptest::sample::select(KEYS.as_slice()).prop_map(str::to_owned),
        "[0-9]{1,4}".prop_map(Value::from),
        0..=KEYS.len(),
    )
    .prop_map(|entries: std::collections::BTreeMap<String, Value>| {
        Value::Object(entries.into_iter().collect())
    })
}

proptest! {
    /// Upcasting twice is upcasting once: the second run has nothing left to do.
    #[test]
    fn upcasting_twice_equals_once(body in body(), start in 1u64..=4) {
        let registry = chain();
        let image = img("party", start, body);
        match upcast(&registry, image.clone(), 5) {
            Ok(once) => {
                prop_assert_eq!(once.entity_schema(), 5);
                let twice = upcast(&registry, once.clone(), 5).expect("already at the target");
                prop_assert_eq!(twice, once);
            }
            Err(err) => {
                // A refused image is refused the same way every time: nothing is random and
                // nothing is read from the environment.
                let again = upcast(&registry, image, 5).expect_err("refused again");
                prop_assert_eq!(again, err);
            }
        }
    }

    /// An image already at the target is a fixed point of the walk.
    #[test]
    fn an_image_at_the_target_is_a_fixed_point(body in body()) {
        let registry = chain();
        let image = img("party", 5, body);
        prop_assert_eq!(upcast(&registry, image.clone(), 5), Ok(image));
    }

    /// Fields the chain does not rewrite survive with the same value.
    #[test]
    fn untouched_fields_survive(body in body(), start in 1u64..=4) {
        let registry = chain();
        let image = img("party", start, body);
        if let Ok(out) = upcast(&registry, image.clone(), 5) {
            for (key, value) in image.body() {
                if matches!(key.as_str(), "legacy_name" | "credit_limit" | "legacy_flag") {
                    continue;
                }
                prop_assert_eq!(out.field(key), Some(value), "field {}", key);
            }
        }
    }
}
