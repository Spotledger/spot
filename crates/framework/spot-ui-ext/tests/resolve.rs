//! Resolution behaviour: dependency order, cross-app extensions, conflicts, permission filtering,

mod support;

use std::collections::BTreeMap;

use spot_ui_ext::{
    AllowAll, AppContribution, Component, ControlKind, InstalledApp, Node, Panel, PermissionList,
    Resolved, SiteContext, Tile, codes, resolve, validate,
};

fn site() -> SiteContext {
    SiteContext::new(
        vec![
            InstalledApp::new("alpha", "1.0.0"),
            InstalledApp::new("beta", "1.0.0"),
        ],
        "0.1.0",
    )
}

fn ids(resolved: &Resolved, slot: &str) -> Vec<String> {
    resolved
        .slot(slot)
        .map(|slot| {
            slot.items()
                .iter()
                .map(|item| item.id().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn codes_of(findings: &[spot_ui_ext::UiExtError]) -> Vec<&'static str> {
    findings.iter().map(|finding| finding.code).collect()
}

fn component(app: &str, name: &str) -> Component {
    Component {
        id: format!("{app}.{name}"),
        origin: format!("app:{app}"),
        props: Vec::new(),
        slots: Vec::new(),
        body: Node::Control {
            control: ControlKind::Label,
            text: Some("x".to_owned()),
            value: None,
            on_click: None,
        },
    }
}

fn tile(app: &str, name: &str, component: &str) -> Tile {
    Tile {
        id: format!("{app}.{name}"),
        origin: format!("app:{app}"),
        label: "Tile".to_owned(),
        icon: None,
        slot: "home.tiles".to_owned(),
        doctype: None,
        order: Some(0),
        visible_if: None,
        permission: None,
        component: component.to_owned(),
        props: BTreeMap::new(),
        open: None,
    }
}

fn app_with(app: &str, depends: &[&str], document: spot_ui_ext::UiDocument) -> AppContribution {
    support::contribution(app, "1.0.0", depends, vec![document])
}

#[test]
fn dependency_order_beats_the_order_hint_and_input_order() {
    let alpha = app_with(
        "alpha",
        &[],
        support::document(
            "alpha",
            vec![support::action("alpha", "later", "shell.sidebar", 10)],
        ),
    );
    let beta = app_with(
        "beta",
        &["alpha"],
        support::document(
            "beta",
            vec![support::action("beta", "first", "shell.sidebar", 0)],
        ),
    );
    let resolved = resolve(&[beta.clone(), alpha.clone()], &site())
        .unwrap_or_else(|findings| panic!("{findings:?}"));
    assert_eq!(resolved.app_order(), vec!["alpha", "beta"]);
    assert_eq!(
        ids(&resolved, "shell.sidebar"),
        vec!["alpha.later", "beta.first"]
    );
    let reordered =
        resolve(&[alpha, beta], &site()).unwrap_or_else(|findings| panic!("{findings:?}"));
    assert_eq!(resolved.to_canonical_json(), reordered.to_canonical_json());
}

#[test]
fn equal_rank_orders_by_order_then_app_then_id() {
    let alpha = app_with(
        "alpha",
        &[],
        support::document(
            "alpha",
            vec![
                support::action("alpha", "zulu", "shell.sidebar", 5),
                support::action("alpha", "alfa", "shell.sidebar", 5),
                support::action("alpha", "early", "shell.sidebar", 1),
            ],
        ),
    );
    let beta = app_with(
        "beta",
        &[],
        support::document(
            "beta",
            vec![support::action("beta", "a", "shell.sidebar", 5)],
        ),
    );
    let resolved =
        resolve(&[beta, alpha], &site()).unwrap_or_else(|findings| panic!("{findings:?}"));
    assert_eq!(
        ids(&resolved, "shell.sidebar"),
        vec!["alpha.early", "alpha.alfa", "alpha.zulu", "beta.a"]
    );
}

#[test]
fn a_dependency_cycle_is_refused_by_code() {
    let alpha = app_with("alpha", &["beta"], support::document("alpha", Vec::new()));
    let beta = app_with("beta", &["alpha"], support::document("beta", Vec::new()));
    let Err(findings) = resolve(&[alpha, beta], &site()) else {
        panic!("a dependency cycle must be refused");
    };
    assert!(codes_of(&findings).contains(&codes::DEPENDENCY_CYCLE));
}

#[test]
fn another_apps_component_needs_a_declared_dependency_and_must_exist() {
    let alpha = app_with(
        "alpha",
        &[],
        support::document("alpha", Vec::new()).with_components(vec![component("alpha", "badge")]),
    );
    let without_dependency = app_with(
        "beta",
        &[],
        support::document(
            "beta",
            vec![support::action("beta", "a", "shell.sidebar", 0)],
        )
        .with_tiles(vec![tile("beta", "home", "alpha.badge")]),
    );
    assert!(
        codes_of(&validate(&without_dependency)).contains(&codes::EXTENSION_WITHOUT_DEPENDENCY)
    );
    let with_dependency = app_with(
        "beta",
        &["alpha"],
        support::document(
            "beta",
            vec![support::action("beta", "a", "shell.sidebar", 0)],
        )
        .with_tiles(vec![tile("beta", "home", "alpha.badge")]),
    );
    assert!(validate(&with_dependency).is_empty());
    let resolved = resolve(&[alpha.clone(), with_dependency], &site())
        .unwrap_or_else(|findings| panic!("{findings:?}"));
    assert!(resolved.component("alpha.badge").is_some());
    assert_eq!(ids(&resolved, "home.tiles"), vec!["beta.home"]);

    let ghost = app_with(
        "beta",
        &["alpha"],
        support::document(
            "beta",
            vec![support::action("beta", "a", "shell.sidebar", 0)],
        )
        .with_tiles(vec![tile("beta", "home", "alpha.ghost")]),
    );
    let Err(findings) = resolve(&[alpha, ghost], &site()) else {
        panic!("the missing component must be refused");
    };
    assert!(codes_of(&findings).contains(&codes::UNKNOWN_COMPONENT));
}

fn sample() -> (Resolved, support::Context) {
    let context = support::context();
    let path = support::contracts_dir().join("examples/sample-app/ui.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let contribution = support::sample_contribution(vec![("ui.json".to_owned(), text)]);
    let installed = context
        .app_order
        .iter()
        .map(|app| InstalledApp::new(app.clone(), "1.0.0"))
        .collect();
    let resolved = resolve(&[contribution], &SiteContext::new(installed, "0.1.0"))
        .unwrap_or_else(|findings| panic!("{findings:?}"));
    (resolved, context)
}

#[test]
fn filtering_by_permission_keeps_unfiltered_slot_hashes_and_explains_drops() {
    let (resolved, context) = sample();
    assert_eq!(
        resolved.for_permissions(&AllowAll).to_canonical_json(),
        resolved.to_canonical_json()
    );
    let mut permissions = PermissionList::new();
    for permission in &context.permissions {
        if permission != "task:write" {
            permissions.insert(permission.clone());
        }
    }
    let filtered = resolved.for_permissions(&permissions);
    assert_eq!(ids(&filtered, "doc.toolbar"), vec!["sample.open_time"]);
    assert!(
        filtered
            .dropped()
            .iter()
            .any(|item| item.id == "sample.start_work" && item.reason.contains("task:write"))
    );
    let hash =
        |resolved: &Resolved, slot: &str| resolved.slot(slot).map(|slot| slot.hash().to_owned());
    assert_eq!(
        hash(&resolved, "doc.sidepanel"),
        hash(&filtered, "doc.sidepanel")
    );
    assert_ne!(
        hash(&resolved, "doc.toolbar"),
        hash(&filtered, "doc.toolbar")
    );
    assert_eq!(
        resolved.explain_slot("doc.footer"),
        vec!["doc.footer: not a slot of contract 0.1.0"]
    );
    let explanation = filtered.explain_slot("doc.toolbar");
    assert!(
        explanation
            .iter()
            .any(|line| line.contains("sample.open_time"))
    );
    assert!(
        explanation
            .iter()
            .any(|line| line.contains("missing: sample.start_work"))
    );
}

#[test]
fn a_panel_whose_query_is_not_readable_is_dropped_whole() {
    let panel = Panel {
        id: "alpha.overview".to_owned(),
        origin: "app:alpha".to_owned(),
        label: "Overview".to_owned(),
        slot: "doc.sidepanel".to_owned(),
        doctype: Some("task".to_owned()),
        order: None,
        visible_if: None,
        permission: None,
        blocks: vec![spot_ui_ext::Block::Stat {
            query: Some("alpha.open".to_owned()),
            label: "Open".to_owned(),
            format: spot_ui_ext::StatFormat::Int,
        }],
    };
    let document = support::document("alpha", Vec::new()).with_panels(vec![panel]);
    let facts = support::simple_facts()
        .with_query(spot_ui_ext::QueryFacts::new(
            "alpha.open",
            spot_ui_ext::QueryKind::Aggregate,
            "task",
        ))
        .with_permission("task:read");
    let contribution = AppContribution::new("alpha", "1.0.0")
        .with_document(
            "alpha.json",
            serde_json::to_string(&document).unwrap_or_default(),
        )
        .with_facts(facts);
    let resolved =
        resolve(&[contribution], &site()).unwrap_or_else(|findings| panic!("{findings:?}"));
    assert!(
        resolved
            .for_permissions(&|p: &str| p == "task:read")
            .slot("doc.sidepanel")
            .is_some()
    );
    let unreadable = resolved.for_permissions(&|_: &str| false);
    assert!(
        unreadable
            .slot("doc.sidepanel")
            .is_some_and(|slot| slot.items().is_empty())
    );
    assert!(
        unreadable
            .dropped()
            .first()
            .is_some_and(|item| item.reason.contains("task:read"))
    );
}

#[test]
fn the_shell_requirement_is_the_maximum_of_the_parts() {
    let mut old = support::document("alpha", Vec::new());
    old.requires_shell = "0.1.0".to_owned();
    let mut new = support::document("beta", Vec::new());
    new.requires_shell = "0.2.0".to_owned();
    let alpha = app_with("alpha", &[], old);
    let beta = app_with("beta", &[], new);
    let resolved = resolve(&[alpha.clone(), beta.clone()], &site())
        .unwrap_or_else(|findings| panic!("{findings:?}"));
    assert_eq!(resolved.requires_shell(), "0.2.0");
    assert_eq!(resolved.shell_upgrade_needed(), Some("0.2.0"));
    let newer = SiteContext::new(
        vec![
            InstalledApp::new("alpha", "1.0.0"),
            InstalledApp::new("beta", "1.0.0"),
        ],
        "0.2.0",
    );
    let resolved =
        resolve(&[alpha, beta], &newer).unwrap_or_else(|findings| panic!("{findings:?}"));
    assert_eq!(resolved.shell_upgrade_needed(), None);
}

#[test]
fn validation_reports_every_problem_at_once() {
    let mut action = support::action("alpha", "bad", "shell.sidebar", 0);
    action.icon = Some("https://cdn.example/play.svg".to_owned());
    action.permission = Some("task:launch".to_owned());
    action.visible_if = Some("doc.status ==".to_owned());
    action.open = None;
    action.navigate_url = Some("/elsewhere".to_owned());
    let findings = validate(&app_with(
        "alpha",
        &[],
        support::document("alpha", vec![action]),
    ));
    for code in [
        codes::UNKNOWN_ICON,
        codes::UNKNOWN_PERMISSION,
        codes::EXPRESSION_INVALID,
        codes::UNKNOWN_ROUTE,
    ] {
        assert!(codes_of(&findings).contains(&code), "{code}: {findings:?}");
    }
}
