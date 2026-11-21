//! Exclusive XML Canonicalization conformance: the specification's vectors, a round-trip
//! property, and the guarantee that arbitrary bytes never panic the entry points.

use proptest::prelude::*;
use spot_sso::saml::{canonicalize, canonicalize_by_id};

/// The specification's "Start and End Tags" example: namespace declarations sorted with the
/// default namespace first, then by prefix, and attributes by namespace URI then local name.
#[test]
fn namespace_and_attribute_axes_are_sorted() {
    let input = concat!(
        "<doc>",
        "<e5 a:attr=\"out\" b:attr=\"sorted\" attr2=\"all\" attr=\"I'm\" ",
        "xmlns:b=\"http://www.ietf.org\" xmlns:a=\"http://www.w3.org\" ",
        "xmlns=\"http://example.org\"></e5>",
        "</doc>"
    );
    let output = canonicalize(input, false).unwrap_or_default();
    assert!(output.contains(concat!(
        "<e5 xmlns=\"http://example.org\" xmlns:a=\"http://www.w3.org\" ",
        "xmlns:b=\"http://www.ietf.org\" attr=\"I'm\" attr2=\"all\" ",
        "b:attr=\"sorted\" a:attr=\"out\"></e5>"
    )));
}

/// Empty elements become start/end tag pairs; whitespace in content is preserved exactly.
#[test]
fn empty_elements_and_whitespace() {
    let output = canonicalize("<e a=\"1\"></e>", false).unwrap_or_default();
    assert_eq!(output, "<e a=\"1\"></e>");
    let spaced = canonicalize("<doc>  <a>  x  </a>\n <b/></doc>", false).unwrap_or_default();
    assert_eq!(spaced, "<doc>  <a>  x  </a>\n <b></b></doc>");
}

/// Character references are resolved, then text `>`, `&`, `<` and `#xD` are re-escaped.
#[test]
fn character_references_round_trip_to_escapes() {
    let output = canonicalize("<v>&#x32;&amp;&lt;&gt;</v>", false).unwrap_or_default();
    assert_eq!(output, "<v>2&amp;&lt;&gt;</v>");
}

/// Exclusive canonicalisation drops a namespace an output ancestor declares but this subtree
/// never utilises, and keeps the one that is visibly utilised.
#[test]
fn exclusive_drops_unutilised_ancestor_namespaces() {
    let input = concat!(
        "<n0:pdu xmlns:n0=\"http://a.example\">",
        "<n1:elem1 xmlns:n1=\"http://b.example\" ID=\"E\">content</n1:elem1>",
        "</n0:pdu>"
    );
    let output = canonicalize_by_id(input, "E", false).unwrap_or_default();
    assert_eq!(
        output,
        "<n1:elem1 xmlns:n1=\"http://b.example\" ID=\"E\">content</n1:elem1>"
    );
}

/// With comments off, comment nodes are dropped; with comments on, they are emitted.
#[test]
fn comments_are_optional() {
    let input = "<doc>a<!--note-->b</doc>";
    assert_eq!(
        canonicalize(input, false).unwrap_or_default(),
        "<doc>ab</doc>"
    );
    assert_eq!(
        canonicalize(input, true).unwrap_or_default(),
        "<doc>a<!--note-->b</doc>"
    );
}

/// A default namespace propagates as `xmlns=""` into an element that leaves it.
#[test]
fn default_namespace_undeclaration() {
    let input = concat!(
        "<a xmlns=\"urn:a\">",
        "<b xmlns=\"urn:b\"><c xmlns=\"\">x</c></b>",
        "</a>"
    );
    let output = canonicalize(input, false).unwrap_or_default();
    assert!(output.contains("<c xmlns=\"\">x</c>"), "{output}");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Canonicalising an already canonical document changes nothing.
    #[test]
    fn canonicalisation_is_idempotent(x in canonical_fragment()) {
        if let Ok(first) = canonicalize(&x, false) {
            let second = canonicalize(&first, false).unwrap_or_default();
            prop_assert_eq!(first, second);
        }
    }

    /// Arbitrary input never panics any entry point.
    #[test]
    fn arbitrary_bytes_never_panic(text in ".{0,300}") {
        let _ = canonicalize(&text, false);
        let _ = canonicalize(&text, true);
        let _ = canonicalize_by_id(&text, "ID", false);
        let _ = spot_sso::saml::request::decode_redirect(&text);
    }
}

/// A small generator of XML-ish fragments that are often well-formed.
fn canonical_fragment() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("<a></a>".to_owned()),
        Just("<a xmlns=\"urn:a\"><b>text</b></a>".to_owned()),
        Just("<a x=\"1\" y=\"2\"><b/> c </a>".to_owned()),
        Just("<a xmlns:n=\"urn:n\"><n:b/></a>".to_owned()),
    ]
}
