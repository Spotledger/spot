//! The inputs of an install plan: the site, the catalogue, the host and the approval, plus the
//! capability vocabulary an approval and a delta are written in.
//!
//! Everything here is plain data. The planner reads it and never changes it; signature checks and
//! bundle opening happened before, in the caller, and arrive as [`Candidate::verified`].

use std::collections::BTreeMap;

use semver::{Version, VersionReq};
use spot_bundle::Digest;
use spot_manifest::{
    AppDependency, AppManifest, Capabilities, DocTypeAccess, Endpoint, FileAccess,
};

/// The capabilities an administrator approved for one app, shaped like the runtime's
/// `GrantedCapabilities` so an approval maps one to one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GrantedCapabilities {
    /// Hosts and ports the app may call.
    pub network: Vec<Endpoint>,
    /// Whether the app may send e-mail.
    pub email: bool,
    /// The file-store access granted.
    pub files: FileAccess,
    /// The DocTypes the app may read and change.
    pub doctypes: DocTypeAccess,
}

/// The capabilities a manifest asks for that an approval does not cover: exactly what an
/// administrator must decide before the plan may run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapabilityDelta {
    /// Endpoints the manifest declares that are not approved.
    pub network: Vec<Endpoint>,
    /// Whether the manifest asks for e-mail that is not approved.
    pub email: bool,
    /// The file access asked for beyond what is approved, when it is higher.
    pub files: Option<FileAccess>,
    /// The `read`/`write` DocTypes asked for that are not approved.
    pub doctypes: DocTypeAccess,
}

impl CapabilityDelta {
    /// The difference between what a manifest declares and what is approved.
    #[must_use]
    pub fn of(requested: &Capabilities, approved: &GrantedCapabilities) -> Self {
        let network = requested
            .network
            .iter()
            .filter(|e| !approved.network.contains(e))
            .cloned()
            .collect();
        let doctypes = DocTypeAccess {
            read: requested
                .doctypes
                .read
                .iter()
                .filter(|d| !approved.doctypes.read.contains(d))
                .cloned()
                .collect(),
            write: requested
                .doctypes
                .write
                .iter()
                .filter(|d| !approved.doctypes.write.contains(d))
                .cloned()
                .collect(),
        };
        Self {
            network,
            email: requested.email && !approved.email,
            files: (file_rank(requested.files) > file_rank(approved.files))
                .then_some(requested.files),
            doctypes,
        }
    }

    /// Whether the manifest asks for nothing beyond the approval.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.network.is_empty()
            && !self.email
            && self.files.is_none()
            && self.doctypes.read.is_empty()
            && self.doctypes.write.is_empty()
    }

    /// A plain list of what is asked for, for the explanation an administrator reads.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        for endpoint in &self.network {
            parts.push(format!("network {}:{}", endpoint.host, endpoint.port));
        }
        if self.email {
            parts.push("e-mail".to_owned());
        }
        if let Some(access) = self.files {
            parts.push(format!("file access {access:?}").to_lowercase());
        }
        if !self.doctypes.read.is_empty() {
            parts.push(format!("read {}", self.doctypes.read.join(", ")));
        }
        if !self.doctypes.write.is_empty() {
            parts.push(format!("write {}", self.doctypes.write.join(", ")));
        }
        parts.join("; ")
    }
}

/// One app whose approval delta the administrator must decide.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityRequest {
    /// The app the capabilities belong to.
    pub app: String,
    /// What it asks for beyond its approval.
    pub delta: CapabilityDelta,
}

/// Whether a plan may run, or is waiting for an administrator's approval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanState {
    /// Every action is approved and the plan may run.
    Ready,
    /// The plan lists capabilities an administrator has not approved; it never runs as it stands.
    NeedsApproval {
        /// One entry per action that asks for more than is approved.
        requests: Vec<CapabilityRequest>,
    },
}

impl PlanState {
    /// Whether the plan may run.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self, PlanState::Ready)
    }
}

/// One app installed on the site, as the site's registry knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledApp {
    /// The app's storage name.
    pub name: String,
    /// The installed version.
    pub version: Version,
    /// The installed bundle's canonical digest.
    pub digest: Digest,
    /// The capabilities approved for it.
    pub approved: GrantedCapabilities,
    /// The patch ids already applied, in the order they ran.
    pub applied_patches: Vec<String>,
    /// The install order, oldest first; uninstalls run in reverse.
    pub order: u32,
    /// The dependencies the installed version declares, so the planner can see who depends on
    /// whom without reopening the bundle.
    pub depends: Vec<AppDependency>,
}

impl InstalledApp {
    /// An installed app with the given identity and no approved capabilities, patches or
    /// dependencies; the caller fills what it knows.
    #[must_use]
    pub fn new(name: &str, version: Version, digest: Digest) -> Self {
        Self {
            name: name.to_owned(),
            version,
            digest,
            approved: GrantedCapabilities::default(),
            applied_patches: Vec::new(),
            order: 0,
            depends: Vec::new(),
        }
    }
}

/// The apps installed on one site.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SiteState {
    /// The installed apps.
    pub apps: Vec<InstalledApp>,
}

impl SiteState {
    /// An empty site.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The installed app with this name, when the site has it.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&InstalledApp> {
        self.apps.iter().find(|a| a.name == name)
    }
}

