//! Fuzz-style property: arbitrary strings transpile without panicking, and
//! whatever comes out is either usable JavaScript or a positioned diagnostic.
//! The parser output is re-parsed as plain JavaScript so drift into emitting
//! invalid JavaScript is caught, too.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;

use spot_ts_transpile::{Options, transpile};

fn re_parses_clean(js: &str) -> bool {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, js, SourceType::mjs()).parse();
    parsed.diagnostics.is_empty()
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

    #[test]
    fn no_input_panics_or_hangs(source in proptest::string::string_regex("[{}()\\[\\]`'\"/*;,.!=<>$\\\\\\\\ \n\t]{0,4000}")
        .expect("strategy")) {
        proptest::prelude::prop_assert!(source.len() <= Options::default().source_size_cap);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            transpile(&source, "fuzz.ts", &Options::default())
        }));
        // The property under test: no panic. (Panics surface as Err here.)
        proptest::prelude::prop_assert!(
            outcome.is_ok(),
            "transpile panicked or aborted on {source:?}"
        );
        match outcome.expect("no panic") {
            Ok(transpiled) => {
                // Whatever is emitted must be valid JavaScript.
                proptest::prelude::prop_assert!(
                    re_parses_clean(&transpiled.js),
                    "emitted invalid JavaScript for {source:?}"
                );
            }
            Err(diagnostics) => {
                for diagnostic in diagnostics {
                    proptest::prelude::prop_assert!(!diagnostic.code.is_empty());
                }
            }
        }
    }
}
