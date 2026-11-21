//! Installing a no-code app on another tenant with the real installer, then applying its
//! declarative content.
//!
//! The install path is the platform's ordinary one:
//!
//! 1. [`open`] verifies the fetched bytes with the validator the installer uses and reads the
//!    app's customisation and declarative payloads;
//! 2. [`install_plan`] builds the installer's plan from the parsed manifest and the caller's
//!    capability approval;
//! 3. the caller runs it with [`spot_installer::Executor`], which journals every step and rolls
//!    a failed upgrade back;
//! 4. [`apply_customisations`] applies the content-addressed customisation bundle to the target
//!    site through `spot-custom-bundle`'s transaction.
//!
//! [`install`] does 2–4 in one call. Designed DocTypes and forms are carried in the declarative
//! payload; the platform's definition writer is not part of the installer's trait surface, so a
//! host that installs designed DocTypes passes them through the no-code publish path
//! (`spot-nocode-model::publish`) before or after this call. This crate never writes a
//! definition itself.

use std::collections::BTreeMap;

use semver::Version;
use spot_bundle::{Bundle, Digest, HostInfo, Limits};
use spot_custom_bundle::{BundleHeader, SectionRegistry, SiteWriter, apply, plan_apply};
use spot_installer::{
    Approval, Candidate, Catalogue, Executor, GrantedCapabilities, Host, InstallPlan, Options,
    Outcome, Request, SiteState, Verified, plan,
};
use spot_manifest::AppManifest;

use crate::declarative::{CUSTOMISATION_ASSET, DECLARATIVE_ASSET, DeclarativeApp};
use crate::error::Error;

/// The WIT world this platform serves. A no-code app has no component exports, but the
/// installer still records which world the bundle targets, and every platform app targets this
/// one.
pub const PLATFORM_WIT_WORLD: &str = "spot:app@0.3.0";

/// A fetched, verified `.spotapp` opened for install.
#[derive(Clone, Debug)]
pub struct OpenedApp {
    /// The bytes, exactly as the registry returned them.
    bytes: Vec<u8>,
    /// The opened bundle.
    bundle: Bundle,
    /// The declarative body.
    declarative: DeclarativeApp,
    /// The customisation bundle, when the app carries one.
    customisation: Option<spot_custom_bundle::Bundle>,
}

impl OpenedApp {
    /// The bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The opened bundle.
    #[must_use]
    pub fn bundle(&self) -> &Bundle {
        &self.bundle
    }

    /// The parsed manifest.
    #[must_use]
    pub fn manifest(&self) -> &AppManifest {
        self.bundle.manifest()
    }

    /// The declarative body (designed DocTypes, forms, processes and reports).
    #[must_use]
    pub fn declarative(&self) -> &DeclarativeApp {
        &self.declarative
    }

    /// The app id.
    #[must_use]
    pub fn app(&self) -> &str {
        self.manifest().app.name.as_str()
    }

    /// The app version.
    #[must_use]
    pub fn version(&self) -> &Version {
        &self.manifest().app.version
    }

    /// The canonical digest.
    #[must_use]
    pub fn digest(&self) -> Digest {
        self.bundle.digest()
    }

    /// The installer's candidate for this bundle, with the verification evidence the fetch
    /// established.
    #[must_use]
    pub fn candidate(&self, verified: Verified) -> Candidate {
        Candidate::new(
            self.manifest().clone(),
            self.digest(),
            verified,
            PLATFORM_WIT_WORLD,
        )
    }

    /// An approval covering exactly the capabilities the manifest declares.
    ///
    /// This is a convenience for a caller that has already made the administrator's decision;
    /// it grants nothing beyond the signed manifest.
    #[must_use]
    pub fn full_approval(&self) -> Approval {
        let caps = &self.manifest().capabilities;
        Approval::new().grant(
            self.app(),
            GrantedCapabilities {
                network: caps.network.clone(),
                email: caps.email,
                files: caps.files,
                doctypes: caps.doctypes.clone(),
            },
        )
    }
}

