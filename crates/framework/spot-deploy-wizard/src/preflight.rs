//! Pre-flight: what must be true of a node before the wizard touches it.
//!
//! [`preflight`] turns one node's [`AgentFacts`] into a list of [`Check`]s, each with a
//! [`Severity`], what was found and the fix in plain words. The plan refuses to start while
//! any check is [`Severity::Blocking`]; warnings are shown and do not stop the deployment.
//!
//! Nothing here reads the machine. The agent reports; this crate judges.
//!
//! # Thresholds
//!
//! The floors are the wizard's own documented defaults, not a hardware specification:
//! 4 GiB of memory and 20 GiB free under the data directory are blocking, 8 GiB and 40 GiB
//! warn; an unsynchronised clock is blocking, more than a minute of skew is blocking and
//! more than five seconds warns. A node must have its role's ports free and reach every
//! other node in the topology.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::error::WizardError;
use crate::facts::{AgentFacts, OsFamily};
use crate::ids::{BRANCH_EDGE_PORT, POSTGRES_PORT, REALTIME_PORT, WEB_PORT};
use crate::topology::{Host, NodeSpec, Role, Topology};

/// Memory below this many mebibytes blocks a deployment.
pub const MIN_RAM_MB_BLOCKING: u32 = 4_096;
/// Memory below this many mebibytes warns.
pub const MIN_RAM_MB_WARNING: u32 = 8_192;
/// Free data-directory space below this many gibibytes blocks a deployment.
pub const MIN_DISK_GB_BLOCKING: u32 = 20;
/// Free data-directory space below this many gibibytes warns.
pub const MIN_DISK_GB_WARNING: u32 = 40;
/// Clock skew above this many seconds blocks a deployment.
pub const MAX_SKEW_SECS_BLOCKING: u32 = 60;
/// Clock skew above this many seconds warns.
pub const MAX_SKEW_SECS_WARNING: u32 = 5;

/// How much a check matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Informational: shown, never stops anything.
    Info,
    /// A problem worth fixing, that does not stop the deployment.
    Warning,
    /// A problem that must be fixed before the plan may start.
    Blocking,
}

impl Severity {
    /// The lower-case wire name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Blocking => "blocking",
        }
    }
}

/// What a check looks at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    /// The agent reported facts for the right host.
    FactsMatch,
    /// The operating system family and release are supported.
    OsSupported,
    /// The data directory has room.
    DiskSpace,
    /// The node has enough memory.
    Memory,
    /// A time-synchronisation service is running.
    TimeSync,
    /// The node's clock does not drift.
    ClockSkew,
    /// A port the node's role needs is free.
    PortFree,
    /// The node can reach a peer.
    PeerReachable,
    /// The agent reported whether the node can reach a peer.
    PeerReported,
    /// The cluster's clocks do not drift apart.
    ClusterClockSkew,
    /// The agent reported facts for every node in the topology.
    FactsPresent,
}

impl CheckKind {
    /// The stable snake-case name the console logs and tests assert on.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::FactsMatch => "facts_match",
            Self::OsSupported => "os_supported",
            Self::DiskSpace => "disk_space",
            Self::Memory => "memory",
            Self::TimeSync => "time_sync",
            Self::ClockSkew => "clock_skew",
            Self::PortFree => "port_free",
            Self::PeerReachable => "peer_reachable",
            Self::PeerReported => "peer_reported",
            Self::ClusterClockSkew => "cluster_clock_skew",
            Self::FactsPresent => "facts_present",
        }
    }
}

/// One finding: the check, its severity, the node, what was found and how to fix it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    /// What was checked.
    pub kind: CheckKind,
    /// How much it matters.
    pub severity: Severity,
    /// The node it is about; `None` for a finding about the topology as a whole.
    pub host: Option<Host>,
    /// What was found.
    pub detail: String,
    /// What to do about it, in plain words.
    pub fix: String,
}

impl Check {
    /// A check of `kind` on `host`, at `severity`.
    #[must_use]
    pub fn new(
        kind: CheckKind,
        severity: Severity,
        host: Option<Host>,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            severity,
            host,
            detail: detail.into(),
            fix: fix.into(),
        }
    }

    /// The one sentence the console shows in the list.
    #[must_use]
    pub fn describe(&self) -> String {
        match &self.host {
            Some(host) => format!("{host}: {} ({})", self.detail, self.fix),
            None => format!("{} ({})", self.detail, self.fix),
        }
    }
}

