//! Inbound signature verification, before anything else is done with a request.
//!
//! HMAC-SHA256 over `<unix seconds> + "." + <raw body>`, compared in constant time (the
//! `spot-webhooks` signing key does the comparison), with a timestamp window. The secret is
//! resolved by the caller at call time and only borrowed here.

use spot_webhooks::SigningKey;
use spot_webhooks::signing::parse_signature_header;

use crate::error::{Error, Result};
use crate::peer::{ExternalPeer, SignatureScheme};

/// The platform's signature header (case-insensitive on the wire).
pub const SPOT_SIGNATURE_HEADER: &str = "x-spot-signature";

fn header<'h>(headers: &'h [(String, String)], name: &str) -> Option<&'h str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// Verifies one inbound request.
///
/// `now_ms` is the receiver's instant (an argument: nothing here reads a clock) and
/// `tolerance_secs` the window either side of it.
///
/// # Errors
/// [`Error::MissingSignature`] when the scheme's headers are absent or unreadable,
/// [`Error::StaleTimestamp`] outside the window, [`Error::BadSignature`] when the signature
/// does not verify, and [`Error::ToleranceTooWide`] when `tolerance_secs` is not in
/// `1..=`[`ExternalPeer::MAX_TOLERANCE_SECS`] (fail closed, even if a caller skipped validation).
pub fn verify_inbound(
    scheme: &SignatureScheme,
    secret: &[u8],
    headers: &[(String, String)],
    body: &[u8],
    now_ms: u64,
    tolerance_secs: i64,
) -> Result<()> {
    if tolerance_secs <= 0 || tolerance_secs > ExternalPeer::MAX_TOLERANCE_SECS {
        return Err(Error::ToleranceTooWide);
    }
    let (timestamp, signature) = match scheme {
        SignatureScheme::SpotHmacSha256 => {
            let value = header(headers, SPOT_SIGNATURE_HEADER).ok_or(Error::MissingSignature)?;
            parse_signature_header(value).map_err(|_| Error::MissingSignature)?
        }
        SignatureScheme::HeaderHmacSha256 {
            signature_header,
            timestamp_header,
        } => {
            let signature = header(headers, signature_header).ok_or(Error::MissingSignature)?;
            let timestamp = header(headers, timestamp_header)
                .and_then(|text| text.trim().parse::<i64>().ok())
                .ok_or(Error::MissingSignature)?;
            (timestamp, signature.trim().to_owned())
        }
    };
    let now_secs = i64::try_from(now_ms / 1_000).map_err(|_| Error::StaleTimestamp)?;
    if now_secs.saturating_sub(timestamp).saturating_abs() > tolerance_secs {
        return Err(Error::StaleTimestamp);
    }
    match SigningKey::new(secret).verify(timestamp, body, &signature) {
        Ok(true) => Ok(()),
        _ => Err(Error::BadSignature),
    }
}
