//! The link store port and its in-memory reference implementation.
//!
//! The PostgreSQL store (WP-CTL-09.T2) lives in one tenant's schema and implements the same
//! trait; the control-plane registry records only that a link exists and never its grants or
//! topics (ADR-058 rule 3, and T2's deliverable).
//!
//! A store is the only place a link gets a version. `insert` assigns 1 and `update` moves the
//! version forward on a compare-and-set, so two administrators cannot silently overwrite each
//! other's revocation.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::error::{Error, Result};
use crate::ids::LinkId;
use crate::model::{Link, LinkState, Party};

/// Durable storage of links for one tenant.
///
/// Implementations must make `insert` and `update` atomic and must fail closed: a store failure
/// is an error, never a default or a silently stale answer.
pub trait LinkStore: Send + Sync {
    /// Stores a new link, validates it and assigns version 1.
    ///
    /// # Errors
    /// [`Error::InvalidParty`]/[`Error::InvalidOffer`] and friends for a malformed record,
    /// [`Error::InvalidTransition`] when a live link already joins the pair, or
    /// [`Error::StoreFailed`].
    fn insert(&self, link: Link) -> Result<Link>;

    /// Reads one link.
    ///
    /// # Errors
    /// [`Error::NotFound`] or [`Error::StoreFailed`].
    fn get(&self, id: &LinkId) -> Result<Link>;

    /// Replaces a link if the stored version is `expected_version`; the stored version becomes
    /// `expected_version + 1`. The id, parties, local side and creation time never change.
    ///
    /// # Errors
    /// [`Error::VersionConflict`], [`Error::NotFound`], [`Error::InvalidTransition`],
    /// [`Error::InvalidParty`] when an immutable field changed, or [`Error::StoreFailed`].
    fn update(&self, link: Link, expected_version: u64) -> Result<Link>;

    /// The live (not revoked or expired) link joining two parties, if any.
    ///
    /// # Errors
    /// [`Error::StoreFailed`].
    fn find(&self, first: &Party, second: &Party) -> Result<Option<Link>>;

    /// Every link this side is part of, ordered by id.
    ///
    /// # Errors
    /// [`Error::StoreFailed`].
    fn list_for(&self, party: &Party) -> Result<Vec<Link>>;
}

/// The in-memory [`LinkStore`], for tests and single-process hosts.
#[derive(Default)]
pub struct MemoryLinkStore {
    links: Mutex<BTreeMap<LinkId, Link>>,
}

impl MemoryLinkStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<LinkId, Link>>> {
        self.links.lock().map_err(|_| Error::StoreFailed {
            reason: "the link store lock is poisoned",
        })
    }
}

fn live(state: LinkState) -> bool {
    !matches!(state, LinkState::Revoked | LinkState::Expired)
}

impl LinkStore for MemoryLinkStore {
    fn insert(&self, mut link: Link) -> Result<Link> {
        link.validate()?;
        if link.version != 0 {
            return Err(Error::StoreFailed {
                reason: "a new link is not already versioned",
            });
        }
        let mut links = self.lock()?;
        if links.contains_key(&link.id) {
            return Err(Error::DuplicateLink {
                link: link.id.to_string(),
            });
        }
        if live(link.state)
            && links.values().any(|other| {
                live(other.state)
                    && other.parties[0] == link.parties[0]
                    && other.parties[1] == link.parties[1]
            })
        {
            return Err(Error::DuplicateLink {
                link: link.id.to_string(),
            });
        }
        link.version = 1;
        links.insert(link.id, link.clone());
        Ok(link)
    }

    fn get(&self, id: &LinkId) -> Result<Link> {
        self.lock()?
            .get(id)
            .cloned()
            .ok_or_else(|| Error::NotFound {
                link: id.to_string(),
            })
    }

    fn update(&self, mut link: Link, expected_version: u64) -> Result<Link> {
        link.validate()?;
        let mut links = self.lock()?;
        let stored = links.get(&link.id).ok_or_else(|| Error::NotFound {
            link: link.id.to_string(),
        })?;
        if stored.version != expected_version {
            return Err(Error::VersionConflict {
                link: link.id.to_string(),
                expected: expected_version,
                found: stored.version,
            });
        }
        if stored.parties != link.parties
            || stored.local != link.local
            || stored.created != link.created
        {
            return Err(Error::InvalidParty {
                reason: "a link's parties, holder and creation time never change",
            });
        }
        if stored.state != link.state && !stored.state.can_become(link.state) {
            return Err(Error::InvalidTransition {
                link: link.id.to_string(),
                from: stored.state,
                to: link.state,
            });
        }
        link.version = expected_version.checked_add(1).ok_or(Error::StoreFailed {
            reason: "version overflow",
        })?;
        links.insert(link.id, link.clone());
        Ok(link)
    }

    fn find(&self, first: &Party, second: &Party) -> Result<Option<Link>> {
        let links = self.lock()?;
        Ok(links
            .values()
            .find(|link| {
                live(link.state)
                    && link.parties.iter().any(|party| party == first)
                    && link.parties.iter().any(|party| party == second)
            })
            .cloned())
    }

    fn list_for(&self, party: &Party) -> Result<Vec<Link>> {
        let links = self.lock()?;
        Ok(links
            .values()
            .filter(|link| link.parties.iter().any(|candidate| candidate == party))
            .cloned()
            .collect())
    }
}
