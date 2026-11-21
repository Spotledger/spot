//! Tool generation: the tools a principal sees, the schema it sees, and the runtime swap.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use serde_json::{Value as Json, json};
use spot_mcp::catalog::ToolCatalog;
use spot_mcp::principal::Credential;
use spot_meta::{Definition, FieldSpec, FieldType, Property, Registry};
use spot_permissions::{Right, Rights, Role, RoleGrant};
use time::OffsetDateTime;

use support::{DOCTYPE, request, result_of, server};

fn catalog_for(principal: &str) -> ToolCatalog {
    let registry = support::registry();
    let engine = support::engine();
    let principals = support::principals();
    let functions = support::functions();
    let principal = principals.get(principal).unwrap();
    ToolCatalog::build(&registry, &engine, principal, &functions)
}

fn tool<'a>(catalog: &'a ToolCatalog, name: &str) -> &'a spot_mcp::catalog::ToolSpec {
    catalog
        .get(name)
        .unwrap_or_else(|| panic!("no tool {name}"))
}

#[test]
fn a_reader_gets_search_and_get_only() {
    let catalog = catalog_for("alice");
    assert!(catalog.get(&format!("search_{DOCTYPE}")).is_some());
    assert!(catalog.get(&format!("get_{DOCTYPE}")).is_some());
    assert!(catalog.get(&format!("create_{DOCTYPE}_draft")).is_none());
    assert!(catalog.get(&format!("update_{DOCTYPE}")).is_none());
    assert!(catalog.get("query").is_some());
    assert!(catalog.get("job_status").is_some());
}

#[test]
fn a_writer_gets_create_and_update() {
    let catalog = catalog_for("carol");
    let create = tool(&catalog, &format!("create_{DOCTYPE}_draft"));
    assert_eq!(
        create.input_schema["properties"]["title"]["type"],
        Json::String("string".to_owned())
    );
    let update = tool(&catalog, &format!("update_{DOCTYPE}"));
    assert!(update.input_schema["properties"]["id"].is_object());
    assert!(update.input_schema["properties"]["version"].is_object());
    assert!(
        update.input_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "id")
    );
}

#[test]
fn a_masked_field_is_not_filterable_and_a_hidden_field_is_absent() {
    let reader = catalog_for("alice");
    let search = tool(&reader, &format!("search_{DOCTYPE}"));
    let filters = &search.input_schema["properties"]["filter"]["properties"];
    // `salary` is readable but masked, so it cannot be a filter (the no-inference rule).
    assert!(filters.get("salary").is_none());
    // `secret` is hidden from a reader, so it is not in the schema at all.
    assert!(filters.get("secret").is_none());
    assert!(search.input_schema["properties"].as_object().is_some());

    let manager = catalog_for("bob");
    let search = tool(&manager, &format!("search_{DOCTYPE}"));
    assert!(search.input_schema["properties"]["filter"]["properties"]["secret"].is_object());
}

#[test]
fn write_schemas_omit_read_only_and_unwritable_fields() {
    let catalog = catalog_for("carol");
    let create = tool(&catalog, &format!("create_{DOCTYPE}_draft"));
    // `title` is required, so it is in `required`.
    assert!(
        create.input_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "title")
    );
    // Every property is a real field.
    for name in create.input_schema["properties"]
        .as_object()
        .unwrap()
        .keys()
    {
        assert!(support::definition().fields.iter().any(|f| &f.name == name));
    }
}

#[test]
fn whitelisted_functions_become_tools() {
    let catalog = catalog_for("alice");
    let sync = tool(&catalog, "call_demo_greet_hello");
    assert!(!sync.kind.async_job());
    let job = tool(&catalog, "call_demo_jobs_run");
    assert!(job.kind.async_job());
}

#[test]
fn tool_names_are_unique_and_stable() {
    let first = catalog_for("alice");
    let second = catalog_for("alice");
    let names: Vec<&str> = first.tools().iter().map(|t| t.name.as_str()).collect();
    let again: Vec<&str> = second.tools().iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, again);
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted);
    let mut deduped = names.clone();
    deduped.dedup();
    assert_eq!(names.len(), deduped.len());
}

#[test]
fn a_runtime_custom_field_appears_after_the_swap() {
    let mut registry = support::registry();
    registry
        .add_custom_field(
            DOCTYPE,
            FieldSpec::new("custom_priority", FieldType::Int),
            None,
        )
        .unwrap();
    let registry = Arc::new(registry);
    let meta = Arc::new(spot_mcp::server::MetaState {
        engine: Arc::new(support::engine_for_meta(
            &registry.effective(),
            &support::model(),
        )),
        registry: registry.clone(),
    });
    let engine = meta.engine.clone();
    let principals = support::principals();
    let principal = principals.get("alice").unwrap();
    let catalog = ToolCatalog::build(&registry, &engine, principal, &support::functions());
    let search = tool(&catalog, &format!("search_{DOCTYPE}"));
    assert!(
        search.input_schema["properties"]["filter"]["properties"]["custom_priority"].is_object()
    );
}

