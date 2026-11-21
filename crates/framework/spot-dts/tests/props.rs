//! Property tests: random field names never panic and always produce valid identifiers or
//! quoted property names.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;

use spot_dts::generate;
use spot_dts::naming::{interface_name, is_identifier, is_reserved_word, property_name};

proptest! {
    /// Any storage-grammar name yields a property spelling that is either a valid identifier
    /// or (everything else) a quoted string.
    #[test]
    fn property_names_are_identifiers_or_quoted(name in "[a-z][a-z0-9_]{0,30}") {
        let spelling = property_name(&name);
        if is_identifier(&name) && !is_reserved_word(&name) {
            prop_assert_eq!(spelling, name);
        } else {
            prop_assert!(spelling.starts_with('"') && spelling.ends_with('"'),
                "`{name}` -> `{spelling}` must be quoted");
        }
    }

    /// Generating from random field names never panics.
    #[test]
    fn generation_never_panics_on_random_names(name in "[a-z][a-z0-9_]{0,30}") {
        let mut d = spot_meta::Definition::new("gen_probe");
        d.fields.push(spot_meta::FieldSpec::new(&name, spot_meta::FieldType::Text));
        let generated = generate(&[d]);
        prop_assert!(generated.is_ok());
        prop_assert!(is_identifier(&property_name(&name)) || property_name(&name).starts_with('"'));
    }

    /// Random snake_case DocType names give PascalCase interface names, never reserved.
    #[test]
    fn interface_names_are_pascal_and_not_reserved(name in "[a-z][a-z0-9_]{0,30}") {
        // A reserved-word collision is an error by design; only a success is checked further.
        if let Ok(ident) = interface_name(&name) {
            prop_assert!(!is_reserved_word(&ident.to_ascii_lowercase()),
                "`{name}` maps onto a reserved word and must error");
            prop_assert!(is_identifier(&ident));
            prop_assert!(ident.chars().next().is_some_and(|c| c.is_ascii_uppercase()));
        }
    }
}
