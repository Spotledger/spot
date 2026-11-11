//! Properties the contract promises: parsers never panic, dependency graphs resolve in an order

mod support;

use proptest::prelude::*;

use spot_ui_ext::{
    AppContribution, InstalledApp, SiteContext, parse_document, parse_document_as, resolve,
    validate,
};

fn site_for(apps: &[String]) -> SiteContext {
    SiteContext::new(
        apps.iter()
            .map(|app| InstalledApp::new(app.clone(), "1.0.0"))
            .collect(),
        "0.1.0",
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn arbitrary_text_never_panics_the_parsers(text in any::<String>()) {
        let _ = parse_document("ui.json", &text);
        let _ = parse_document("ui.toml", &text);
        let _ = parse_document_as("ui.toml", &text, spot_ui_ext::Format::Toml);
        let contribution = AppContribution::new("sample", "1.0.0")
            .with_document("ui.json", text.clone())
            .with_document("ui.toml", text);
        let _ = validate(&contribution);
    }

    #[test]
    fn acyclic_dependency_graphs_resolve_dependencies_first(
        shape in prop::collection::vec(prop::collection::vec(any::<bool>(), 1..5), 0..6)
    ) {
        let apps: Vec<String> = (0..shape.len().max(1)).map(|index| format!("a{index}")).collect();
        let contributions: Vec<AppContribution> = shape
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let depends: Vec<&str> = row
                    .iter()
                    .enumerate()
                    .take(index)
                    .filter(|(_, wanted)| **wanted)
                    .map(|(dependency, _)| apps[dependency].as_str())
                    .collect();
                support::contribution(
                    &apps[index],
                    "1.0.0",
                    &depends,
                    vec![support::document(
                        &apps[index],
                        vec![support::action(&apps[index], "one", "shell.sidebar", 0)],
                    )],
                )
            })
            .collect();
        let resolved = resolve(&contributions, &site_for(&apps))
            .unwrap_or_else(|findings| panic!("{findings:?}"));
        let order = resolved.app_order();
        let position = |app: &str| order.iter().position(|name| *name == app);
        for contribution in &contributions {
            let here = position(&contribution.app).unwrap_or_else(|| panic!("{}", contribution.app));
            for dependency in &contribution.depends {
                let there = position(dependency).unwrap_or_else(|| panic!("{dependency}"));
                prop_assert!(there < here, "{dependency} must come before {}", contribution.app);
            }
        }
        prop_assert_eq!(order.len(), contributions.len());
    }

    #[test]
    fn resolved_output_is_invariant_under_input_permutation(
        keys in prop::collection::vec(any::<u8>(), 2..5),
        orders in prop::collection::vec(-5i64..5, 2..5),
    ) {
        let count = keys.len().min(orders.len());
        let apps: Vec<String> = (0..count).map(|index| format!("p{index}")).collect();
        let contributions: Vec<AppContribution> = apps
            .iter()
            .enumerate()
            .map(|(index, app)| {
                support::contribution(
                    app,
                    "1.0.0",
                    &[],
                    vec![support::document(
                        app,
                        vec![support::action(app, "one", "shell.sidebar", orders[index])],
                    )],
                )
            })
            .collect();
        let first = resolve(&contributions, &site_for(&apps))
            .unwrap_or_else(|findings| panic!("{findings:?}"));
        let mut keyed: Vec<(u8, AppContribution)> =
            keys.iter().copied().zip(contributions.iter().cloned()).collect();
        keyed.sort_by_key(|(key, _)| *key);
        let shuffled: Vec<AppContribution> = keyed.into_iter().map(|(_, app)| app).collect();
        let second = resolve(&shuffled, &site_for(&apps))
            .unwrap_or_else(|findings| panic!("{findings:?}"));
        prop_assert_eq!(first.to_canonical_json(), second.to_canonical_json());
        prop_assert_eq!(first.hash(), second.hash());
        for slot in first.slots() {
            prop_assert_eq!(
                Some(slot.hash().to_owned()),
                second.slot(slot.name()).map(|other| other.hash().to_owned())
            );
        }
    }
}
