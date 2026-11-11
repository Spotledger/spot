//! The publish gate.

use spot_bpm_model::ProcessVersion;
use spot_seams::Registry;

use crate::check::check;
use crate::error::{ChecksError, Result};
use crate::meta::EffectMeta;
use crate::problem::Problem;

/// The hook the process publish path calls before it makes a version live.
#[derive(Clone, Copy, Debug, Default)]
pub struct PublishGate;

impl PublishGate {
    /// Runs [`check`] and refuses when any problem is an error. Warnings do not stop a publish;
    /// they are returned so the caller can show them.
    ///
    /// # Errors
    /// [`ChecksError::PublishRefused`] with the error-level problems.
    pub fn check(
        process: &ProcessVersion,
        registry: &Registry,
        meta: &dyn EffectMeta,
    ) -> Result<Vec<Problem>> {
        let (errors, warnings): (Vec<Problem>, Vec<Problem>) = check(process, registry, meta)
            .into_iter()
            .partition(Problem::is_error);
        if errors.is_empty() {
            Ok(warnings)
        } else {
            Err(ChecksError::PublishRefused { problems: errors })
        }
    }
}
