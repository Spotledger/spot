//! One observed meter fact, under the idempotency key that makes retries safe.

use crate::meter::{MemberDigest, Meter};

/// One observation a producer reports: who, which meter, how much (or which member),
/// and when it happened.
///
/// `id` is the idempotency key: recording the same id twice counts once, which is the
/// guarantee a retried job needs. Producers derive it from the fact itself — the
/// document id for a posted invoice, the sign-in's session event id for an active
/// user, the LLM request id for AI tokens — so that "the same fact seen again" and
/// "the same event id" are one and the same thing.
///
/// For a distinct-set meter, `amount_or_member` carries the member digest and any
/// amount is ignored. For a counter, it carries the counted amount. For a gauge, it
/// carries the observed level; the period total keeps the maximum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeterEvent {
    /// The idempotency key, unique per fact. A store keeps the first event for an id
    /// and drops later ones.
    pub id: String,
    /// The tenant the usage belongs to.
    pub tenant: String,
    /// Which meter the fact feeds.
    pub meter: Meter,
    /// The counted amount (counter), the observed level (gauge), or ignored in favour
    /// of `member` (distinct set).
    pub amount: u64,
    /// The member digest, set for distinct-set meters ([`Meter::ActiveUsers`]).
    pub member: Option<MemberDigest>,
    /// When the fact happened, UTC seconds since the Unix epoch. The period it lands
    /// in is derived from this, never from a clock.
    pub at: i64,
}

impl MeterEvent {
    /// A counter or gauge event: one amount at one instant.
    #[must_use]
    pub fn amount(
        id: impl Into<String>,
        tenant: impl Into<String>,
        meter: Meter,
        amount: u64,
        at: i64,
    ) -> Self {
        Self {
            id: id.into(),
            tenant: tenant.into(),
            meter,
            amount,
            member: None,
            at,
        }
    }

    /// A distinct-set event: one member seen at one instant. The amount is not used.
    #[must_use]
    pub fn member(
        id: impl Into<String>,
        tenant: impl Into<String>,
        meter: Meter,
        member: MemberDigest,
        at: i64,
    ) -> Self {
        Self {
            id: id.into(),
            tenant: tenant.into(),
            meter,
            amount: 0,
            member: Some(member),
            at,
        }
    }

    /// The event's meter kind must agree with its payload: a counter carries an
    /// amount, a distinct set carries a member. A mismatched event is refused at the
    /// sink so a producer bug cannot quietly corrupt a period total.
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        match self.meter.kind() {
            crate::meter::MeterKind::Counter | crate::meter::MeterKind::Gauge => {
                self.member.is_none()
            }
            crate::meter::MeterKind::DistinctSet => self.member.is_some(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_formed_tracks_the_kind() {
        let counter = MeterEvent::amount("t1", "acme", Meter::Transactions, 3, 0);
        assert!(counter.is_well_formed());

        let member = MeterEvent::member(
            "s1",
            "acme",
            Meter::ActiveUsers,
            MemberDigest::new("a1b2"),
            0,
        );
        assert!(member.is_well_formed());

        // A distinct-set event without a member is malformed.
        let bad_set = MeterEvent::amount("s2", "acme", Meter::ActiveUsers, 1, 0);
        assert!(!bad_set.is_well_formed());

        // A counter event carrying a member is malformed.
        let mut bad_counter = counter.clone();
        bad_counter.member = Some(MemberDigest::new("zz"));
        assert!(!bad_counter.is_well_formed());
    }
}
