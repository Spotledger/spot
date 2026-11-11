//! Replica reads and their safety rules (ADR-24).
//!
//! A replica read trades freshness for load: it takes work off the primary, but a caller that just
//! wrote must still see its own write. The rules here are the brief's, encoded:
//!
//! - **A write never reads a replica.** A request that carries a write reads the primary; the
//!   read preference is [`ReadPreference::Primary`] no matter what a rule says.
//! - **Read-your-writes wins over staleness tolerance.** For a window after a session's last
//!   write, a replica may serve the read only if it has applied at least the session's high-water
//!   token. This is a token comparison, not a timestamp comparison, so a lagging replica is
//!   excluded exactly when it would show stale data.
//! - **Lag over the limit falls back to the primary.** The rule's `max_lag` bounds how stale a
//!   replica may be; a replica slower than that is not used.
//!
//! The chosen [`ReadPreference`] is the hint a request context carries for `spot-postgres` to
//! honour; adapting the storage layer is a follow-up (see the task's open questions).

use std::time::Duration;

use time::OffsetDateTime;

use crate::node::NodeId;

/// A monotonically increasing token for one session's writes. The platform's commit tokens are
/// the natural source; the comparison only needs the ordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SessionToken(pub u64);

/// What a session last wrote, for the read-your-writes rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct SessionReadState {
    /// The highest commit token the session has observed from its own writes.
    pub high_water: SessionToken,
    /// When the session last wrote, if it has written at all.
    pub last_write_at: Option<OffsetDateTime>,
}

impl SessionReadState {
    /// A session that has never written: any replica within the lag limit may serve its reads.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// A session that just wrote under a commit token.
    #[must_use]
    pub fn after_write(high_water: SessionToken, at: OffsetDateTime) -> Self {
        Self {
            high_water,
            last_write_at: Some(at),
        }
    }

    /// Whether the session is inside its read-your-writes window at `now`.
    #[must_use]
    pub fn within_window(&self, now: OffsetDateTime, window: Duration) -> bool {
        match self.last_write_at {
            Some(wrote_at) => {
                // A clock that went backwards counts as "just wrote": the safe side.
                let elapsed = now - wrote_at;
                elapsed < window
            }
            None => false,
        }
    }
}

/// One replica's live status, as the host reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplicaStatus {
    /// The replica's node id.
    pub id: NodeId,
    /// The highest commit token the replica has applied.
    pub applied_token: SessionToken,
    /// How far behind the primary the replica is.
    pub lag: Duration,
}

/// Where a read runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadPreference {
    /// The primary database: always fresh, always safe.
    Primary,
    /// A replica, if its lag is within `max_lag`.
    Replica {
        /// The maximum acceptable replication lag.
        max_lag: Duration,
    },
}

impl ReadPreference {
    /// Whether this preference names a replica.
    #[must_use]
    pub fn is_replica(self) -> bool {
        matches!(self, ReadPreference::Replica { .. })
    }
}

/// The outcome of a read decision: the preference, the replica chosen (if any) and the reason a
/// replica was refused (a stable code, for metrics).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadDecision {
    /// The hint the request context carries.
    pub preference: ReadPreference,
    /// The replica chosen, when the preference is a replica.
    pub replica: Option<NodeId>,
    /// Why a replica was refused, as a [`crate::codes`] constant.
    pub reason: Option<&'static str>,
}

impl ReadDecision {
    fn primary(reason: Option<&'static str>) -> Self {
        Self {
            preference: ReadPreference::Primary,
            replica: None,
            reason,
        }
    }
}

