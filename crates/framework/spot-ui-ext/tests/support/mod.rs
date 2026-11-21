//! Shared helpers for the integration tests: the contract directory and the sample app's

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;
use spot_ui_ext::{AppContribution, AppFacts, MethodFacts, QueryFacts, QueryKind, UiDocument};

pub fn contracts_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../contracts/ui-ext")
}

/// The sample app's stand-in context: what its manifest and registry declare today.
#[derive(Deserialize)]
pub struct Context {
    pub app: String,
    pub app_order: Vec<String>,
    pub methods: BTreeMap<String, Vec<String>>,
    pub queries: BTreeMap<String, QueryInfo>,
    pub permissions: Vec<String>,
    pub destinations: Vec<String>,
    pub routes: Vec<String>,
}

#[derive(Deserialize)]
pub struct QueryInfo {
    pub kind: String,
    pub doctype: String,
    #[serde(default)]
    pub columns: Vec<String>,
}

pub fn context() -> Context {
    let path = contracts_dir().join("examples/sample-app/context.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The facts the context declares.
pub fn facts_from(context: &Context) -> AppFacts {
    let mut facts = AppFacts::new();
    for (doctype, methods) in &context.methods {
        for method in methods {
            facts = facts.with_method(MethodFacts::new(doctype.clone(), method.clone()));
        }
    }
    for (id, info) in &context.queries {
        let kind =
            QueryKind::parse(&info.kind).unwrap_or_else(|| panic!("query {id}: {}", info.kind));
        facts = facts.with_query(
            QueryFacts::new(id.clone(), kind, info.doctype.clone())
                .with_columns(info.columns.clone()),
        );
    }
    for permission in &context.permissions {
        facts = facts.with_permission(permission.clone());
    }
    for destination in &context.destinations {
        facts = facts.with_destination(destination.clone());
    }
    for route in &context.routes {
        facts = facts.with_route(route.clone());
    }
    facts
}

pub fn sample_contribution(files: Vec<(String, String)>) -> AppContribution {
    let context = context();
    let mut contribution =
        AppContribution::new(context.app.clone(), "1.0.0").with_dependency("platform");
    for (file, text) in files {
        contribution = contribution.with_document(file, text);
    }
    contribution.with_facts(facts_from(&context))
}

pub fn simple_facts() -> AppFacts {
    AppFacts::new()
        .with_destination("platform.home")
        .with_route("/")
}

pub fn action(app: &str, name: &str, slot: &str, order: i64) -> spot_ui_ext::Action {
    spot_ui_ext::Action {
        id: format!("{app}.{name}"),
        origin: format!("app:{app}"),
        label: "Do".to_owned(),
        icon: None,
        slot: slot.to_owned(),
        doctype: None,
        order: Some(order),
        visible_if: None,
        enabled_if: None,
        permission: None,
        confirm: None,
        call: None,
        open: Some(spot_ui_ext::DestinationRef {
            destination: "platform.home".to_owned(),
            filters: BTreeMap::new(),
        }),
        navigate_url: None,
    }
}

pub fn document(app: &str, actions: Vec<spot_ui_ext::Action>) -> UiDocument {
    UiDocument {
        contract_version: spot_ui_ext::CONTRACT_VERSION.to_owned(),
        requires_shell: "0.1.0".to_owned(),
        app: app.to_owned(),
        comment: None,
        actions,
        panels: Vec::new(),
        components: Vec::new(),
        tiles: Vec::new(),
        tabs: Vec::new(),
    }
}

pub fn contribution(
    app: &str,
    version: &str,
    depends: &[&str],
    documents: Vec<UiDocument>,
) -> AppContribution {
    let mut contribution = AppContribution::new(app, version).with_facts(simple_facts());
    for dependency in depends {
        contribution = contribution.with_dependency(*dependency);
    }
    for (index, document) in documents.into_iter().enumerate() {
        let text = serde_json::to_string(&document).unwrap_or_else(|e| panic!("{app}: {e}"));
        contribution = contribution.with_document(format!("{app}-{index}.json"), text);
    }
    contribution
}
