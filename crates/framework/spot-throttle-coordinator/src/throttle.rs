//! [`CoordinatorThrottle`]: four takes in a fixed order, under a [`SaltRing`].

use std::sync::Arc;
use std::time::Duration;

use spot_coordinator::{Admit, BoxFuture, Coordinator, RateLimit};

use crate::attempt::{Attempt, Verdict};
use crate::error::ThrottleError;
use crate::policy::{ThrottlePolicy, ThrottleSalt};
use crate::ring::SaltRing;

/// The four dimensions, in the order they are charged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dimension {
    Source,
    Destination,
    Account,
    Tenant,
}

impl Dimension {
    fn label(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Destination => "destination",
            Self::Account => "account",
            Self::Tenant => "tenant",
        }
    }
}

/// The sign-in throttle on the coordinator: shared by every node and kept across a restart.
///
/// The buckets are keyed by the ring's salts, not by one salt: while the ring holds a previous
/// salt (one full window after a rotation, see [`SaltRing`]), every attempt is counted against
/// the current salt's buckets **and** checked against the previous salt's, so a rotation never
/// hands an attacker a free reset. A bucket exhausted before the rotation stays exhausted until
/// it refills on its own.
#[derive(Clone)]
pub struct CoordinatorThrottle {
    coordinator: Arc<dyn Coordinator>,
    ring: SaltRing,
    policy: ThrottlePolicy,
}

impl std::fmt::Debug for CoordinatorThrottle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoordinatorThrottle")
            .field("policy", &self.policy)
            .field("ring", &self.ring)
            .finish_non_exhaustive()
    }
}

impl CoordinatorThrottle {
    /// A throttle over `coordinator` with a single salt.
    ///
    /// Equivalent to [`CoordinatorThrottle::with_ring`] with `SaltRing::new(salt)`: there is no
    /// previous salt to check. A deployment that rotates uses the ring form.
    pub fn new(
        coordinator: Arc<dyn Coordinator>,
        salt: ThrottleSalt,
        policy: ThrottlePolicy,
    ) -> Self {
        Self::with_ring(coordinator, SaltRing::new(salt), policy)
    }

    /// A throttle over `coordinator` keyed by every salt `ring` holds.
    ///
    /// A rotation builds a new throttle from the rotated ring (`ring.rotate(new)`) and swaps it
    /// in; nothing here mutates the ring, so a running node keeps a consistent view.
    pub fn with_ring(
        coordinator: Arc<dyn Coordinator>,
        ring: SaltRing,
        policy: ThrottlePolicy,
    ) -> Self {
        Self {
            coordinator,
            ring,
            policy,
        }
    }

    /// The salts this throttle counts under.
    pub fn ring(&self) -> &SaltRing {
        &self.ring
    }

    /// The limits this throttle applies.
    pub fn policy(&self) -> &ThrottlePolicy {
        &self.policy
    }

    /// Counts one attempt against every dimension that applies and says whether it may go on.
    ///
    /// `Err(Unavailable)` means the coordinator failed: **refuse**. A malformed attempt is
    /// `Err(BadAttempt)` and counts nothing. `Err(SaltUnavailable)` comes from
    /// [`crate::SaltSource`]; it is not produced here, and it is equally a refusal.
    pub fn admit<'a>(
        &'a self,
        attempt: &'a Attempt<'a>,
    ) -> BoxFuture<'a, Result<Verdict, ThrottleError>> {
        Box::pin(async move {
            attempt.validate()?;
            // The previous salt's buckets go first. They belong to a window that is being
            // retired, and an attempt they refuse costs nothing anywhere: charging the fresh
            // buckets first would let a flood during the overlap pre-spend the current salt's
            // allowance for the account it targets, which is the denial the ring exists to
            // prevent.
            if let Some(previous) = self.ring.previous()
                && let Some(retry_after) = self.charge(attempt, previous).await?
            {
                return Ok(Verdict::Deny { retry_after });
            }
            if let Some(retry_after) = self.charge(attempt, self.ring.current()).await? {
                return Ok(Verdict::Deny { retry_after });
            }
            Ok(Verdict::Allow)
        })
    }

    /// Charges every bucket the attempt applies to under `salt`; `Some(retry_after)` is a denial.
    ///
    /// A denial leaves the denying bucket untouched and everything after it uncharged; what was
    /// charged before it stays (the coordinator has no refund).
    async fn charge(
        &self,
        attempt: &Attempt<'_>,
        salt: &ThrottleSalt,
    ) -> Result<Option<Duration>, ThrottleError> {
        for (key, limit) in self.plan(attempt, salt) {
            match self.coordinator.take(&key, limit, 1).await {
                Ok(Admit::Allowed { .. }) => {}
                Ok(Admit::Denied { retry_after }) => return Ok(Some(retry_after)),
                Err(error) => return Err(ThrottleError::Unavailable { cause: error.code }),
            }
        }
        Ok(None)
    }

    /// The buckets this attempt is charged to under `salt`, in charging order.
    fn plan(&self, attempt: &Attempt<'_>, salt: &ThrottleSalt) -> Vec<(String, RateLimit)> {
        let tenant = attempt.tenant;
        let key = |dimension: Dimension, parts: &[&str]| {
            format!("thr:{tenant}:{}:{}", dimension.label(), salt.digest(parts))
        };
        let mut plan = Vec::with_capacity(4);

        // 1. Source first: it is the dimension an attacker controls, so a flood from one source
        //    is refused here before it can charge anybody else's account or destination bucket.
        let (what, value) = if attempt.source.address.trim().is_empty() {
            // Recorded security finding: no address is a bucket per connection, never one shared bucket.
            (
                "connection",
                attempt.source.connection_id.unwrap_or_default(),
            )
        } else {
            ("address", attempt.source.address)
        };
        let value = value.trim().to_ascii_lowercase();
        plan.push((
            key(Dimension::Source, &[tenant, what, &value]),
            self.policy.per_source(),
        ));

        // 2. Destination: shared by every kind, so register + code + recovery together cannot
        //    pump more messages to one e-mail or phone than the limit.
        if let Some(destination) = attempt.destination {
            let lowered = destination.trim().to_lowercase();
            plan.push((
                key(Dimension::Destination, &[tenant, &lowered]),
                self.policy.per_destination(),
            ));
        }

        // 3. Account: per kind, so a flood of recovery requests for a name cannot deny that
        //    name's sign-in. Known and unknown names take this very path at this very cost.
        if let Some(account) = attempt.account {
            let lowered = account.trim().to_lowercase();
            plan.push((
                key(
                    Dimension::Account,
                    &[tenant, attempt.kind.label(), &lowered],
                ),
                self.policy.per_account(),
            ));
        }

        // 4. Tenant ceiling last: it protects the database, and it is the one bucket every
        //    other attempt shares.
        plan.push((key(Dimension::Tenant, &[tenant]), self.policy.per_tenant()));
        plan
    }
}