/// Decide whether a read may use a replica.
///
/// `is_write` is `true` for any request that carries a write; such a request always reads the
/// primary. `max_lag` is the matched rule's limit (callers with no rule pass a preference for the
/// primary instead). `replicas` is the live replica set; `session` is the caller's read-your-writes
/// state and `window` the configured window; `now` is the caller's clock.
#[must_use]
pub fn choose_read(
    is_write: bool,
    max_lag: Duration,
    replicas: &[ReplicaStatus],
    session: &SessionReadState,
    window: Duration,
    now: OffsetDateTime,
) -> ReadDecision {
    if is_write {
        return ReadDecision::primary(Some(crate::codes::WRITE_TO_REPLICA));
    }
    if replicas.is_empty() {
        return ReadDecision::primary(Some(crate::codes::NO_REPLICA));
    }
    let within_lag: Vec<&ReplicaStatus> = replicas
        .iter()
        .filter(|replica| replica.lag <= max_lag)
        .collect();
    if within_lag.is_empty() {
        return ReadDecision::primary(Some(crate::codes::REPLICA_LAG));
    }
    if session.within_window(now, window) {
        let caught_up: Vec<&ReplicaStatus> = within_lag
            .iter()
            .copied()
            .filter(|replica| replica.applied_token >= session.high_water)
            .collect();
        if caught_up.is_empty() {
            return ReadDecision::primary(Some(crate::codes::READ_YOUR_WRITES));
        }
        let Some(best) = caught_up
            .into_iter()
            .min_by_key(|replica| (replica.lag, replica.id.clone()))
        else {
            return ReadDecision::primary(Some(crate::codes::READ_YOUR_WRITES));
        };
        return ReadDecision {
            preference: ReadPreference::Replica { max_lag },
            replica: Some(best.id.clone()),
            reason: None,
        };
    }
    let Some(best) = within_lag
        .into_iter()
        .min_by_key(|replica| (replica.lag, replica.id.clone()))
    else {
        return ReadDecision::primary(Some(crate::codes::NO_REPLICA));
    };
    ReadDecision {
        preference: ReadPreference::Replica { max_lag },
        replica: Some(best.id.clone()),
        reason: None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000 + seconds).expect("timestamp")
    }

    fn replica(id: &str, token: u64, lag_ms: u64) -> ReplicaStatus {
        ReplicaStatus {
            id: NodeId::new(id),
            applied_token: SessionToken(token),
            lag: Duration::from_millis(lag_ms),
        }
    }

    #[test]
    fn a_write_never_reads_a_replica() {
        let replicas = vec![replica("r1", 100, 0)];
        let decision = choose_read(
            true,
            Duration::from_secs(5),
            &replicas,
            &SessionReadState::none(),
            Duration::from_secs(2),
            at(0),
        );
        assert_eq!(decision.preference, ReadPreference::Primary);
        assert_eq!(decision.reason, Some(crate::codes::WRITE_TO_REPLICA));
    }

    #[test]
    fn no_replica_falls_back_to_primary() {
        let decision = choose_read(
            false,
            Duration::from_secs(5),
            &[],
            &SessionReadState::none(),
            Duration::from_secs(2),
            at(0),
        );
        assert_eq!(decision.preference, ReadPreference::Primary);
        assert_eq!(decision.reason, Some(crate::codes::NO_REPLICA));
    }

    #[test]
    fn lag_over_the_limit_falls_back_to_primary() {
        let replicas = vec![replica("r1", 100, 9_000)];
        let decision = choose_read(
            false,
            Duration::from_secs(5),
            &replicas,
            &SessionReadState::none(),
            Duration::from_secs(2),
            at(0),
        );
        assert_eq!(decision.reason, Some(crate::codes::REPLICA_LAG));
        assert!(decision.replica.is_none());
    }

    #[test]
    fn read_your_writes_requires_a_caught_up_replica() {
        let replicas = vec![replica("r1", 50, 10)];
        let session = SessionReadState::after_write(SessionToken(100), at(0));
        let decision = choose_read(
            false,
            Duration::from_secs(5),
            &replicas,
            &session,
            Duration::from_secs(2),
            at(1),
        );
        assert_eq!(decision.reason, Some(crate::codes::READ_YOUR_WRITES));
        assert_eq!(decision.preference, ReadPreference::Primary);

        // Once the replica catches up, the read may use it.
        let replicas = vec![replica("r1", 100, 10)];
        let decision = choose_read(
            false,
            Duration::from_secs(5),
            &replicas,
            &session,
            Duration::from_secs(2),
            at(1),
        );
        assert!(decision.preference.is_replica());
        assert_eq!(decision.replica, Some(NodeId::new("r1")));
    }

    #[test]
    fn window_expiry_relaxes_the_token_requirement() {
        let replicas = vec![replica("r1", 10, 10)];
        let session = SessionReadState::after_write(SessionToken(100), at(0));
        let decision = choose_read(
            false,
            Duration::from_secs(5),
            &replicas,
            &session,
            Duration::from_secs(2),
            at(10),
        );
        assert!(decision.preference.is_replica());
    }

    #[test]
    fn the_least_lagging_replica_is_chosen() {
        let replicas = vec![replica("r1", 100, 500), replica("r2", 100, 20)];
        let decision = choose_read(
            false,
            Duration::from_secs(5),
            &replicas,
            &SessionReadState::none(),
            Duration::from_secs(2),
            at(0),
        );
        assert_eq!(decision.replica, Some(NodeId::new("r2")));
    }
}