/// Opens a fetched `.spotapp` and validates it against `host`.
///
/// # Errors
/// [`Error::Bundle`] when the bytes fail `spot-bundle`, or [`Error::Draft`] when the
/// declarative payload is malformed or names another app.
pub fn open(bytes: &[u8], host: &HostInfo) -> Result<OpenedApp, Error> {
    let bundle = Bundle::open(bytes, &Limits::default(), host).map_err(Error::bundle)?;
    let declarative_bytes = bundle
        .assets()
        .get(DECLARATIVE_ASSET)
        .ok_or_else(|| Error::Draft {
            message: format!("the bundle carries no `{DECLARATIVE_ASSET}`"),
        })?;
    let declarative_text = std::str::from_utf8(declarative_bytes).map_err(|_| Error::Draft {
        message: "the declarative payload is not UTF-8".to_owned(),
    })?;
    let declarative = DeclarativeApp::from_toml(declarative_text)?;
    if declarative.app.name != bundle.manifest().app.name.as_str()
        || declarative.app.version != bundle.manifest().app.version.to_string()
    {
        return Err(Error::Draft {
            message: "the declarative payload names a different app or version".to_owned(),
        });
    }
    let customisation = match bundle.assets().get(CUSTOMISATION_ASSET) {
        Some(raw) => Some(
            spot_custom_bundle::Bundle::open(
                raw,
                &spot_custom_bundle::Limits::default(),
                &SectionRegistry::new(),
                false,
            )
            .map_err(|errors| Error::CustomBundle {
                message: errors
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; "),
            })?,
        ),
        None => None,
    };
    Ok(OpenedApp {
        bytes: bytes.to_vec(),
        bundle,
        declarative,
        customisation,
    })
}

/// Builds the install plan for `app` on a site in `installed` state, with `dependencies`
/// resolved from the same registry (the installer refuses a missing dependency).
///
/// # Errors
/// [`Error::Install`] with the planner's stable `installer.*` code and message.
pub fn install_plan(
    app: &OpenedApp,
    installed: &SiteState,
    host: &Host,
    approval: &Approval,
    verified: Verified,
    dependencies: &[Candidate],
) -> Result<InstallPlan, Error> {
    let requirement =
        semver::VersionReq::parse(&format!("={}", app.version())).map_err(|error| {
            Error::Install {
                message: format!("the app version cannot be required: {error}"),
            }
        })?;
    let mut candidates = vec![app.candidate(verified)];
    candidates.extend_from_slice(dependencies);
    plan(
        installed,
        &Catalogue::new(candidates),
        host,
        &Request::Install {
            app: app.app().to_owned(),
            version_req: requirement,
        },
        approval,
    )
    .map_err(|error| Error::Install {
        message: format!("{}: {}", error.code, error.message),
    })
}

/// Builds the uninstall plan for `app` on a site in `installed` state.
///
/// # Errors
/// [`Error::Install`] with the planner's stable `installer.*` code and message.
pub fn uninstall_plan(
    app: &str,
    purge: bool,
    installed: &SiteState,
    host: &Host,
) -> Result<InstallPlan, Error> {
    plan(
        installed,
        &Catalogue::new(Vec::new()),
        host,
        &Request::Uninstall {
            app: app.to_owned(),
            purge,
        },
        &Approval::new(),
    )
    .map_err(|error| Error::Install {
        message: format!("{}: {}", error.code, error.message),
    })
}

/// Everything an [`install`] needs from the caller.
#[derive(Clone, Debug)]
pub struct InstallRequest<'a> {
    /// The fetched app.
    pub app: &'a OpenedApp,
    /// The tenant's installed apps.
    pub installed: &'a SiteState,
    /// The host the plan is built for.
    pub host: &'a Host,
    /// The administrator's capability approval.
    pub approval: &'a Approval,
    /// The fetch's verification evidence.
    pub verified: Verified,
    /// Candidates for the app's dependencies, resolved from the same registry.
    pub dependencies: &'a [Candidate],
}

impl<'a> InstallRequest<'a> {
    /// An install request for `app` on a tenant in `installed` state.
    #[must_use]
    pub fn new(
        app: &'a OpenedApp,
        installed: &'a SiteState,
        host: &'a Host,
        approval: &'a Approval,
        verified: Verified,
        dependencies: &'a [Candidate],
    ) -> Self {
        Self {
            app,
            installed,
            host,
            approval,
            verified,
            dependencies,
        }
    }
}

/// Runs an install plan, then applies the app's customisations when it succeeded.
///
/// A failed run rolls the installer's steps back and **does not** apply customisations, so the
/// tenant keeps its previous state.
///
/// # Errors
/// [`Error::Install`] when the executor refuses or journal work fails; [`Error::CustomBundle`]
/// when the declarative content does not apply.
pub async fn install(
    executor: &Executor,
    site: &spot_core::SiteId,
    request: &InstallRequest<'_>,
    options: &Options,
    target: &mut dyn SiteWriter,
) -> Result<Outcome, Error> {
    let plan = install_plan(
        request.app,
        request.installed,
        request.host,
        request.approval,
        request.verified.clone(),
        request.dependencies,
    )?;
    let outcome = executor
        .run(site, &plan, plan.hash(), options)
        .await
        .map_err(|error| Error::Install {
            message: format!("{}: {}", error.code, error.message),
        })?;
    if outcome.is_applied() {
        apply_customisations(request.app, target)?;
    }
    Ok(outcome)
}

