//! `plan` is a borrowed lookup into a table built once.
//!
//! The brief asks for a counting global allocator here. That needs `unsafe impl GlobalAlloc`,
//! and the workspace forbids `unsafe_code` (ADR-45 lints; the same brief forbids `unsafe`).
//! So this test proves the property structurally instead: every call returns the *same*
//! slice at the same address, so no call can build a fresh `Vec`; `plan` is also declared to
//! return `&[PlannedStep]`, which cannot own anything. See the task report.
#![allow(clippy::unwrap_used)]

mod common;

use common::*;
use spot_app_api::OverrideKind::*;

#[test]
fn plan_returns_the_same_borrowed_slice_every_time() {
    let mut b = base();
    b.add_override("ext", ov("core:b", Before, Some("ext:x")));
    b.add_override("ext", ov("core:b", Wrap, Some("ext:y")));
    let r = b.build().unwrap();
    let first = r.plan("item", &before_save());
    assert!(!first.is_empty());
    for _ in 0..1000 {
        let again = r.plan("item", &before_save());
        assert!(std::ptr::eq(first, again));
    }
    // An absent point is the shared empty slice, not a new allocation.
    let a = r.plan("nothing", &before_save());
    assert_eq!(a.len(), 0);
}
