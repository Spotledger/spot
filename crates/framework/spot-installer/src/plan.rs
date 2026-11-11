//! The plan itself: ordered actions, each with its risk, reversibility and capability delta; the
//! deterministic hash an approval binds to; and the plain sentences the Admin Center shows.

use semver::Version;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use spot_bundle::Digest;
use spot_manifest::PatchIntent;

use crate::model::{CapabilityDelta, GrantedCapabilities, PlanState};

/// The domain prefix of the plan hash, so the digest is a Spotledger value and never collides
/// with a hash of any other structure.
const HASH_DOMAIN: &[u8] = b"spotledger-install-plan-v1\0";

/// How risky one action is.
///
/// The vocabulary is copied from the data-layer plan (`spot-pg-migrate::plan::Risk`) so the two
/// plans read alike; the installer does not depend on that crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    /// Metadata only: no schema and no document data change.
    Low,
    /// At least one data patch: documents change.
    Medium,
    /// A schema patch, an irreversible patch, or a downgrade.
    High,
}

/// What one action does to the site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operation {
    /// Install an app that is not on the site.
    Install {
        /// The app.
        app: String,
        /// The version installed.
        version: Version,
        /// The bundle's canonical digest.
        digest: Digest,
    },
    /// Change an installed app's version; `from` is higher for a downgrade.
    Upgrade {
        /// The app.
        app: String,
        /// The installed version.
        from: Version,
        /// The version installed.
        to: Version,
        /// The new bundle's canonical digest.
        digest: Digest,
    },
    /// Remove an installed app.
    Uninstall {
        /// The app.
        app: String,
        /// Whether the app's data is deleted as well as its component, fixtures and registry
        /// row. A purge is `High` risk and irreversible; the plan hash covers the flag, so a
        /// purge never runs under an approval that saw a keep-data uninstall.
        purge: bool,
    },
}

impl Operation {
    /// The app the action concerns.
    #[must_use]
    pub fn app(&self) -> &str {
        match self {
            Operation::Install { app, .. }
            | Operation::Upgrade { app, .. }
            | Operation::Uninstall { app, .. } => app,
        }
    }

    /// Whether the action installs an app (rather than changing or removing one).
    #[must_use]
    pub fn is_install(&self) -> bool {
        matches!(self, Operation::Install { .. })
    }
}

/// One ordered action of a plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    /// What it does.
    pub op: Operation,
    /// The app's human title, for the explanation; its name when the manifest states none.
    pub title: String,
    /// Patches from the manifest that have not run yet, in manifest order, each with whether it
    /// can be undone. The executor undoes only the reversible ones and relies on the snapshot
    /// for the rest.
    pub patches: Vec<PatchStep>,
    /// Fixture files the manifest declares, in manifest order.
    pub fixtures_to_load: Vec<String>,
    /// How risky the action is.
    pub risk: Risk,
    /// Whether every patch the action runs can be undone.
    pub reversible: bool,
    /// The capabilities the action asks for beyond the app's approval.
    pub capability_delta: CapabilityDelta,
    /// The capabilities the app's manifest declares, which an approval covers when the plan is
    /// [`PlanState::Ready`]. The executor records them in the registry on commit. Not covered by
    /// the plan hash: the bundle digest already binds the manifest.
    pub capabilities: GrantedCapabilities,
    /// The strongest intent among the patches the action runs; `None` when it runs none.
    pub(crate) patch_intent: Option<PatchIntent>,
}

/// One patch an action will run, and whether it can be undone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatchStep {
    /// The patch id, from the manifest.
    pub id: String,
    /// Whether the manifest declares the patch reversible.
    pub reversible: bool,
}

impl Action {
    /// The app the action concerns.
    #[must_use]
    pub fn app(&self) -> &str {
        self.op.app()
    }

    /// The plain sentence the administrator reads for this action.
    #[must_use]
    pub fn sentence(&self) -> String {
        let mut parts = vec![match &self.op {
            Operation::Install { version, .. } => {
                format!("Installs {} {version}", self.title)
            }
            Operation::Upgrade { from, to, .. } => {
                format!("Upgrades {} from {from} to {to}", self.title)
            }
            Operation::Uninstall { .. } => format!("Uninstalls {}", self.title),
        }];
        if !self.patches.is_empty() {
            parts.push(format!(
                "runs {} {} {}",
                self.patches.len(),
                self.patch_kind(),
                plural(self.patches.len())
            ));
        }
        if !self.reversible {
            parts.push("cannot be undone".to_owned());
        }
        parts.join(", ")
    }

    /// The kind word for the patches the action runs, for the sentence.
    fn patch_kind(&self) -> &'static str {
        match self.patch_intent {
            Some(PatchIntent::Schema) => "schema",
            Some(PatchIntent::Data) => "data",
            Some(PatchIntent::Metadata) => "metadata",
            None => "metadata",
        }
    }

    /// The ids of the patches the action runs, in order.
    #[must_use]
    pub fn patches_to_run(&self) -> Vec<&str> {
        self.patches.iter().map(|p| p.id.as_str()).collect()
    }
}

/// `patch`/`patches` for a count.
fn plural(count: usize) -> &'static str {
    if count == 1 { "patch" } else { "patches" }
}

/// A ready plan, or one waiting for capability approval: the ordered actions plus the inputs an
/// approval binds to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallPlan {
    actions: Vec<Action>,
    state: PlanState,
    host_version: Version,
    revocation_generation: u64,
    hash: String,
}

