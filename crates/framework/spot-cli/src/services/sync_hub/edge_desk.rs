//! The hub's [`EdgeDesk`] (WP-AI-07.T9): every enrolled edge with its expected and actual queue
//! position, and a resync of one edge through the catch-up path. It is the seam the remedy host's
//! edge part (`world_access/edge.rs`) is written over; before this task no module provided it, so
//! `edge.repair_drift` answered "no edge" on every site.
//!
//! # Where the positions come from
//!
//! The relay keeps one durable queue per destination edge and its driver thread publishes each
//! destination's window into the [`ServeCache`] (`spot-sync-hub-host`). The queue numbers its items
//! by *serve sequence* and trims an item when the edge's next pull acknowledges it, so the window
//! of a destination is the run of items the edge has **not** acknowledged yet:
//!
//! - `actual` is the position the edge has acknowledged: the serve sequence just below the front
//!   of the window;
//! - `expected` is the relay's head for the destination: the newest serve sequence the relay
//!   assigned, which the driver publishes into the cache with each round when its core can say
//!   (`ServeCache::head`, WP-AI-07.T16), so it is exact however deep the queue is. When the core
//!   publishes no head the desk falls back to `actual` plus the number of items in the published
//!   window (at most the driver's `window` setting), which understates a queue deeper than the
//!   window.
//!
//! An edge with nothing waiting is in sync (`expected == actual`); its position is the published
//! head, else the head the desk saw last (0 until it has seen one). Both numbers are counts read through the cache's
//! public methods, never a table, and the reads take no relay lock and send no acknowledgement
//! (a read from sequence 1 never acknowledges). The two reads (`depth` and the front record) are
//! separate, so a round of the driver between them can make one reading a step off; the next
//! reading is right.
//!
//! # Who is listed
//!
//! An [`EdgeRoster`] names the enrolled edges and whether each is revoked. The roster is chosen
//! when the service registers: a module's own `dyn EdgeRoster` port first; else
//! [`DirectoryRoster`] over the hub's enrolment directory (WP-AI-07.T16), which sees every
//! enrolled edge with its status; else, on a hub with no directory, the relay's own destinations
//! ([`RelayRoster`]), which cannot see a revocation. A revoked edge is neither listed nor
//! resynced. A revocation made through this process shows at once; one made elsewhere shows after
//! the directory's next refresh, which `sync_hub_parts` runs every `directory_refresh_secs`
//! (1 to 5, default 2). A directory that could not refresh for 30 seconds is unreadable and the
//! desk says so rather than list a stale roster.
//!
//! # Resync
//!
//! `resync(edge)` asks the catch-up path ([`Catchup`], implemented for `CatchupHub`) how the edge
//! comes back: the same call an edge's own catch-up request makes, decided from the edge's own
//! acknowledged position. It never deletes or rewrites relay data and never skips a queued item;
//! asking twice is the same as asking once (a stored plan resumes). It then answers the edge's
//! state. Where no catch-up path is wired (the catch-up wiring needs a feed, a chunk server and a
//! director, see `sync_catchup_feed`) the answer is the fixed refusal `not supported`.
//!
//! Every refusal is a fixed sentence; logs name an edge id and nothing else.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use spot_sync_enrol::{DurableDirectory, NodeStatus};
use spot_sync_http::HubSource;
use spot_sync_hub_host::ServeCache;
use spot_sync_protocol::SourceId;

use crate::services::sync_catchup_wiring::CatchupHub;
use crate::services::world_access::edge::{DeskFuture, EdgeDesk, EdgeState};

/// The hub cannot read its edges now.
pub const UNREADABLE: &str = "the hub cannot read its edges";
/// No enrolled edge of that name.
pub const UNKNOWN_EDGE: &str = "no such edge";
/// The edge is revoked.
pub const REVOKED_EDGE: &str = "the edge is revoked";
/// No catch-up path is wired on this hub.
pub const NOT_SUPPORTED: &str = "not supported";
/// The catch-up path refused the edge.
pub const CATCHUP_REFUSED: &str = "the catch-up path refused the edge";

