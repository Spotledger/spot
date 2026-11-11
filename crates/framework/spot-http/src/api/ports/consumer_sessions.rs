//! The consumer session seam of `POST /api/v1/auth/consumer/renew` (`auth_consumer_renew`,
//! contract 0.10.0, WP-CON-03.T14; ADR-083, ADR-088 section 7.2, ADR-092 section 10.1).
//!
//! The renewal cookie is the only credential of the call. It works **once**: a renewal consumes
//! the cookie it was sent with and issues the next in the same family. A cookie that was already
//! used or revoked and is sent again is treated as stolen, and the port revokes the whole family
//! (every cookie and every session token issued from that sign-in). A cookie of another site is
//! unknown here, so it can neither renew nor revoke anything. The port is the only place a
//! secret exists: nothing in this file logs one, and [`Renewal`] redacts them in `Debug`.
//!
//! [`MemoryConsumerSessions`] is the in-memory fake the tests use, timed by an injected clock.
//! Its secrets come from a seeded hash and are **not** unpredictable: a test double only.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use sha2::{Digest, Sha256};
use spot_core::Clock;
use time::OffsetDateTime;

use super::PortKey;
use crate::api::service::{BoxFuture, RequestCtx};

/// The port key of [`ConsumerSessions`].
pub const KEY: PortKey = PortKey::of::<dyn ConsumerSessions>();

/// The renewal cookie's name (the `__Host-` prefix makes a browser insist on `Path=/`, `Secure`
/// and no `Domain`).
pub const COOKIE_NAME: &str = "__Host-spot_consumer_renew";

/// The renewal cookie's lifetime: 30 days.
pub const COOKIE_MAX_AGE_SECS: u64 = 2_592_000;

/// A successful renewal: the new session token and the next cookie of the family.
#[derive(Clone, PartialEq, Eq)]
pub struct Renewal {
    /// The new opaque bearer token (a secret).
    pub token: String,
    /// When the token ends: a UTC RFC 3339 instant ending in `Z`.
    pub expires_at: String,
    /// The next renewal cookie value (a secret): cookie-safe characters only.
    pub next_cookie: String,
}

impl std::fmt::Debug for Renewal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renewal")
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("next_cookie", &"<redacted>")
            .finish()
    }
}

/// Why a renewal did not happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenewError {
    /// The cookie is unknown, expired, revoked or replayed (a replay also revoked the family).
    /// One answer for all of them: the caller learns nothing about which.
    SessionExpired,
    /// The store cannot answer now; the cookie was not consumed.
    Unavailable,
}

/// Renews a consumer's session from the renewal cookie.
pub trait ConsumerSessions: Send + Sync + 'static {
    /// Swaps the one-use `cookie` (the value only) for a new token and the next cookie, on
    /// `ctx.site`. On a replay of a used or revoked cookie, revokes the whole family.
    fn renew<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        cookie: &'a str,
    ) -> BoxFuture<'a, Result<Renewal, RenewError>>;
}

struct CookieRecord {
    site: String,
    family: u64,
    used: bool,
    expires_at: OffsetDateTime,
}

struct TokenRecord {
    site: String,
    family: u64,
    expires_at: OffsetDateTime,
}

#[derive(Default)]
struct Inner {
    counter: u64,
    cookies: HashMap<[u8; 32], CookieRecord>,
    tokens: HashMap<[u8; 32], TokenRecord>,
    revoked_families: Vec<u64>,
}

/// An in-memory [`ConsumerSessions`] for tests, with an injected clock.
pub struct MemoryConsumerSessions {
    clock: Arc<dyn Clock>,
    seed: String,
    token_ttl: time::Duration,
    inner: Mutex<Inner>,
}

fn digest(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

fn rfc3339(at: OffsetDateTime) -> String {
    let at = at.replace_nanosecond(0).unwrap_or(at);
    at.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

impl MemoryConsumerSessions {
    /// A source whose tokens live `token_ttl` and whose secrets derive from `seed`.
    pub fn new(clock: Arc<dyn Clock>, seed: &str, token_ttl: time::Duration) -> Self {
        MemoryConsumerSessions {
            clock,
            seed: seed.to_owned(),
            token_ttl,
            inner: Mutex::new(Inner::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn secret(&self, inner: &mut Inner, label: &str) -> String {
        inner.counter += 1;
        let raw = Sha256::digest(format!("{}:{label}:{}", self.seed, inner.counter).as_bytes());
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
    }

    fn issue(&self, inner: &mut Inner, site: &str, family: u64) -> (String, String, String) {
        let now = self.clock.now_utc();
        let cookie = self.secret(inner, "cookie");
        inner.cookies.insert(
            digest(&cookie),
            CookieRecord {
                site: site.to_owned(),
                family,
                used: false,
                expires_at: now + time::Duration::seconds(COOKIE_MAX_AGE_SECS as i64),
            },
        );
        let token = self.secret(inner, "token");
        let token_expires = now + self.token_ttl;
        inner.tokens.insert(
            digest(&token),
            TokenRecord {
                site: site.to_owned(),
                family,
                expires_at: token_expires,
            },
        );
        (cookie, token, rfc3339(token_expires))
    }

    /// A sign-in on `site`: starts a family and returns its first (cookie, token).
    pub fn sign_in(&self, site: &str) -> (String, String) {
        let mut inner = self.lock();
        let family = inner.counter + 1;
        let (cookie, token, _) = self.issue(&mut inner, site, family);
        (cookie, token)
    }

    /// Whether `token` is a live session token on `site` (not ended, family not revoked).
    pub fn token_is_live(&self, site: &str, token: &str) -> bool {
        let inner = self.lock();
        inner.tokens.get(&digest(token)).is_some_and(|t| {
            t.site == site
                && t.expires_at > self.clock.now_utc()
                && !inner.revoked_families.contains(&t.family)
        })
    }

    /// Whether any cookie of `cookie`'s family is still usable on `site`.
    pub fn family_is_revoked(&self, site: &str, cookie: &str) -> bool {
        let inner = self.lock();
        inner
            .cookies
            .get(&digest(cookie))
            .is_some_and(|c| c.site == site && inner.revoked_families.contains(&c.family))
    }
}

impl ConsumerSessions for MemoryConsumerSessions {
    fn renew<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        cookie: &'a str,
    ) -> BoxFuture<'a, Result<Renewal, RenewError>> {
        Box::pin(async move {
            let site = ctx.site.as_str();
            let mut inner = self.lock();
            let key = digest(cookie);
            let (family, used) = match inner.cookies.get(&key) {
                // Another site's cookie is unknown here: it must not revoke that site's family.
                Some(c) if c.site == site => (c.family, c.used),
                _ => return Err(RenewError::SessionExpired),
            };
            if used || inner.revoked_families.contains(&family) {
                if !inner.revoked_families.contains(&family) {
                    inner.revoked_families.push(family);
                }
                return Err(RenewError::SessionExpired);
            }
            let expired = inner
                .cookies
                .get(&key)
                .is_none_or(|c| c.expires_at <= self.clock.now_utc());
            if expired {
                return Err(RenewError::SessionExpired);
            }
            if let Some(c) = inner.cookies.get_mut(&key) {
                c.used = true;
            }
            let (next_cookie, token, expires_at) = self.issue(&mut inner, site, family);
            Ok(Renewal {
                token,
                expires_at,
                next_cookie,
            })
        })
    }
}
