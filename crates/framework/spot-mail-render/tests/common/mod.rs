#![allow(dead_code)]
//! Shared helpers: the checked-in fixture roots and the contexts of the golden templates.

use std::path::PathBuf;

use spot_mail_render::{MailRenderer, TemplateRoots, context_from_json};
use spot_template::Context;

pub fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// The site root first, then the app root: the site overrides the app.
pub fn renderer() -> MailRenderer {
    MailRenderer::new(TemplateRoots::new(vec![
        fixtures().join("site"),
        fixtures().join("app"),
    ]))
}

pub fn ctx(json: serde_json::Value) -> Context {
    match context_from_json(json) {
        Ok(context) => context,
        Err(error) => panic!("context: {error}"),
    }
}

pub fn contexts() -> Vec<(&'static str, Context)> {
    vec![
        (
            "invoice",
            ctx(serde_json::json!({
                "customer": "Khan & Sons",
                "number": "INV-001",
                "company": "Acme",
                "total": {"$decimal": "12345.50"},
                "due": {"$date": "2026-03-10"},
            })),
        ),
        (
            "reset",
            ctx(serde_json::json!({"name": "Bilal", "code": "K7-42", "hours": 2})),
        ),
        ("plain", ctx(serde_json::json!({}))),
        ("htmlonly", ctx(serde_json::json!({"name": "Bilal"}))),
        ("notice", ctx(serde_json::json!({"name": "Bilal"}))),
        (
            "statement",
            ctx(serde_json::json!({
                "month": "March",
                "lines": [
                    {"desc": "Rent", "amount": {"$decimal": "1000.00"}},
                    {"desc": "Fuel", "amount": {"$decimal": "250.50"}},
                ],
                "total": {"$decimal": "1250.50"},
            })),
        ),
    ]
}
