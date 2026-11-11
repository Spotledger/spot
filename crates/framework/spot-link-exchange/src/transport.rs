//! The party a carrier adapter authenticated for one delivery.
//!
//! The receiver and the receipt path must attribute an input to the *connection*, never to a
//! value the input carries: a carrier that takes the party from the message would let a peer
//! name itself. [`TransportParty`] is that distinction in the type system — a carrier adapter
//! builds one from the identity it verified (a pinned fingerprint or a mutual-TLS subject it
//! already matched to a party), and the exchange crate cannot be handed anything else.

use spot_tenant_link::Party;

/// The party the transport authenticated for this connection.
///
/// The only ways to obtain one are [`TransportParty::from_verified_connection`], which a carrier
/// adapter calls with the identity it verified itself, and the `testing` double. Neither reads
/// the message, so an input can never name its own sender.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportParty(Party);

impl TransportParty {
    /// The party behind a connection whose identity the caller verified: a pinned fingerprint or
    /// a mutual-TLS subject, matched to a party by the carrier adapter.
    ///
    /// **Only a carrier adapter may call this.** A value taken from the message, from a header
    /// the peer controls, or from an unverified certificate is not a transport party: it must not
    /// reach here.
    #[must_use]
    pub fn from_verified_connection(party: Party) -> Self {
        Self(party)
    }

    /// What the connection authenticated.
    #[must_use]
    pub fn party(&self) -> &Party {
        &self.0
    }
}

#[cfg(any(test, feature = "testing"))]
impl TransportParty {
    /// A double for the crates' own tests and for a conformance harness, which have no
    /// connection to verify.
    #[must_use]
    pub fn for_test(party: Party) -> Self {
        Self(party)
    }
}