/// The TCP ports a role needs free on its node.
///
/// The wizard's default table: `web` answers on 443, `database` on 5432, `realtime` on 4000
/// and a `branch edge` on 8443. `jobs` listens on nothing inbound.
#[must_use]
pub fn required_ports(role: Role) -> &'static [u16] {
    match role {
        Role::Web => &[WEB_PORT],
        Role::Jobs => &[],
        Role::Realtime => &[REALTIME_PORT],
        Role::Database => &[POSTGRES_PORT],
        Role::BranchEdge => &[BRANCH_EDGE_PORT],
    }
}

/// Every check one node's facts fail, in check order; an empty list means the node is ready.
#[must_use]
pub fn preflight(node: &NodeSpec, facts: &AgentFacts) -> Vec<Check> {
    let mut checks = Vec::new();
    let host = Some(facts.host.clone());

    if facts.host != node.host {
        checks.push(Check::new(
            CheckKind::FactsMatch,
            Severity::Blocking,
            host.clone(),
            format!(
                "the agent's facts are for {} but the wizard asked about {}",
                facts.host, node.host
            ),
            "Re-read the server's facts from the agent on the server the wizard is deploying to.",
        ));
    }

    if facts.os.supported {
        checks.push(Check::new(
            CheckKind::OsSupported,
            Severity::Info,
            host.clone(),
            format!("{} is supported", facts.os.release),
            "No action needed.",
        ));
    } else {
        checks.push(Check::new(
            CheckKind::OsSupported,
            Severity::Blocking,
            host.clone(),
            format!("{} is not a supported production release", facts.os.release),
            match facts.os.family {
                OsFamily::Linux => {
                    "Install a RHEL-compatible distribution, Ubuntu LTS or Fedora, then add the server again."
                }
                OsFamily::Windows => {
                    "Install Windows Server, or Windows 10/11 for development, then add the server again."
                }
            },
        ));
    }

    if facts.data_dir_free_gb < MIN_DISK_GB_BLOCKING {
        checks.push(Check::new(
            CheckKind::DiskSpace,
            Severity::Blocking,
            host.clone(),
            format!(
                "only {} GiB is free under the data directory (at least {MIN_DISK_GB_BLOCKING} GiB is needed)",
                facts.data_dir_free_gb
            ),
            "Free space on the data volume, or move the data directory to a larger volume, then check again.",
        ));
    } else if facts.data_dir_free_gb < MIN_DISK_GB_WARNING {
        checks.push(Check::new(
            CheckKind::DiskSpace,
            Severity::Warning,
            host.clone(),
            format!(
                "{} GiB is free under the data directory; {MIN_DISK_GB_WARNING} GiB or more is recommended",
                facts.data_dir_free_gb
            ),
            "Free space on the data volume before the database grows.",
        ));
    } else {
        checks.push(Check::new(
            CheckKind::DiskSpace,
            Severity::Info,
            host.clone(),
            format!(
                "{} GiB is free under the data directory",
                facts.data_dir_free_gb
            ),
            "No action needed.",
        ));
    }

    if facts.ram_mb < MIN_RAM_MB_BLOCKING {
        checks.push(Check::new(
            CheckKind::Memory,
            Severity::Blocking,
            host.clone(),
            format!(
                "{} MiB of memory is too little (at least {MIN_RAM_MB_BLOCKING} MiB is needed)",
                facts.ram_mb
            ),
            "Add memory to the server, or move this role to a larger server.",
        ));
    } else if facts.ram_mb < MIN_RAM_MB_WARNING {
        checks.push(Check::new(
            CheckKind::Memory,
            Severity::Warning,
            host.clone(),
            format!(
                "{} MiB of memory is below the recommended {MIN_RAM_MB_WARNING} MiB",
                facts.ram_mb
            ),
            "Add memory if the server will serve web and database traffic together.",
        ));
    }

    if !facts.time.synchronized {
        checks.push(Check::new(
            CheckKind::TimeSync,
            Severity::Blocking,
            host.clone(),
            "no time-synchronisation service is keeping the clock right",
            "Enable and start the time service (chrony on Linux, the Windows Time service), then check again.",
        ));
    }
    if facts.time.skew_secs > MAX_SKEW_SECS_BLOCKING {
        checks.push(Check::new(
            CheckKind::ClockSkew,
            Severity::Blocking,
            host.clone(),
            format!(
                "the clock is {} seconds away from the console's clock",
                facts.time.skew_secs
            ),
            "Synchronise the clock before deploying; signed commands and replication both refuse large skew.",
        ));
    } else if facts.time.skew_secs > MAX_SKEW_SECS_WARNING {
        checks.push(Check::new(
            CheckKind::ClockSkew,
            Severity::Warning,
            host.clone(),
            format!(
                "the clock is {} seconds away from the console's clock",
                facts.time.skew_secs
            ),
            "Check the time service; a few seconds of drift is tolerated but not healthy.",
        ));
    }

    let bound: BTreeSet<u16> = facts.listening_ports.iter().copied().collect();
    let needed: BTreeSet<u16> = node
        .roles
        .iter()
        .flat_map(|role| required_ports(*role).iter().copied())
        .collect();
    for port in needed {
        if bound.contains(&port) {
            checks.push(Check::new(
                CheckKind::PortFree,
                Severity::Blocking,
                host.clone(),
                format!("port {port} is already in use"),
                format!(
                    "Stop the program listening on {port}, or give this role a different port before deploying."
                ),
            ));
        } else {
            checks.push(Check::new(
                CheckKind::PortFree,
                Severity::Info,
                host.clone(),
                format!("port {port} is free"),
                "No action needed.",
            ));
        }
    }

    for peer in &facts.peers {
        if peer.reachable {
            checks.push(Check::new(
                CheckKind::PeerReachable,
                Severity::Info,
                host.clone(),
                format!("{} is reachable", peer.host),
                "No action needed.",
            ));
        } else {
            checks.push(Check::new(
                CheckKind::PeerReachable,
                Severity::Blocking,
                host.clone(),
                format!("{} cannot be reached", peer.host),
                "Open the management network between the servers, then check again.",
            ));
        }
    }

    checks
}

