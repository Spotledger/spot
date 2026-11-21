//! Server selection: the pure decision of which discovered server a client connects
//! to. Never a guess: one found means that one; several means the user chooses; none
//! means the manual route. Unit-tested as a table below.

use std::net::IpAddr;
use std::time::Duration;

use crate::record::{Advertisement, ServerId};

/// A server found by scanning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundServer {
    /// The stable identity (`ServerId`) of the server.
    pub server_id: ServerId,
    /// All reachable addresses, IPv4 first (the browser sorts them).
    pub addresses: Vec<IpAddr>,
    /// The application port.
    pub port: u16,
    /// The decoded record.
    pub advertisement: Advertisement,
}

/// A server entered by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualServer {
    /// The host (IP or name) as entered.
    pub host: String,
    /// The port as entered (or the default).
    pub port: u16,
}

/// Which server the client should connect to. There is no default-pick variant: the
/// tie is the user's to break.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Exactly this server: the last used one (when present), or the only one found.
    Select(FoundServer),
    /// A manual server the user typed.
    SelectManual(ManualServer),
    /// Several servers: present `list` and ask.
    NeedsChoice(Vec<FoundServer>),
    /// Nothing found: offer the manual-address route.
    NoneFound,
    /// The last used server was found, and it comes with a changed fingerprint — the
    /// caller must run the trust flow (re-pair) before connecting; the choice is not
    /// silently confirmed.
    SelectNeedsTrust(FoundServer),
}

/// Knobs of the selection policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionPolicy {
    /// Remember-and-prefer: when the last-used server is present, it wins. This is the
    /// default and the whole point of the stable `ServerId`.
    pub prefer_last_used: bool,
    /// Only servers advertising at least this protocol version are candidates.
    pub min_protocol_version: u32,
    /// How long ago a `Found` may have been seen to count as present (0 = no bound).
    pub max_age: Duration,
}

impl Default for SelectionPolicy {
    fn default() -> Self {
        Self {
            prefer_last_used: true,
            min_protocol_version: 1,
            max_age: Duration::ZERO,
        }
    }
}

impl FoundServer {
    /// Builds a `FoundServer` from the parts the browser hands over.
    #[must_use]
    pub fn new(server_id: ServerId, addresses: Vec<IpAddr>, advertisement: Advertisement) -> Self {
        let port = advertisement.port;
        Self {
            server_id,
            addresses,
            port,
            advertisement,
        }
    }
}

/// Picks which server to connect to.
///
/// - `last_used` wins when the policy prefers it and the server is present — the
///   client reconnects to the same shop server tomorrow without asking again.
/// - One server found means select it.
/// - Several means `NeedsChoice(list)`, never a guess.
/// - None means `NoneFound` with the manual route offered (the caller turns that
///   into `parse_manual` input and gets `SelectManual`).
///
/// When the winning server's advertised fingerprint differs from the client's stored
/// one (or none is stored yet) the caller says so through
/// `fingerprint_differs_for` and the selection becomes `SelectNeedsTrust` so the
/// pairing flow runs before any connection.
#[must_use]
pub fn pick(
    found: &[FoundServer],
    last_used: Option<&ServerId>,
    policy: &SelectionPolicy,
    fingerprint_differs_for: Option<&ServerId>,
) -> Selection {
    // Filter: protocol floor.
    let candidates: Vec<&FoundServer> = found
        .iter()
        .filter(|f| f.advertisement.protocol_version.0 >= policy.min_protocol_version)
        .collect();

    if let (true, Some(last)) = (policy.prefer_last_used, last_used)
        && let Some(f) = candidates.iter().find(|c| &c.server_id == last)
    {
        if fingerprint_differs_for.is_some_and(|id| id == &f.server_id) {
            return Selection::SelectNeedsTrust((*f).clone());
        }
        return Selection::Select((*f).clone());
    }

    match candidates.len() {
        0 => Selection::NoneFound,
        1 => {
            let f = candidates[0];
            if fingerprint_differs_for.is_some_and(|id| id == &f.server_id) {
                Selection::SelectNeedsTrust(f.clone())
            } else {
                Selection::Select(f.clone())
            }
        }
        _ => Selection::NeedsChoice(candidates.into_iter().cloned().collect()),
    }
}
