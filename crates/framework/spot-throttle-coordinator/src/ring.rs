//! [`SaltRing`]: the current salt and, for one window, the salt it replaced (WP-IAM-11.T12).
//!
//! A [`ThrottleSalt`] names every bucket (`thr:{tenant}:{dimension}:{digest}`), so rotating it
//! alone would start every counter afresh at once. An attacker who can wait for a rotation —
//! a schedule is a known time — would then get a free reset on every account and source he has
//! been failing for. The ring closes that: a rotation moves the old salt to `previous`, and while
//! a previous salt is present [`crate::CoordinatorThrottle::admit`] checks the buckets named by
//! both, so a denial from either is a denial. **A rotation keeps every limit that was exhausted
//! at rotation time exhausted until it refills on its own.**
//!
//! # The window
//!
//! The previous salt is retired when one full window of the **longest** bucket has passed since
//! the rotation ([`SaltRing::safe_to_retire`], [`crate::ThrottlePolicy::longest_window`]): by
//! then every bucket the old salt names has refilled to full on its own, so dropping it forgets
//! nothing. Retire earlier and whatever was still exhausted is forgotten — which is exactly the
//! free reset this type exists to prevent — so a caller that retires by its own clock should
//! check [`SaltRing::safe_to_retire`] first, or use [`SaltRing::rotate_checked`] for the next
//! rotation.
//!
//! # Time is the caller's
//!
//! Nothing here reads a clock: [`SaltRing::safe_to_retire`] takes the rotation instant and the
//! current instant as arguments. That keeps the crate testable and the decision auditable — a
//! caller that logs rotations can recompute the same answer later.
//!
//! # Rotations are the caller's, and explicit
//!
//! [`SaltRing::rotate`] is infallible and does exactly one thing: `current` becomes `previous`
//! and the new salt becomes `current`. **Rotating twice drops the older previous**: the ring
//! holds one previous salt, not a history, because two windows of buckets are enough to cover
//! the longest bucket's refill and a longer history would only widen the window an attacker's
//! earlier failures keep hurting honest callers. If the rotation comes before the previous
//! salt's window ended, that salt is dropped anyway — see [`SaltRing::rotate_checked`], which
//! refuses that case instead.
//!
//! # What is never written
//!
//! Neither [`ThrottleSalt`] nor this type writes key material: `Debug` prints
//! `ThrottleSalt(<redacted>)` for each slot and only says whether a previous salt exists.

use std::time::Duration;

use time::OffsetDateTime;

use crate::error::ThrottleError;
use crate::policy::{ThrottlePolicy, ThrottleSalt};

/// The current salt, and the salt it replaced while that one is still inside its window.
#[derive(Clone)]
pub struct SaltRing {
    current: ThrottleSalt,
    previous: Option<ThrottleSalt>,
}

impl SaltRing {
    /// A ring with only a current salt: the first release, or after the previous one was
    /// retired.
    pub fn new(current: ThrottleSalt) -> Self {
        Self {
            current,
            previous: None,
        }
    }

    /// A ring that is already in its overlap: `current` is charged, `previous` is still checked.
    ///
    /// This is what a restarted node rebuilds — the same salts from the same source — and what a
    /// rotation produces through [`SaltRing::rotate`].
    pub fn with_previous(current: ThrottleSalt, previous: ThrottleSalt) -> Self {
        Self {
            current,
            previous: Some(previous),
        }
    }

    /// The salt every new charge is keyed by.
    pub fn current(&self) -> &ThrottleSalt {
        &self.current
    }

    /// The salt still being checked, if the overlap has not ended.
    pub fn previous(&self) -> Option<&ThrottleSalt> {
        self.previous.as_ref()
    }

    /// Whether a previous salt is still being checked.
    pub fn has_previous(&self) -> bool {
        self.previous.is_some()
    }

