//! The move plan, its checked state machine and the persisted halt record.

use std::fmt;

use serde::{Deserialize, Serialize};
use spot_tenancy::{Placement, Region, TenantId};

use crate::error::{Error, Result};

/// How a move may treat the source while the dump is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveMode {
    /// The source is closed for the whole move; the window is explicit.
    Offline,
    /// The source may keep serving until the dump starts, for the shortest possible
    /// cut-over window. The state machine still quiesces before the export, because a
    /// fingerprint may only be compared against a source that cannot change.
    NearOnline,
}

/// A stable move id: 3 to 64 lower-case letters, digits, `-` or `_`, starting with a letter.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct MoveId(String);

impl MoveId {
    /// Validates and wraps the id.
    ///
    /// # Errors
    /// [`Error::InvalidMoveId`] when the id is empty, too long or has a forbidden character.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        let valid = (3..=64).contains(&value.len())
            && value.starts_with(|c: char| c.is_ascii_lowercase())
            && value
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
        if valid {
            Ok(Self(value))
        } else {
            Err(Error::InvalidMoveId {
                reason: "a move id is 3 to 64 lower-case letters, digits, '-' or '_', starting with a letter",
            })
        }
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for MoveId {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl From<MoveId> for String {
    fn from(value: MoveId) -> Self {
        value.0
    }
}

impl fmt::Display for MoveId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The plan for one tenant move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovePlan {
    /// The tenant that moves.
    pub tenant: TenantId,
    /// The placement being left.
    pub from: Placement,
    /// The placement being filled.
    pub to: Placement,
    /// The destination region; the residency policy is asked about exactly this region.
    pub region: Region,
    /// Whether the source is closed for the whole move or only at cut-over.
    pub mode: MoveMode,
}

impl MovePlan {
    /// Builds and validates a plan.
    ///
    /// # Errors
    /// [`Error::InvalidPlan`] when source and target are the same placement or a schema is
    /// not the one derived from the tenant id.
    pub fn new(
        tenant: TenantId,
        from: Placement,
        to: Placement,
        region: Region,
        mode: MoveMode,
    ) -> Result<Self> {
        let plan = Self {
            tenant,
            from,
            to,
            region,
            mode,
        };
        plan.validate()?;
        Ok(plan)
    }

    /// Re-checks the plan's invariants.
    ///
    /// # Errors
    /// [`Error::InvalidPlan`] naming the broken rule.
    pub fn validate(&self) -> Result<()> {
        if self.from == self.to {
            return Err(Error::InvalidPlan {
                reason: "the source and target placement are the same",
            });
        }
        self.from
            .validate_for(&self.tenant)
            .map_err(|_| Error::InvalidPlan {
                reason: "the source schema is not the one derived from the tenant id",
            })?;
        self.to
            .validate_for(&self.tenant)
            .map_err(|_| Error::InvalidPlan {
                reason: "the target schema is not the one derived from the tenant id",
            })?;
        Ok(())
    }
}

/// The forward-only states of a move. `Aborted` is the terminal off-path state an
/// operator reaches through [`crate::MoveEngine::abort`] before cut-over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveState {
    /// The plan is recorded and the residency policy has allowed it.
    Planned,
    /// The admission gate is closed and the side-effect hold is engaged.
    Quiesced,
    /// Every export chunk is checksummed and accounted.
    Exported,
    /// Every exported chunk has been imported into the target.
    Imported,
    /// The source and target fingerprints are identical.
    Verified,
    /// The placement map points at the target and the routing caches are invalid.
    CutOver,
    /// The hold is released, the gate is open and the transfer staging is discarded.
    Cleaned,
    /// The move was rolled back before cut-over; the source serves again.
    Aborted,
}

impl MoveState {
    /// The fixed name used in reports and logs.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Quiesced => "quiesced",
            Self::Exported => "exported",
            Self::Imported => "imported",
            Self::Verified => "verified",
            Self::CutOver => "cut_over",
            Self::Cleaned => "cleaned",
            Self::Aborted => "aborted",
        }
    }

    /// The next forward state, or `None` at a terminal state.
    #[must_use]
    pub fn next(self) -> Option<Self> {
        match self {
            Self::Planned => Some(Self::Quiesced),
            Self::Quiesced => Some(Self::Exported),
            Self::Exported => Some(Self::Imported),
            Self::Imported => Some(Self::Verified),
            Self::Verified => Some(Self::CutOver),
            Self::CutOver => Some(Self::Cleaned),
            Self::Cleaned | Self::Aborted => None,
        }
    }

    /// Whether the forward transition `self -> to` is the one allowed edge.
    #[must_use]
    pub fn can_advance_to(self, to: Self) -> bool {
        self.next() == Some(to)
    }

    /// Whether the state is terminal (nothing follows).
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Cleaned | Self::Aborted)
    }

    /// Whether abort is still allowed: every state before cut-over.
    #[must_use]
    pub fn is_before_cut_over(self) -> bool {
        matches!(
            self,
            Self::Planned | Self::Quiesced | Self::Exported | Self::Imported | Self::Verified
        )
    }
}

