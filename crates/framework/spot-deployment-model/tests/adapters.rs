//! The adapters read each authoritative crate into facts, and the facts compose into one model.

#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use semver::Version;
use spot_app_api::{
    AppResult, BoxFuture, Controller, HookContext, HookPoint, OverrideKind, Phase, StepDecl,
    StepName, StepOverride, Target,
};
use spot_bundle::Digest;
use spot_deployment_model::adapters::{
    AgentFacts, FleetFacts, InstallerFacts, RuntimeFacts, SeamsFacts, SettingsFacts,
};
use spot_deployment_model::{
    AppState, CacheBackend, Coordinator, FactSource, FactSourceKind, Facts, MemoryFacts, NodeRole,
    Placement, ReplicaRole, TopologyMode, refresh, verify,
};
use spot_fleet_health::{
    Finding, FleetHealth, Health, NodeId, RuleId, Signal, Subject, SubjectHealth, Timestamp,
};
use spot_installer::{InstalledApp, SiteState};
use spot_seams::RegistryBuilder;
use spot_settings::{EffectiveValue, Origin, ParamValue, Resolved};

fn digest() -> Digest {
    "0".repeat(64).parse().expect("a zero digest parses")
}

fn installed(name: &str, version: Version) -> InstalledApp {
    InstalledApp::new(name, version, digest())
}

fn resolved(value: ParamValue) -> Resolved {
    Resolved {
        value: EffectiveValue::Value(value),
        origin: Origin::Default,
        chain: Vec::new(),
    }
}

fn node(id: &str) -> NodeId {
    NodeId::try_new(id).expect("the node id is valid")
}

fn node_health(id: &str, healthy: bool) -> FleetHealth {
    let subject = Subject::Node(node(id));
    let findings = if healthy {
        Vec::new()
    } else {
        vec![Finding {
            subject: subject.clone(),
            rule_id: RuleId::try_new("health_probe_failing").expect("the rule id is valid"),
            signal: Some(Signal::HealthProbe),
            health: Health::Red,
            value: Some(0),
            since: Timestamp::from_secs(1_700_000_000),
            observed_at: Timestamp::from_secs(1_700_000_060),
            sentence: "the node did not answer".to_owned(),
            action: "check the node".to_owned(),
        }]
    };
    FleetHealth {
        generated_at: Timestamp::from_secs(1_700_000_060),
        subjects: vec![SubjectHealth {
            subject,
            health: if healthy { Health::Green } else { Health::Red },
            sentence: "the node answered".to_owned(),
            action: None,
            findings,
            last_observed_at: Some(Timestamp::from_secs(1_700_000_000)),
            stale: false,
            no_data: false,
        }],
    }
}

#[test]
fn the_installer_adapter_reads_apps_and_can_disable_one() {
    let site = SiteState {
        apps: vec![
            installed("erp_selling", Version::new(1, 2, 0)),
            installed("erp_buying", Version::new(3, 0, 1)),
        ],
    };
    let facts = InstallerFacts::new(4, &site)
        .expect("the site is readable")
        .with_runtime_version("0.9.0")
        .with_state("erp_buying", AppState::Disabled)
        .expect("the app is installed");
    assert_eq!(facts.source(), FactSourceKind::Installer);
    assert_eq!(facts.epoch(), 4);
    let facts = facts.facts().expect("the facts are readable");
    let apps = facts.apps.expect("the adapter reads apps");
    assert_eq!(apps["erp_selling"].version, "1.2.0");
    assert_eq!(apps["erp_selling"].state, AppState::Installed);
    assert_eq!(apps["erp_buying"].state, AppState::Disabled);
    assert_eq!(facts.runtime_version.as_deref(), Some("0.9.0"));
}

#[test]
fn a_duplicate_app_in_the_site_is_refused() {
    let site = SiteState {
        apps: vec![
            installed("erp_selling", Version::new(1, 0, 0)),
            installed("erp_selling", Version::new(1, 1, 0)),
        ],
    };
    assert!(InstallerFacts::new(1, &site).is_err());
}

#[test]
fn the_settings_adapter_maps_the_profile_and_refuses_a_secret() {
    let redis = SettingsFacts::new(2)
        .with_profile(&resolved(ParamValue::Text("cluster-redis".to_owned())))
        .expect("the profile is text");
    let facts = redis.facts().expect("the facts are readable");
    let coordination = facts.coordination.expect("the profile shapes coordination");
    assert_eq!(coordination.cache, CacheBackend::Redis);
    assert_eq!(coordination.coordinator, Coordinator::Redis);

    let policies = SettingsFacts::new(3)
        .with_profile(&resolved(ParamValue::Text("standalone".to_owned())))
        .expect("the profile is text")
        .with_replay(&resolved(ParamValue::Text("always".to_owned())))
        .expect("the replay policy is text");
    let policies = policies.facts().expect("the facts are readable").policies;
    assert_eq!(
        policies.expect("the policies are read").replay,
        spot_deployment_model::ReplayPolicy::Always
    );

    // A secret setting is redacted, never mapped into a deployment fact.
    let secret = Resolved {
        value: EffectiveValue::Redacted,
        origin: Origin::Default,
        chain: Vec::new(),
    };
    assert!(SettingsFacts::new(4).with_profile(&secret).is_err());
}

