//! The session model: [`Session`], its state, policy defaults and the bounded identifiers.
//!
//! Defaults come from ADR-070 (sessions and tokens rule, §5.4) and its NIST anchor: the
//! idle and absolute bounds are **tenant policy**; the values below are this crate's
//! shipped defaults, chosen to fall inside the bounds ADR-070 §12.13.d leans on (NIST
//! SP 800-63B's 12 h / 24 h session guidance):
//!
//! - idle 30 minutes — the ordinary interactive gap (a tea break is not a hijack);
//! - absolute 12 hours — a day's work, after which a fresh sign-in is required;
//! - rotate_after 8 hours — token rotation due before the absolute end (rotation re-issues
//!   the token and kills the old digest at once, §5.5's "a password change ends the others"
//!   is enforced through [`SessionStore::revoke_all_for_user`]);
//! - max_per_user 8 concurrent sessions — enough for two machines and a phone with margin,
//!   and the eviction cap that keeps a leaked client from multiplying sessions forever.
//!
//! Every default is overridable field by field: [`SessionPolicy::new`] is the const
//! constructor, [`SessionPolicy::DEFAULT`] the cited preset.
use std::fmt;
use std::time::Duration;

use spot_auth::TokenDigest;

pub(crate) use crate::crypto::{fill_os_random, sha256_concat};

/// The idle timeout of the shipped default policy: 30 minutes.
pub const DEFAULT_IDLE: Duration = Duration::from_secs(30 * 60);
/// The absolute lifetime of the shipped default policy: 12 hours (the NIST 800-63B anchor
/// ADR-070 cites for interactive sessions).
pub const DEFAULT_ABSOLUTE: Duration = Duration::from_secs(12 * 60 * 60);
/// The token age after which rotation is due in the shipped default policy: 8 hours.
pub const DEFAULT_ROTATE_AFTER: Duration = Duration::from_secs(8 * 60 * 60);
/// The concurrent-session cap of the shipped default policy (ADR-070 §5.4 lists and ends
/// sessions; this bound keeps the list reviewable).
pub const DEFAULT_MAX_PER_USER: usize = 8;

/// Touch granularity: a touch that would extend the idle window by less than this is
/// **skipped**, so a busy client polling every request does not write the store per request.
/// The store contract (half-open windows, second resolution) makes one minute exact.
pub const TOUCH_GRANULARITY: Duration = Duration::from_secs(60);
/// Longest bounded string [`ClientInfo`] stores, per field.
pub const CLIENT_INFO_MAX_CHARS: usize = 64;

/// Identifies the user a session belongs to: the stable record id of the tenant's `User`
/// DocType (ADR-070 §1.1 — a user is never deleted, so ids are stable forever).
///
/// An opaque text key on purpose: the sessions crate knows nothing about record ids and the
/// directory layer (WP-IAM-02) hands them over as strings.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UserKey(String);

impl UserKey {
    /// Wraps a user id. Empty keys are refused: a session must belong to someone.
    pub fn new(id: impl Into<String>) -> Result<Self, String> {
        let id = id.into();
        if id.is_empty() {
            return Err("the user key is empty".to_owned());
        }
        if id.len() > 320 {
            return Err("the user key is longer than 320 bytes".to_owned());
        }
        Ok(Self(id))
    }

    /// The user id, for stores and logs. A user id is not a secret (ADR-070 §8.4 events
    /// name the user openly).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for UserKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a sign-in came from, as the at-rest form: the salted digest of the client's
/// address, never the address itself.
///
/// ADR-070 §5.4 has every session record its source address; storing the raw address would
/// put personal data (and a loggable secret) in every session row, so [`SaltKey`] salts a
/// SHA-256 over it and only this digest is carried or stored. Same address + same salt ⇒
/// same [`SourceHash`]; different salt ⇒ no cross-site linkage.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SourceHash([u8; 32]);

impl SourceHash {
    /// The 32 digest bytes, for the storage layer.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Rebuilds a digest from stored bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let array: [u8; 32] = bytes
            .try_into()
            .map_err(|_| "the source digest is not 32 bytes".to_owned())?;
        Ok(Self(array))
    }
}

