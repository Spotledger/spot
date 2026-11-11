//! The optional service principal on a Document node (WP-BPM-01.T4).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_bpm_model::{
    AllowAllMeta, DocumentAction, DocumentConfig, Limits, NodeKind, ProcessVersion, codes, compile,
    from_text, to_text, validate_version,
};

fn text(extra: &str) -> String {
    format!(
        r#"name = "demo"
app = "demo_app"
version = 1
status = "draft"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "post"
kind = "document"
action = "post"
doctype = "Invoice"
{extra}

[[nodes]]
id = "end"
kind = "logic"
logic = "branch"
terminal = true

[[edges]]
from = "start"
to = "post"

[[edges]]
from = "post"
to = "end"
"#
    )
}

fn document(version: &ProcessVersion) -> DocumentConfig {
    version
        .nodes
        .iter()
        .find_map(|n| match &n.kind {
            NodeKind::Document(c) => Some(c.clone()),
            _ => None,
        })
        .unwrap()
}

fn problems(version: &ProcessVersion) -> Vec<spot_bpm_model::Problem> {
    validate_version(version, &AllowAllMeta, &Limits::new())
}

#[test]
fn parses_with_and_without_a_principal() {
    assert_eq!(document(&from_text(&text("")).unwrap()).principal, None);
    let with = from_text(&text("principal = \"svc_billing\"")).unwrap();
    assert_eq!(document(&with).principal.as_deref(), Some("svc_billing"));
    assert!(
        problems(&with)
            .iter()
            .all(|p| p.code != codes::PRINCIPAL_INVALID)
    );
}

#[test]
fn round_trips_and_absent_stays_absent() {
    let with = from_text(&text("principal = \"svc_billing\"")).unwrap();
    let again = from_text(&to_text(&with)).unwrap();
    assert_eq!(document(&again).principal.as_deref(), Some("svc_billing"));
    let without = from_text(&text("")).unwrap();
    assert!(!to_text(&without).contains("principal"));
}

#[test]
fn compiled_node_config_carries_the_principal_under_its_key() {
    let version = from_text(&text("principal = \"svc_billing\"")).unwrap();
    let compiled = compile(&version).unwrap();
    let node = compiled
        .definition
        .nodes
        .iter()
        .find(|n| n.id.as_str() == "post")
        .unwrap();
    assert_eq!(node.config["principal"], "svc_billing");
    let plain = compile(&from_text(&text("")).unwrap()).unwrap();
    let node = plain
        .definition
        .nodes
        .iter()
        .find(|n| n.id.as_str() == "post")
        .unwrap();
    assert!(node.config.get("principal").is_none());
}

#[test]
fn an_invalid_principal_is_one_problem_without_the_value() {
    let long = "a".repeat(129);
    for bad in ["", "has space", "tab\\there", "new\\nline", long.as_str()] {
        let version = from_text(&text(&format!("principal = \"{bad}\""))).unwrap();
        let found = problems(&version);
        let hits: Vec<_> = found
            .iter()
            .filter(|p| p.code == codes::PRINCIPAL_INVALID)
            .collect();
        assert_eq!(hits.len(), 1, "{bad:?}: {found:?}");
        assert_eq!(hits[0].node.as_ref().unwrap().as_str(), "post");
        if !bad.is_empty() {
            let shown = format!("{:?}", hits[0]);
            assert!(
                !shown.contains("has space") && !shown.contains(&long),
                "{shown}"
            );
        }
    }
    let ok = "a".repeat(128);
    let version = from_text(&text(&format!("principal = \"{ok}\""))).unwrap();
    assert!(
        problems(&version)
            .iter()
            .all(|p| p.code != codes::PRINCIPAL_INVALID)
    );
}

#[test]
fn another_unknown_field_is_still_refused() {
    assert!(from_text(&text("principle = \"x\"")).is_err());
    let _ = DocumentAction::Post;
}
