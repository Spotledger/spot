//! The system trust store, used when the operator gave no `--ca`.
//!
//! On Linux and other Unix hosts the distribution's CA bundle is a PEM file at a well-known
//! place; the first candidate that holds a usable certificate wins. The candidate list is an
//! argument so tests use their own files. On Windows the certificate store is not a file:
//! this build reports that clearly and the operator passes `--ca` (see the task report for
//! the `rustls-native-certs` follow-up, which needs a `Cargo.toml` line outside the owned
//! paths).
//!
//! Nothing here is a trust decision: it only collects certificates; rustls validates the
//! gateway chain against them.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::error::AgentError;

/// Where Unix distributions keep the CA bundle, in the order tried.
pub const UNIX_BUNDLES: &[&str] = &[
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/ca-bundle.pem",
    "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
    "/etc/ssl/cert.pem",
];

/// A bundle holds hundreds of certificates and the occasional odd one: a block that does not
/// decode is skipped, not fatal (the pinned `--ca` parser is strict on purpose).
fn lenient_pem_certificates(pem: &str) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut current: Option<String> = None;
    for line in pem.lines() {
        let line = line.trim();
        if line == "-----BEGIN CERTIFICATE-----" {
            current = Some(String::new());
        } else if line == "-----END CERTIFICATE-----"
            && let Some(body) = current.take()
            && let Ok(der) = STANDARD.decode(body.as_bytes())
            && !der.is_empty()
        {
            out.push(der);
        } else if let Some(body) = current.as_mut() {
            body.push_str(line);
        }
    }
    out
}

/// The DER certificates of the first of `candidates` that exists and holds at least one.
///
/// # Errors
/// [`AgentError::Transport`] when no candidate holds a certificate; the text asks for
/// `--ca` and names no path.
pub fn system_roots_from(candidates: &[PathBuf]) -> Result<Vec<Vec<u8>>, AgentError> {
    for path in candidates {
        if let Some(certs) = read_bundle(path) {
            return Ok(certs);
        }
    }
    Err(AgentError::Transport(
        "no system trust store was found on this host; pass --ca with the gateway CA file"
            .to_owned(),
    ))
}

fn read_bundle(path: &Path) -> Option<Vec<Vec<u8>>> {
    let text = std::fs::read_to_string(path).ok()?;
    let certs = lenient_pem_certificates(&text);
    (!certs.is_empty()).then_some(certs)
}

/// The system trust store of this host.
///
/// # Errors
/// [`AgentError::Transport`] when none can be read (always on Windows in this build).
pub fn system_roots() -> Result<Vec<Vec<u8>>, AgentError> {
    #[cfg(unix)]
    {
        let candidates: Vec<PathBuf> = UNIX_BUNDLES.iter().map(PathBuf::from).collect();
        system_roots_from(&candidates)
    }
    #[cfg(not(unix))]
    {
        system_roots_from(&[])
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn pem(bytes: &[u8]) -> String {
        format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            STANDARD.encode(bytes)
        )
    }

    #[test]
    fn first_usable_candidate_wins_and_odd_blocks_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty.pem");
        std::fs::write(&empty, "no certificates here\n").unwrap();
        let good = dir.path().join("good.pem");
        let text = format!(
            "{}-----BEGIN CERTIFICATE-----\n!!!not base64!!!\n-----END CERTIFICATE-----\n{}",
            pem(b"one"),
            pem(b"two")
        );
        std::fs::write(&good, text).unwrap();
        let roots = system_roots_from(&[dir.path().join("missing.pem"), empty, good]).unwrap();
        assert_eq!(roots, vec![b"one".to_vec(), b"two".to_vec()]);
    }

    #[test]
    fn no_candidate_is_a_clear_error() {
        let err = system_roots_from(&[PathBuf::from("/nonexistent/bundle.pem")]).unwrap_err();
        assert_eq!(err.code(), "agent.transport");
        assert!(err.to_string().contains("--ca"));
    }
}