impl InstallPlan {
    /// Builds a plan and its hash from its parts.
    pub(crate) fn build(
        actions: Vec<Action>,
        state: PlanState,
        host_version: Version,
        revocation_generation: u64,
    ) -> Self {
        let hash = compute_hash(&actions, &host_version);
        Self {
            actions,
            state,
            host_version,
            revocation_generation,
            hash,
        }
    }

    /// The ordered actions.
    #[must_use]
    pub fn actions(&self) -> &[Action] {
        &self.actions
    }

    /// Whether the plan is ready or waits for approval.
    #[must_use]
    pub fn state(&self) -> &PlanState {
        &self.state
    }

    /// The host version the plan was built for, covered by the hash.
    #[must_use]
    pub fn host_version(&self) -> &Version {
        &self.host_version
    }

    /// The highest revocation-list generation the signature checks used, recorded so the plan
    /// says which generation it was decided under.
    #[must_use]
    pub fn revocation_generation(&self) -> u64 {
        self.revocation_generation
    }

    /// The SHA-256 hex hash over the canonical encoding of the plan.
    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Whether `hash` is this plan's hash; the check the approval step runs.
    #[must_use]
    pub fn verify_hash(&self, hash: &str) -> bool {
        self.hash == hash
    }

    /// Whether the plan changes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// Whether any action deletes an app's data as well as its component, fixtures and registry
    /// row. A purge is irreversible, so a plan that purges is snapshotted before it runs.
    #[must_use]
    pub fn purges(&self) -> bool {
        self.actions
            .iter()
            .any(|action| matches!(&action.op, Operation::Uninstall { purge: true, .. }))
    }

    /// One plain sentence per action, plus one per unapproved capability request: the text an
    /// administrator and the Admin Center show.
    #[must_use]
    pub fn explain(&self) -> Vec<String> {
        let mut lines: Vec<String> = self.actions.iter().map(Action::sentence).collect();
        if let PlanState::NeedsApproval { requests } = &self.state {
            for request in requests {
                lines.push(format!(
                    "{} waits for approval of: {}.",
                    request.app,
                    request.delta.describe()
                ));
            }
        }
        lines
    }
}

/// The canonical, sorted shape the hash covers: actions, digests, patch ids, fixtures, capability
/// deltas and the host version. Sorted map iteration and fixed field order make the bytes
/// independent of any `HashMap` ordering.
#[derive(Serialize)]
struct HashInput<'a> {
    host_version: String,
    actions: Vec<HashAction<'a>>,
}

/// One action of [`HashInput`].
#[derive(Serialize)]
struct HashAction<'a> {
    #[serde(flatten)]
    op: HashOp<'a>,
    patches: Vec<&'a str>,
    fixtures: &'a [String],
    risk: &'a str,
    reversible: bool,
    network: Vec<String>,
    email: bool,
    files: Option<&'a str>,
    doctypes_read: Vec<&'a str>,
    doctypes_write: Vec<&'a str>,
}

/// The operation of [`HashAction`], flattened into it.
#[derive(Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum HashOp<'a> {
    /// An install.
    Install {
        app: &'a str,
        version: String,
        digest: String,
    },
    /// An upgrade or downgrade.
    Upgrade {
        app: &'a str,
        from: String,
        to: String,
        digest: String,
    },
    /// An uninstall.
    Uninstall { app: &'a str, purge: bool },
}

/// Computes the plan hash: SHA-256 over the domain prefix and the canonical JSON of the plan.
fn compute_hash(actions: &[Action], host_version: &Version) -> String {
    let input = HashInput {
        host_version: host_version.to_string(),
        actions: actions.iter().map(hash_action).collect(),
    };
    let json = serde_json::to_vec(&input).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(HASH_DOMAIN);
    hasher.update(&json);
    let bytes = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in bytes {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// The hashed view of one action.
fn hash_action(action: &Action) -> HashAction<'_> {
    let op = match &action.op {
        Operation::Install {
            app,
            version,
            digest,
        } => HashOp::Install {
            app,
            version: version.to_string(),
            digest: digest.to_hex(),
        },
        Operation::Upgrade {
            app,
            from,
            to,
            digest,
        } => HashOp::Upgrade {
            app,
            from: from.to_string(),
            to: to.to_string(),
            digest: digest.to_hex(),
        },
        Operation::Uninstall { app, purge } => HashOp::Uninstall { app, purge: *purge },
    };
    let delta = &action.capability_delta;
    let mut network: Vec<String> = delta
        .network
        .iter()
        .map(|e| format!("{}:{}", e.host, e.port))
        .collect();
    network.sort();
    let mut doctypes_read: Vec<&str> = delta.doctypes.read.iter().map(String::as_str).collect();
    doctypes_read.sort();
    let mut doctypes_write: Vec<&str> = delta.doctypes.write.iter().map(String::as_str).collect();
    doctypes_write.sort();
    HashAction {
        op,
        patches: action.patches.iter().map(|p| p.id.as_str()).collect(),
        fixtures: &action.fixtures_to_load,
        risk: risk_name(action.risk),
        reversible: action.reversible,
        network,
        email: delta.email,
        files: delta.files.map(file_name),
        doctypes_read,
        doctypes_write,
    }
}

/// The stable name of a risk, for the hash.
fn risk_name(risk: Risk) -> &'static str {
    match risk {
        Risk::Low => "low",
        Risk::Medium => "medium",
        Risk::High => "high",
    }
}

/// The stable name of a file access, for the hash.
fn file_name(access: spot_manifest::FileAccess) -> &'static str {
    match access {
        spot_manifest::FileAccess::None => "none",
        spot_manifest::FileAccess::Read => "read",
        spot_manifest::FileAccess::ReadWrite => "read_write",
    }
}
