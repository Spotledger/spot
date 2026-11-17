//! First-start enrolment: the one-time code file, the redemption and the stored pin.
//!
//! An edge that has an enrolment code configured (`services.sync_node.enrol_code_file`, a path)
//! reads it **once**, redeems it at the hub, stores the hub certificate pin next to its own
//! identity, and then overwrites and removes the file: it never needs the code again. No code
//! and no pin means no identity to present: the loop does not start and readiness says
//! `sync_node: not enrolled`.
//!
//! The file is a few `key=value` lines (or one bare line, the code):
//!
//! ```text
//! code=<one-time code>
//! hub_fingerprint=<sha-256 hex of the hub certificate>
//! ```
//!
//! # Security
//!
//! The file must be a regular file (not a symlink) with mode `0600` on Unix; anything looser is
//! refused with `sync_node_host.code_file_unsafe` before it is read. The code is never logged,
//! never put in an error, and the file is overwritten with zeros, flushed and deleted after use
//! (emptied when it cannot be deleted). The private key is the identity's own business
//! (`spot_sync_enrol::NodeIdentity`, `0600`).

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use spot_sync_enrol::NodeIdentity;
use spot_sync_http::BoxFuture;
use spot_sync_protocol::NodeId;

use crate::error::{Error, Result};

/// The name of the stored hub pin inside the identity directory.
pub const PIN_FILE: &str = "hub.pin";

/// What the code file says.
#[derive(Clone, PartialEq, Eq)]
pub struct EnrolmentFile {
    code: String,
    hub_fingerprint: Option<String>,
}

impl std::fmt::Debug for EnrolmentFile {
    /// Never shows the code.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrolmentFile")
            .field("has_pin", &self.hub_fingerprint.is_some())
            .finish_non_exhaustive()
    }
}

impl EnrolmentFile {
    /// The one-time code.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// The hub certificate fingerprint the file names, if it does.
    pub fn hub_fingerprint(&self) -> Option<&str> {
        self.hub_fingerprint.as_deref()
    }

    /// Parses the file text; `None` for an empty file.
    ///
    /// # Errors
    ///
    /// `sync_node_host.code_file_unsafe` for an unknown key or a duplicate.
    pub fn parse(text: &str) -> Result<Option<Self>> {
        let mut code = None;
        let mut fingerprint = None;
        for line in text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            match line.split_once('=') {
                Some(("code", value)) if code.is_none() => code = Some(value.trim().to_owned()),
                Some(("hub_fingerprint", value)) if fingerprint.is_none() => {
                    fingerprint = Some(value.trim().to_owned());
                }
                None if code.is_none() => code = Some(line.to_owned()),
                _ => {
                    return Err(Error::code_file_unsafe(
                        "the enrolment file has an unknown or repeated line",
                    ));
                }
            }
        }
        Ok(code.filter(|code| !code.is_empty()).map(|code| Self {
            code,
            hub_fingerprint: fingerprint.filter(|f| !f.is_empty()),
        }))
    }
}

/// Reads the code file: `None` when it does not exist or is empty.
///
/// # Errors
///
/// `sync_node_host.code_file_unsafe` when it is not a regular file, is readable by group or
/// others, or cannot be read.
pub fn read_code_file(path: &Path) -> Result<Option<EnrolmentFile>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(Error::code_file_unsafe(
                "the enrolment file cannot be inspected",
            ));
        }
    };
    if !metadata.is_file() {
        return Err(Error::code_file_unsafe(
            "the enrolment file is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(Error::code_file_unsafe(
                "the enrolment file must be mode 0600",
            ));
        }
    }
    let text = fs::read_to_string(path)
        .map_err(|_| Error::code_file_unsafe("the enrolment file cannot be read"))?;
    EnrolmentFile::parse(&text)
}

/// Overwrites the file with zeros, flushes it and deletes it (empties it when it cannot be
/// deleted). A missing file is fine.
///
/// # Errors
///
/// `sync_node_host.code_file_unsafe` when the file can neither be removed nor emptied.
pub fn wipe_code_file(path: &Path) -> Result<()> {
    let length = match fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => {
            return Err(Error::code_file_unsafe(
                "the enrolment file cannot be inspected",
            ));
        }
    };
    let overwritten = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|mut file| {
            let zeros = vec![0_u8; usize::try_from(length).unwrap_or(0)];
            file.write_all(&zeros)?;
            file.sync_all()
        })
        .is_ok();
    if fs::remove_file(path).is_ok() {
        return Ok(());
    }
    if overwritten && fs::write(path, b"").is_ok() {
        return Ok(());
    }
    Err(Error::code_file_unsafe(
        "the enrolment file could neither be removed nor emptied",
    ))
}

/// The stored hub pin of an identity directory, if the node is enrolled.
pub fn load_pin(dir: &Path) -> Option<String> {
    let text = fs::read_to_string(dir.join(PIN_FILE)).ok()?;
    let pin = text.trim().to_owned();
    (!pin.is_empty()).then_some(pin)
}

/// Stores the hub pin (public data: a certificate fingerprint).
///
/// # Errors
///
/// `sync_node_host.store_failed`.
pub fn store_pin(dir: &Path, fingerprint: &str) -> Result<()> {
    fs::create_dir_all(dir)
        .map_err(|_| Error::store_failed("the identity directory is not writable"))?;
    let tmp = dir.join(format!("{PIN_FILE}.tmp"));
    let mut file = fs::File::create(&tmp)
        .map_err(|_| Error::store_failed("the hub pin could not be written"))?;
    file.write_all(fingerprint.trim().as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|_| Error::store_failed("the hub pin could not be written"))?;
    fs::rename(&tmp, dir.join(PIN_FILE))
        .map_err(|_| Error::store_failed("the hub pin could not be stored"))
}

