//! Typed faults with stable `journal_ship.*` codes. A message never carries an outbox entry, a
//! key, a path or a personal value.

use spot_journal_commit::ShipError as ChunkError;

/// Everything that can stop or delay a step of the shipper.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ShipFault {
    /// The outbox could not be read (database down, pool exhausted). Retryable.
    #[error("the outbox could not be read")]
    OutboxUnavailable,
    /// The outbox holds something the shipper cannot ship in order: an entry that is not an
    /// envelope, a position that is not consecutive, a page that spans generations. Not skipped,
    /// never: the shipper stops advancing and keeps this reason.
    #[error("the outbox cannot be shipped in order: {0}")]
    OutboxInvalid(&'static str),
    /// Protecting a line failed (a key store outage, a field description that cannot be read).
    /// Fail closed: nothing is written, the step is retried.
    #[error("a line could not be protected: {code}")]
    Protect {
        /// The protect layer's stable code.
        code: &'static str,
    },
    /// The journal store is unreachable or the relay asks for a resume: a later attempt may
    /// succeed.
    #[error("the journal store is unavailable: {code}")]
    StoreDown {
        /// The relay's stable code.
        code: &'static str,
    },
    /// A failure a retry cannot cure by itself (a broken chain, another writer, a bad batch).
    #[error("the relay stopped: {code}")]
    Stopped {
        /// The relay's stable code.
        code: &'static str,
    },
    /// The persisted watermark could not be read or written (the table is created by
    /// `spot migrate`).
    #[error("the shipper's watermark could not be read or written")]
    Watermark,
    /// The shipper's configuration is unusable.
    #[error("the shipper configuration is invalid: {0}")]
    Config(&'static str),
    /// The folder target is not a directory the journal may be written to.
    #[error("the journal target is unsafe: {0}")]
    TargetUnsafe(&'static str),
    /// The verification pass found the stored chain damaged.
    #[error("the journal chain is damaged: {code}")]
    ChainDamaged {
        /// The verifier's finding code.
        code: &'static str,
    },
}

impl ShipFault {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::OutboxUnavailable => "journal_ship.outbox_unavailable",
            Self::OutboxInvalid(_) => "journal_ship.outbox_invalid",
            Self::Protect { .. } => "journal_ship.protect_failed",
            Self::StoreDown { .. } => "journal_ship.store_down",
            Self::Stopped { .. } => "journal_ship.relay_stopped",
            Self::Watermark => "journal_ship.watermark_failed",
            Self::Config(_) => "journal_ship.config_invalid",
            Self::TargetUnsafe(_) => "journal_ship.target_unsafe",
            Self::ChainDamaged { .. } => "journal_ship.chain_damaged",
        }
    }

    /// Whether a later attempt, with no change by a person, can succeed.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::OutboxUnavailable
                | Self::StoreDown { .. }
                | Self::Protect { .. }
                | Self::Watermark
        )
    }
}

impl From<ChunkError> for ShipFault {
    fn from(error: ChunkError) -> Self {
        match error {
            ChunkError::StoreDown { code } => Self::StoreDown { code },
            ChunkError::Stopped { code } => Self::Stopped { code },
        }
    }
}
