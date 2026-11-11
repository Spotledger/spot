//! Dependency resolution: the shapes the brief names, and a property test that every random
//! acyclic catalogue resolves in dependency order with every requirement satisfied.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;

use common::*;
use proptest::prelude::*;
use semver::Version;
use spot_installer::{Approval, Catalogue, Operation, Request, SiteState};
use spot_manifest::AppManifest;

/// The version one action installs, or `None` for an uninstall.
fn installed_version(action: &spot_installer::Action) -> Option<Version> {
    match &action.op {
        Operation::Install { version, .. } => Some(version.clone()),
        Operation::Upgrade { to, .. } => Some(to.clone()),
        Operation::Uninstall { .. } => None,
    }
}

#[test]
fn narrowing_ranges_choose_the_highest_that_satisfies_all() {
    let catalogue = Catalogue::new(vec![
        Spec::new("a", "1.0.0")
            .dep("b", "^1")
            .dep("c", "^1")
            .candidate(1),
        Spec::new("b", "1.0.0").dep("d", ">=1.2, <2").candidate(2),
        Spec::new("c", "1.0.0").dep("d", "^1").candidate(3),
        Spec::new("d", "1.0.0").candidate(4),
        Spec::new("d", "1.5.0").candidate(5),
    ]);
    let plan = spot_installer::plan(
        &SiteState::new(),
        &catalogue,
        &host("0.1.0"),
        &Request::Install {
            app: "a".to_owned(),
            version_req: req("^1"),
        },
        &Approval::new(),
    )
    .unwrap();
    let d = plan
        .actions()
        .iter()
        .find(|action| action.app() == "d")
        .expect("d is installed");
    assert_eq!(installed_version(d), Some(ver("1.5.0")));
}

#[test]
fn a_free_choice_picks_the_highest_version() {
    let catalogue = Catalogue::new(vec![
        Spec::new("x", "1.0.0").candidate(1),
        Spec::new("x", "1.2.0").candidate(2),
        Spec::new("x", "2.0.0").candidate(3),
    ]);
    let plan = spot_installer::plan(
        &SiteState::new(),
        &catalogue,
        &host("0.1.0"),
        &Request::Install {
            app: "x".to_owned(),
            version_req: req("^1"),
        },
        &Approval::new(),
    )
    .unwrap();
    assert_eq!(installed_version(&plan.actions()[0]), Some(ver("1.2.0")));
}

#[test]
fn an_upgrade_backtracks_past_a_version_that_breaks_a_dependent() {
    let dependent = Spec::new("c", "1.0.0").dep("d", "^1");
    let site = SiteState {
        apps: vec![installed("d", "1.0.0", 1), installed_from(&dependent, 2)],
    };
    let catalogue = Catalogue::new(vec![
        Spec::new("d", "1.5.0").candidate(2),
        Spec::new("d", "2.0.0").candidate(3),
        dependent.candidate(2),
    ]);
    let plan = spot_installer::plan(
        &site,
        &catalogue,
        &host("0.1.0"),
        &Request::Upgrade {
            app: "d".to_owned(),
            to: None,
        },
        &Approval::new(),
    )
    .unwrap();
    assert_eq!(installed_version(&plan.actions()[0]), Some(ver("1.5.0")));
}

proptest! {
    /// For any acyclic catalogue, the plan installs every dependency before its dependent and
    /// every installed app's requirements hold in the resulting site.
    #[test]
    fn random_acyclic_catalogues_resolve_consistently(
        n in 1usize..6,
        raw in prop::collection::vec((any::<u8>(), any::<u8>()), 0..8),
    ) {
        let mut edges: Vec<(usize, usize)> = Vec::new();
        for (a, b) in &raw {
            let x = (*a as usize) % n;
            let y = (*b as usize) % n;
            let (from, to) = if x < y { (x, y) } else { (y, x) };
            if from != to && !edges.contains(&(from, to)) {
                edges.push((from, to));
            }
        }

        let mut specs: Vec<Spec> = Vec::new();
        for i in 0..n {
            let mut spec = Spec::new(&format!("app{i}"), "1.0.0");
            for (from, to) in &edges {
                if *from == i {
                    spec = spec.dep(&format!("app{to}"), "^1");
                }
            }
            specs.push(spec);
        }
        let catalogue = Catalogue::new(
            specs
                .iter()
                .enumerate()
                .map(|(i, spec)| spec.candidate(i as u8 + 1))
                .collect(),
        );

        let plan = spot_installer::plan(
            &SiteState::new(),
            &catalogue,
            &host("0.1.0"),
            &Request::Install {
                app: "app0".to_owned(),
                version_req: req("^1"),
            },
            &Approval::new(),
        )
        .expect("a complete acyclic catalogue always resolves");

        let manifests: BTreeMap<String, AppManifest> = specs
            .iter()
            .map(|spec| (spec.name.clone(), spec.manifest()))
            .collect();

        // The resulting site: apply the actions in order.
        let mut site: BTreeMap<String, Version> = BTreeMap::new();
        for action in plan.actions() {
            match installed_version(action) {
                Some(version) => {
                    site.insert(action.app().to_owned(), version);
                }
                None => {
                    site.remove(action.app());
                }
            }
        }

        // Every requirement holds in the resulting site.
        for (app, version) in &site {
            for dep in &manifests[app].app.depends {
                let present = site
                    .get(dep.app.as_str())
                    .unwrap_or_else(|| panic!("{app} needs {} but it is absent", dep.app));
                prop_assert!(
                    dep.version.matches(present),
                    "{} {} needs {} {} but {present} is installed",
                    app,
                    version,
                    dep.app,
                    dep.version
                );
            }
        }

        // Every dependency that is itself an action runs before its dependent.
        let index: BTreeMap<&str, usize> = plan
            .actions()
            .iter()
            .enumerate()
            .filter(|(_, action)| installed_version(action).is_some())
            .map(|(i, action)| (action.app(), i))
            .collect();
        for (i, action) in plan.actions().iter().enumerate() {
            if installed_version(action).is_none() {
                continue;
            }
            for dep in &manifests[action.app()].app.depends {
                if let Some(dep_index) = index.get(dep.app.as_str()) {
                    prop_assert!(
                        *dep_index < i,
                        "{} runs at {i} before its dependency {} at {dep_index}",
                        action.app(),
                        dep.app
                    );
                }
            }
        }
    }
}
