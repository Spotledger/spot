#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

fn node(host: &str, roles: &[Role]) -> NodeSpec {
    NodeSpec::new(host, roles).unwrap()
}

fn reference() -> Topology {
    Topology::new(
        vec![
            node("app1.example.com", &[Role::Web, Role::Jobs, Role::Realtime]),
            node("db1.example.com", &[Role::Database]),
            node("db2.example.com", &[Role::Database]),
        ],
        Profile::ClusterBasic,
    )
    .with_web_address("erp.example.com")
}

fn rules(topology: &Topology) -> Vec<Rule> {
    topology.validate().into_iter().map(|v| v.rule).collect()
}

#[test]
fn the_reference_topology_is_valid() {
    assert!(
        reference().validate().is_empty(),
        "{:?}",
        reference().validate()
    );
    assert!(reference().ensure_valid().is_ok());
}

#[test]
fn role_and_profile_names_match_the_agent_runtime() {
    for role in Role::ALL {
        assert_eq!(role.name(), role.to_runtime().name());
        assert_eq!(
            spot_agent_runtime::NodeRole::from_name(role.name()),
            Some(role.to_runtime())
        );
    }
    for profile in Profile::ALL {
        assert_eq!(profile.name(), profile.to_runtime().name());
        assert_eq!(
            spot_agent_runtime::Profile::from_name(profile.name()),
            Some(profile.to_runtime())
        );
    }
}

#[test]
fn hosts_are_validated_and_lower_cased() {
    assert_eq!(
        Host::try_new("APP1.Example.com").unwrap().as_str(),
        "app1.example.com"
    );
    assert!(Host::try_new("").is_err());
    assert!(Host::try_new("app one").is_err());
    assert!(Host::try_new("-app.example.com").is_err());
    assert!(Host::try_new("app.example.com.").is_err());
    assert!(Host::try_new("app/one").is_err());
    assert_eq!(
        serde_json::to_string(&Host::try_new("db1").unwrap()).unwrap(),
        "\"db1\""
    );
    assert!(serde_json::from_str::<Host>("\"bad host\"").is_err());
    assert_eq!(
        serde_json::from_str::<Host>("\"DB1\"").unwrap().as_str(),
        "db1"
    );
}

#[test]
fn an_empty_topology_is_refused_once() {
    let topology = Topology::new(Vec::new(), Profile::ClusterBasic);
    assert_eq!(rules(&topology), [Rule::EmptyTopology]);
}

#[test]
fn missing_roles_are_all_reported_together() {
    let topology = Topology::new(
        vec![node("app1.example.com", &[Role::Web])],
        Profile::ClusterBasic,
    );
    let rules = rules(&topology);
    assert!(!rules.contains(&Rule::DuplicateHost));
    assert!(rules.contains(&Rule::NoDatabase));
    assert!(rules.contains(&Rule::NoJobs));
    assert!(rules.contains(&Rule::NoRealtime));
    assert!(rules.contains(&Rule::WebAddressMissing));
    assert!(rules.contains(&Rule::ClusterTooSmall));
    assert_eq!(rules.len(), 5, "{rules:?}");
}

#[test]
fn a_branch_edge_must_stand_alone() {
    let topology = Topology::new(
        vec![
            node("app1.example.com", &[Role::Web, Role::Jobs, Role::Realtime]),
            node("db1.example.com", &[Role::Database, Role::BranchEdge]),
        ],
        Profile::ClusterBasic,
    )
    .with_web_address("erp.example.com");
    let violations = topology.validate();
    assert_eq!(violations[0].rule, Rule::BranchEdgeAlone);
    assert!(violations[0].fix.contains("own server"));
}

#[test]
fn at_most_one_primary_and_it_must_be_a_database() {
    let topology = Topology::new(
        vec![
            node("app1.example.com", &[Role::Web, Role::Jobs, Role::Realtime]).as_primary(),
            node("db1.example.com", &[Role::Database]),
        ],
        Profile::ClusterBasic,
    )
    .with_web_address("erp.example.com");
    assert!(rules(&topology).contains(&Rule::PrimaryNotDatabase));

    let two = reference();
    let mut nodes = two.nodes.clone();
    nodes[1].primary = true;
    nodes[2].primary = true;
    let both = Topology::new(nodes, two.profile);
    assert!(rules(&both).contains(&Rule::MultiplePrimary));
}