impl fmt::Display for SourceHash {
    /// Renders the first 8 bytes Base16-style, for audit lines that must show *that* two
    /// sessions came from one source without storing the address.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0[..8] {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// The per-site salt a [`SourceHash`] is taken under.
///
/// One site owns one salt (rotating it invalidates every stored digest at once — the site
/// regenerates them on next use, which is the point: a salt is cheap to change). The salt
/// is **not** secret-class material (it defends against cross-site linkage, not against a
/// direct lookup), but it is still never logged next to a [`SourceHash`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SaltKey([u8; 32]);

impl SaltKey {
    /// A fresh random salt (32 CSPRNG bytes).
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        fill_os_random(&mut bytes);
        Self(bytes)
    }

    /// Wraps provided salt bytes (for a salt persisted by the site).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let array: [u8; 32] = bytes
            .try_into()
            .map_err(|_| "the salt is not 32 bytes".to_owned())?;
        Ok(Self(array))
    }

    /// The salt bytes, for persisting it.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Digests a raw source address under this salt: SHA-256(salt || 0x00 || address).
    ///
    /// The zero byte separates the two lengths so no salt/address split can collide
    /// (`salt=A|addr=BC` can never equal `salt=AB|addr=C`).
    #[must_use]
    pub fn digest_source(&self, source: &str) -> SourceHash {
        SourceHash(sha256_concat(self.0, source.as_bytes()))
    }
}

/// The bounded, labelled context a session was opened from: device label and client
/// description as the *user* named them (ADR-070 §5.4: every session records the method,
/// source address and device label).
///
/// Every field is capped at [`CLIENT_INFO_MAX_CHARS`] characters and carries no secrets
/// (a label is display text; tokens and addresses never live here). Construction refuses
/// anything longer so a hostile login payload cannot bloat session rows.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct ClientInfo {
    /// The device or browser label ("Ayosha's ThinkPad", "pos-terminal-3").
    pub device: Option<String>,
    /// The client application's own free-text description.
    pub client: Option<String>,
}

impl ClientInfo {
    /// Wraps optional labels, refusing values over [`CLIENT_INFO_MAX_CHARS`] characters.
    pub fn new(device: Option<&str>, client: Option<&str>) -> Result<Self, String> {
        let bounded = |value: Option<&str>| -> Result<Option<String>, String> {
            match value {
                None => Ok(None),
                Some(text) => {
                    if text.chars().count() > CLIENT_INFO_MAX_CHARS {
                        return Err(format!(
                            "the client label is over {CLIENT_INFO_MAX_CHARS} characters"
                        ));
                    }
                    Ok(Some(text.to_owned()))
                }
            }
        };
        Ok(Self {
            device: bounded(device)?,
            client: bounded(client)?,
        })
    }
}

/// The policy a set of sessions runs under (ADR-070 §5.4: idle and absolute timeouts are
/// tenant policy). Defaults are [`SessionPolicy::DEFAULT`] with the citation above.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionPolicy {
    /// A session ends when it has not been used for this long.
    pub idle: Duration,
    /// A session never lives past this long after its creation, however busy.
    pub absolute: Duration,
    /// A token older than this should be rotated (see [`SessionManager`](crate::manager)).
    /// Zero disables rotation. Always below `absolute`: a policy where rotation is due at
    /// or after the absolute end never rotates, which [`SessionPolicy::new`] refuses.
    pub rotate_after: Duration,
    /// The concurrent sessions one user may hold; the oldest is evicted (reason
    /// `evicted`) when a new one would exceed it.
    pub max_per_user: usize,
}

impl SessionPolicy {
    /// The shipped default (cited above): 30 min idle / 12 h absolute / 8 h rotation /
    /// 8 sessions per user (ADR-070 §5.4, NIST SP 800-63B anchor).
    pub const DEFAULT: Self = Self {
        idle: DEFAULT_IDLE,
        absolute: DEFAULT_ABSOLUTE,
        rotate_after: DEFAULT_ROTATE_AFTER,
        max_per_user: DEFAULT_MAX_PER_USER,
    };

    /// Builds a policy, refusing a rotation age at or beyond the absolute lifetime (such a
    /// policy could never rotate) and an idle bound past the absolute one (meaningless).
    pub fn new(
        idle: Duration,
        absolute: Duration,
        rotate_after: Duration,
        max_per_user: usize,
    ) -> Result<Self, String> {
        if rotate_after != Duration::ZERO && rotate_after >= absolute {
            return Err(
                "rotate_after must be below absolute for rotation to ever happen".to_owned(),
            );
        }
        if idle >= absolute {
            return Err(
                "idle must be below absolute, or every session dies of age first".to_owned(),
            );
        }
        if max_per_user == 0 {
            return Err("max_per_user must be at least 1".to_owned());
        }
        Ok(Self {
            idle,
            absolute,
            rotate_after,
            max_per_user,
        })
    }
}

