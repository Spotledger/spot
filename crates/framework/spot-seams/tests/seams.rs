//! Seam resolution, replacement and `method.not_found`.
#![allow(clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use common::*;
use serde_json::json;
use spot_app_api::{ErrorClass, Seams};

#[tokio::test]
async fn a_seam_call_reaches_its_provider() {
    let mut b = base();
    b.add_seam_provider(
        "tax",
        "tax:line",
        1,
        Arc::new(Const(json!({"tax": "1.00"}))),
    );
    let r = b.build().unwrap();
    let out = r.call_json("tax:line", 1, json!({})).await.unwrap();
    assert_eq!(out, json!({"tax": "1.00"}));
    let info = r.seam_info("tax:line", 1).unwrap();
    assert_eq!(info.provider_app, "tax");
    assert!(info.replaced_by.is_none());
}

#[tokio::test]
async fn a_replacement_answers_instead_of_the_original() {
    let mut b = base();
    b.add_seam_provider("tax", "tax:line", 1, Arc::new(Const(json!("original"))));
    b.add_seam_provider("tax", "tax:line", 2, Arc::new(Const(json!("v2"))));
    b.replace_seam(
        "local_tax",
        "tax:line",
        1,
        Arc::new(Const(json!("replacement"))),
        "local rules",
    );
    let r = b.build().unwrap();
    assert_eq!(
        r.call_json("tax:line", 1, json!({})).await.unwrap(),
        json!("replacement")
    );
    // Other versions are untouched.
    assert_eq!(
        r.call_json("tax:line", 2, json!({})).await.unwrap(),
        json!("v2")
    );
    let info = r.seam_info("tax:line", 1).unwrap();
    assert_eq!(info.provider_app, "tax");
    assert_eq!(
        info.replaced_by,
        Some(("local_tax".to_owned(), "local rules".to_owned()))
    );
}

#[tokio::test]
async fn an_unknown_seam_or_version_is_method_not_found() {
    let mut b = base();
    b.add_seam_provider("tax", "tax:line", 1, Arc::new(Const(json!(1))));
    let r = b.build().unwrap();
    for (name, version) in [("tax:line", 9), ("tax:other", 1)] {
        let e = r.call_json(name, version, json!({})).await.unwrap_err();
        assert_eq!(e.class, ErrorClass::MethodNotFound);
        assert_eq!(e.code, "method.not_found");
    }
}