/// The cache of the running hub, shared between the service (which creates it when it runs) and
/// the desk (which reads it). Empty before the service runs and after it stopped.
#[derive(Default)]
pub struct EdgeLink {
    cache: Mutex<Option<Arc<ServeCache>>>,
}

impl EdgeLink {
    /// Records the serve cache of the running hub.
    pub fn attach(&self, cache: Arc<ServeCache>) {
        *self.cache.lock().unwrap_or_else(PoisonError::into_inner) = Some(cache);
    }

    /// Forgets the cache (the hub stopped).
    pub fn detach(&self) {
        *self.cache.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// The cache of the running hub, if it runs.
    #[must_use]
    pub fn cache(&self) -> Option<Arc<ServeCache>> {
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// One enrolled edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeStatus {
    /// The edge's name: the partition of its node id, the relay's destination.
    pub edge: String,
    /// Whether its enrolment was revoked.
    pub revoked: bool,
}

/// The enrolled edges of the hub's site.
pub trait EdgeRoster: Send + Sync {
    /// Every enrolled edge, revoked ones included (marked).
    ///
    /// # Errors
    ///
    /// A fixed reason when the roster cannot be read.
    fn enrolled(&self) -> Result<Vec<EdgeStatus>, &'static str>;

    /// The node id (`<site>#<partition>`) the catch-up path knows `edge` by, when the roster
    /// knows it. The default is `None`: the catch-up path is then asked under the edge's own name.
    fn node_of(&self, _edge: &str) -> Option<String> {
        None
    }
}

/// The roster read from the relay's own destinations: every destination the driver publishes,
/// none revoked (the relay does not know a revocation).
pub struct RelayRoster {
    link: Arc<EdgeLink>,
}

impl RelayRoster {
    /// A roster over the running hub's cache.
    #[must_use]
    pub fn new(link: Arc<EdgeLink>) -> Self {
        Self { link }
    }
}

impl EdgeRoster for RelayRoster {
    fn enrolled(&self) -> Result<Vec<EdgeStatus>, &'static str> {
        let cache = self.link.cache().ok_or(UNREADABLE)?;
        Ok(cache
            .destinations()
            .into_iter()
            .map(|edge| EdgeStatus {
                edge,
                revoked: false,
            })
            .collect())
    }
}

/// The roster read from the hub's enrolment directory (WP-AI-07.T16): every enrolled edge with its
/// status. It reads the directory's in-memory snapshot, never the database, so a call is cheap and
/// takes no blocking read. The directory is the tenant's own (its store is per tenant), so every
/// node in it is this hub's. An edge is named by the partition of its node id
/// (`<site>#<partition>`, the relay's destination); a node id with no partition is not an edge the
/// relay can serve and is not listed. A pending enrolment (a code not yet redeemed) is not listed;
/// a revoked edge is listed with `revoked = true`; a suspended one is listed as live (the desk
/// shows its queue, and the relay is what stops serving it).
pub struct DirectoryRoster {
    directory: Arc<DurableDirectory>,
}

impl DirectoryRoster {
    /// A roster over `directory`.
    #[must_use]
    pub fn new(directory: Arc<DurableDirectory>) -> Self {
        Self { directory }
    }
}

impl EdgeRoster for DirectoryRoster {
    fn enrolled(&self) -> Result<Vec<EdgeStatus>, &'static str> {
        // An empty list from a directory too old to trust is not "no edges".
        if !self.directory.is_fresh() {
            return Err(UNREADABLE);
        }
        Ok(self
            .directory
            .enrolled()
            .into_iter()
            .filter_map(|(node, status)| {
                node.partition().map(|edge| EdgeStatus {
                    edge: edge.to_owned(),
                    revoked: status == NodeStatus::Revoked,
                })
            })
            .collect())
    }

    fn node_of(&self, edge: &str) -> Option<String> {
        self.directory
            .enrolled()
            .into_iter()
            .find(|(node, _)| node.partition() == Some(edge))
            .map(|(node, _)| node.as_str().to_owned())
    }
}

/// The catch-up path, as far as the desk needs it.
pub trait Catchup: Send + Sync {
    /// Restarts `edge` from its own acknowledged position. Blocking (the director's store is).
    ///
    /// # Errors
    ///
    /// A fixed reason when the path refuses the edge.
    fn restart(&self, edge: &str) -> Result<(), &'static str>;
}

