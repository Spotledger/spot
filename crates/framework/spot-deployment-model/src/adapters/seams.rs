//! The seam adapter: the step overrides the site has applied.
//!
//! `spot-seams` owns the step and seam registry. This adapter reads its installed overrides
//! ([`Registry::overrides`]) into the model's [`Override`] list, sorted and deduplicated, so
//! the digest does not depend on registration order.

use spot_seams::Registry;
use spot_self_model::EvidenceRef;

use crate::DeploymentError;
use crate::facts::Facts;
use crate::model::{FactSourceKind, Override, OverrideKind};

/// The seam registry's ground truth.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeamsFacts {
    epoch: u64,
    overrides: Vec<Override>,
    evidence: Option<EvidenceRef>,
}

impl SeamsFacts {
    /// Reads a registry's installed overrides.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::Invalid`] when an override is not a usable override (an unknown
    /// declaration shape cannot reach a model).
    pub fn new(epoch: u64, registry: &Registry) -> Result<Self, DeploymentError> {
        let overrides = registry
            .overrides()
            .iter()
            .map(|installed| {
                let kind = match installed.over.kind {
                    spot_app_api::OverrideKind::Before => OverrideKind::Before,
                    spot_app_api::OverrideKind::After => OverrideKind::After,
                    spot_app_api::OverrideKind::Wrap => OverrideKind::Wrap,
                    spot_app_api::OverrideKind::Replace => OverrideKind::Replace,
                    spot_app_api::OverrideKind::Disable => OverrideKind::Disable,
                };
                Override::new(
                    installed.app.clone(),
                    installed.over.target.as_str(),
                    kind,
                    installed.over.by.as_ref().map(|by| by.as_str().to_owned()),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            epoch,
            overrides: crate::event::sorted_overrides(overrides),
            evidence: None,
        })
    }

    /// Names the record that proves the read.
    #[must_use]
    pub fn with_evidence(mut self, evidence: EvidenceRef) -> Self {
        self.evidence = Some(evidence);
        self
    }
}

impl crate::facts::FactSource for SeamsFacts {
    fn source(&self) -> FactSourceKind {
        FactSourceKind::Seams
    }

    fn epoch(&self) -> u64 {
        self.epoch
    }

    fn facts(&self) -> Result<Facts, DeploymentError> {
        Ok(Facts {
            overrides: Some(self.overrides.clone()),
            ..Facts::new()
        })
    }

    fn evidence(&self) -> Option<EvidenceRef> {
        self.evidence.clone()
    }
}