/// Runs an uninstall plan, then removes the app's captured customisations when it succeeded.
///
/// # Errors
/// As [`install`]. The removal is scoped to the app's own files ([`remove_app_customisations`]):
/// the tenant's other customisations survive.
pub async fn uninstall(
    executor: &Executor,
    site: &spot_core::SiteId,
    app: &OpenedApp,
    installed: &SiteState,
    host: &Host,
    options: &Options,
    target: &mut dyn SiteWriter,
) -> Result<Outcome, Error> {
    let plan = uninstall_plan(app.app(), false, installed, host)?;
    let outcome = executor
        .run(site, &plan, plan.hash(), options)
        .await
        .map_err(|error| Error::Install {
            message: format!("{}: {}", error.code, error.message),
        })?;
    if outcome.is_applied() {
        remove_app_customisations(app, target)?;
    }
    Ok(outcome)
}

/// Applies the app's customisation bundle to `target`.
///
/// # Errors
/// [`Error::CustomBundle`] when the plan is refused; nothing is written.
pub fn apply_customisations(app: &OpenedApp, target: &mut dyn SiteWriter) -> Result<(), Error> {
    let Some(customisation) = &app.customisation else {
        return Ok(());
    };
    let registry = SectionRegistry::new();
    let plan =
        plan_apply(customisation, target, &registry).map_err(|errors| Error::CustomBundle {
            message: errors
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        })?;
    apply(&plan, target, &registry).map_err(Error::from)?;
    Ok(())
}

/// Removes only what the app added: the files of its customisation bundle that the tenant still
/// holds byte for byte. Anything else the tenant customised, or an app file the tenant has since
/// edited, is kept.
///
/// # Errors
/// [`Error::CustomBundle`] when the tenant cannot be captured or the removal cannot be applied.
pub fn remove_app_customisations(
    app: &OpenedApp,
    target: &mut dyn SiteWriter,
) -> Result<(), Error> {
    let Some(customisation) = &app.customisation else {
        return Ok(());
    };
    let options = spot_custom_bundle::CaptureOptions::new(
        "app-uninstall",
        "0.0.0",
        "installer",
        customisation.header().created_at.clone(),
    );
    let current = spot_custom_bundle::capture(&*target, &options).map_err(Error::from)?;
    let mut remaining = current.sections().clone();
    for (kind, section) in &mut remaining {
        if let Some(added) = customisation.section(kind) {
            section
                .files
                .retain(|name, bytes| added.files.get(name) != Some(bytes));
        }
    }
    remaining.retain(|_, section| !section.files.is_empty());
    let wanted = spot_custom_bundle::Bundle::pack(
        current.header().clone(),
        remaining,
        &spot_custom_bundle::Limits::default(),
    )
    .map_err(Error::from)?;
    let registry = SectionRegistry::new();
    let plan = plan_apply(&wanted, target, &registry).map_err(|errors| Error::CustomBundle {
        message: errors
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join("; "),
    })?;
    apply(&plan, target, &registry).map_err(Error::from)?;
    Ok(())
}

/// Removes every item the customisation sections can carry from `target`: a wipe, not an
/// uninstall. [`uninstall`] uses the scoped [`remove_app_customisations`] instead.
///
/// The empty bundle has no section entries, so `spot-custom-bundle` plans a removal for every
/// current item in the built-in sections. That is exactly right for a tenant whose only
/// customisations came from the app; the crate's docs say what a scoped removal would need.
///
/// # Errors
/// [`Error::CustomBundle`] when the removal cannot be planned or applied.
pub fn remove_customisations(target: &mut dyn SiteWriter) -> Result<(), Error> {
    let header = BundleHeader::new(
        "app-uninstall",
        "0.0.0",
        "installer",
        "",
        target.host_version().to_string(),
        Vec::new(),
    );
    let empty = spot_custom_bundle::Bundle::pack(
        header,
        BTreeMap::new(),
        &spot_custom_bundle::Limits::default(),
    )
    .map_err(Error::from)?;
    let registry = SectionRegistry::new();
    let plan = plan_apply(&empty, target, &registry).map_err(|errors| Error::CustomBundle {
        message: errors
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join("; "),
    })?;
    apply(&plan, target, &registry).map_err(Error::from)?;
    Ok(())
}