    /// Rotates to `new`: the current salt becomes the previous one, and any older previous salt
    /// is **dropped** (the ring keeps one window, never a history).
    ///
    /// Infallible by design: a rotation is an operation the caller decides on and audits. Use
    /// [`SaltRing::rotate_checked`] when the rotation should refuse to cut a window short.
    #[must_use]
    pub fn rotate(mut self, new: ThrottleSalt) -> Self {
        self.previous = Some(std::mem::replace(&mut self.current, new));
        self
    }

    /// [`SaltRing::rotate`], refusing the two rotations that would forget something:
    ///
    /// - `new` is the salt already in force (`throttle_coordinator.bad_rotation`): the previous
    ///   salt would be dropped without a new one being introduced, which is a reset with no gain;
    /// - the previous salt's window has not fully elapsed since `rotated_at`
    ///   (`throttle_coordinator.bad_rotation`): rotating now drops it while some bucket it names
    ///   may still be exhausted, which is the free reset the ring exists to prevent
    ///   ([`SaltRing::safe_to_retire`]).
    ///
    /// `rotated_at` is when the salt in force was introduced; `now` is the caller's clock. With
    /// no previous salt both checks pass and this is [`SaltRing::rotate`].
    pub fn rotate_checked(
        self,
        new: ThrottleSalt,
        rotated_at: OffsetDateTime,
        now: OffsetDateTime,
        policy: &ThrottlePolicy,
    ) -> Result<Self, ThrottleError> {
        if self.current.same_key(&new) {
            return Err(ThrottleError::BadRotation {
                reason: "the new salt is the salt already in force",
            });
        }
        if self.previous.is_some() && !Self::safe_to_retire(rotated_at, now, policy) {
            return Err(ThrottleError::BadRotation {
                reason: "the previous salt has not finished one window of the longest bucket; \
                         retiring it now would forget limits that are still exhausted",
            });
        }
        Ok(self.rotate(new))
    }

    /// Drops the previous salt. Only call it once [`SaltRing::safe_to_retire`] says so: from then
    /// on, a bucket the previous salt named is not checked any more.
    pub fn retire_previous(&mut self) {
        self.previous = None;
    }

    /// Whether one full window of the longest bucket has passed since `rotated_at`, so that every
    /// bucket the previous salt names has refilled on its own and may be forgotten.
    ///
    /// True at exactly one window (a bucket refills continuously, so at that instant it is full
    /// again). False when `now` is before `rotated_at`: a clock that went backwards cannot prove
    /// anything, and refusing to retire is the safe answer.
    pub fn safe_to_retire(
        rotated_at: OffsetDateTime,
        now: OffsetDateTime,
        policy: &ThrottlePolicy,
    ) -> bool {
        let elapsed = now.unix_timestamp_nanos() - rotated_at.unix_timestamp_nanos();
        if elapsed < 0 {
            // The clock went backwards: nothing can be proved, and refusing is the safe answer.
            return false;
        }
        let window = i128::try_from(policy.longest_window().as_nanos()).unwrap_or(i128::MAX);
        elapsed >= window
    }

    /// How long is left of the window before the previous salt may be retired: `None` when there
    /// is no previous salt or the window has already passed.
    pub fn remaining_window(
        &self,
        rotated_at: OffsetDateTime,
        now: OffsetDateTime,
        policy: &ThrottlePolicy,
    ) -> Option<Duration> {
        self.previous.as_ref()?;
        let elapsed = (now.unix_timestamp_nanos() - rotated_at.unix_timestamp_nanos()).max(0);
        let window = policy.longest_window().as_nanos();
        let left = window.saturating_sub(u128::try_from(elapsed).unwrap_or(u128::MAX));
        let left = u64::try_from(left).unwrap_or(u64::MAX);
        (left > 0).then(|| Duration::from_nanos(left).max(Duration::ZERO))
    }
}

impl std::fmt::Debug for SaltRing {
    /// Names the slots, never their contents: whether a previous salt exists is an operational
    /// fact, its key is not.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SaltRing")
            .field("current", &self.current)
            .field("previous", &self.previous)
            .finish()
    }
}
