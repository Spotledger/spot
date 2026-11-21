//! The privilege policy: what a bundle may declare, and the check that rejects
//! over-privileged bundles.
//!
//! The host sets the policy at install time; the bundle is only ever as privileged as both
//! the manifest and the policy allow.

use spot_manifest::{Capabilities, FileAccess};

use crate::error::{BundleError, IMPORT_WITHOUT_CAPABILITY, OVER_PRIVILEGED, UNKNOWN_IMPORT};

/// The upper bounds a bundle's declared capabilities must respect.
///
/// `false` and explicit values are the cap; `None` means "uncapped here" (other layers may
/// still cap them).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// The most `network` endpoints a bundle may declare.
    pub max_network_endpoints: Option<u32>,
    /// Whether a bundle may declare a wildcard host (`*`, or any host containing one);
    /// default `false`.
    pub allow_wildcard_hosts: bool,
    /// Whether a bundle may declare e-mail; default `false`.
    pub allow_email: bool,
    /// The most file-store access a bundle may declare, in the `none < read < read-write`
    /// order; `None` allows everything.
    pub max_file_access: Option<FileAccess>,
    /// The most DocTypes a bundle may declare write access to.
    pub max_write_doctypes: Option<u32>,
    /// DocTypes no bundle may declare write access to, whatever the manifest says.
    pub forbidden_doctypes: Vec<String>,
}

impl Policy {
    /// The default policy: no wildcard hosts, no e-mail; every other bound uncapped, no
    /// forbidden DocTypes.
    pub fn new() -> Self {
        Self {
            max_network_endpoints: None,
            allow_wildcard_hosts: false,
            allow_email: false,
            max_file_access: None,
            max_write_doctypes: None,
            forbidden_doctypes: Vec::new(),
        }
    }
}

impl Default for Policy {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether a declared endpoint's host is a wildcard: `*`, an empty host, or any host
/// containing a `*`.
fn is_wildcard_host(host: &str) -> bool {
    host.is_empty() || host.contains('*')
}

/// A declared capability the component never exercises: warning level, for the admin to
/// read, never an error (see [`crate::Report`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnusedCapability {
    /// What was declared but not exercised, phrased for the admin.
    pub note: String,
}

/// Compares the manifest's declared capabilities against `policy`; every violation is one
/// `bundle.over_privileged` error naming the capability and the limit.
pub(crate) fn over_privilege(caps: &Capabilities, policy: &Policy) -> Vec<BundleError> {
    let mut errs = Vec::new();
    // --- network ---------------------------------------------------------------------
    if let Some(max) = policy.max_network_endpoints
        && caps.network.len() > max as usize
    {
        errs.push(BundleError::new(
            OVER_PRIVILEGED,
            format!(
                "capabilities.network: it declares {} endpoints; the policy allows {max}",
                caps.network.len()
            ),
        ));
    }
    if !policy.allow_wildcard_hosts {
        for endpoint in &caps.network {
            if is_wildcard_host(&endpoint.host) {
                errs.push(BundleError::about(
                    OVER_PRIVILEGED,
                    format!("capabilities.network: {}:{}", endpoint.host, endpoint.port),
                    "the host is a wildcard; the policy does not allow wildcard hosts".to_owned(),
                ));
            }
        }
    }
    // --- e-mail ----------------------------------------------------------------------
    if caps.email && !policy.allow_email {
        errs.push(BundleError::new(
            OVER_PRIVILEGED,
            "capabilities.email: the manifest declares e-mail; the policy does not allow it",
        ));
    }
    // --- files -----------------------------------------------------------------------
    if let Some(max) = policy.max_file_access
        && file_rank(caps.files) > file_rank(max)
    {
        errs.push(BundleError::new(
            OVER_PRIVILEGED,
            format!(
                "capabilities.files: the manifest declares {:?}; the policy allows at most {:?}",
                caps.files, max
            ),
        ));
    }
    // --- DocTypes --------------------------------------------------------------------
    if let Some(max) = policy.max_write_doctypes
        && caps.doctypes.write.len() > max as usize
    {
        errs.push(BundleError::new(
            OVER_PRIVILEGED,
            format!(
                "capabilities.doctypes.write: it lists {} DocTypes; the policy allows {max}",
                caps.doctypes.write.len()
            ),
        ));
    }
    for doctype in &caps.doctypes.write {
        if policy.forbidden_doctypes.iter().any(|f| f == doctype) {
            errs.push(BundleError::about(
                OVER_PRIVILEGED,
                format!("capabilities.doctypes.write: {doctype}"),
                "the policy forbids write access to this DocType".to_owned(),
            ));
        }
    }
    errs
}

/// The rank of a `FileAccess` in the `none < read < read-write` order.
fn file_rank(access: FileAccess) -> u8 {
    match access {
        FileAccess::None => 0,
        FileAccess::Read => 1,
        FileAccess::ReadWrite => 2,
    }
}

/// The import side of the privilege check: every import must be known and covered by a
/// declared capability. Returns the `bundle.*` errors and the declared-but-never-exercised
/// capabilities (warning level).
///
/// With the world as it stands ([`crate::imports::IMPORTS`]), only `get-doc` exercises a
/// capability (`doctypes.read`); the world has no import for network, e-mail or files yet,
/// so any declaration of those is reported as unused (and any future import lands in the
/// table together with the link-time check in `spot-wasm`).
pub(crate) fn check_imports(
    imports: &[String],
    caps: &Capabilities,
) -> (Vec<BundleError>, Vec<UnusedCapability>) {
    let mut errs = Vec::new();
    let mut unused = Vec::new();
    let mut reads_documents = false;
    for import in imports {
        match crate::imports::IMPORTS
            .iter()
            .find(|(name, _)| name == import)
        {
            None => errs.push(BundleError::new(
                UNKNOWN_IMPORT,
                format!("the component imports `{import}`, which the host does not provide"),
            )),
            Some((_, crate::imports::Need::Nothing)) => {
                // Carries no authority: nothing declared is spent on it.
            }
            Some((_, crate::imports::Need::ReadDocTypes)) => {
                if caps.doctypes.read.is_empty() {
                    errs.push(BundleError::new(
                        IMPORT_WITHOUT_CAPABILITY,
                        format!(
                            "the component imports `{import}` but the manifest declares no \
                             DocType read access (`capabilities.doctypes.read`)"
                        ),
                    ));
                } else {
                    reads_documents = true;
                }
            }
        }
    }
    if !caps.doctypes.read.is_empty() && !reads_documents {
        unused.push(UnusedCapability {
            note: "the manifest declares DocType read access but the component imports \
                   nothing that reads documents (`get-doc`)"
                .to_owned(),
        });
    }
    if !caps.doctypes.write.is_empty() {
        unused.push(UnusedCapability {
            note: format!(
                "the manifest declares write access to {} DocTypes but the component imports \
                 nothing that changes documents",
                caps.doctypes.write.len()
            ),
        });
    }
    if !caps.network.is_empty() {
        unused.push(UnusedCapability {
            note: format!(
                "the manifest declares {} network endpoints but the component imports \
                 nothing that calls them",
                caps.network.len()
            ),
        });
    }
    if caps.email {
        unused.push(UnusedCapability {
            note: "the manifest declares the e-mail capability but the component imports \
                   nothing that sends mail"
                .to_owned(),
        });
    }
    if caps.files != FileAccess::None {
        unused.push(UnusedCapability {
            note: "the manifest declares file-store access but the component imports nothing \
                   that touches files"
                .to_owned(),
        });
    }
    (errs, unused)
}