impl Catchup for CatchupHub {
    fn restart(&self, edge: &str) -> Result<(), &'static str> {
        // The same call an edge's own catch-up request makes: the director decides from the
        // edge's gap, never from the caller. A stored plan resumes; nothing is deleted.
        self.route(edge).map(|_| ()).map_err(|_| CATCHUP_REFUSED)
    }
}

/// The expected and actual position from what the cache publishes for one destination.
///
/// `front` is the serve sequence of the oldest waiting item, `depth` how many wait, `last` the
/// head the desk saw when it last saw items (0 before). Nothing waiting is in sync at `last`.
#[must_use]
pub fn positions(front: Option<u64>, depth: usize, last: u64) -> (u64, u64) {
    match front {
        Some(front) if depth > 0 => {
            let actual = front.saturating_sub(1);
            let waiting = u64::try_from(depth).unwrap_or(u64::MAX);
            (actual.saturating_add(waiting), actual)
        }
        _ => (last, last),
    }
}

/// As [`positions`], with the relay's published queue head when the cache has one: `expected` is
/// then the head itself (never below what the window shows), exact however deep the queue is. With
/// no head it is [`positions`].
#[must_use]
pub fn positions_with_head(
    front: Option<u64>,
    depth: usize,
    last: u64,
    head: Option<u64>,
) -> (u64, u64) {
    let Some(head) = head else {
        return positions(front, depth, last);
    };
    match front {
        Some(front) if depth > 0 => {
            let actual = front.saturating_sub(1);
            let waiting = u64::try_from(depth).unwrap_or(u64::MAX);
            (head.max(actual.saturating_add(waiting)), actual)
        }
        // Nothing waiting: the edge has acknowledged everything the relay assigned.
        _ => (head, head),
    }
}

/// The hub's desk over the serve cache, a roster and (when wired) the catch-up path.
pub struct HubEdgeDesk {
    site: String,
    link: Arc<EdgeLink>,
    roster: Arc<dyn EdgeRoster>,
    catchup: Option<Arc<dyn Catchup>>,
    heads: Mutex<BTreeMap<String, u64>>,
}

impl HubEdgeDesk {
    /// A desk for the hub of `site`.
    #[must_use]
    pub fn new(
        site: &str,
        link: Arc<EdgeLink>,
        roster: Arc<dyn EdgeRoster>,
        catchup: Option<Arc<dyn Catchup>>,
    ) -> Self {
        Self {
            site: site.to_owned(),
            link,
            roster,
            catchup,
            heads: Mutex::new(BTreeMap::new()),
        }
    }

    /// The state of one edge from the cache.
    fn state_of(&self, cache: &ServeCache, edge: &str) -> Result<EdgeState, &'static str> {
        let source = SourceId::new(&format!("{}#{edge}", self.site)).map_err(|_| UNREADABLE)?;
        let depth = cache.depth(edge);
        // A read from sequence 1 acknowledges nothing; one record is enough to see the front.
        let front = cache
            .read(&source, 1, 1)
            .map_err(|_| UNREADABLE)?
            .first()
            .map(|record| record.seq);
        let mut heads = self.heads.lock().unwrap_or_else(PoisonError::into_inner);
        let last = heads.get(edge).copied().unwrap_or(0);
        let (expected, actual) = positions_with_head(front, depth, last, cache.head(edge));
        heads.insert(edge.to_owned(), expected);
        Ok(EdgeState {
            edge: edge.to_owned(),
            expected,
            actual,
        })
    }
}

