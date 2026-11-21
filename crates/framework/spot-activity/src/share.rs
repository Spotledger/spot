//! Share grants and the rights a user effectively holds (WP-DOC-07.T1).
//!
//! A [`Share`] grants rights on one document to one grantee. [`effective_share_rights`]
//! computes what a user holds *now*: every unexpired grant, with a later grant for the same
//! (doc, grantee) pair replacing the earlier one, and an expired grant ignored — including on
//! the exact expiry boundary (a grant expired when `now >= expires_at`; the boundary test is
//! in the suite).
//!
//! Rights union: holding `Read` from one live grant and `Write` from another means both, so a
//! user's effective rights are the bitwise union of the live grants' right sets. Expiry is
//! compared against the `now` parameter — the function never reads a clock (blueprint §4).

use serde::{Deserialize, Serialize};
use spot_app_api::prelude::{OffsetDateTime, RecordId};
use std::time::Duration;

use crate::error::ActivityError;

/// One right a share can grant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareRight {
    /// Read the document.
    Read,
    /// Write (edit) it.
    Write,
    /// Share it onward.
    Share,
    /// Submit (post) it.
    Submit,
}

/// The rights of one share: any non-empty subset. A grant with none is refused on
/// construction ([`Share::new`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ShareRights(Vec<ShareRight>);

impl ShareRights {
    /// The rights from an iterator, deduplicated and in canonical order (`Read < Write <
    /// Share < Submit`, the enum's order) so equal right sets serialize the same.
    ///
    /// # Errors
    ///
    /// [`ActivityError::PathInvalid`] (the closest stable code: a malformed record) when the
    /// set is empty — a share without a right grants nothing and says nothing.
    pub fn new(rights: impl IntoIterator<Item = ShareRight>) -> Result<ShareRights, ActivityError> {
        let set: std::collections::BTreeSet<ShareRight> = rights.into_iter().collect();
        if set.is_empty() {
            return Err(ActivityError::PathInvalid(
                "a share must grant at least one right".to_owned(),
            ));
        }
        Ok(ShareRights(set.into_iter().collect()))
    }

    /// The rights, in canonical order.
    #[must_use]
    pub fn as_slice(&self) -> &[ShareRight] {
        &self.0
    }

    /// Whether the set holds `right`.
    #[must_use]
    pub fn contains(&self, right: ShareRight) -> bool {
        self.0.contains(&right)
    }

    /// The union of two sets, canonical order kept.
    #[must_use]
    pub fn union(&self, other: &ShareRights) -> ShareRights {
        let mut set: std::collections::BTreeSet<ShareRight> = self.0.iter().copied().collect();
        set.extend(other.0.iter().copied());
        ShareRights(set.into_iter().collect())
    }

    /// True when the set holds nothing (cannot be constructed through [`ShareRights::new`];
    /// only a rebuilt-from-storage record can).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// One grant of rights on a document to a grantee.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Share {
    /// The document the rights are on.
    #[serde(with = "crate::wire::record_id")]
    pub doc: RecordId,
    /// Who holds the rights, as the user's storage name.
    pub grantee: String,
    /// The rights granted.
    pub rights: ShareRights,
    /// Who granted them (the user's or component's name).
    pub granted_by: String,
    /// When the grant was made (a parameter).
    #[serde(with = "crate::wire::rfc3339")]
    pub at: OffsetDateTime,
    /// When the grant expires, when it does. A grant expires when `now >= expires_at` (the
    /// exact boundary is an expiry, tested in the suite).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::wire::opt_rfc3339"
    )]
    pub expires_at: Option<OffsetDateTime>,
}

impl Share {
    /// A new grant; fails on an empty right set.
    ///
    /// # Errors
    ///
    /// [`ActivityError::PathInvalid`] when `rights` is empty.
    pub fn new(
        doc: RecordId,
        grantee: &str,
        rights: impl IntoIterator<Item = ShareRight>,
        granted_by: &str,
        at: OffsetDateTime,
        expires_at: Option<OffsetDateTime>,
    ) -> Result<Share, ActivityError> {
        Ok(Share {
            doc,
            grantee: grantee.to_owned(),
            rights: ShareRights::new(rights)?,
            granted_by: granted_by.to_owned(),
            at,
            expires_at,
        })
    }

    /// Whether the grant is still alive at `now`.
    #[must_use]
    pub fn is_live_at(&self, now: OffsetDateTime) -> bool {
        match self.expires_at {
            None => true,
            Some(expires) => now < expires,
        }
    }
}

/// The rights `user` effectively holds on `doc` at `now`.
///
/// Rules, in order:
///
/// 1. only grants for `doc` and `user` (or for one of the user's `roles`, granted to the role's
///    name) count;
/// 2. expired grants are ignored — `now >= expires_at` is expired, the exact boundary tested;
/// 3. among the grants that remain, a **later** grant (`at`, then list position) for the same
///    (doc, grantee) pair replaces the earlier — a replacement, not a union;
/// 4. the surviving grants' rights **union** (one live `Read` plus one live `Write` is both).
///
/// Read-only in every input; the caller passes the clock value.
#[must_use]
pub fn effective_share_rights(
    shares: &[Share],
    doc: RecordId,
    user: &str,
    roles: &[&str],
    now: OffsetDateTime,
) -> ShareRights {
    let mut principals: Vec<&str> = vec![user];
    principals.extend(roles.iter().copied());
    // The latest live grant per principal (replacement beats union).
    let mut latest: Vec<(&str, &Share)> = Vec::new();
    for share in shares {
        if share.doc != doc || !principals.contains(&share.grantee.as_str()) {
            continue;
        }
        if !share.is_live_at(now) {
            continue;
        }
        match latest
            .iter_mut()
            .find(|(p, _)| *p == share.grantee.as_str())
        {
            Some((_, held)) if held.at > share.at => {}
            Some(slot) => *slot = (slot.0, share),
            None => latest.push((share.grantee.as_str(), share)),
        }
    }
    // Union across principals.
    let mut set: std::collections::BTreeSet<ShareRight> = std::collections::BTreeSet::new();
    for (_, share) in &latest {
        set.extend(share.rights.as_slice().iter().copied());
    }
    ShareRights(set.into_iter().collect())
}

/// How long a share lives when the caller gives no expiry — a parameter, defined here so
/// callers share one constant. Seven days.
pub const DEFAULT_SHARE_DURATION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
