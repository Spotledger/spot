//! Property tests (WP-ALM-01): the reader never panics on arbitrary bytes, and the insertion
//! order of a site's customizations never changes the bundle's digest.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use proptest::prelude::*;
use spot_custom_bundle::{Bundle, CaptureOptions, FakeSite, Limits, SectionRegistry, capture};

fn options() -> CaptureOptions {
    CaptureOptions::new("site-a", "1.0.0", "glm", "2026-01-01T00:00:00Z")
}

/// A site whose six customization items are inserted in the given order.
fn site_in_order(order: &[usize]) -> FakeSite {
    let mut site = empty_site();
    for &index in order {
        site = match index {
            0 => site.with_custom_field(
                "sales_invoice",
                spot_meta::FieldSpec::new("custom_a", spot_meta::FieldType::Text),
                None,
            ),
            1 => site.with_custom_field(
                "sales_invoice",
                spot_meta::FieldSpec::new("custom_b", spot_meta::FieldType::Text),
                Some("customer"),
            ),
            2 => site.with_custom_field(
                "sales_invoice",
                spot_meta::FieldSpec::new("custom_c", spot_meta::FieldType::Text),
                None,
            ),
            3 => site.with_override(
                "sales_invoice",
                Some("note"),
                spot_meta::Property::Label("Notes".to_owned()),
            ),
            4 => site.with_override(
                "sales_invoice",
                Some("note"),
                spot_meta::Property::Required(true),
            ),
            5 => site.with_override(
                "sales_invoice",
                Some("internal_note"),
                spot_meta::Property::Hidden(true),
            ),
            _ => site,
        };
    }
    site
}

proptest! {
    /// Any bytes at all: the reader returns an error or a bundle, never a panic.
    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..2048)) {
        let _ = Bundle::open(&bytes, &Limits::default(), &SectionRegistry::new(), true);
        let _ = Bundle::open(&bytes, &Limits::default(), &SectionRegistry::new(), false);
    }

    /// Random permutations of the same items produce the same digest: capture sorts, so the
    /// content address does not depend on the order the site recorded its changes.
    #[test]
    fn insertion_order_does_not_change_the_digest(keys in prop::collection::vec(any::<u8>(), 6)) {
        let reference = site_in_order(&[0, 1, 2, 3, 4, 5]);
        let mut order: Vec<usize> = (0..6).collect();
        order.sort_by_key(|&index| (keys[index], index));
        let permuted = site_in_order(&order);
        let expected = capture(&reference, &options()).unwrap().digest();
        let got = capture(&permuted, &options()).unwrap().digest();
        prop_assert_eq!(expected, got);
    }
}
