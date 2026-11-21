//! What the host agent reports before the wizard plans anything: the facts pre-flight
//! checks are computed from.
//!
//! The facts are a plain snapshot the console collects from each node's agent. They are
//! **inputs**, never ambient reads: no check here touches the machine, the network or a
//! clock, so the same facts produce the same checks in a test and on a live server
//! (the project coding rules).

use serde::{Deserialize, Serialize};

use crate::topology::Host;

/// The operating system family a node runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OsFamily {
    /// A Linux distribution (RHEL-compatible, Ubuntu LTS or Fedora).
    Linux,
    /// Windows Server or Windows 10/11.
    Windows,
}

impl OsFamily {
    /// The lower-case wire name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Windows => "windows",
        }
    }
}

/// What the agent reports about the operating system.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsFacts {
    /// The installed family.
    pub family: OsFamily,
    /// A human release name, for example `Ubuntu 24.04 LTS`.
    pub release: String,
    /// Whether the family and release are supported for production.
    pub supported: bool,
}

/// What the agent reports about time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeFacts {
    /// Whether a time-synchronisation service is running and in sync.
    pub synchronized: bool,
    /// How far this node's clock differs from the console's, in whole seconds.
    pub skew_secs: u32,
}

/// Whether one peer is reachable from this node on the management network.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerFacts {
    /// The peer.
    pub host: Host,
    /// Whether the agent can open an outbound connection to the peer.
    pub reachable: bool,
}

/// The facts one node's agent reports; the complete input to [`crate::preflight`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentFacts {
    /// The node the facts are about.
    pub host: Host,
    /// The operating system.
    pub os: OsFacts,
    /// Total memory, in whole mebibytes.
    pub ram_mb: u32,
    /// Free space under the data directory, in whole gibibytes.
    pub data_dir_free_gb: u32,
    /// Clock state.
    pub time: TimeFacts,
    /// TCP ports already bound on the node.
    pub listening_ports: Vec<u16>,
    /// The other nodes this one can reach.
    pub peers: Vec<PeerFacts>,
}

impl AgentFacts {
    /// Facts for `host` with the given operating system and empty peer and port lists.
    ///
    /// The remaining numbers are the wizard's ordinary floor for a production node, so a
    /// caller only fills in what it cares about.
    #[must_use]
    pub fn new(host: Host, os: OsFacts) -> Self {
        Self {
            host,
            os,
            ram_mb: 8_192,
            data_dir_free_gb: 100,
            time: TimeFacts {
                synchronized: true,
                skew_secs: 0,
            },
            listening_ports: Vec::new(),
            peers: Vec::new(),
        }
    }

    /// The same facts with `mb` mebibytes of memory.
    #[must_use]
    pub fn with_ram_mb(mut self, mb: u32) -> Self {
        self.ram_mb = mb;
        self
    }

    /// The same facts with `gb` gibibytes free under the data directory.
    #[must_use]
    pub fn with_data_dir_free_gb(mut self, gb: u32) -> Self {
        self.data_dir_free_gb = gb;
        self
    }

    /// The same facts with `ports` already bound.
    #[must_use]
    pub fn with_listening_ports(mut self, ports: &[u16]) -> Self {
        self.listening_ports = ports.to_vec();
        self
    }

    /// The same facts with a stated clock state.
    #[must_use]
    pub fn with_time(mut self, synchronized: bool, skew_secs: u32) -> Self {
        self.time = TimeFacts {
            synchronized,
            skew_secs,
        };
        self
    }

    /// The same facts with `peers` reachable or not.
    #[must_use]
    pub fn with_peers(mut self, peers: Vec<PeerFacts>) -> Self {
        self.peers = peers;
        self
    }

    /// Whether this node's agent reported `host` as reachable.
    #[must_use]
    pub fn reaches(&self, host: &Host) -> Option<bool> {
        self.peers
            .iter()
            .find(|peer| &peer.host == host)
            .map(|peer| peer.reachable)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn linux() -> OsFacts {
        OsFacts {
            family: OsFamily::Linux,
            release: "Ubuntu 24.04 LTS".to_owned(),
            supported: true,
        }
    }

    #[test]
    fn facts_round_trip_through_json() {
        let host = Host::try_new("app1.example.com").unwrap();
        let peer = Host::try_new("db1.example.com").unwrap();
        let facts = AgentFacts::new(host, linux())
            .with_ram_mb(16_384)
            .with_peers(vec![PeerFacts {
                host: peer.clone(),
                reachable: true,
            }]);
        let json = serde_json::to_string(&facts).unwrap();
        assert_eq!(serde_json::from_str::<AgentFacts>(&json).unwrap(), facts);
        assert_eq!(facts.reaches(&peer), Some(true));
        assert_eq!(
            facts.reaches(&Host::try_new("db9.example.com").unwrap()),
            None
        );
    }
}