impl EdgeDesk for HubEdgeDesk {
    fn edges(&self) -> DeskFuture<'_, Vec<EdgeState>> {
        Box::pin(async move {
            let cache = self.link.cache().ok_or(UNREADABLE)?;
            let mut roster = self.roster.enrolled()?;
            roster.sort_by(|a, b| a.edge.cmp(&b.edge));
            let mut states = Vec::with_capacity(roster.len());
            for entry in roster.iter().filter(|entry| !entry.revoked) {
                states.push(self.state_of(&cache, &entry.edge)?);
            }
            Ok::<Vec<EdgeState>, &'static str>(states)
        })
    }

    fn resync<'a>(&'a self, edge: &'a str) -> DeskFuture<'a, EdgeState> {
        Box::pin(async move {
            let cache = self.link.cache().ok_or(UNREADABLE)?;
            let roster = self.roster.enrolled()?;
            match roster.iter().find(|entry| entry.edge == edge) {
                None => return Err(UNKNOWN_EDGE),
                Some(entry) if entry.revoked => return Err(REVOKED_EDGE),
                Some(_) => {}
            }
            let catchup = self.catchup.clone().ok_or(NOT_SUPPORTED)?;
            // The catch-up path knows a node by its id; the desk by its partition. Asking under
            // the id is what makes this the same call the edge's own catch-up request makes.
            let name = self.roster.node_of(edge).unwrap_or_else(|| edge.to_owned());
            // The director's store is blocking and refuses a runtime thread.
            let restarted = tokio::task::spawn_blocking(move || catchup.restart(&name))
                .await
                .map_err(|_| UNREADABLE)?;
            restarted?;
            tracing::info!(
                edge,
                "sync_hub: an edge was asked to catch up from its position"
            );
            self.state_of(&cache, edge)
        })
    }
}

fn provided() -> &'static Mutex<BTreeMap<String, Arc<dyn EdgeDesk>>> {
    static PROVIDED: OnceLock<Mutex<BTreeMap<String, Arc<dyn EdgeDesk>>>> = OnceLock::new();
    PROVIDED.get_or_init(Mutex::default)
}

/// Records the desk a site provides, so a reload provides the same one.
pub fn remember(site: &str, desk: Arc<dyn EdgeDesk>) {
    provided()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(site.to_owned(), desk);
}

/// The desk `site` provided at its first build, if any.
#[must_use]
pub fn recall(site: &str) -> Option<Arc<dyn EdgeDesk>> {
    provided()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(site)
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_waiting_is_in_sync_at_the_last_head() {
        assert_eq!(positions(None, 0, 0), (0, 0));
        assert_eq!(positions(None, 0, 12), (12, 12));
        // A front with no depth is a reading that raced a round: treat it as in sync.
        assert_eq!(positions(Some(5), 0, 4), (4, 4));
    }

    #[test]
    fn waiting_items_put_the_head_above_the_acknowledged_position() {
        // Items 8, 9, 10 wait: acknowledged through 7, head 10.
        assert_eq!(positions(Some(8), 3, 0), (10, 7));
        assert_eq!(positions(Some(1), 1, 0), (1, 0));
    }

    #[test]
    fn a_published_head_makes_the_expected_position_exact() {
        // A queue of 50 items behind a window of 5: acknowledged through 10, window 11..=15,
        // head 60. The window alone would say 15.
        assert_eq!(positions_with_head(Some(11), 5, 0, Some(60)), (60, 10));
        assert_eq!(positions(Some(11), 5, 0), (15, 10));
        // Nothing waiting: in sync at the head.
        assert_eq!(positions_with_head(None, 0, 3, Some(60)), (60, 60));
        // A head below what the window shows (a reading that raced a round) never lowers it.
        assert_eq!(positions_with_head(Some(11), 5, 0, Some(12)), (15, 10));
        // No head: the old reading.
        assert_eq!(
            positions_with_head(Some(8), 3, 0, None),
            positions(Some(8), 3, 0)
        );
        assert_eq!(positions_with_head(None, 0, 12, None), (12, 12));
    }

    #[test]
    fn a_huge_depth_saturates_instead_of_wrapping() {
        let (expected, actual) = positions(Some(u64::MAX), usize::MAX, 0);
        assert!(expected >= actual);
    }

    #[test]
    fn the_link_holds_the_cache_only_while_the_hub_runs() {
        let link = EdgeLink::default();
        assert!(link.cache().is_none());
        let (cache, _acks) = ServeCache::new();
        link.attach(cache);
        assert!(link.cache().is_some());
        link.detach();
        assert!(link.cache().is_none());
    }
}
