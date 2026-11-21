//! Which carrier each direction of each link uses (ADR-058 rule 11).
//!
//! `send` is how this side's outgoing messages reach the peer: [`Carrier::Direct`] means this
//! side dials the peer's `POST /messages`; [`Carrier::Pull`] means this side keeps a mailbox the
//! peer fetches (and acknowledges) from. `receive` is the mirror: `Direct` means this side
//! serves `POST /messages` for the peer to dial; `Pull` means this side long-polls the peer's
//! mailbox. A side behind NAT therefore uses `send = Direct, receive = Pull`, and its peer the
//! opposite. `External` is the connector carrier (WP-CTL-11) and serves nothing here.

use std::collections::BTreeMap;
use std::sync::RwLock;

use spot_tenant_link::{Carrier, Link, LinkId};

/// The carrier of each direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkCarriers {
    /// How this side's messages reach the peer.
    pub send: Carrier,
    /// How the peer's messages reach this side.
    pub receive: Carrier,
}

impl LinkCarriers {
    /// Both directions on `carrier`.
    #[must_use]
    pub fn both(carrier: Carrier) -> Self {
        Self {
            send: carrier,
            receive: carrier,
        }
    }

    /// Whether this side serves `POST /messages`.
    #[must_use]
    pub fn accepts_pushes(self) -> bool {
        self.receive == Carrier::Direct
    }

    /// Whether this side serves the mailbox and `POST /acks`.
    #[must_use]
    pub fn serves_mailbox(self) -> bool {
        self.send == Carrier::Pull
    }
}

/// Per-link overrides of the carriers; a link without one uses its own `carrier` both ways.
#[derive(Default)]
pub struct CarrierTable {
    overrides: RwLock<BTreeMap<LinkId, LinkCarriers>>,
}

impl CarrierTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the carriers of one link.
    pub fn set(&self, link: LinkId, carriers: LinkCarriers) {
        let mut overrides = self
            .overrides
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        overrides.insert(link, carriers);
    }

    /// The carriers of `link`.
    #[must_use]
    pub fn of(&self, link: &Link) -> LinkCarriers {
        let overrides = self
            .overrides
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        overrides
            .get(&link.id)
            .copied()
            .unwrap_or_else(|| LinkCarriers::both(link.carrier))
    }
}