#[test]
fn the_catalogue_is_versioned_by_the_metadata_it_built() {
    let mut registry = support::registry();
    let principals = support::principals();
    let functions = support::functions();
    let principal = principals.get("alice").unwrap();

    let before = ToolCatalog::build(
        &registry,
        &support::engine_for(&registry, &support::model()),
        principal,
        &functions,
    );
    assert_eq!(
        before.meta_version(),
        registry.effective().version(),
        "the catalogue takes the version the tools were built from"
    );
    // Every DocType view those tools were generated from carries the same stamp.
    for view in registry.effective().doctypes() {
        assert_eq!(before.meta_version(), view.version);
    }

    // A custom field: the effective metadata moves, and the catalogue says so.
    registry
        .add_custom_field(
            DOCTYPE,
            FieldSpec::new("custom_priority", FieldType::Int),
            None,
        )
        .unwrap();
    let after_field = ToolCatalog::build(
        &registry,
        &support::engine_for(&registry, &support::model()),
        principal,
        &functions,
    );
    assert_eq!(after_field.meta_version(), registry.effective().version());
    assert!(after_field.meta_version() > before.meta_version());

    // A property override moves it again, and the catalogue still tracks the metadata.
    registry
        .set_property(
            DOCTYPE,
            Some("title"),
            Property::Label("Heading".to_owned()),
        )
        .unwrap();
    let after_override = ToolCatalog::build(
        &registry,
        &support::engine_for(&registry, &support::model()),
        principal,
        &functions,
    );
    assert_eq!(
        after_override.meta_version(),
        registry.effective().version()
    );
    assert!(after_override.meta_version() > after_field.meta_version());
}

#[test]
fn an_equal_version_with_different_metadata_gives_a_different_tool_list() {
    // A rollback or an adopted snapshot can serve different DocTypes at the same version, so
    // the version alone is not an identity of the catalogue's content.
    let one = Arc::new(Registry::new(vec![support::definition()]).unwrap());
    let mut other = Definition::new("other");
    other.label = Some("Other".to_owned());
    other.fields = vec![FieldSpec::new("title", FieldType::Text)];
    let two = Arc::new(Registry::new(vec![support::definition(), other]).unwrap());
    assert_eq!(
        one.effective().version(),
        two.effective().version(),
        "the two sites are at one metadata version"
    );

    let mut model = support::model();
    let principals = support::principals();
    let functions = support::functions();
    let principal = principals.get("alice").unwrap();
    let first = ToolCatalog::build(
        &one,
        &support::engine_for(&one, &model),
        principal,
        &functions,
    );
    model.grants.push(RoleGrant {
        doctype: "other".to_owned(),
        role: Role("reader".to_owned()),
        rights: Rights::new([Right::Read]),
        if_owner: false,
    });
    let second = ToolCatalog::build(
        &two,
        &support::engine_for(&two, &model),
        principal,
        &functions,
    );

    assert_eq!(first.meta_version(), second.meta_version());
    let names = |catalogue: &ToolCatalog| -> Vec<String> {
        catalogue.tools().iter().map(|t| t.name.clone()).collect()
    };
    assert_ne!(
        names(&first),
        names(&second),
        "the tool list follows the metadata, not the version"
    );
    assert!(second.get("search_other").is_some());
}

#[tokio::test]
async fn the_live_swap_shows_a_new_doctype_on_the_next_list() {
    let (server, meta) = server().await;
    let body = request(1, "tools/list", json!({}));
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("alice")), now())
        .await;
    let names = tool_names(&messages);
    assert!(!names.iter().any(|n| n == "search_other"));

    // A new DocType is added at runtime and the metadata is swapped.
    let mut other = Definition::new("other");
    other.label = Some("Other".to_owned());
    other.fields = vec![FieldSpec::new("title", FieldType::Text)];
    let registry = Arc::new(Registry::new(vec![support::definition(), other]).unwrap());
    let mut model = support::model();
    model.grants.push(RoleGrant {
        doctype: "other".to_owned(),
        role: Role("reader".to_owned()),
        rights: Rights::new([Right::Read]),
        if_owner: false,
    });
    let engine = Arc::new(support::engine_for_meta(&registry.effective(), &model));
    meta.swap(Arc::new(spot_mcp::server::MetaState { registry, engine }));

    let messages = server
        .handle_text(&body, Some(&Credential::bearer("alice")), now())
        .await;
    let names = tool_names(&messages);
    assert!(names.iter().any(|n| n == "search_other"));
}

fn tool_names(messages: &[Json]) -> Vec<String> {
    result_of(messages)["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_owned))
        .collect()
}

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}