/// Redeems a one-time code at the hub: registers the node's certificate against the code. The
/// hub half (WP-RTB-02.T3 `EnrolService::redeem`, hosted by WP-RTB-02.T4) decides.
pub trait Enroller: Send + Sync {
    /// Redeems `code` for `node`, presenting `identity`'s certificate, over a connection that
    /// pins `hub_fingerprint`. Never logs the code.
    fn redeem(
        &self,
        node: &NodeId,
        code: &str,
        identity: &NodeIdentity,
        hub_fingerprint: &str,
    ) -> BoxFuture<Result<()>>;
}

/// The path the redemption is posted to on the hub.
pub const ENROL_PATH: &str = "/sync/v1/enrol";

/// Builds a transport for one redemption: pinned to `hub_fingerprint`, presenting `identity`.
pub type TransportFactory =
    Arc<dyn Fn(&NodeIdentity, &str) -> Result<Arc<dyn spot_sync_http::Transport>> + Send + Sync>;

/// [`Enroller`] over the wire: `POST /sync/v1/enrol` with the node, the code and the certificate
/// (hex). The hub half (WP-RTB-02.T3's `EnrolService::redeem`, hosted by WP-RTB-02.T4) answers
/// 2xx when the code is accepted; any other answer is an error carrying the hub's own code
/// (`sync_enrol.code_invalid` for an unknown, expired or used code: the same answer for all
/// three, on purpose).
pub struct HttpEnroller {
    transport: TransportFactory,
}

impl HttpEnroller {
    /// An enroller that dials with transports from `transport`.
    pub fn new(transport: TransportFactory) -> Self {
        Self { transport }
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

impl Enroller for HttpEnroller {
    fn redeem(
        &self,
        node: &NodeId,
        code: &str,
        identity: &NodeIdentity,
        hub_fingerprint: &str,
    ) -> BoxFuture<Result<()>> {
        let transport = (self.transport)(identity, hub_fingerprint);
        let body = serde_json::json!({
            "node": node.as_str(),
            "code": code,
            "certificate": hex(identity.certificate_der()),
        })
        .to_string();
        Box::pin(async move {
            let transport = transport?;
            let request = spot_sync_http::HttpRequest::post(ENROL_PATH, body)
                .with_content_type("application/json");
            let response = transport.request(request).await?;
            if response.is_success() {
                return Ok(());
            }
            // `{ "error": { "code": ... } }`: keep the hub's code, never echo the body.
            #[derive(serde::Deserialize)]
            struct Remote {
                error: RemoteDetail,
            }
            #[derive(serde::Deserialize)]
            struct RemoteDetail {
                code: String,
                message: String,
            }
            match serde_json::from_slice::<Remote>(&response.body) {
                Ok(remote) => Err(spot_sync_http::Error::from_code(
                    &remote.error.code,
                    remote.error.message,
                )
                .into()),
                Err(_) => Err(Error::new(
                    spot_sync_http::error::UNAVAILABLE,
                    format!("the hub answered {} to the enrolment", response.status),
                )),
            }
        })
    }
}

/// The result of the start-up identity step.
pub enum Identity {
    /// Enrolled: the node's identity and the pinned hub fingerprint.
    Enrolled {
        /// The node's key pair and self-signed certificate.
        identity: NodeIdentity,
        /// The hub certificate fingerprint to pin.
        hub_fingerprint: String,
    },
    /// No code and no pin: the loop does not start.
    NotEnrolled,
}

/// Where the identity step reads and writes.
#[derive(Clone, Debug)]
pub struct IdentityPaths {
    /// The identity directory (key, certificate, pin).
    pub dir: PathBuf,
    /// `services.sync_node.enrol_code_file`, when configured.
    pub code_file: Option<PathBuf>,
    /// The pin configured in the site's configuration, used when the code file names none.
    pub configured_pin: Option<String>,
}

/// The start-up identity step: loads or creates the key pair, redeems a present code file once,
/// stores the pin, wipes the file.
///
/// A node already pinned never redeems again; a leftover code file next to a stored pin is wiped.
///
/// # Errors
///
/// `sync_node_host.code_file_unsafe` for an unsafe file, the identity's own failures as
/// `sync_node_host.store_failed`, and the enroller's error when the redemption is refused (the
/// code file is left in place so the operator can see it and replace it).
pub async fn identity_step(
    paths: &IdentityPaths,
    node: &NodeId,
    now_unix: i64,
    enroller: &dyn Enroller,
) -> Result<Identity> {
    let identity = NodeIdentity::load_or_create(&paths.dir, node, now_unix)
        .map_err(|_| Error::store_failed("the node identity could not be loaded or created"))?;
    if let Some(pin) = load_pin(&paths.dir) {
        if let Some(path) = &paths.code_file {
            wipe_code_file(path)?;
        }
        return Ok(Identity::Enrolled {
            identity,
            hub_fingerprint: pin,
        });
    }
    let Some(path) = &paths.code_file else {
        return Ok(Identity::NotEnrolled);
    };
    let Some(file) = read_code_file(path)? else {
        return Ok(Identity::NotEnrolled);
    };
    let Some(pin) = file
        .hub_fingerprint()
        .map(str::to_owned)
        .or_else(|| paths.configured_pin.clone())
    else {
        return Err(Error::not_enrolled(
            "the enrolment file and the configuration name no hub certificate fingerprint",
        ));
    };
    enroller.redeem(node, file.code(), &identity, &pin).await?;
    store_pin(&paths.dir, &pin)?;
    wipe_code_file(path)?;
    Ok(Identity::Enrolled {
        identity,
        hub_fingerprint: pin,
    })
}
