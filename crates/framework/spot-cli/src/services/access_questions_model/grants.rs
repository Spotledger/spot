//! The installed apps' side of the model: the shipped grant entries with the app that ships each,
//! the duty definitions (`duties.toml`) and the SoD rules (`sod_rules.toml`).
//!
//! These files change only with an app install, so they are read once when the site is assembled
//! ([`StaticInputs::from_site`]); the runtime default grants of customised DocTypes, which a swap
//! adds, are merged per rebuild ([`StaticInputs::grant_inputs`]). The grants are read per app, not
//! from the merged model the engine holds, because the reason of an answer names the app.

use std::path::Path;

use serde::Deserialize;
use spot_iam_roles::Duty;
use spot_permissions::{AppPermissionGrant, RoleGrant};
use spot_sod::RuleSet;

use super::ModelError;
use super::assemble::{DEFAULT_PACK, GrantInputs};
use crate::appdir;
use crate::config::{Loaded, Site};
use crate::{apps, services::controllers};

/// Any failure to read or parse an app file is the same refusal: the text of the failure may name
/// a path or a value and is dropped.
fn bad<E>(_: E) -> ModelError {
    ModelError::GrantsUnavailable
}

const DUTIES_FILE: &str = "duties.toml";
const RULES_FILE: &str = "sod_rules.toml";

#[derive(Deserialize)]
struct DutyFile {
    #[serde(default)]
    duty: Vec<Duty>,
}

/// What the installed apps ship, read once.
#[derive(Clone, Debug, Default)]
pub struct StaticInputs {
    /// DocType grants with their app.
    pub grants: Vec<(String, RoleGrant)>,
    /// Field grants with their app.
    pub field_grants: Vec<(String, spot_permissions::FieldGrant)>,
    /// Protection groups.
    pub groups: Vec<spot_permissions::ProtectionGroup>,
    /// Shares.
    pub shares: Vec<spot_permissions::Share>,
    /// The named app permissions granted to roles.
    pub app_permissions: Vec<AppPermissionGrant>,
    /// The duty definitions.
    pub duties: Vec<Duty>,
    /// The SoD rules.
    pub rules: RuleSet,
}

impl StaticInputs {
    /// Reads the installed apps of `site`: the platform's embedded permission texts when the
    /// catalogue does not carry it, and every app directory's `permissions/*.toml`, `duties.toml`
    /// and `sod_rules.toml`. `app_permissions` are the site engine's named-permission grants.
    ///
    /// # Errors
    ///
    /// [`ModelError::GrantsUnavailable`] when an installed app is not in the catalogue or a file
    /// does not parse: the questions refuse rather than answer from part of the grants.
    pub fn from_site(
        root: &Path,
        loaded: &Loaded,
        site: &Site,
        app_permissions: Vec<AppPermissionGrant>,
    ) -> Result<StaticInputs, ModelError> {
        let catalogue = apps::catalogue(root, loaded).map_err(|_| ModelError::GrantsUnavailable)?;
        let mut out = StaticInputs {
            app_permissions,
            ..StaticInputs::default()
        };
        // An empty install list is a host-wide site: the engine has no app model for it either.
        for name in &site.apps {
            let texts = match catalogue.iter().find(|a| &a.name == name) {
                Some(app) => appdir::read_permission_texts(&app.dir.join("permissions")),
                None if name == appdir::PLATFORM => spot_platform_app::permission_texts(),
                None => return Err(ModelError::GrantsUnavailable),
            };
            let model = spot_permissions::load(&texts).map_err(bad)?;
            out.grants
                .extend(model.grants.into_iter().map(|g| (name.clone(), g)));
            out.field_grants
                .extend(model.field_grants.into_iter().map(|g| (name.clone(), g)));
            out.groups.extend(model.groups);
            out.shares.extend(model.shares);
        }
        let mut sod = String::new();
        for app in
            controllers::installed(root, loaded, site).map_err(|_| ModelError::GrantsUnavailable)?
        {
            if let Ok(text) = std::fs::read_to_string(app.dir.join(DUTIES_FILE)) {
                let file: DutyFile = toml::from_str(&text).map_err(bad)?;
                out.duties.extend(file.duty);
            }
            if let Ok(text) = std::fs::read_to_string(app.dir.join(RULES_FILE)) {
                sod.push_str(&text);
                sod.push('\n');
            }
        }
        if !sod.trim().is_empty() {
            out.rules = RuleSet::from_toml(&sod).map_err(bad)?;
        }
        Ok(out)
    }

    /// The grant inputs of one rebuild: the shipped ones plus `runtime`, the default grants of the
    /// customised DocTypes (they belong to no app, so they carry the platform's name).
    #[must_use]
    pub fn grant_inputs(&self, runtime: Vec<RoleGrant>) -> GrantInputs {
        let mut grants = self.grants.clone();
        grants.extend(runtime.into_iter().map(|g| (DEFAULT_PACK.to_owned(), g)));
        GrantInputs {
            grants,
            field_grants: self.field_grants.clone(),
            groups: self.groups.clone(),
            app_permissions: self.app_permissions.clone(),
            shares: self.shares.clone(),
        }
    }
}
