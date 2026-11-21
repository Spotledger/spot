//! The routing table: chains, lookups, and the TOML parser's all-errors report.

use std::collections::BTreeMap;

use spot_llm_router::Route;
use spot_llm_router::RouterError;
use spot_llm_router::RoutingTable;

/// The document the crate documentation shows: two tasks, two and one routes.
const DOCUMENT: &str = r#"
[routes.chat]
chain = [
  { provider = "anthropic", model = "claude-sonnet-4" },
  { provider = "openrouter", model = "meta-llama/llama-3.1-70b" },
]

[routes.classify]
chain = [{ provider = "ollama", model = "llama3.2:3b" }]
"#;

#[test]
fn parses_a_document_into_ordered_chains() {
    let table = match RoutingTable::from_toml(DOCUMENT) {
        Ok(table) => table,
        Err(error) => panic!("the document should parse: {error}"),
    };
    let chat = table
        .chain("chat")
        .unwrap_or_else(|| panic!("chat is routed"));
    assert_eq!(chat.len(), 2);
    assert_eq!(chat[0], Route::new("anthropic", "claude-sonnet-4"));
    assert_eq!(
        chat[1],
        Route::new("openrouter", "meta-llama/llama-3.1-70b")
    );
    let classify = table
        .chain("classify")
        .unwrap_or_else(|| panic!("classify is routed"));
    assert_eq!(classify.len(), 1);
    assert_eq!(classify[0], Route::new("ollama", "llama3.2:3b"));
    assert!(!table.knows("plan"));
}

#[test]
fn every_problem_is_reported_at_once() {
    // One task name typo'd empty, one empty chain, one route without a provider and one
    // without a model, plus a duplicated route: five problems, all in one error.
    let document = r#"
[routes.chat]
chain = []

[routes.summarise]
chain = [
  { provider = "", model = "m" },
  { provider = "ollama", model = "" },
  { provider = "ollama", model = "llama3.2:3b" },
  { provider = "ollama", model = "llama3.2:3b" },
]
"#;
    let problems = match RoutingTable::from_toml(document) {
        Ok(table) => panic!("the document should not parse, but produced {table:?}"),
        Err(RouterError::ConfigInvalid { problems }) => problems,
        Err(other) => panic!("a parser failure is config_invalid, not {other:?}"),
    };
    let joined = problems.join("\n");
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("has an empty chain")),
        "the empty chain is one of the problems: {joined}"
    );
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("names no provider")),
        "the provider-less route is named: {joined}"
    );
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("names no model")),
        "the model-less route is named: {joined}"
    );
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("twice; the second is never reached")),
        "the duplicate route is named: {joined}"
    );
    assert!(problems.len() >= 4, "every problem at once: {joined}");
}

#[test]
fn a_document_outside_toml_names_the_syntax_error() {
    let problems = match RoutingTable::from_toml("[routes.chat chain = [") {
        Ok(table) => panic!("broken TOML should not parse, but produced {table:?}"),
        Err(RouterError::ConfigInvalid { problems }) => problems,
        Err(other) => panic!("a parser failure is config_invalid, not {other:?}"),
    };
    assert_eq!(problems.len(), 1);
    assert!(
        problems[0].contains("not valid TOML"),
        "the syntax problem is named: {}",
        problems[0]
    );
}

#[test]
fn a_document_without_routes_is_refused() {
    let problems = match RoutingTable::from_toml("# nothing here\n") {
        Ok(table) => panic!("an empty document should not parse, but produced {table:?}"),
        Err(RouterError::ConfigInvalid { problems }) => problems,
        Err(other) => panic!("a parser failure is config_invalid, not {other:?}"),
    };
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("declares no routes")),
        "the missing routes are named: {:?}",
        problems
    );
}

#[test]
fn router_error_codes_are_stable() {
    assert_eq!(
        RouterError::ConfigInvalid {
            problems: vec!["x".into()]
        }
        .code(),
        "router.config_invalid"
    );
    assert_eq!(
        RouterError::TaskUnrouted {
            task: "plan".into()
        }
        .code(),
        "router.task_unrouted"
    );
    assert_eq!(
        RouterError::RoutesExhausted {
            task: "plan".into(),
            detail: "why".into(),
        }
        .code(),
        "router.routes_exhausted"
    );
    assert_eq!(
        RouterError::BudgetExceeded {
            scope: "tenant `acme`".into(),
            spent: 5,
            limit: 4,
        }
        .code(),
        "router.budget_exceeded"
    );
    assert_eq!(
        RouterError::TaskUnrouted {
            task: "plan".into()
        }
        .with_code(),
        "router.task_unrouted: no route for task `plan`"
    );
}

#[test]
fn programmatic_tables_read_the_same() {
    let mut routes: BTreeMap<String, Vec<Route>> = BTreeMap::new();
    routes.insert(
        "chat".into(),
        vec![Route::new("mock-a", "m1"), Route::new("mock-b", "m2")],
    );
    let built = RoutingTable::new(routes);
    let parsed = match RoutingTable::from_toml(
        "[routes.chat]\nchain = [\n{ provider = \"mock-a\", model = \"m1\" },\n{ provider = \"mock-b\", model = \"m2\" },\n]\n",
    ) {
        Ok(parsed) => parsed,
        Err(error) => panic!("the document should parse: {error}"),
    };
    assert_eq!(built, parsed);
    assert_eq!(built.tasks().collect::<Vec<_>>(), ["chat"]);
}