/// The state a session is in. Lifetime transitions are one-way: `Active` → `Revoked`
/// (by the user, an administrator, rotation or eviction) and `Active` → `Expired` (lazy
/// determination at validate/purge time), never back (ADR-070 §6.3: ends are final).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// Signed in and usable.
    Active,
    /// Ended on purpose; revocation is final and immediate.
    Revoked {
        /// Why the session ended (audit text, never secret material).
        reason: SessionEndReason,
    },
}

impl fmt::Display for SessionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionState::Active => f.write_str("active"),
            SessionState::Revoked { reason } => write!(f, "revoked ({reason})"),
        }
    }
}

/// Why a session ended. The reason is audit text and never secret material.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEndReason {
    /// The user ended it (logout) or an administrator ended it for them.
    Ended,
    /// Rotation re-issued the token; the old one died.
    Rotated,
    /// Deactivation or a password change ended every other session.
    PasswordChanged,
    /// The user was deactivated (ADR-070 §6.3: sessions die at once).
    Deactivated,
    /// The store evicted this session because a newer one exceeded the cap.
    Evicted,
    /// Free-form reason text for administrator actions.
    Other(String),
}

impl fmt::Display for SessionEndReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionEndReason::Ended => f.write_str("ended"),
            SessionEndReason::Rotated => f.write_str("rotated"),
            SessionEndReason::PasswordChanged => f.write_str("password_changed"),
            SessionEndReason::Deactivated => f.write_str("deactivated"),
            SessionEndReason::Evicted => f.write_str("evicted"),
            SessionEndReason::Other(text) => f.write_str(text),
        }
    }
}

/// One session: the record a [`SessionStore`](crate::store) keeps and a
/// [`SessionManager`](crate::manager) answers with.
///
/// The token itself is never here — only [`Session::token_digest`]. Every time field is a
/// unix-seconds count; expiry is half-open (`absolute_expires` is already past).
///
/// `Clone`, `PartialEq` and `Eq` are written by hand because [`TokenDigest`] deliberately
/// derives none of them: a digest is secret-class material, so its copy is rebuilt from
/// the 32 bytes the type already exposes to storage layers, and its equality is the
/// constant-time comparison `TokenDigest::matches` — never a derive that would compare
/// bytes with data-dependent early exit.
#[derive(Debug)]
pub struct Session {
    /// The session's id, unique within its store.
    pub id: String,
    /// Who the session belongs to.
    pub user: UserKey,
    /// The stored verifier for the session token; the token text never reaches storage.
    pub token_digest: TokenDigest,
    /// The unix-seconds instant the session started.
    pub created: i64,
    /// The unix-seconds instant of the last observed use (creation counts).
    pub last_seen: i64,
    /// Unix seconds; at or after it the session is expired (half-open bound).
    pub idle_expires: i64,
    /// Unix seconds; at or after it the session is expired whatever the activity.
    pub absolute_expires: i64,
    /// The labelled device/client context.
    pub client: ClientInfo,
    /// The salted digest of the source address.
    pub source: SourceHash,
    /// The current state (active or revoked; expiry is derived, not stored).
    pub state: SessionState,
}

impl Clone for Session {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            user: self.user.clone(),
            // The digest copy: from its 32 exposed bytes, the same path a storage layer
            // takes; the bytes are the at-rest form by design.
            token_digest: match TokenDigest::from_bytes(self.token_digest.as_bytes()) {
                Ok(copy) => copy,
                Err(_) => unreachable!("32 bytes from a TokenDigest always rebuild one"),
            },
            created: self.created,
            last_seen: self.last_seen,
            idle_expires: self.idle_expires,
            absolute_expires: self.absolute_expires,
            client: self.client.clone(),
            source: self.source.clone(),
            state: self.state.clone(),
        }
    }
}

impl PartialEq for Session {
    /// Equality for stores and tests. Digests compare in constant time (`matches`); every
    /// other field is non-secret and compares directly.
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.user == other.user
            && self.token_digest.matches(&other.token_digest)
            && self.created == other.created
            && self.last_seen == other.last_seen
            && self.idle_expires == other.idle_expires
            && self.absolute_expires == other.absolute_expires
            && self.client == other.client
            && self.source == other.source
            && self.state == other.state
    }
}

impl Eq for Session {}

