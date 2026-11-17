//! The identity lease clock: fail closed at expiry, and a clock moved back never extends it.
//!
//! The lease is granted at a wall instant and a monotonic instant (both arguments). The time that
//! has passed is the **larger** of the wall elapsed (zero when the wall clock moved back) and the
//! monotonic elapsed (which never goes back). Moving the wall clock back therefore cannot add
//! time; moving it forward shortens the lease, which is the safe direction.

use std::time::Duration;

/// The default lease, hours.
pub const DEFAULT_LEASE_HOURS: u64 = 24;
/// The hard cap on the lease, hours (`services.edge_offline.identity_lease_hours`).
pub const MAX_LEASE_HOURS: u64 = 168;

/// The lease length for a configured number of hours, capped.
pub fn lease_length(hours: Option<u64>) -> Duration {
    let hours = hours
        .unwrap_or(DEFAULT_LEASE_HOURS)
        .clamp(1, MAX_LEASE_HOURS);
    Duration::from_secs(hours * 3_600)
}

/// One granted lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdentityLease {
    granted_wall: i64,
    granted_mono: Duration,
    length: Duration,
}

/// What the lease allows right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// Alive, with this much time left.
    Alive(Duration),
    /// Lapsed: refuse new sign-ins and privileged actions; already signed-in sessions follow the
    /// offline-identity crate's own read rules.
    Lapsed,
}

impl IdentityLease {
    /// A lease granted at wall time `granted_wall` (Unix seconds) and monotonic `granted_mono`.
    pub fn new(granted_wall: i64, granted_mono: Duration, length: Duration) -> Self {
        Self {
            granted_wall,
            granted_mono,
            length,
        }
    }

    /// The lease's standing at `now_wall` / `now_mono`.
    pub fn standing(&self, now_wall: i64, now_mono: Duration) -> Standing {
        let wall = Duration::from_secs(
            u64::try_from(now_wall.saturating_sub(self.granted_wall)).unwrap_or(0),
        );
        let mono = now_mono.saturating_sub(self.granted_mono);
        let elapsed = wall.max(mono);
        match self.length.checked_sub(elapsed) {
            Some(left) if !left.is_zero() => Standing::Alive(left),
            _ => Standing::Lapsed,
        }
    }

    /// Whether a new sign-in or a privileged action may proceed.
    pub fn allows_privileged(&self, now_wall: i64, now_mono: Duration) -> bool {
        matches!(self.standing(now_wall, now_mono), Standing::Alive(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: u64 = 3_600;

    #[test]
    fn the_length_is_capped_and_defaults_to_a_day() {
        assert_eq!(lease_length(None), Duration::from_secs(24 * H));
        assert_eq!(lease_length(Some(1_000)), Duration::from_secs(168 * H));
        assert_eq!(lease_length(Some(0)), Duration::from_secs(H));
    }

    #[test]
    fn it_lapses_at_the_end_and_not_before() {
        let lease = IdentityLease::new(1_000, Duration::from_secs(50), Duration::from_secs(24 * H));
        assert_eq!(
            lease.standing(1_000 + 7 * H as i64, Duration::from_secs(50 + 7 * H)),
            Standing::Alive(Duration::from_secs(17 * H))
        );
        assert_eq!(
            lease.standing(1_000 + 24 * H as i64, Duration::from_secs(50 + 24 * H)),
            Standing::Lapsed
        );
    }

    #[test]
    fn moving_the_wall_clock_back_does_not_extend_the_lease() {
        let lease = IdentityLease::new(1_000_000, Duration::ZERO, Duration::from_secs(24 * H));
        // 23 hours really passed (monotonic), but the wall clock was set back to the grant time.
        let standing = lease.standing(1_000_000 - 10 * H as i64, Duration::from_secs(23 * H));
        assert_eq!(standing, Standing::Alive(Duration::from_secs(H)));
        // And once the monotonic time says 25 hours, a wall clock set back does not revive it.
        assert!(!lease.allows_privileged(0, Duration::from_secs(25 * H)));
    }

    #[test]
    fn moving_the_wall_clock_forward_shortens_it() {
        let lease = IdentityLease::new(0, Duration::ZERO, Duration::from_secs(24 * H));
        assert!(!lease.allows_privileged(30 * H as i64, Duration::from_secs(H)));
    }
}
