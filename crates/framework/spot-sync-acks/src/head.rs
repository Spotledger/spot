//! The hub's stream head, as its own type.

/// The position of the newest entry the **hub's own stream** holds.
///
/// It is a newtype, not a bare `u64`, because [`AckLedger::on_ack`](crate::AckLedger::on_ack)
/// takes two `u64` values of different trust: the node's `applied_prefix` and the hub's head.
/// Two arguments of the same type and a similar meaning were exactly how they could be swapped
/// by position without the compiler noticing. The value must come from the hub's stream, never
/// from a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamHead(u64);

impl StreamHead {
    /// The head at position `position`.
    #[must_use]
    pub const fn new(position: u64) -> Self {
        Self(position)
    }

    /// The position.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}