#[test]
fn several_databases_pick_the_first_in_host_order_until_one_is_marked() {
    let topology = Topology::new(
        vec![
            node("app1.example.com", &[Role::Web, Role::Jobs, Role::Realtime]),
            node("db1.example.com", &[Role::Database]),
            node("db2.example.com", &[Role::Database]),
        ],
        Profile::ClusterBasic,
    )
    .with_web_address("erp.example.com");
    assert!(topology.validate().is_empty());
    assert_eq!(
        topology.primary().map(|node| node.host.as_str()),
        Some("db1.example.com")
    );
    assert_eq!(topology.replicas().len(), 1);
    assert_eq!(topology.replicas()[0].host.as_str(), "db2.example.com");

    let mut marked = topology;
    marked.nodes[2].primary = true;
    assert!(marked.validate().is_empty());
    assert_eq!(
        marked.primary().map(|node| node.host.as_str()),
        Some("db2.example.com")
    );
    assert_eq!(
        marked
            .replicas()
            .iter()
            .map(|node| node.host.as_str())
            .collect::<Vec<_>>(),
        ["db1.example.com"]
    );
}

#[test]
fn one_database_node_is_the_primary_without_being_marked() {
    let topology = Topology::new(
        vec![
            node("app1.example.com", &[Role::Web, Role::Jobs, Role::Realtime]),
            node("db1.example.com", &[Role::Database]),
        ],
        Profile::ClusterBasic,
    )
    .with_web_address("erp.example.com:8443");
    assert!(topology.validate().is_empty());
    assert_eq!(
        topology.primary().map(|node| node.host.as_str()),
        Some("db1.example.com")
    );
    assert!(topology.replicas().is_empty());
}

#[test]
fn web_addresses_are_checked() {
    let base = || {
        vec![
            node("app1.example.com", &[Role::Web, Role::Jobs, Role::Realtime]),
            node("db1.example.com", &[Role::Database]),
        ]
    };
    let cluster = Profile::ClusterBasic;
    for bad in [
        "https://erp.example.com",
        "erp.example.com/app",
        "erp example.com",
        "erp.example.com:0",
        "erp.example.com:port",
    ] {
        let topology = Topology::new(base(), cluster).with_web_address(bad);
        assert!(
            rules(&topology).contains(&Rule::WebAddressInvalid),
            "{bad} is refused"
        );
    }
    let no_web = Topology::new(
        vec![
            node("jobs1.example.com", &[Role::Jobs, Role::Realtime]),
            node("db1.example.com", &[Role::Database]),
        ],
        cluster,
    )
    .with_web_address("erp.example.com");
    assert!(rules(&no_web).contains(&Rule::NoWeb));
    assert!(rules(&no_web).contains(&Rule::WebAddressWithoutWeb));
}

#[test]
fn standalone_is_one_node_running_everything() {
    let one = Topology::new(
        vec![node(
            "all1.example.com",
            &[Role::Web, Role::Jobs, Role::Realtime, Role::Database],
        )],
        Profile::Standalone,
    )
    .with_web_address("erp.example.com");
    assert!(one.validate().is_empty());

    let partial = Topology::new(
        vec![node("all1.example.com", &[Role::Web, Role::Database])],
        Profile::Standalone,
    )
    .with_web_address("erp.example.com");
    let partial_rules = rules(&partial);
    assert!(partial_rules.contains(&Rule::NoJobs));
    assert!(partial_rules.contains(&Rule::NoRealtime));
    assert!(partial_rules.contains(&Rule::StandaloneShape));

    let several =
        Topology::new(reference().nodes, Profile::Standalone).with_web_address("erp.example.com");
    assert!(rules(&several).contains(&Rule::StandaloneShape));
}

#[test]
fn duplicate_hosts_are_refused() {
    let topology = Topology::new(
        vec![
            node("app1.example.com", &[Role::Web]),
            node("APP1.example.com", &[Role::Jobs, Role::Realtime]),
            node("db1.example.com", &[Role::Database]),
        ],
        Profile::ClusterBasic,
    )
    .with_web_address("erp.example.com");
    let violations = topology.validate();
    assert!(violations.iter().any(|v| v.rule == Rule::DuplicateHost));
}

#[test]
fn validation_is_serde_round_trippable_for_the_console() {
    let topology = reference();
    let json = serde_json::to_string(&topology).unwrap();
    assert_eq!(serde_json::from_str::<Topology>(&json).unwrap(), topology);
}