#[test]
fn the_runtime_adapter_maps_profile_roles_and_replicas() {
    let facts = RuntimeFacts::new(
        5,
        "ln-2",
        spot_agent_runtime::Profile::ClusterRedis,
        [
            spot_agent_runtime::NodeRole::Web,
            spot_agent_runtime::NodeRole::BranchEdge,
        ],
    )
    .expect("the roles are valid")
    .with_backend(
        "16.4",
        [
            (
                "ln-1".to_owned(),
                spot_agent_runtime::ReplicaStatus {
                    role: spot_agent_runtime::ReplicaRole::Primary,
                    streaming: false,
                    primary_host: None,
                    receive_lsn: None,
                    replay_lsn: None,
                },
            ),
            (
                "ln-2".to_owned(),
                spot_agent_runtime::ReplicaStatus {
                    role: spot_agent_runtime::ReplicaRole::Standby,
                    streaming: true,
                    primary_host: Some("ln-1".to_owned()),
                    receive_lsn: Some("0/16B6C50".to_owned()),
                    replay_lsn: Some("0/16B6C50".to_owned()),
                },
            ),
        ],
    )
    .expect("the replicas are valid")
    .with_runtime_version("0.1.0");

    assert_eq!(facts.node(), "ln-2");
    let facts = facts.facts().expect("the facts are readable");
    let topology = facts.topology.expect("the runtime knows the topology");
    assert_eq!(topology.mode, TopologyMode::Cluster);
    assert_eq!(topology.placement, Placement::BranchEdge);
    assert_eq!(
        topology.nodes["ln-2"]
            .roles
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![NodeRole::Web, NodeRole::BranchEdge]
    );
    let backend = facts.backend.expect("the runtime knows the backend");
    assert_eq!(backend.version, "16.4");
    assert_eq!(backend.replicas.len(), 2);
    assert_eq!(backend.replicas[0].role, ReplicaRole::Primary);
    assert_eq!(backend.replicas[1].role, ReplicaRole::Standby);
    assert_eq!(backend.replicas[0].node, "ln-1");
}

#[test]
fn the_fleet_adapter_reads_reachability_from_a_node_finding() {
    let down = FleetFacts::from_health(6, &node_health("ln-1", false), &node("ln-1"));
    let facts = down.facts().expect("the facts are readable");
    let connectivity = facts.connectivity.expect("fleet health knows connectivity");
    assert!(!connectivity.hub_reachable);
    assert_eq!(connectivity.last_sync_at, Some(1_700_000_000_000_000));

    let up = FleetFacts::from_health(7, &node_health("ln-1", true), &node("ln-1"));
    let facts = up.facts().expect("the facts are readable");
    assert!(facts.connectivity.expect("connectivity").hub_reachable);

    // A node the view does not know is not reachable, by construction.
    let unknown = FleetFacts::from_health(8, &node_health("ln-1", true), &node("ln-9"));
    let facts = unknown.facts().expect("the facts are readable");
    assert!(!facts.connectivity.expect("connectivity").hub_reachable);
}

struct Passthrough {
    doctype: String,
    steps: Vec<StepDecl>,
}

impl Controller for Passthrough {
    fn doctype(&self) -> &str {
        &self.doctype
    }

