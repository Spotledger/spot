//! Serving the node's repair pull and scoped re-send (WP-SYN-02.T7, ADR-064 sections 7 and 8).
//!
//! A pull is the one place a node names a record id of its own choosing, so the hub re-checks
//! everything for every key here. The answer for a key the destination may not see is the single
//! [`PullItem::NotAvailable`], whatever the reason; the reasons are counted for the operator in
//! [`PullReport`] (counts only, never a key).

use crate::deliver::{held_keys, pull_record};
use crate::error::HubError;
use crate::relay::HubRelay;
use crate::traits::{HubStream, SendQueue};
use serde::Serialize;
use spot_sync_closure::{BundleRecord, ImageSource};
use spot_sync_repair_pull::{PullRequest, ScopedResend};

/// The answer for one requested key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum PullItem {
    /// The record, redacted for the destination, without its closure prerequisites (the node asks
    /// again for what it lacks).
    Record(BundleRecord),
    /// The record was deleted and the key is in the destination's scope or extras set.
    Gone,
    /// The single answer for a key that is out of scope, owned by the destination, held, unknown
    /// or of another tenant, or beyond the request bound. It says nothing about which.
    NotAvailable,
}

/// Why keys were answered the way they were: counts for the operator, never ids.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct PullReport {
    /// Keys answered with a record.
    pub served: usize,
    /// Keys answered `Gone`.
    pub gone: usize,
    /// Keys outside the destination's scope and extras set.
    pub out_of_scope: usize,
    /// Keys the destination owns (it never needs them from the hub).
    pub owned: usize,
    /// Keys withheld by a version-skew hold, or whose record needs one.
    pub held: usize,
    /// In-scope keys the hub has no record for.
    pub unknown: usize,
    /// Keys of a request that named another tenant than the relay's.
    pub foreign_tenant: usize,
    /// Keys beyond `RelayConfig::max_pull_keys`.
    pub truncated: usize,
}

/// The answer to a [`PullRequest`]: one item per requested key, in request order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PullAnswer {
    /// One item per requested key, in request order.
    pub items: Vec<PullItem>,
    /// The operator's counts. Not part of what is sent to the node.
    #[serde(skip)]
    pub report: PullReport,
}

impl<Q: SendQueue, S: HubStream, I: ImageSource> HubRelay<Q, S, I> {
    /// Answer a node's pull for the records a parked bundle lacks.
    ///
    /// Per key, in this order: another tenant's request, a key owned by the destination, a key
    /// outside its scope and extras set, a held key and an unknown key are all
    /// [`PullItem::NotAvailable`]; a deleted record in scope is [`PullItem::Gone`]; any other key
    /// is [`PullItem::Record`], built and redacted by the same function as the stream path. At
    /// most `max_pull_keys` keys are answered per request (the rest are `NotAvailable`), and a
    /// destination is answered at most `max_pull_keys_per_tick` keys per tick.
    ///
    /// # Errors
    /// `UnknownDestination`; `PullThrottled` when the request would exceed the tick's allowance
    /// (no work is done and nothing is counted); the image source's or the closure's failure for a
    /// key the destination is entitled to.
    pub fn serve_pull(&mut self, dest: &str, req: &PullRequest) -> Result<PullAnswer, HubError> {
        let Self {
            cfg,
            images,
            refs,
            dests,
            ..
        } = self;
        let st = dests
            .get_mut(dest)
            .ok_or_else(|| HubError::UnknownDestination {
                dest: dest.to_owned(),
            })?;
        let answered = req.keys.len().min(cfg.max_pull_keys);
        if st.pulled_keys.saturating_add(answered) > cfg.max_pull_keys_per_tick {
            return Err(HubError::PullThrottled {
                dest: dest.to_owned(),
                limit: cfg.max_pull_keys_per_tick,
            });
        }
        st.pulled_keys += answered;

        let mut report = PullReport {
            truncated: req.keys.len() - answered,
            ..PullReport::default()
        };
        let mut items = Vec::with_capacity(req.keys.len());
        if req.tenant != cfg.tenant {
            report.foreign_tenant = answered;
            items.resize(req.keys.len(), PullItem::NotAvailable);
            return Ok(PullAnswer { items, report });
        }
        let held = held_keys(st);
        for key in req.keys.iter().take(answered) {
            if st.dest.scope.owns(key) {
                report.owned += 1;
                items.push(PullItem::NotAvailable);
                continue;
            }
            if !(st.dest.scope.covers_key(key) || st.extras.contains(key)) {
                report.out_of_scope += 1;
                items.push(PullItem::NotAvailable);
                continue;
            }
            if held.contains(key) {
                report.held += 1;
                items.push(PullItem::NotAvailable);
                continue;
            }
            let Some(image) = images.image(key)? else {
                report.unknown += 1;
                items.push(PullItem::NotAvailable);
                continue;
            };
            if image.deleted {
                report.gone += 1;
                items.push(PullItem::Gone);
                continue;
            }
            match pull_record(images, refs.as_ref(), cfg, st, image)? {
                Some(record) => {
                    report.served += 1;
                    items.push(PullItem::Record(record));
                }
                None => {
                    report.held += 1;
                    items.push(PullItem::NotAvailable);
                }
            }
        }
        items.resize(req.keys.len(), PullItem::NotAvailable);
        Ok(PullAnswer { items, report })
    }

    /// Serve a node's scoped re-send: everything of its scope from `from_position` on goes out
    /// again over the following ticks.
    ///
    /// The scope string must equal the identity the hub registered for the destination
    /// ([`Destination::with_scope_identity`](crate::Destination::with_scope_identity), hub data,
    /// not a node claim) and the tenant must be the relay's; otherwise `ResendScopeMismatch` and
    /// nothing changes. `from_position` (`None` is the start) is clamped to the hub's own head.
    /// The destination's catch-up position is lowered to it and its acknowledged prefix to
    /// `from_position - 1` (neither is ever raised): the stream path skips what is at or below the
    /// acknowledged prefix, so without lowering it a node that lost data below its old prefix
    /// would be re-sent nothing. Queue idempotency makes duplicates harmless.
    ///
    /// # Errors
    /// `UnknownDestination`, `ResendScopeMismatch` or `StreamFailed` (the head cannot be read).
    pub fn serve_resend(&mut self, dest: &str, req: &ScopedResend) -> Result<(), HubError> {
        let head = self.stream.head()?;
        let tenant_ok = req.tenant == self.cfg.tenant;
        let st = self
            .dests
            .get_mut(dest)
            .ok_or_else(|| HubError::UnknownDestination {
                dest: dest.to_owned(),
            })?;
        if !tenant_ok || st.dest.scope_identity.as_deref() != Some(req.scope.as_str()) {
            return Err(HubError::ResendScopeMismatch {
                dest: dest.to_owned(),
            });
        }
        let from = req.from_position.unwrap_or(0).min(head);
        st.catchup_from = Some(st.catchup_from.map_or(from, |c| c.min(from)));
        if let Some(acked) = st.acked {
            st.acked = from.checked_sub(1).map(|prefix| acked.min(prefix));
        }
        Ok(())
    }
}
