//! Seam requirements at install: version compatibility (WP-EXT-03, ADR-08, ADR-17).
#![allow(clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use common::*;
use serde_json::json;
use spot_app_api::OverrideKind;
use spot_seams::codes;

#[tokio::test]
async fn a_matching_major_installs() {
    let mut b = base();
    b.add_seam_provider("tax", "tax:line", 1, Arc::new(Const(json!(1))));
    b.require_seam("erp", "tax:line", 1);
    assert!(b.build().is_ok());
}

#[tokio::test]
async fn a_different_major_is_refused_at_install() {
    let mut b = base();
    b.add_seam_provider("tax", "tax:line", 2, Arc::new(Const(json!(1))));
    b.require_seam("erp", "tax:line", 1);
    let errs = match b.build() {
        Ok(_) => panic!("install must be refused"),
        Err(errs) => errs,
    };
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(errs[0].code, codes::SEAM_VERSION_MISMATCH);
    assert_eq!(errs[0].apps, vec!["erp".to_owned(), "tax".to_owned()]);
    assert!(errs[0].message.contains("erp"), "{}", errs[0].message);
    assert!(errs[0].message.contains("tax"), "{}", errs[0].message);
    assert!(errs[0].message.contains("version 1"), "{}", errs[0].message);
    assert!(errs[0].message.contains("2"), "{}", errs[0].message);
}

#[tokio::test]
async fn a_missing_requirement_is_refused() {
    let mut b = base();
    b.require_seam("erp", "tax:line", 1);
    let errs = match b.build() {
        Ok(_) => panic!("install must be refused"),
        Err(errs) => errs,
    };
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(errs[0].code, codes::SEAM_REQUIRED_MISSING);
    assert_eq!(errs[0].apps, vec!["erp".to_owned()]);
}

#[tokio::test]
async fn every_problem_is_reported_at_once() {
    // A version mismatch, a missing requirement and an unrelated override problem in one build.
    let mut b = base();
    b.add_seam_provider("tax", "tax:line", 2, Arc::new(Const(json!(1))));
    b.require_seam("erp", "tax:line", 1);
    b.require_seam("erp", "other:seam", 3);
    // `ext` has no Replace of `core:a` yet and `ext:x` wraps `core:b` elsewhere? Keep it simple:
    // an unknown override target is an override-scope problem unrelated to seams.
    b.add_override("erp", ov("core:missing", OverrideKind::Wrap, Some("ext:x")));
    let errs = match b.build() {
        Ok(_) => panic!("install must be refused"),
        Err(errs) => errs,
    };
    let got: Vec<&'static str> = errs.iter().map(|e| e.code).collect();
    assert!(got.contains(&codes::SEAM_VERSION_MISMATCH), "{got:?}");
    assert!(got.contains(&codes::SEAM_REQUIRED_MISSING), "{got:?}");
    assert!(got.iter().any(|c| c.starts_with("override.")), "{got:?}");
}

#[tokio::test]
async fn several_majors_side_by_side_resolve_per_caller() {
    let mut b = base();
    b.add_seam_provider("tax", "tax:line", 1, Arc::new(Const(json!("v1"))));
    b.add_seam_provider("tax", "tax:line", 2, Arc::new(Const(json!("v2"))));
    b.require_seam("erp", "tax:line", 2);
    b.require_seam("legacy", "tax:line", 1);
    let r = b.build().unwrap();
    assert_eq!(r.seam_versions("tax:line"), vec![1, 2]);
    assert_eq!(r.seam_versions("nope"), Vec::<u32>::new());
    use spot_app_api::Seams;
    assert_eq!(
        r.call_json("tax:line", 1, json!({})).await.unwrap(),
        json!("v1")
    );
    assert_eq!(
        r.call_json("tax:line", 2, json!({})).await.unwrap(),
        json!("v2")
    );
}