impl fmt::Display for MoveState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// One line of the persisted move history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveEvent {
    /// The state the move entered.
    pub state: MoveState,
    /// Unix milliseconds.
    pub at: u64,
}

/// Why a move stopped short of `Cleaned`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum MoveHalt {
    /// The source and target fingerprints differ; the move stays in `Imported`.
    FingerprintMismatch {
        /// The source fingerprint digest (a hash, never tenant data).
        source_digest: String,
        /// The target fingerprint digest.
        target_digest: String,
        /// How many rows or measures differ.
        differences: u64,
    },
    /// An operator rolled the move back before cut-over.
    Aborted {
        /// The fixed reason the operator recorded.
        reason: String,
    },
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use spot_tenancy::{ClusterId, DatabaseId};

    fn tenant() -> TenantId {
        TenantId::new_v7(1_700_000_000_000, [7; 10])
    }

    fn placement(cluster: &str, database: &str) -> Placement {
        Placement {
            cluster: ClusterId::new(cluster).unwrap(),
            database: DatabaseId::new(database).unwrap(),
            schema: tenant().schema_name(),
        }
    }

    #[test]
    fn a_move_id_must_be_lower_case_and_start_with_a_letter() {
        assert!(MoveId::new("move-01").is_ok());
        assert!(MoveId::new("Move-01").is_err());
        assert!(MoveId::new("1move").is_err());
        assert!(MoveId::new("ab").is_err());
    }

    #[test]
    fn a_plan_refuses_the_same_placement() {
        let placement = placement("a", "db1");
        let result = MovePlan::new(
            tenant(),
            placement.clone(),
            placement,
            Region::new("eu-west").unwrap(),
            MoveMode::Offline,
        );
        assert_eq!(result.unwrap_err().code(), crate::codes::INVALID_PLAN);
    }

    #[test]
    fn a_plan_refuses_a_foreign_schema() {
        let mut to = placement("b", "db2");
        to.schema = "t_not_the_tenant".to_owned();
        let result = MovePlan::new(
            tenant(),
            placement("a", "db1"),
            to,
            Region::new("eu-west").unwrap(),
            MoveMode::Offline,
        );
        assert_eq!(result.unwrap_err().code(), crate::codes::INVALID_PLAN);
    }

    #[test]
    fn the_state_machine_is_forward_only() {
        let states = [
            MoveState::Planned,
            MoveState::Quiesced,
            MoveState::Exported,
            MoveState::Imported,
            MoveState::Verified,
            MoveState::CutOver,
            MoveState::Cleaned,
        ];
        for pair in states.windows(2) {
            assert!(pair[0].can_advance_to(pair[1]));
        }
        assert!(!MoveState::Planned.can_advance_to(MoveState::Exported));
        assert!(MoveState::Verified.is_before_cut_over());
        assert!(!MoveState::CutOver.is_before_cut_over());
    }
}