    fn steps(&self) -> Vec<StepDecl> {
        self.steps.clone()
    }

    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        _ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

fn step(name: &str) -> StepDecl {
    StepDecl {
        name: StepName::new(name).expect("the step name is valid"),
        point: HookPoint::of(Phase::Before, Target::Save),
        priority: 10,
        budget: Duration::from_millis(100),
    }
}

#[test]
fn the_seam_adapter_reads_the_applied_overrides() {
    let mut builder = RegistryBuilder::new();
    builder.add_controller(
        "erp_selling",
        Arc::new(Passthrough {
            doctype: "Sales Order".to_owned(),
            steps: vec![step("erp_selling:check_credit")],
        }),
    );
    builder.add_controller(
        "erp_buying",
        Arc::new(Passthrough {
            doctype: "Sales Order".to_owned(),
            steps: vec![step("erp_buying:check_margin")],
        }),
    );
    builder.add_override(
        "erp_buying",
        StepOverride {
            target: StepName::new("erp_selling:check_credit").expect("the step name is valid"),
            kind: OverrideKind::Before,
            by: Some(StepName::new("erp_buying:check_margin").expect("the step name is valid")),
            reason: "check the margin first".to_owned(),
        },
    );
    let registry = builder.build().expect("the registry is consistent");

    let facts = SeamsFacts::new(9, &registry)
        .expect("the registry is readable")
        .facts()
        .expect("the facts are readable");
    let overrides = facts.overrides.expect("the registry has overrides");
    assert_eq!(overrides.len(), 1);
    assert_eq!(overrides[0].app, "erp_buying");
    assert_eq!(overrides[0].target, "erp_selling:check_credit");
    assert_eq!(overrides[0].by.as_deref(), Some("erp_buying:check_margin"));
}

#[test]
fn the_agent_adapter_reports_the_ceiling() {
    let facts = AgentFacts::from_tier(10, spot_agent_tiers::Tier::T2)
        .facts()
        .expect("the facts are readable");
    assert_eq!(facts.agent_tier, Some(spot_agent_tiers::Tier::T2));
}

#[test]
fn the_adapters_compose_into_one_model_and_verify_against_it() {
    let site = SiteState {
        apps: vec![installed("erp_selling", Version::new(1, 0, 0))],
    };
    let installer = InstallerFacts::new(1, &site)
        .expect("the site is readable")
        .with_runtime_version("0.1.0");
    let settings = SettingsFacts::new(2)
        .with_profile(&resolved(ParamValue::Text("standalone".to_owned())))
        .expect("the profile is text")
        .with_policies(spot_deployment_model::SitePolicies::default());
    let runtime = RuntimeFacts::new(
        3,
        "ln-1",
        spot_agent_runtime::Profile::Standalone,
        [
            spot_agent_runtime::NodeRole::Web,
            spot_agent_runtime::NodeRole::Database,
        ],
    )
    .expect("the roles are valid")
    .with_backend(
        "16.4",
        [(
            "ln-1".to_owned(),
            spot_agent_runtime::ReplicaStatus {
                role: spot_agent_runtime::ReplicaRole::Primary,
                streaming: false,
                primary_host: None,
                receive_lsn: None,
                replay_lsn: None,
            },
        )],
    )
    .expect("the replica is valid");
    let fleet = FleetFacts::from_health(4, &node_health("ln-1", true), &node("ln-1"));
    let agent = AgentFacts::from_tier(5, spot_agent_tiers::Tier::T0);
    let memory = MemoryFacts::with_facts(
        6,
        Facts {
            edition: Some(spot_deployment_model::Edition::new("business").expect("the edition")),
            platform: Some(
                spot_deployment_model::Platform::new(
                    spot_deployment_model::OsFamily::Linux,
                    "Ubuntu 24.04",
                )
                .expect("the platform"),
            ),
            sync: Some(spot_deployment_model::SyncProfile::standalone()),
            overrides: Some(Vec::new()),
            migrations: Some(Vec::new()),
            ..Facts::new()
        },
    );

    let model = refresh(
        None,
        &[&installer, &settings, &runtime, &fleet, &agent, &memory],
        1_000,
    )
    .expect("the sources compose");
    assert_eq!(model.edition.as_str(), "business");
    assert_eq!(model.runtime_version, "0.1.0");
    assert!(model.app("erp_selling").is_some());
    assert!(model.connectivity.hub_reachable);
    assert_eq!(model.agent_tier, spot_agent_tiers::Tier::T0);
    assert_eq!(model.sources.len(), 6, "every source's version is recorded");

    // Every adapter's own slice agrees with the model it helped build.
    for source in [
        &installer as &dyn FactSource,
        &settings as &dyn FactSource,
        &runtime as &dyn FactSource,
        &fleet as &dyn FactSource,
        &agent as &dyn FactSource,
        &memory as &dyn FactSource,
    ] {
        let facts = source.facts().expect("the source is readable");
        assert!(
            verify(&model, &facts).is_empty(),
            "{} disagrees: {:?}",
            source.source(),
            verify(&model, &facts)
        );
    }
}

#[test]
fn a_source_that_disagrees_with_another_is_a_typed_conflict() {
    let one = MemoryFacts::with_facts(
        1,
        Facts {
            runtime_version: Some("0.1.0".to_owned()),
            ..Facts::new()
        },
    );
    let two = MemoryFacts::with_facts(
        2,
        Facts {
            runtime_version: Some("0.2.0".to_owned()),
            ..Facts::new()
        },
    );
    let error = refresh(None, &[&one, &two], 1_000).expect_err("the sources disagree");
    assert_eq!(error.code(), "deployment_model.facts_conflict");
}

#[test]
fn an_incomplete_source_set_names_the_missing_fact() {
    let source = MemoryFacts::with_facts(1, Facts::new());
    let error = refresh(None, &[&source], 1_000).expect_err("nothing is known");
    assert_eq!(error.code(), "deployment_model.facts_incomplete");
}
