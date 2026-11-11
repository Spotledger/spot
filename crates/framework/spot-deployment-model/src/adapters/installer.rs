//! The installer adapter: the installed apps and the host's runtime version.
//!
//! `spot-installer` owns the site's installed-app state. This adapter reads its
//! [`SiteState`] into [`AppEntry`] values; a caller that knows an app is switched off marks it
//! with [`InstallerFacts::with_state`] rather than inventing a second app list.

use std::collections::BTreeMap;

use spot_installer::{InstalledApp, SiteState};
use spot_self_model::EvidenceRef;

use crate::DeploymentError;
use crate::error::MAX_TEXT_BYTES;
use crate::facts::Facts;
use crate::model::{AppEntry, AppState, FactSourceKind};

/// The installer's ground truth.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallerFacts {
    epoch: u64,
    apps: BTreeMap<String, AppEntry>,
    runtime_version: Option<String>,
    evidence: Option<EvidenceRef>,
}

impl InstallerFacts {
    /// Reads a site's installed apps.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::Invalid`] when the site lists an app twice or a version is unusable.
    pub fn new(epoch: u64, site: &SiteState) -> Result<Self, DeploymentError> {
        Self::from_apps(epoch, site.apps.iter())
    }

    /// Reads a list of installed apps.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::Invalid`] when the list names an app twice or a version is unusable.
    pub fn from_apps<'a>(
        epoch: u64,
        apps: impl IntoIterator<Item = &'a InstalledApp>,
    ) -> Result<Self, DeploymentError> {
        let mut entries = BTreeMap::new();
        for app in apps {
            if entries
                .insert(
                    app.name.clone(),
                    AppEntry::new(app.version.to_string(), AppState::Installed)?,
                )
                .is_some()
            {
                return Err(DeploymentError::invalid(
                    "apps name",
                    format!("app `{}` appears twice", app.name),
                ));
            }
        }
        Ok(Self {
            epoch,
            apps: entries,
            runtime_version: None,
            evidence: None,
        })
    }

    /// Records the runtime version the installer checked the host against.
    #[must_use]
    pub fn with_runtime_version(mut self, version: impl Into<String>) -> Self {
        self.runtime_version = Some(version.into());
        self
    }

    /// Marks one installed app as active or disabled.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::Invalid`] when the app is not installed.
    pub fn with_state(mut self, app: &str, state: AppState) -> Result<Self, DeploymentError> {
        let entry = self.apps.get_mut(app).ok_or_else(|| {
            DeploymentError::invalid("apps name", format!("`{app}` is not installed"))
        })?;
        entry.state = state;
        Ok(self)
    }

    /// Names the record that proves the read.
    #[must_use]
    pub fn with_evidence(mut self, evidence: EvidenceRef) -> Self {
        self.evidence = Some(evidence);
        self
    }
}

impl crate::facts::FactSource for InstallerFacts {
    fn source(&self) -> FactSourceKind {
        FactSourceKind::Installer
    }

    fn epoch(&self) -> u64 {
        self.epoch
    }

    fn facts(&self) -> Result<Facts, DeploymentError> {
        for (name, app) in &self.apps {
            if name.len() > MAX_TEXT_BYTES || app.version.trim().is_empty() {
                return Err(DeploymentError::invalid(
                    "apps",
                    "an installed app has an unusable name or version",
                ));
            }
        }
        Ok(Facts {
            apps: Some(self.apps.clone()),
            runtime_version: self.runtime_version.clone(),
            ..Facts::new()
        })
    }

    fn evidence(&self) -> Option<EvidenceRef> {
        self.evidence.clone()
    }
}