/// What a bundle's signature check established, as the caller (which holds `spot-bundle-signing`)
/// fills it in.
///
/// `ok` is `false` when the caller's verification refused the bundle; the planner then refuses
/// the whole plan with `installer.unverified` rather than placing an unverified bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    /// Whether the caller's signature check passed.
    pub ok: bool,
    /// The publisher the certificate names.
    pub publisher_id: String,
    /// The signing key id.
    pub key_id: String,
    /// When it was signed (UTC seconds).
    pub signed_at: i64,
    /// The revocation-list generation the check used.
    pub revocation_generation: u64,
}

impl Verified {
    /// A verified bundle signed by `publisher_id`, for tests and simple callers.
    #[must_use]
    pub fn trusted(publisher_id: &str, revocation_generation: u64) -> Self {
        Self {
            ok: true,
            publisher_id: publisher_id.to_owned(),
            key_id: String::new(),
            signed_at: 0,
            revocation_generation,
        }
    }

    /// A bundle the caller's signature check refused.
    #[must_use]
    pub fn refused() -> Self {
        Self {
            ok: false,
            publisher_id: String::new(),
            key_id: String::new(),
            signed_at: 0,
            revocation_generation: 0,
        }
    }
}

/// One bundle available to install: a parsed manifest, its canonical digest, whether the caller
/// verified its signature, and the WIT world it targets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// The parsed, validated manifest.
    pub manifest: AppManifest,
    /// The bundle's canonical digest, from `Bundle::digest`.
    pub digest: Digest,
    /// What the caller's signature check established.
    pub verified: Verified,
    /// The versioned WIT world the bundle targets.
    pub wit_world: String,
}

impl Candidate {
    /// A candidate from its parts.
    #[must_use]
    pub fn new(manifest: AppManifest, digest: Digest, verified: Verified, wit_world: &str) -> Self {
        Self {
            manifest,
            digest,
            verified,
            wit_world: wit_world.to_owned(),
        }
    }

    /// The app's storage name.
    #[must_use]
    pub fn app(&self) -> &str {
        self.manifest.app.name.as_str()
    }

    /// The app's version.
    #[must_use]
    pub fn version(&self) -> &Version {
        &self.manifest.app.version
    }
}

/// Every bundle the store can offer this site; several versions of one app may be present.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Catalogue {
    /// The candidate bundles.
    pub candidates: Vec<Candidate>,
}

impl Catalogue {
    /// A catalogue from its candidates.
    #[must_use]
    pub fn new(candidates: Vec<Candidate>) -> Self {
        Self { candidates }
    }
}

/// The host a plan is built for: its platform version and the WIT world it serves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Host {
    /// This host's platform version, checked against each manifest's `requires_host`.
    pub version: Version,
    /// The WIT world this host serves, checked against each candidate's world.
    pub wit_world: String,
}

impl Host {
    /// A host at the given version and WIT world.
    #[must_use]
    pub fn new(version: Version, wit_world: &str) -> Self {
        Self {
            version,
            wit_world: wit_world.to_owned(),
        }
    }
}

/// What the administrator asks the planner to do on one site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// Install an app at the highest catalogue version matching `version_req`.
    Install {
        /// The app to install.
        app: String,
        /// The versions of it the requester accepts.
        version_req: VersionReq,
    },
    /// Upgrade an installed app, to a named version or to the highest that does not break a
    /// dependent.
    Upgrade {
        /// The app to upgrade.
        app: String,
        /// The target version, or `None` for the highest compatible one.
        to: Option<Version>,
    },
    /// Move an installed app to a lower version; the only request that permits a downgrade.
    Downgrade {
        /// The app to move down.
        app: String,
        /// The lower version to move to.
        to: Version,
    },
    /// Remove an installed app. `purge` deletes the app's data as well as its component,
    /// fixtures and registry row; the plan records it as `High` risk and the hash covers it.
    Uninstall {
        /// The app to remove.
        app: String,
        /// Whether the app's data is deleted too. Kept by default.
        purge: bool,
    },
    /// Upgrade every installed app to the highest version that does not break a dependent.
    UpgradeAll,
}

impl Request {
    /// The app this request names, when it names one.
    #[must_use]
    pub fn app(&self) -> Option<&str> {
        match self {
            Request::Install { app, .. }
            | Request::Upgrade { app, .. }
            | Request::Downgrade { app, .. }
            | Request::Uninstall { app, .. } => Some(app),
            Request::UpgradeAll => None,
        }
    }
}

/// The capabilities an administrator has approved, by app name.
///
/// An app with no entry is approved for nothing, so any capability it declares shows up as a
/// delta and the plan waits in [`PlanState::NeedsApproval`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Approval {
    grants: BTreeMap<String, GrantedCapabilities>,
}

impl Approval {
    /// Nothing approved.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the capabilities approved for one app.
    #[must_use]
    pub fn grant(mut self, app: &str, caps: GrantedCapabilities) -> Self {
        self.grants.insert(app.to_owned(), caps);
        self
    }

    /// The capabilities approved for `app`, or nothing when it has no entry.
    #[must_use]
    pub fn approved(&self, app: &str) -> GrantedCapabilities {
        self.grants.get(app).cloned().unwrap_or_default()
    }

    /// Whether any approval is recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }
}

/// The rank of a file access in the `none < read < read-write` order.
fn file_rank(access: FileAccess) -> u8 {
    match access {
        FileAccess::None => 0,
        FileAccess::Read => 1,
        FileAccess::ReadWrite => 2,
    }
}
