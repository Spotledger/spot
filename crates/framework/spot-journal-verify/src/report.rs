//! The question asked and the answer given.

use serde::{Deserialize, Serialize};
use spot_journal_store::TenantId;

use crate::cursor::VerifyCursor;
use crate::error::VerifyError;
use crate::finding::VerifyFinding;

/// What the caller already knows to be true, so nothing is taken from the objects being checked.
///
/// With `generation: None` the pass is **discovery only**: it verifies the newest manifest the
/// store lists, so a deleted or hidden newest manifest leaves an older, correctly signed one that
/// verifies clean (a silent rollback). A caller that knows the generation it expects must use
/// [`PinnedExpected`] (see [`Expected::pin`]) and [`crate::verify_pinned`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expected {
    /// The tenant whose prefix is verified.
    pub tenant: TenantId,
    /// The source whose stream the manifest and every segment must name.
    pub source: String,
    /// The manifest generation to verify; `None` means the newest manifest the store holds.
    pub generation: Option<u64>,
}

impl Expected {
    /// The pinned form of this expectation for `generation`, which the caller recorded when the
    /// last fork or seal happened. The generation given replaces `self.generation`.
    ///
    /// Generation 0 is refused when the pin is verified ([`crate::verify_pinned`]), or at once by
    /// [`PinnedExpected::new`].
    #[must_use]
    pub fn pin(self, generation: u64) -> PinnedExpected {
        PinnedExpected {
            tenant: self.tenant,
            source: self.source,
            generation,
        }
    }
}

/// An expectation whose generation is mandatory: the manifest verified is exactly this
/// generation's, and no listing decides which generation that is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedExpected {
    /// The tenant whose prefix is verified.
    pub tenant: TenantId,
    /// The source whose stream the manifest and every segment must name.
    pub source: String,
    /// The manifest generation to verify (1 or later).
    pub generation: u64,
}

impl PinnedExpected {
    /// A pinned expectation.
    ///
    /// # Errors
    /// [`VerifyError::ExpectedInvalid`] for an empty source or generation 0.
    pub fn new(
        tenant: TenantId,
        source: impl Into<String>,
        generation: u64,
    ) -> Result<Self, VerifyError> {
        let pinned = Self {
            tenant,
            source: source.into(),
            generation,
        };
        pinned.validate()?;
        Ok(pinned)
    }

    /// Refuses an empty source and generation 0 (the fields are public, so a pin can also be
    /// built without [`Self::new`]).
    pub(crate) fn validate(&self) -> Result<(), VerifyError> {
        if self.source.trim().is_empty() {
            return Err(VerifyError::ExpectedInvalid("the expected source is empty"));
        }
        if self.generation == 0 {
            return Err(VerifyError::ExpectedInvalid(
                "generations start at 1, so generation 0 cannot be pinned",
            ));
        }
        Ok(())
    }

    /// The same question as an [`Expected`] for the driver.
    pub(crate) fn as_expected(&self) -> Expected {
        Expected {
            tenant: self.tenant.clone(),
            source: self.source.clone(),
            generation: Some(self.generation),
        }
    }
}

/// How much a pass reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Mode {
    /// The manifest signature and ownership, every listed segment present, and the hash of each
    /// from the store's own object metadata. No segment bytes are read.
    Light,
    /// Also reads each segment, checks its header, block checksums and chain link, and compares
    /// the header with the manifest's summary.
    Deep,
}

/// The default byte budget of one call: 256 MB.
pub const DEFAULT_BUDGET_BYTES: u64 = 256 * 1024 * 1024;

/// Options of one call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyOptions {
    /// The most bytes one call reads (the manifest and, in deep mode, segment bytes).
    pub budget_bytes: u64,
    /// What an earlier pass verified, if the caller kept it.
    pub cursor: Option<VerifyCursor>,
}

impl Default for VerifyOptions {
    fn default() -> Self {
        Self {
            budget_bytes: DEFAULT_BUDGET_BYTES,
            cursor: None,
        }
    }
}

/// The answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyReport {
    /// The tenant verified.
    pub tenant: String,
    /// The source expected.
    pub source: String,
    /// The generation of the manifest verified (`0` when none was found).
    pub generation: u64,
    /// The mode of this pass.
    pub mode: Mode,
    /// Whether the generation was pinned by the caller ([`crate::verify_pinned`]); `false` means
    /// the newest manifest the store listed was verified (discovery only, blind to a rollback).
    pub pinned: bool,
    /// Everything wrong, in journal order.
    pub findings: Vec<VerifyFinding>,
    /// How many segments the manifest lists.
    pub segments_listed: usize,
    /// How many of them an earlier pass had verified and this one did not read.
    pub segments_skipped: usize,
    /// How many of them this pass examined.
    pub segments_examined: usize,
    /// The bytes this pass read from the store.
    pub bytes_read: u64,
    /// Whether the byte budget stopped the pass before the last listed segment; run again with
    /// [`VerifyReport::cursor`] to continue.
    pub partial: bool,
    /// What to pass to the next call.
    pub cursor: VerifyCursor,
}

impl VerifyReport {
    /// Whether the pass found nothing wrong (and, if [`Self::partial`], found nothing in what it
    /// read).
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }

    /// Whether any finding is evidence of tampering or loss.
    #[must_use]
    pub fn has_tampering(&self) -> bool {
        self.findings.iter().any(VerifyFinding::is_tampering)
    }
}