/// Every check the whole topology fails, gathered from one facts snapshot per node.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preflight {
    /// The checks, in node order and then check order.
    pub checks: Vec<Check>,
}

impl Preflight {
    /// Runs pre-flight for every node of `topology` against `facts`.
    ///
    /// A node with no facts, an unreported peer and a clock the cluster disagrees on are
    /// findings too: the wizard refuses to guess about a server it could not ask.
    #[must_use]
    pub fn for_topology(topology: &Topology, facts: &[AgentFacts]) -> Self {
        let mut checks = Vec::new();
        for node in &topology.nodes {
            match facts.iter().find(|entry| entry.host == node.host) {
                Some(reported) => checks.extend(preflight(node, reported)),
                None => checks.push(Check::new(
                    CheckKind::FactsPresent,
                    Severity::Blocking,
                    Some(node.host.clone()),
                    "the agent did not report this server's facts",
                    "Make sure the agent is enrolled and connected, then check again.",
                )),
            }
        }
        for node in &topology.nodes {
            let Some(reported) = facts.iter().find(|entry| entry.host == node.host) else {
                continue;
            };
            for peer in &topology.nodes {
                if peer.host == node.host {
                    continue;
                }
                if reported.reaches(&peer.host).is_none() {
                    checks.push(Check::new(
                        CheckKind::PeerReported,
                        Severity::Blocking,
                        Some(node.host.clone()),
                        format!(
                            "the agent did not report whether {} is reachable",
                            peer.host
                        ),
                        "Update the agent so it reports connectivity, then check again.",
                    ));
                }
            }
        }
        let skews: Vec<u32> = facts.iter().map(|entry| entry.time.skew_secs).collect();
        if let (Some(least), Some(most)) = (skews.iter().min(), skews.iter().max()) {
            let spread = most - least;
            if spread > MAX_SKEW_SECS_BLOCKING {
                checks.push(Check::new(
                    CheckKind::ClusterClockSkew,
                    Severity::Blocking,
                    None,
                    format!("the servers' clocks differ by {spread} seconds"),
                    "Synchronise every server to the same time source before deploying.",
                ));
            } else if spread > MAX_SKEW_SECS_WARNING {
                checks.push(Check::new(
                    CheckKind::ClusterClockSkew,
                    Severity::Warning,
                    None,
                    format!("the servers' clocks differ by {spread} seconds"),
                    "Check the time service on every server; replication prefers clocks within a second.",
                ));
            }
        }
        Self { checks }
    }

    /// The blocking checks, in order.
    pub fn blockers(&self) -> impl Iterator<Item = &Check> {
        self.checks
            .iter()
            .filter(|check| check.severity == Severity::Blocking)
    }

    /// The warnings, in order.
    pub fn warnings(&self) -> impl Iterator<Item = &Check> {
        self.checks
            .iter()
            .filter(|check| check.severity == Severity::Warning)
    }

