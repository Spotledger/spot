//! Compile-time diagnostics: every misuse is rejected with a message that says what to write.
#![allow(missing_docs)]

#[test]
fn misuse_is_rejected_with_helpful_messages() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/*.rs");
}
