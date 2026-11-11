//! The planner's refusals, its capability approval state, its explanations and its hash — the
//! acceptance line of WP-STR-04.T1.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_installer::{
    Action, Approval, Candidate, Catalogue, Host, InstallPlan, Operation, PlanError, PlanState,
    Request, Risk, SiteState,
};

/// Runs a plan and returns it, or the refusal.
fn run(
    site: &SiteState,
    catalogue: &Catalogue,
    host: &Host,
    request: &Request,
    approval: &Approval,
) -> Result<InstallPlan, PlanError> {
    spot_installer::plan(site, catalogue, host, request, approval)
}

/// A table row: a name and a closure producing a result.
type Case = (&'static str, fn() -> Result<InstallPlan, PlanError>);

/// Every refusal the planner can produce, one row each.
fn refusal_cases() -> Vec<Case> {
    vec![
        ("installer.unverified", || {
            let cat = Catalogue::new(vec![Spec::new("a", "1.0.0").candidate_unverified(1)]);
            run(
                &SiteState::new(),
                &cat,
                &host("0.1.0"),
                &Request::Install {
                    app: "a".to_owned(),
                    version_req: req("^1"),
                },
                &Approval::new(),
            )
        }),
        ("installer.requires_host", || {
            let cat = Catalogue::new(vec![Spec::new("a", "1.0.0").host_req(">=0.2").candidate(1)]);
            run(
                &SiteState::new(),
                &cat,
                &host("0.1.0"),
                &Request::Install {
                    app: "a".to_owned(),
                    version_req: req("^1"),
                },
                &Approval::new(),
            )
        }),
        ("installer.wit_world_mismatch", || {
            let cat = Catalogue::new(vec![
                Spec::new("a", "1.0.0").candidate_world(1, "spot:app@9.0.0"),
            ]);
            run(
                &SiteState::new(),
                &cat,
                &host("0.1.0"),
                &Request::Install {
                    app: "a".to_owned(),
                    version_req: req("^1"),
                },
                &Approval::new(),
            )
        }),
        ("installer.downgrade", || {
            let site = SiteState {
                apps: vec![installed("a", "2.0.0", 1)],
            };
            let cat = Catalogue::new(vec![Spec::new("a", "1.0.0").candidate(2)]);
            run(
                &site,
                &cat,
                &host("0.1.0"),
                &Request::Upgrade {
                    app: "a".to_owned(),
                    to: Some(ver("1.0.0")),
                },
                &Approval::new(),
            )
        }),
        ("installer.downgrade_unsafe", || {
            let installed_spec = Spec::new("a", "2.0.0").patch("p1", "metadata", true);
            let site = SiteState {
                apps: vec![installed_from(&installed_spec, 1)],
            };
            let cat = Catalogue::new(vec![
                Spec::new("a", "1.0.0")
                    .patch("p1", "metadata", true)
                    .patch("p2", "data", true)
                    .candidate(2),
            ]);
            run(
                &site,
                &cat,
                &host("0.1.0"),
                &Request::Downgrade {
                    app: "a".to_owned(),
                    to: ver("1.0.0"),
                },
                &Approval::new(),
            )
        }),
        ("installer.digest_changed", || {
            let site = SiteState {
                apps: vec![installed("a", "1.0.0", 1)],
            };
            let cat = Catalogue::new(vec![Spec::new("a", "1.0.0").candidate(2)]);
            run(
                &site,
                &cat,
                &host("0.1.0"),
                &Request::Upgrade {
                    app: "a".to_owned(),
                    to: Some(ver("1.0.0")),
                },
                &Approval::new(),
            )
        }),
        ("installer.already_installed", || {
            let site = SiteState {
                apps: vec![installed("a", "1.0.0", 1)],
            };
            let cat = Catalogue::new(vec![Spec::new("a", "1.0.0").candidate(1)]);
            run(
                &site,
                &cat,
                &host("0.1.0"),
                &Request::Install {
                    app: "a".to_owned(),
                    version_req: req("^1"),
                },
                &Approval::new(),
            )
        }),
        ("installer.not_installed", || {
            let cat = Catalogue::new(vec![Spec::new("a", "1.0.0").candidate(1)]);
            run(
                &SiteState::new(),
                &cat,
                &host("0.1.0"),
                &Request::Upgrade {
                    app: "a".to_owned(),
                    to: None,
                },
                &Approval::new(),
            )
        }),
        ("installer.not_installed", || {
            let cat = Catalogue::new(vec![Spec::new("a", "1.0.0").candidate(1)]);
            run(
                &SiteState::new(),
                &cat,
                &host("0.1.0"),
                &Request::Uninstall {
                    app: "a".to_owned(),
                    purge: false,
                },
                &Approval::new(),
            )
        }),
        ("installer.has_dependents", || {
            let dependent = Spec::new("b", "1.0.0").dep("a", "^1");
            let site = SiteState {
                apps: vec![installed("a", "1.0.0", 1), installed_from(&dependent, 2)],
            };
            let cat = Catalogue::new(vec![
                Spec::new("a", "1.0.0").candidate(1),
                dependent.candidate(2),
            ]);
            run(
                &site,
                &cat,
                &host("0.1.0"),
                &Request::Uninstall {
                    app: "a".to_owned(),
                    purge: false,
                },
                &Approval::new(),
            )
        }),
        ("installer.patch_removed", || {
            let installed_spec = Spec::new("a", "1.0.0").patch("p1", "metadata", true);
            let site = SiteState {
                apps: vec![installed_from(&installed_spec, 1)],
            };
            let cat = Catalogue::new(vec![
                Spec::new("a", "2.0.0")
                    .patch("p2", "data", true)
                    .candidate(2),
            ]);
            run(
                &site,
                &cat,
                &host("0.1.0"),
                &Request::Upgrade {
                    app: "a".to_owned(),
                    to: None,
                },
                &Approval::new(),
            )
        }),
        ("installer.missing_dependency", || {
            let cat = Catalogue::new(vec![Spec::new("a", "1.0.0").dep("b", "^1").candidate(1)]);
            run(
                &SiteState::new(),
                &cat,
                &host("0.1.0"),
                &Request::Install {
                    app: "a".to_owned(),
                    version_req: req("^1"),
                },
                &Approval::new(),
            )
        }),
        ("installer.conflicting_requirement", || {
            let cat = Catalogue::new(vec![
                Spec::new("a", "1.0.0")
                    .dep("b", "^1")
                    .dep("c", "^1")
                    .candidate(1),
                Spec::new("b", "1.0.0").dep("c", "^2").candidate(2),
                Spec::new("c", "1.5.0").candidate(3),
            ]);
            run(
                &SiteState::new(),
                &cat,
                &host("0.1.0"),
                &Request::Install {
                    app: "a".to_owned(),
                    version_req: req("^1"),
                },
                &Approval::new(),
            )
        }),
        ("installer.no_candidate", || {
            let cat = Catalogue::new(vec![Spec::new("a", "1.0.0").candidate(1)]);
            run(
                &SiteState::new(),
                &cat,
                &host("0.1.0"),
                &Request::Install {
                    app: "a".to_owned(),
                    version_req: req("^9"),
                },
                &Approval::new(),
            )
        }),
        ("installer.breaks_dependent", || {
            let dependent = Spec::new("c", "1.0.0").dep("d", "^1");
            let site = SiteState {
                apps: vec![installed("d", "1.0.0", 1), installed_from(&dependent, 2)],
            };
            let cat = Catalogue::new(vec![
                Spec::new("d", "2.0.0").candidate(2),
                dependent.candidate(2),
            ]);
            run(
                &site,
                &cat,
                &host("0.1.0"),
                &Request::Upgrade {
                    app: "d".to_owned(),
                    to: Some(ver("2.0.0")),
                },
                &Approval::new(),
            )
        }),
        ("installer.dependency_cycle", || {
            let cat = Catalogue::new(vec![
                Spec::new("a", "1.0.0").dep("b", "^1").candidate(1),
                Spec::new("b", "1.0.0").dep("a", "^1").candidate(2),
            ]);
            run(
                &SiteState::new(),
                &cat,
                &host("0.1.0"),
                &Request::Install {
                    app: "a".to_owned(),
                    version_req: req("^1"),
                },
                &Approval::new(),
            )
        }),
    ]
}

#[test]
fn every_refusal_has_its_code() {
    for (expected, case) in refusal_cases() {
        let err = case().expect_err("the case must be refused");
        assert_eq!(err.code, expected, "wrong code for {expected}: {err}");
    }
}

#[test]
fn explain_names_the_upgrade_and_its_patches() {
    let site = SiteState {
        apps: vec![installed("tax_pack", "1.2.0", 1)],
    };
    let catalogue = Catalogue::new(vec![
        Spec::new("tax_pack", "1.3.0")
            .title("Tax Pack")
            .patch("p1", "data", false)
            .patch("p2", "data", false)
            .candidate(2),
    ]);
    let plan = run(
        &site,
        &catalogue,
        &host("0.1.0"),
        &Request::Upgrade {
            app: "tax_pack".to_owned(),
            to: None,
        },
        &Approval::new(),
    )
    .unwrap();
    assert_eq!(
        plan.explain(),
        vec![
            "Upgrades Tax Pack from 1.2.0 to 1.3.0, runs 2 data patches, cannot be undone"
                .to_owned()
        ]
    );
}

#[test]
fn risks_follow_the_patches() {
    let cases = [
        (
            Spec::new("a", "1.0.0").patch("p1", "metadata", true),
            Risk::Low,
            true,
        ),
        (
            Spec::new("a", "1.0.0").patch("p1", "data", true),
            Risk::Medium,
            true,
        ),
        (
            Spec::new("a", "1.0.0").patch("p1", "schema", true),
            Risk::High,
            true,
        ),
        (
            Spec::new("a", "1.0.0").patch("p1", "metadata", false),
            Risk::High,
            false,
        ),
    ];
    for (spec, risk, reversible) in cases {
        let catalogue = Catalogue::new(vec![spec.candidate(1)]);
        let plan = run(
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
        let action = &plan.actions()[0];
        assert_eq!(action.risk, risk);
        assert_eq!(action.reversible, reversible);
    }
}

#[test]
fn installing_dependencies_orders_them_first() {
    let catalogue = Catalogue::new(vec![
        Spec::new("a", "1.0.0")
            .dep("b", "^1")
            .dep("c", "^1")
            .candidate(1),
        Spec::new("b", "1.0.0").dep("d", "^1").candidate(2),
        Spec::new("c", "1.0.0").dep("d", "^1").candidate(3),
        Spec::new("d", "1.0.0").candidate(4),
    ]);
    let plan = run(
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
    let names: Vec<&str> = plan.actions().iter().map(Action::app).collect();
    assert_eq!(names, vec!["d", "b", "c", "a"]);
}

#[test]
fn needs_approval_lists_the_delta_and_never_runs() {
    let spec = Spec::new("a", "1.0.0").network("api.example", 443).email();
    let catalogue = Catalogue::new(vec![spec.candidate(1)]);
    let request = Request::Install {
        app: "a".to_owned(),
        version_req: req("^1"),
    };
    let unapproved = run(
        &SiteState::new(),
        &catalogue,
        &host("0.1.0"),
        &request,
        &Approval::new(),
    )
    .unwrap();
    assert!(!unapproved.state().is_ready());
    match unapproved.state() {
        PlanState::NeedsApproval { requests } => {
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].app, "a");
            assert!(requests[0].delta.email);
        }
        PlanState::Ready => panic!("expected NeedsApproval"),
    }
    assert!(
        unapproved
            .explain()
            .iter()
            .any(|line| line.contains("approval"))
    );

    let approved = Approval::new().grant("a", fully_approved(&spec));
    let ready = run(
        &SiteState::new(),
        &catalogue,
        &host("0.1.0"),
        &request,
        &approved,
    )
    .unwrap();
    assert!(ready.state().is_ready());
}

#[test]
fn the_hash_is_independent_of_catalogue_order() {
    let specs = vec![
        Spec::new("a", "1.0.0").dep("b", "^1").dep("c", "^1"),
        Spec::new("b", "1.0.0").dep("d", "^1"),
        Spec::new("c", "1.0.0").dep("d", "^1"),
        Spec::new("d", "1.0.0"),
        Spec::new("e", "1.0.0"),
    ];
    let request = Request::Install {
        app: "a".to_owned(),
        version_req: req("^1"),
    };
    let baseline = run(
        &SiteState::new(),
        &shuffled_catalogue(&specs, 1),
        &host("0.1.0"),
        &request,
        &Approval::new(),
    )
    .unwrap();
    for seed in 2..102 {
        let plan = run(
            &SiteState::new(),
            &shuffled_catalogue(&specs, seed),
            &host("0.1.0"),
            &request,
            &Approval::new(),
        )
        .unwrap();
        assert_eq!(plan.hash(), baseline.hash(), "seed {seed}");
    }
}

/// Builds a catalogue from `specs` in a deterministic pseudo-random order.
fn shuffled_catalogue(specs: &[Spec], seed: u64) -> Catalogue {
    let mut candidates: Vec<Candidate> = specs
        .iter()
        .enumerate()
        .map(|(i, spec)| spec.candidate(i as u8 + 1))
        .collect();
    let mut state = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    for i in (1..candidates.len()).rev() {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let j = (state >> 33) as usize % (i + 1);
        candidates.swap(i, j);
    }
    Catalogue::new(candidates)
}

#[test]
fn the_golden_hash_is_stable() {
    let site = SiteState {
        apps: vec![installed("base", "1.0.0", 1)],
    };
    let catalogue = Catalogue::new(vec![
        Spec::new("base", "1.0.0").candidate(1),
        Spec::new("tax_pack", "1.3.0")
            .title("Tax Pack")
            .dep("base", "^1")
            .patch("2026_09_a", "data", true)
            .patch("2026_10_b", "schema", false)
            .fixture("Role", "fixtures/roles.json")
            .candidate(2),
    ]);
    let plan = run(
        &site,
        &catalogue,
        &host("0.1.0"),
        &Request::Install {
            app: "tax_pack".to_owned(),
            version_req: req("^1"),
        },
        &Approval::new(),
    )
    .unwrap();
    assert_eq!(plan.actions().len(), 1);
    assert!(matches!(plan.actions()[0].op, Operation::Install { .. }));
    assert_eq!(
        plan.hash(),
        "e815e52e75205e086c7c6deb58d8d6ea119f9e8d0fd09534222d44c0d77e1ea4"
    );
    assert!(plan.verify_hash(plan.hash()));
    assert!(!plan.verify_hash("not the hash"));
}
