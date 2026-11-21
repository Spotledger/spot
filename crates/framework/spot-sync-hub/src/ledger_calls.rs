//! The relay's calls into the acknowledgement book. Each passes the relay's **own** configured
//! tenant ([`RelayConfig::tenant`](crate::RelayConfig)); no node-supplied value names a tenant.

use crate::error::HubError;
use crate::model::{EntryRole, StreamEntry};
use crate::relay::HubRelay;
use crate::traits::{HubStream, SendQueue};
use spot_sync_acks::{NodeReportRow, StreamHead};
use spot_sync_closure::ImageSource;
use spot_sync_obligations::{Lane, MandatoryEvent};

impl<Q: SendQueue, S: HubStream, I: ImageSource> HubRelay<Q, S, I> {
    fn book(&mut self) -> Result<&mut Box<dyn crate::traits::AckBook>, HubError> {
        self.ledger.as_mut().ok_or(HubError::InvalidConfig {
            reason: "no acknowledgement ledger is set",
        })
    }

    fn known(&self, dest: &str) -> Result<(), HubError> {
        if self.dests.contains_key(dest) {
            Ok(())
        } else {
            Err(HubError::UnknownDestination {
                dest: dest.to_owned(),
            })
        }
    }

    /// Register a destination with the ledger for this relay's tenant, with its software
    /// version tag, at hub time `at`. Idempotent; returns whether the pair was new. A node
    /// registered here is known to this relay's tenant only.
    ///
    /// # Errors
    /// `UnknownDestination`, `InvalidConfig` when no ledger is set, or the ledger's refusal.
    pub fn register_node(&mut self, dest: &str, version: &str, at: i64) -> Result<bool, HubError> {
        self.known(dest)?;
        let tenant = self.cfg.tenant.clone();
        self.book()?
            .register_node(dest, &tenant, version, at)
            .map_err(|e| self.ack_failure(dest, e))
    }

    /// Register a mandatory-lane stream entry as a mandatory event of this relay's tenant, at
    /// the entry's position in **this relay's** stream. `published_at_millis` and
    /// `effective_from_millis` come from the host (the policy), not from a node.
    ///
    /// An entry of another tenant or source, or not on the mandatory lane, is dropped before
    /// registration and registers nothing (`Ok(false)`). A satellite of a transaction is not a
    /// root event and is dropped too. Returns whether the event was new.
    ///
    /// # Errors
    /// `InvalidConfig` when no ledger is set.
    pub fn register_mandatory(
        &mut self,
        entry: &StreamEntry,
        published_at_millis: i64,
        effective_from_millis: Option<i64>,
    ) -> Result<bool, HubError> {
        if entry.tenant != self.cfg.tenant
            || entry.source != self.cfg.source
            || entry.lane != Lane::Mandatory
            || entry.role != EntryRole::Root
        {
            return Ok(false);
        }
        let tenant = self.cfg.tenant.clone();
        let event = MandatoryEvent {
            position: entry.position,
            published_at_millis,
            effective_from_millis,
            record: format!("{}/{}", entry.key.entity, entry.key.id),
        };
        Ok(self.book()?.register_mandatory(&tenant, event))
    }

    /// The destination reported at the hub time of the last tick and applied nothing new: as
    /// [`on_checkin_at`](Self::on_checkin_at).
    ///
    /// # Errors
    /// As [`on_checkin_at`](Self::on_checkin_at).
    pub fn on_checkin(&mut self, dest: &str) -> Result<(), HubError> {
        let at = self.last_now;
        self.on_checkin_at(dest, at)
    }

    /// The destination reported at hub time `at` and applied nothing new. Only this relay's
    /// tenant record of the node is touched.
    ///
    /// # Errors
    /// `UnknownDestination`, `InvalidConfig` when no ledger is set, or the ledger's refusal.
    pub fn on_checkin_at(&mut self, dest: &str, at: i64) -> Result<(), HubError> {
        self.known(dest)?;
        let tenant = self.cfg.tenant.clone();
        self.book()?
            .on_checkin(dest, &tenant, at)
            .map_err(|e| self.ack_failure(dest, e))
    }

    /// The destination reports the mandatory positions it holds back, heard at hub time `at`.
    /// The positions are bounded by the head of this relay's tenant stream, read from the hub;
    /// one above it marks the destination suspect.
    ///
    /// # Errors
    /// `UnknownDestination`, `StreamFailed`, `InvalidConfig` when no ledger is set, or the
    /// ledger's refusal.
    pub fn set_holds_at(&mut self, dest: &str, holds: Vec<u64>, at: i64) -> Result<(), HubError> {
        self.known(dest)?;
        let head = self.stream.head()?;
        let tenant = self.cfg.tenant.clone();
        self.book()?
            .set_holds(dest, &tenant, holds, StreamHead::new(head), at)
            .map_err(|e| self.ack_failure(dest, e))
    }

    /// Who has not applied this relay's tenant's mandatory event at `position`.
    ///
    /// # Errors
    /// `InvalidConfig` when no ledger is set, or `UnknownEvent` from the ledger.
    pub fn who_is_missing(&self, position: u64) -> Result<Vec<NodeReportRow>, HubError> {
        let Some(book) = self.ledger.as_ref() else {
            return Err(HubError::InvalidConfig {
                reason: "no acknowledgement ledger is set",
            });
        };
        book.who_is_missing(&self.cfg.tenant, position)
            .map_err(HubError::Ack)
    }
}
