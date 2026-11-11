//! The per-link bounds on what a partner can make this tenant accumulate.
//!
//! A partner is authenticated but not trusted: every message it sends may open a conversation or
//! be held in the input quarantine, and nothing in the link exchange stops it from doing that for
//! ever. [`LinkLimits`] caps both per link, with defaults that are generous for an honest partner
//! and small enough that a hostile one cannot grow one tenant's state without limit. Reaching a
//! bound is a refusal with its own catalogued code and an audit row
//! ([`crate::receive::ConformantInbox`]), never a silent drop.
//!
//! Both counts are the stores' own, read when asked: the conversation count from the
//! [`ConversationStore`](crate::record::ConversationStore) (records without an ending) and the
//! quarantine count from the [`InputQuarantine`] port. Nothing is kept beside them, so an input an
//! operator released from the quarantine frees its slot at once, and a process that restarts over
//! a full quarantine sees it full instead of starting from zero.
//!
//! The count and the hold of one link are one step under a per-link lock
//! ([`LinkLimits::hold`]), so two receipts for the same link cannot both take the last slot.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use spot_link_exchange::{InputQuarantine, QuarantinedInput};
use spot_tenant_link::LinkId;

/// The most conversations one link may hold open at once.
pub const MAX_OPEN_CONVERSATIONS: usize = 64;
/// The most inputs one link may hold in the quarantine at once.
pub const MAX_HELD_INPUTS: usize = 512;

/// The bounds of one tenant, and the per-link locks that keep a count and a hold together.
#[derive(Debug)]
pub struct LinkLimits {
    max_conversations: usize,
    max_held: usize,
    /// One lock per link, held across the count and the hold of that link only, so links never
    /// wait on each other.
    holding: Mutex<BTreeMap<LinkId, Arc<Mutex<()>>>>,
}

impl Default for LinkLimits {
    fn default() -> Self {
        Self {
            max_conversations: MAX_OPEN_CONVERSATIONS,
            max_held: MAX_HELD_INPUTS,
            holding: Mutex::new(BTreeMap::new()),
        }
    }
}

impl LinkLimits {
    /// The safe defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The same bounds with other numbers, for a tenant that knows its traffic.
    #[must_use]
    pub fn with(max_conversations: usize, max_held: usize) -> Self {
        Self {
            max_conversations,
            max_held,
            holding: Mutex::new(BTreeMap::new()),
        }
    }

    /// The most conversations one link may hold open.
    #[must_use]
    pub fn max_conversations(&self) -> usize {
        self.max_conversations
    }

    /// The most inputs one link may hold in the quarantine.
    #[must_use]
    pub fn max_held(&self) -> usize {
        self.max_held
    }

    /// Whether a link may open one more conversation, given the `open` it already has.
    #[must_use]
    pub fn may_open(&self, open: usize) -> bool {
        open < self.max_conversations
    }

    /// Whether `link` may hold one more refused input, counted from `quarantine` itself now.
    ///
    /// # Errors
    /// [`Error::QuarantineCountUnavailable`](spot_link_exchange::Error::QuarantineCountUnavailable)
    /// when the quarantine cannot say how many inputs it holds for the link. A caller that must
    /// decide fails closed on that: see [`LinkLimits::hold`].
    pub fn may_hold(
        &self,
        link: &LinkId,
        quarantine: &dyn InputQuarantine,
    ) -> spot_link_exchange::Result<bool> {
        Ok(quarantine.held_count(link)? < self.max_held)
    }

    /// Holds `input` when its link has room, counting and holding as one step: another receipt
    /// for the same link waits for it, so two of them cannot both take the last slot.
    ///
    /// `Ok(false)` is "not held", and the input is dropped: the link is at its bound, or the
    /// quarantine cannot say how much it holds (fail closed). A caller still answers the sender
    /// with the refusal receipt it already decided on; only the quarantine entry is lost.
    ///
    /// An input the quarantine **already** holds is answered `Ok(true)` without counting it
    /// again: holding the same message twice leaves one item
    /// ([`InputQuarantine::hold`]), so a re-hold at the bound takes no second slot and is not
    /// dropped by a bound it already occupies. A quarantine that cannot answer
    /// [`InputQuarantine::holds`] is treated as not holding the input, so the count and the bound
    /// apply to it as before.
    ///
    /// # Errors
    /// The quarantine refused to hold an input it had room for, so nothing was acknowledged.
    pub fn hold(
        &self,
        quarantine: &dyn InputQuarantine,
        input: QuarantinedInput,
    ) -> spot_link_exchange::Result<bool> {
        let lock = self.lock_of(&input.link);
        let guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if matches!(quarantine.holds(&input.link, &input.message.id), Ok(true)) {
            drop(guard);
            return Ok(true);
        }
        let room = matches!(self.may_hold(&input.link, quarantine), Ok(true));
        if !room {
            return Ok(false);
        }
        quarantine.hold(input)?;
        drop(guard);
        Ok(true)
    }

    /// The lock one link's count-and-hold holds, created on first use.
    fn lock_of(&self, link: &LinkId) -> Arc<Mutex<()>> {
        self.holding().entry(*link).or_default().clone()
    }

    fn holding(&self) -> MutexGuard<'_, BTreeMap<LinkId, Arc<Mutex<()>>>> {
        self.holding
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