impl Session {
    /// Whether the session is expired at unix-seconds `now`: past the absolute bound, or
    /// idle past `idle_expires`. Half-open: exactly at a bound is expired.
    #[must_use]
    pub fn expired_at(&self, now: i64) -> bool {
        now >= self.absolute_expires || now >= self.idle_expires
    }

    /// Whether the session is revoked (state, not expiry).
    #[must_use]
    pub fn is_revoked(&self) -> bool {
        matches!(self.state, SessionState::Revoked { .. })
    }

    /// Whether the session is alive at `now` (active and not expired).
    #[must_use]
    pub fn alive_at(&self, now: i64) -> bool {
        !self.is_revoked() && !self.expired_at(now)
    }

    /// The token's age at `now`, in seconds (never negative).
    #[must_use]
    pub fn token_age_secs(&self, now: i64) -> i64 {
        (now - self.created).max(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_cited_policy() {
        assert_eq!(SessionPolicy::DEFAULT.idle, Duration::from_secs(30 * 60));
        assert_eq!(
            SessionPolicy::DEFAULT.absolute,
            Duration::from_secs(12 * 60 * 60)
        );
        assert_eq!(
            SessionPolicy::DEFAULT.rotate_after,
            Duration::from_secs(8 * 60 * 60)
        );
        assert_eq!(SessionPolicy::DEFAULT.max_per_user, 8);
    }

    #[test]
    fn policy_validation_refuses_non_rotation_and_cap_of_zero() {
        // Rotation due exactly at the absolute end can never fire.
        assert!(SessionPolicy::new(DEFAULT_IDLE, DEFAULT_ABSOLUTE, DEFAULT_ABSOLUTE, 8).is_err());
        assert!(
            SessionPolicy::new(DEFAULT_ABSOLUTE, DEFAULT_ABSOLUTE, DEFAULT_ROTATE_AFTER, 8)
                .is_err()
        );
        assert!(
            SessionPolicy::new(DEFAULT_IDLE, DEFAULT_ABSOLUTE, DEFAULT_ROTATE_AFTER, 0).is_err()
        );
        assert!(
            SessionPolicy::new(DEFAULT_IDLE, DEFAULT_ABSOLUTE, DEFAULT_ROTATE_AFTER, 1).is_ok()
        );
    }

    #[test]
    fn expiry_is_half_open() {
        let session = Session {
            id: "s1".to_owned(),
            user: match UserKey::new("u") {
                Ok(key) => key,
                Err(reason) => panic!("{reason}"),
            },
            token_digest: crate::tests_support::digest_of("t"),
            created: 100,
            last_seen: 100,
            idle_expires: 250,
            absolute_expires: 300,
            client: ClientInfo::default(),
            source: match SaltKey::from_bytes(&[7; 32]) {
                Ok(salt) => salt.digest_source("10.0.0.1"),
                Err(reason) => panic!("{reason}"),
            },
            state: SessionState::Active,
        };
        // The idle bound first: the session above dies at its idle bound.
        assert!(session.alive_at(249));
        assert!(!session.alive_at(250)); // idle bound: half-open.
        // The absolute bound needs a session whose idle window still covers the instant:
        // busy client, touched every minute, absolute still ends it.
        let busy = Session {
            idle_expires: 999,
            ..session.clone()
        };
        assert!(busy.alive_at(299));
        assert!(!busy.alive_at(300)); // absolute bound: half-open.
        assert_eq!(session.token_age_secs(250), 150);
        assert_eq!(session.token_age_secs(50), 0);
    }

    #[test]
    fn client_info_caps_labels() {
        assert!(ClientInfo::new(Some(&"x".repeat(64)), None).is_ok());
        assert!(ClientInfo::new(Some(&"x".repeat(65)), None).is_err());
    }

    #[test]
    fn source_digest_is_stable_under_one_salt_and_differs_across_salts() {
        let salt = match SaltKey::from_bytes(&[9; 32]) {
            Ok(inner) => inner,
            Err(reason) => panic!("{reason}"),
        };
        let first = salt.digest_source("203.0.113.9");
        let second = salt.digest_source("203.0.113.9");
        assert_eq!(first, second);
        let other_salt = match SaltKey::from_bytes(&[8; 32]) {
            Ok(inner) => inner,
            Err(reason) => panic!("{reason}"),
        };
        assert_ne!(other_salt.digest_source("203.0.113.9"), first);
        // And no plain rendering of the digest can contain the raw address.
        assert!(!format!("{first}").contains("203.0.113"));
    }
}
