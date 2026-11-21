//! Plain data types: events, obligations, settings, levels and notifications.

use serde::{Deserialize, Serialize};

use crate::error::ObligationError;

const HOUR_MS: i64 = 3_600_000;

/// A mandatory event of a source stream that is in scope for a destination.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MandatoryEvent {
    /// Position in the source stream.
    pub position: u64,
    /// When the event was published, milliseconds since the Unix epoch.
    pub published_at_millis: i64,
    /// When the change takes effect, when the policy names an effective-from field.
    pub effective_from_millis: Option<i64>,
    /// The record the event is about (an opaque key such as `tax_rate/v7`).
    pub record: String,
}

/// An outstanding obligation: a mandatory event the destination has not applied.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Obligation {
    /// The event that must be applied.
    pub event: MandatoryEvent,
    /// True when the position is at or below the applied prefix but the record sits in a hold
    /// (parked, held for a version or quarantined), so it is still unsatisfied.
    pub held: bool,
}

/// The tunable settings of the ledger (ADR-065 accepted values; all are arguments).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObligationSettings {
    /// Time allowed from publication to application. Default 24 hours.
    pub delivery_window_millis: i64,
    /// Margin before an effective date by which the change must be applied. Default 12 hours.
    pub safety_margin_millis: i64,
    /// Red grace as a whole percent of the delivery window. Default 25.
    pub grace_percent: u32,
    /// Lower bound of the red grace. Default 2 hours.
    pub grace_min_millis: i64,
    /// An unapplied obligation whose effective date is this close is red (ADR-065 section 6).
    /// Default 24 hours.
    pub effective_red_lead_millis: i64,
}

impl Default for ObligationSettings {
    fn default() -> Self {
        Self {
            delivery_window_millis: 24 * HOUR_MS,
            safety_margin_millis: 12 * HOUR_MS,
            grace_percent: 25,
            grace_min_millis: 2 * HOUR_MS,
            effective_red_lead_millis: 24 * HOUR_MS,
        }
    }
}

impl ObligationSettings {
    /// Check every field is in range.
    ///
    /// # Errors
    /// [`ObligationError::SettingsInvalid`] naming the first bad field.
    pub fn validate(&self) -> Result<(), ObligationError> {
        if self.delivery_window_millis <= 0 {
            return Err(ObligationError::SettingsInvalid("delivery_window_millis"));
        }
        if self.safety_margin_millis < 0 {
            return Err(ObligationError::SettingsInvalid("safety_margin_millis"));
        }
        if self.grace_percent > 1000 {
            return Err(ObligationError::SettingsInvalid("grace_percent"));
        }
        if self.grace_min_millis < 0 {
            return Err(ObligationError::SettingsInvalid("grace_min_millis"));
        }
        if self.effective_red_lead_millis < 0 {
            return Err(ObligationError::SettingsInvalid(
                "effective_red_lead_millis",
            ));
        }
        Ok(())
    }

    /// The red grace: the window fraction, but at least the minimum.
    #[must_use]
    pub fn grace_millis(&self) -> i64 {
        let fraction = self
            .delivery_window_millis
            .saturating_mul(i64::from(self.grace_percent))
            / 100;
        fraction.max(self.grace_min_millis)
    }
}

/// The escalation level of an obligation or of a node (the worst of its obligations).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Level {
    /// Inside the first half of the window.
    Ok,
    /// Past half the window: information, nobody is paged.
    Amber,
    /// Past the deadline: the node's contact is notified.
    Overdue,
    /// Past the grace period, or an effective date is near: `sync_admin` is notified.
    Red,
}

/// Who a notification goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Recipient {
    /// The contact recorded on the node in the registry.
    NodeContact,
    /// The `sync_admin` role.
    SyncAdmin,
}

/// A notification to send, produced only on a level transition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notify {
    /// Who receives it.
    pub who: Recipient,
    /// The level that triggered it.
    pub level: Level,
    /// The node the obligations are outstanding at.
    pub node: String,
    /// Positions of the obligations at or above `level`, in position order.
    pub obligations: Vec<u64>,
}

/// The result of one [`ladder`](crate::ladder) evaluation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LadderOutcome {
    /// The node's level now; the caller stores it as the last notified level.
    pub level: Level,
    /// Notifications to send now (empty when the level did not rise).
    pub notifications: Vec<Notify>,
}

/// The audit event written when a node's applied prefix crosses mandatory events.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObligationSatisfied {
    /// The node that applied.
    pub node: String,
    /// The applied prefix after the crossing.
    pub through_position: u64,
    /// How many mandatory events the crossing covered.
    pub count: u64,
    /// When the node says it applied, milliseconds.
    pub applied_at_reported_millis: i64,
    /// When the hub observed the report, milliseconds.
    pub received_at_observed_millis: i64,
}
