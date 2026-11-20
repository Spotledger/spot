//! Renewal from the control plane (WP-CTL-04.T3): fetch `GET /cp/v1/host/licence`, verify, apply.
//!
//! The control plane answers `{"data": {"licence": "<text form>", "proof": "...", "valid_until":
//! "...", "revoked": ["<licence id>", ...]}}`; only `licence` is required (`revoked` is an optional
//! list of licence ids the control plane has revoked, `proof` and `valid_until` are informative:
//! the signature is the proof). The licence is verified against the **local** trusted keys, so a
//! control plane (or anything pretending to be one) cannot install a licence the issuer did not
//! sign. A licence that is older or equal in serial, for another tenant, or revoked is refused
//! with its stable code and changes nothing. Offline for the whole grace period is normal: an
//! unreachable control plane is a recorded attempt, not an error anyone sees.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use spot_egress::{EgressConnector, HttpRequest, Sender};
use spot_license::Status;

use super::code;
use super::host::{HostError, Installed, LicenceGate, LicenceHost, Standing};

/// The control plane's endpoint.
pub const ENDPOINT: &str = "/cp/v1/host/licence";
/// The largest response read.
pub const MAX_RESPONSE: u64 = 64 * 1024;

/// What a fetch returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetched {
    /// The licence's text form.
    pub licence_text: String,
    /// Licence ids the control plane reports revoked.
    pub revoked: Vec<String>,
}

/// Why a fetch gave nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchError {
    /// No answer, a 5xx or a 429: try again later.
    Unreachable,
    /// The control plane has no licence for this tenant (404: none issued, or revoked).
    NotAvailable,
    /// An answer that is not the documented shape.
    Malformed,
}

impl FetchError {
    /// The stable code recorded as the attempt's result.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Unreachable => code::FETCH_UNREACHABLE,
            Self::NotAvailable => code::FETCH_NONE,
            Self::Malformed => code::FETCH_MALFORMED,
        }
    }
}

/// Fetches the tenant's current licence over mutual TLS.
#[async_trait]
pub trait LicenceFetcher: Send + Sync {
    /// One fetch.
    async fn fetch(&self) -> Result<Fetched, FetchError>;
}

#[derive(Deserialize)]
struct Envelope {
    data: Data,
}

#[derive(Deserialize)]
struct Data {
    licence: String,
    #[serde(default)]
    revoked: Vec<String>,
}

/// Parses the response body.
///
/// # Errors
/// [`FetchError::Malformed`] for anything but the documented shape.
pub fn parse_response(body: &[u8]) -> Result<Fetched, FetchError> {
    let envelope: Envelope = serde_json::from_slice(body).map_err(|_| FetchError::Malformed)?;
    if envelope.data.licence.is_empty() || envelope.data.revoked.len() > 1_000 {
        return Err(FetchError::Malformed);
    }
    Ok(Fetched {
        licence_text: envelope.data.licence,
        revoked: envelope.data.revoked,
    })
}

/// Maps an HTTP status and body to a fetch result.
///
/// # Errors
/// The [`FetchError`] the status means.
pub fn interpret(status: u16, body: &[u8]) -> Result<Fetched, FetchError> {
    match status {
        200 => parse_response(body),
        404 => Err(FetchError::NotAvailable),
        _ => Err(FetchError::Unreachable),
    }
}

/// A fetcher over the shared egress guard: the control-plane URL is authorised first (a missing
/// allow-list entry is refused and recorded like any denied call), then sent through `sender`,
/// which the host builds as an `HttpSender::with_tls` over the host certificate, key and CA
/// (WP-SEC-09.T8). Redirects are not followed.
pub struct EgressFetcher {
    connector: EgressConnector,
    sender: Arc<dyn Sender>,
    url: url::Url,
}

impl EgressFetcher {
    /// A fetcher for `control_plane` (`https://host[:port]`).
    ///
    /// # Errors
    /// A [`HostError`] when the URL does not parse.
    pub fn new(
        connector: EgressConnector,
        sender: Arc<dyn Sender>,
        control_plane: &str,
    ) -> Result<Self, HostError> {
        let url = url::Url::parse(&format!(
            "{}{ENDPOINT}",
            control_plane.trim_end_matches('/')
        ))
        .map_err(|_| {
            HostError::new(
                code::INVALID_SETTING,
                "services.licence.control_plane must be an https://host[:port] URL",
            )
        })?;
        Ok(Self {
            connector,
            sender,
            url,
        })
    }
}

#[async_trait]
impl LicenceFetcher for EgressFetcher {
    async fn fetch(&self) -> Result<Fetched, FetchError> {
        let authorized = self
            .connector
            .authorize(&self.url)
            .await
            .map_err(|_| FetchError::Unreachable)?;
        let request = HttpRequest::get(self.url.clone(), http::HeaderMap::new());
        let response = self
            .sender
            .send_request(&request, &authorized, MAX_RESPONSE)
            .await
            .map_err(|_| FetchError::Unreachable)?;
        interpret(response.status.as_u16(), &response.body)
    }
}

/// What one renewal attempt did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenewReport {
    /// `ok`, or the stable code of why nothing changed.
    pub code: String,
    /// The licence now in force, when the attempt installed one.
    pub installed: Option<Installed>,
}

/// Whether a standing is one a renewal is worth attempting from: the renewal point has passed, the
/// licence has expired into grace or restriction, or the clock is suspect (a renewal ends that).
#[must_use]
pub fn wants_renewal(standing: Standing) -> bool {
    matches!(
        standing,
        Standing::Licensed(
            Status::RenewalDue
                | Status::Grace { .. }
                | Status::Restricted
                | Status::ClockSuspect { .. }
        )
    )
}

/// One attempt: fetch, verify, apply, record. `force` skips the standing check (the
/// `spot licence renew` command).
pub async fn renew_once(
    host: &LicenceHost,
    fetcher: &dyn LicenceFetcher,
    force: bool,
) -> RenewReport {
    if !force && !wants_renewal(LicenceGate::standing(host)) {
        return RenewReport {
            code: "not_due".to_owned(),
            installed: None,
        };
    }
    let report = match fetcher.fetch().await {
        Err(error) => RenewReport {
            code: error.code().to_owned(),
            installed: None,
        },
        Ok(fetched) => match host.install_text(&fetched.licence_text, true, &fetched.revoked) {
            Ok(installed) => RenewReport {
                code: "ok".to_owned(),
                installed: Some(installed),
            },
            Err(error) => RenewReport {
                code: error.code,
                installed: None,
            },
        },
    };
    host.record_attempt(&report.code);
    report
}