    /// How many checks block the deployment.
    #[must_use]
    pub fn blocking_count(&self) -> usize {
        self.blockers().count()
    }

    /// Whether the plan may start.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.blocking_count() == 0
    }

    /// Refuses to go on while a blocking check fails.
    ///
    /// # Errors
    /// [`WizardError::PreflightBlocked`] naming the first blocking check and how many
    /// others there are.
    pub fn ensure_ready(&self) -> Result<(), WizardError> {
        let blocking: Vec<&Check> = self.blockers().collect();
        match blocking.split_first() {
            None => Ok(()),
            Some((first, rest)) => Err(WizardError::PreflightBlocked {
                count: blocking.len(),
                first: format!("{} ({} more)", first.describe(), rest.len()),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::facts::{OsFacts, PeerFacts};

    fn host(text: &str) -> Host {
        Host::try_new(text).unwrap()
    }

    fn linux(os: &str) -> OsFacts {
        OsFacts {
            family: OsFamily::Linux,
            release: os.to_owned(),
            supported: true,
        }
    }

    fn node(host: &str, roles: &[Role]) -> NodeSpec {
        NodeSpec::new(host, roles).unwrap()
    }

    fn server(host_text: &str, roles: &[Role]) -> (NodeSpec, AgentFacts) {
        let node = node(host_text, roles);
        let facts = AgentFacts::new(node.host.clone(), linux("Ubuntu 24.04 LTS"));
        (node, facts)
    }

    fn kinds(checks: &[Check]) -> Vec<CheckKind> {
        checks.iter().map(|check| check.kind).collect()
    }

    #[test]
    fn a_healthy_database_node_passes() {
        let (node, facts) = server("db1.example.com", &[Role::Database]);
        let checks = preflight(&node, &facts);
        assert!(
            checks
                .iter()
                .all(|check| check.severity != Severity::Blocking)
        );
        assert!(kinds(&checks).contains(&CheckKind::PortFree));
        assert!(kinds(&checks).contains(&CheckKind::OsSupported));
    }

    #[test]
    fn an_unsupported_os_blocks() {
        let (node, facts) = server("db1.example.com", &[Role::Database]);
        let facts = AgentFacts {
            os: OsFacts {
                family: OsFamily::Linux,
                release: "CentOS 6".to_owned(),
                supported: false,
            },
            ..facts
        };
        let checks = preflight(&node, &facts);
        assert!(checks.iter().any(|check| {
            check.kind == CheckKind::OsSupported && check.severity == Severity::Blocking
        }));
    }

    #[test]
    fn small_disk_and_memory_block_and_low_numbers_warn() {
        let (node, facts) = server("db1.example.com", &[Role::Database]);
        let small = preflight(
            &node,
            &facts.clone().with_data_dir_free_gb(5).with_ram_mb(2_048),
        );
        assert!(small.iter().any(
            |check| check.kind == CheckKind::DiskSpace && check.severity == Severity::Blocking
        ));
        assert!(
            small.iter().any(
                |check| check.kind == CheckKind::Memory && check.severity == Severity::Blocking
            )
        );

        let low = preflight(&node, &facts.with_data_dir_free_gb(30).with_ram_mb(6_000));
        assert!(
            low.iter()
                .any(|check| check.kind == CheckKind::DiskSpace
                    && check.severity == Severity::Warning)
        );
        assert!(low.iter().any(|check| check.kind == CheckKind::Memory
            && check.severity == Severity::Warning));
    }

    #[test]
    fn time_problems_block_and_skew_warns() {
        let (node, facts) = server("db1.example.com", &[Role::Database]);
        let unsynchronised = preflight(&node, &facts.clone().with_time(false, 0));
        assert!(
            unsynchronised
                .iter()
                .any(|check| check.kind == CheckKind::TimeSync
                    && check.severity == Severity::Blocking)
        );

        let skewed = preflight(&node, &facts.clone().with_time(true, 90));
        assert!(skewed.iter().any(
            |check| check.kind == CheckKind::ClockSkew && check.severity == Severity::Blocking
        ));

        let drifting = preflight(&node, &facts.with_time(true, 20));
        assert!(
            drifting
                .iter()
                .any(|check| check.kind == CheckKind::ClockSkew
                    && check.severity == Severity::Warning)
        );
    }

    #[test]
    fn a_bound_port_blocks_only_the_roles_that_need_it() {
        let (web, facts) = server("app1.example.com", &[Role::Web]);
        let blocked = preflight(&web, &facts.clone().with_listening_ports(&[WEB_PORT]));
        assert!(
            blocked
                .iter()
                .any(|check| check.kind == CheckKind::PortFree
                    && check.severity == Severity::Blocking)
        );

        let (_, facts) = server("app1.example.com", &[Role::Jobs]);
        assert!(
            preflight(
                &node("app1.example.com", &[Role::Jobs]),
                &facts.with_listening_ports(&[POSTGRES_PORT])
            )
            .iter()
            .all(|check| check.kind != CheckKind::PortFree)
        );
    }

    #[test]
    fn an_unreachable_peer_blocks() {
        let (node, facts) = server("app1.example.com", &[Role::Web]);
        let facts = facts.with_peers(vec![PeerFacts {
            host: host("db1.example.com"),
            reachable: false,
        }]);
        let checks = preflight(&node, &facts);
        assert!(
            checks
                .iter()
                .any(|check| check.kind == CheckKind::PeerReachable
                    && check.severity == Severity::Blocking)
        );
    }

    #[test]
    fn facts_for_another_host_block() {
        let (node, _) = server("app1.example.com", &[Role::Web]);
        let facts = AgentFacts::new(host("app9.example.com"), linux("Ubuntu 24.04 LTS"));
        let checks = preflight(&node, &facts);
        assert!(checks.iter().any(
            |check| check.kind == CheckKind::FactsMatch && check.severity == Severity::Blocking
        ));
    }

    fn reference_facts(topology: &Topology) -> Vec<AgentFacts> {
        topology
            .nodes
            .iter()
            .map(|node| {
                let peers = topology
                    .nodes
                    .iter()
                    .filter(|peer| peer.host != node.host)
                    .map(|peer| PeerFacts {
                        host: peer.host.clone(),
                        reachable: true,
                    })
                    .collect();
                AgentFacts::new(node.host.clone(), linux("Ubuntu 24.04 LTS")).with_peers(peers)
            })
            .collect()
    }

    fn reference() -> Topology {
        Topology::new(
            vec![
                node("app1.example.com", &[Role::Web, Role::Jobs, Role::Realtime]),
                node("db1.example.com", &[Role::Database]),
                node("db2.example.com", &[Role::Database]),
            ],
            crate::topology::Profile::ClusterBasic,
        )
        .with_web_address("erp.example.com")
    }

    #[test]
    fn a_healthy_cluster_is_ready() {
        let topology = reference();
        let report = Preflight::for_topology(&topology, &reference_facts(&topology));
        assert!(
            report.is_ready(),
            "{:?}",
            report.blockers().collect::<Vec<_>>()
        );
        assert!(report.ensure_ready().is_ok());
    }

    #[test]
    fn a_missing_facts_snapshot_blocks() {
        let topology = reference();
        let mut facts = reference_facts(&topology);
        facts.remove(1);
        let report = Preflight::for_topology(&topology, &facts);
        assert!(!report.is_ready());
        assert!(
            report
                .blockers()
                .any(|check| check.kind == CheckKind::FactsPresent)
        );
    }

    #[test]
    fn an_unreported_peer_blocks() {
        let topology = reference();
        let mut facts = reference_facts(&topology);
        facts[0]
            .peers
            .retain(|peer| peer.host.as_str() != "db2.example.com");
        let report = Preflight::for_topology(&topology, &facts);
        assert!(
            report
                .blockers()
                .any(|check| check.kind == CheckKind::PeerReported)
        );
    }

    #[test]
    fn a_cluster_clock_spread_blocks() {
        let topology = reference();
        let mut facts = reference_facts(&topology);
        facts[0].time.skew_secs = 0;
        facts[1].time.skew_secs = 120;
        facts[2].time.skew_secs = 0;
        let report = Preflight::for_topology(&topology, &facts);
        assert!(
            report
                .blockers()
                .any(|check| check.kind == CheckKind::ClusterClockSkew)
        );
        let error = report.ensure_ready().unwrap_err();
        assert_eq!(error.code(), crate::error::PREFLIGHT_BLOCKED);
    }

    #[test]
    fn every_supported_role_has_a_port_except_jobs() {
        assert!(required_ports(Role::Web).contains(&443));
        assert!(required_ports(Role::Database).contains(&5432));
        assert!(required_ports(Role::Realtime).contains(&4000));
        assert!(required_ports(Role::BranchEdge).contains(&8443));
        assert!(required_ports(Role::Jobs).is_empty());
    }
}
