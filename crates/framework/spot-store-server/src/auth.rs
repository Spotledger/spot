//! Authentication: publisher keys, the operator credential and site licences.
//!
//! A publisher key is `Authorization: Bearer spk_...`. Only its SHA-256 is stored; the presented
//! key is hashed and the hashes are compared in constant time, and a key that is unknown, revoked
//! or malformed answers the same `store.unauthorized`. A key is bound to exactly one publisher id
//! and cannot act for another. Failed checks are counted per source address and, past the limit,
//! refused before they reach the store.

use axum::http::{HeaderMap, header};
use spot_store_commerce::{SignedAppLicence, StoreSigner, check_licence, trusted_keys};

use crate::error::{ApiError, Wire};
use crate::keys::{ct_eq, hash_key, hex, is_publisher_token};
use crate::metrics::Metrics;
use crate::ratelimit::Class;
use crate::service::{Core, Service};

/// Facts about the request every handler needs (the source address); the context middleware
/// puts it in the request.
#[derive(Clone, Debug)]
pub(crate) struct ReqCtx {
    pub addr: String,
}

/// An authenticated publisher key.
#[derive(Clone, Debug)]
pub(crate) struct Principal {
    pub publisher_id: String,
    /// A handle for rate limiting this key; derived from the stored hash, never the key.
    pub rate_key: String,
}

fn unauthorized() -> ApiError {
    ApiError::wire(Wire::Unauthorized)
}

/// The bearer credential of a request: `Ok(None)` when there is none, an error when the header is
/// repeated, malformed or longer than `max` bytes.
pub(crate) fn bearer(headers: &HeaderMap, max: usize) -> Result<Option<String>, ApiError> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let Some(first) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(unauthorized());
    }
    let text = first.to_str().map_err(|_| unauthorized())?;
    let (scheme, token) = text.split_once(' ').ok_or_else(unauthorized)?;
    if !scheme.eq_ignore_ascii_case("bearer")
        || token.is_empty()
        || token.len() > max
        || token.contains(' ')
    {
        return Err(unauthorized());
    }
    Ok(Some(token.to_owned()))
}

/// Looks the key up and checks it, on a worker thread. The comparison runs in constant time on
/// the SHA-256 of the presented key, also when no such key exists, and the answer to every
/// failure is the same.
pub(crate) fn authenticate_sync(core: &Core, token: &str) -> Result<Principal, ApiError> {
    let presented = hash_key(token);
    let record = core.state.lookup_publisher_key(&presented)?;
    let stored = record
        .as_ref()
        .map_or([0_u8; 32], |found| *found.key_hash.as_bytes());
    let same = ct_eq(&stored, presented.as_bytes());
    match record {
        Some(found) if same && found.revoked_at.is_none() => Ok(Principal {
            publisher_id: found.publisher_id,
            rate_key: hex(&presented.as_bytes()[..8]),
        }),
        _ => Err(unauthorized()),
    }
}

/// Requires a publisher key: counts a failure against the source address and refuses an address
/// that fails too often.
pub(crate) async fn require_publisher(
    service: &Service,
    addr: &str,
    headers: &HeaderMap,
) -> Result<Principal, ApiError> {
    let now = service.core.clock.now();
    if let Err(wait) = service.limiter.check(Class::AuthFailure, addr, now) {
        Metrics::bump(&service.core.metrics.rate_limited);
        return Err(ApiError::rate_limited(wait));
    }
    let token = bearer(headers, service.core.config.limits.max_credential_bytes)?
        .ok_or_else(unauthorized)?;
    if !is_publisher_token(&token) {
        return Err(refuse(service, addr, now));
    }
    let core = std::sync::Arc::clone(&service.core);
    match service
        .pool
        .call(move || authenticate_sync(&core, &token))
        .await?
    {
        Ok(principal) => Ok(principal),
        Err(error) if error.code == unauthorized().code => Err(refuse(service, addr, now)),
        Err(error) => Err(error),
    }
}

/// Reads a bearer that may be a publisher key (optional credential on a public read): `None`
/// without a credential, the principal with a valid key, `store.unauthorized` with a bad one.
pub(crate) async fn optional_publisher(
    service: &Service,
    addr: &str,
    headers: &HeaderMap,
) -> Result<Option<Principal>, ApiError> {
    if headers.get(header::AUTHORIZATION).is_none() {
        return Ok(None);
    }
    require_publisher(service, addr, headers).await.map(Some)
}

fn refuse(service: &Service, addr: &str, now: i64) -> ApiError {
    // A failed check counts against the source address; the window it opens is what stops a
    // key-guessing client. The count is never an error here: the credential error is.
    let _ = service.limiter.hit(Class::AuthFailure, addr, now);
    Metrics::bump(&service.core.metrics.auth_failures);
    unauthorized()
}

/// Whether the operator credential was presented.
pub(crate) fn admin_check(core: &Core, headers: &HeaderMap) -> Result<(), ApiError> {
    let token = bearer(headers, core.config.limits.max_credential_bytes)
        .map_err(|_| ApiError::admin_unauthorized())?
        .ok_or_else(ApiError::admin_unauthorized)?;
    if core.admin.matches(&token) {
        Ok(())
    } else {
        Err(ApiError::admin_unauthorized())
    }
}

/// Whether `token` is a site licence that covers `app` right now. A licence that does not parse,
/// does not verify under the store key, covers another app or has lapsed is "not entitled".
pub(crate) fn entitled(core: &Core, token: Option<&str>, app: &str, now: i64) -> bool {
    let Some(text) = token else {
        return false;
    };
    let Ok(signed) = SignedAppLicence::from_text(text) else {
        return false;
    };
    // The licence names its tenant; this version of the store has no tenant of its own to compare
    // it with (WP-STR-08), so the licence's own tenant is the one checked.
    let tenant = signed.licence().tenant.clone();
    let signer: &dyn StoreSigner = &*core.store_key;
    let Ok(trusted) = trusted_keys(&[signer]) else {
        return false;
    };
    check_licence(&signed, &trusted, &tenant, app, now).is_ok_and(|check| check.grants_use())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::bearer;
    use axum::http::{HeaderMap, HeaderValue, header};

    fn headers(values: &[&str]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for value in values {
            map.append(header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn a_bearer_is_read_and_everything_else_is_refused() {
        assert_eq!(bearer(&headers(&[]), 100).unwrap(), None);
        assert_eq!(
            bearer(&headers(&["Bearer abc"]), 100).unwrap().as_deref(),
            Some("abc")
        );
        assert_eq!(
            bearer(&headers(&["bearer abc"]), 100).unwrap().as_deref(),
            Some("abc")
        );
        assert!(bearer(&headers(&["Basic abc"]), 100).is_err());
        assert!(bearer(&headers(&["Bearer "]), 100).is_err());
        assert!(bearer(&headers(&["Bearer a b"]), 100).is_err());
        assert!(bearer(&headers(&["Bearer abcdef"]), 3).is_err());
        assert!(bearer(&headers(&["Bearer a", "Bearer b"]), 100).is_err());
    }
}
