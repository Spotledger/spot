//! The [`SessionManager`]: every decision about sessions in one place, over any store.
//!
//! `validate` follows the fixed order the brief sets: digest lookup, state, idle and
//! absolute checks, then the side effects (touch; rotation when due). The caller-visible
//! error is always [`SessionError::Invalid`] for a dead session — one `session.invalid`
//! code with the internal reason for the audit log, never a three-way distinction that
//! would let a client probe which tokens exist (`auth.invalid_credentials` /
//! `auth.session_expired` in the API mapping, ADR-074's problem-details registry).
//!
//! `start` enforces [`SessionPolicy::max_per_user`]: the oldest active session is evicted
//! (reason `evicted`) when a new session would exceed the cap (ADR-070 §5.4: a user can
//! list and end sessions; the platform bounds how many exist).
use std::sync::Arc;

use spot_auth::{OpaqueToken, TokenDigest};

use crate::error::{InternalReason, SessionEndReasonText, SessionError, invalid};
use crate::model::{Session, SessionEndReason, SessionPolicy, SessionState, SourceHash, UserKey};
use crate::store::{SessionRecord, SessionStore};

/// What `end` or `end_others` did, for the caller's response and audit note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndReport {
    /// How many live sessions were ended (0 when the id was already gone or revoked).
    pub ended: usize,
}

/// The result of a successful [`SessionManager::start`]: the token the caller hands to the
/// user exactly once, and the record now in the store (digest only).
pub struct Started {
    /// The bearer token: shown once, never stored.
    pub token: OpaqueToken,
    /// The stored record.
    pub session: SessionRecord,
}

/// A rotated token: the new token goes to the caller of `validate` (the HTTP layer
/// replaces the session token in its response when this is `Some`).
pub struct Validated {
    /// The live session.
    pub session: SessionRecord,
    /// Set when rotation fell due during this validation: the new token, whose digest is
    /// already in the store and whose old token is dead. The caller must deliver it.
    pub renewed_token: Option<OpaqueToken>,
}

/// Manages sessions for one site over one store.
///
/// `S` is any [`SessionStore`]; hold it as `Arc<dyn SessionStore>` to swap the in-memory
/// fake for the storage-backed backend (T3) without a type change.
pub struct SessionManager<S: SessionStore> {
    store: S,
    policy: SessionPolicy,
    /// The site's source-address salt: every `start` digests the source under it
    /// ([`crate::model::SaltKey`]), so raw addresses never reach the store.
    salt: Arc<crate::model::SaltKey>,
}

impl<S: SessionStore> SessionManager<S> {
    /// Builds a manager over `store` with `policy` and the site's `salt`.
    pub fn new(store: S, policy: SessionPolicy, salt: Arc<crate::model::SaltKey>) -> Self {
        Self {
            store,
            policy,
            salt,
        }
    }

    /// The store (for callers that also administer it directly, e.g. purge jobs).
    pub fn store(&self) -> &S {
        &self.store
    }

    /// The policy in force.
    pub fn policy(&self) -> &SessionPolicy {
        &self.policy
    }

    /// Signs a user in: creates the token, enforces `max_per_user` (evicting the oldest
    /// active session with reason `evicted` when needed), stores the digest-only record.
    ///
    /// `source` is the **raw** source address string; it is salted-digested here and never
    /// stored or logged. `now` and the deadlines derive from it (unix seconds).
    pub async fn start(
        &self,
        user: UserKey,
        source: &str,
        client: crate::model::ClientInfo,
        now: i64,
    ) -> Result<Started, SessionError> {
        // The cap, before the create: one more than the policy allows ⇒ evict the oldest.
        let count = self
            .store
            .list_for_user(&user)
            .await?
            .into_iter()
            .filter(|record| record.alive_at(now))
            .count();
        if count >= self.policy.max_per_user
            && let Some(oldest) = self.store.oldest_active(&user, now).await?
        {
            self.store
                .revoke(&oldest.id, SessionEndReason::Evicted.to_string())
                .await?;
        }

        let token = OpaqueToken::generate();
        let digest = token.digest();
        if self.store.exists(&digest).await? {
            // One in 2^256: a bug or a broken CSPRNG, either way refuse loudly.
            return Err(SessionError::StoreFailed(
                "the fresh token's digest already exists; refusing to reuse it".to_owned(),
            ));
        }
        let record = Session {
            id: String::new(), // The store assigns the id.
            user,
            token_digest: digest,
            created: now,
            last_seen: now,
            idle_expires: now + duration_secs(self.policy.idle),
            absolute_expires: now + duration_secs(self.policy.absolute),
            client,
            source: self.salt.digest_source(source),
            state: SessionState::Active,
        };
        let stored = self.store.create(record).await?;
        Ok(Started {
            token,
            session: stored,
        })
    }

    /// Validates a presented token at `now`: digest lookup, state, idle and absolute
    /// checks; on success the touch and (when due) rotation side effects run.
    ///
    /// A dead session (unknown digest, revoked, idle-expired, absolutely-expired) is one
    /// error: [`SessionError::Invalid`], whose internal reason logs the real cause.
    /// A malformed token text is refused before any digest is computed (and never echoes
    /// the input).
    pub async fn validate(&self, token: &str, now: i64) -> Result<Validated, SessionError> {
        let presented = OpaqueToken::parse(token).map_err(|_| SessionError::Malformed)?;
        let digest = presented.digest();
        let Some(record) = self.store.find_by_digest(&digest).await? else {
            return Err(invalid(InternalReason::UnknownDigest));
        };
        if let SessionState::Revoked { reason } = &record.state {
            return Err(invalid(InternalReason::Revoked(SessionEndReasonText(
                reason.to_string(),
            ))));
        }
        if record.absolute_expires <= now {
            return Err(invalid(InternalReason::AbsoluteExpired));
        }
        if record.idle_expires <= now {
            return Err(invalid(InternalReason::IdleExpired));
        }

        // Alive: touch (side effect 1). The store skips writes under the granularity and
        // clamps to the absolute bound itself.
        self.store
            .touch(
                &record.id,
                now,
                duration_secs(self.policy.idle),
                record.absolute_expires,
            )
            .await?;

        // Rotation due (side effect 2): the token is older than `rotate_after`. Rotation
        // issues a fresh token whose digest swaps into the same record in place (same
        // id, window, identity) — the old token dies at once, the session lives on.
        let due = self.policy.rotate_after != std::time::Duration::ZERO
            && record.token_age_secs(now) >= duration_secs(self.policy.rotate_after);
        let renewed_token = if due {
            let fresh = OpaqueToken::generate();
            let fresh_digest = fresh.digest();
            // The store takes its own copy of the digest bytes (TokenDigest is not Clone:
            // it is secret-class material, so rebuild one from the 32 bytes).
            let for_store = match TokenDigest::from_bytes(fresh_digest.as_bytes()) {
                Ok(copy) => copy,
                Err(_) => {
                    return Err(SessionError::StoreFailed(
                        "the fresh digest could not be copied for the store".to_owned(),
                    ));
                }
            };
            self.store.rotate(&record.id, for_store).await?;
            Some(fresh)
        } else {
            None
        };

        // The post-touch (and post-rotation) record: re-read so the caller sees the
        // extended window and the swapped digest. The digest to look up is the *fresh*
        // one after rotation, the presented one otherwise.
        let lookup = match &renewed_token {
            Some(fresh) => fresh.digest(),
            None => digest,
        };
        let refreshed = self
            .store
            .find_by_digest(&lookup)
            .await?
            .ok_or_else(|| invalid(InternalReason::UnknownDigest))?;
        Ok(Validated {
            session: refreshed,
            renewed_token,
        })
    }

    /// Ends one session by id (logout). Idempotent: an unknown id ends 0, an already
    /// revoked session keeps its first reason and adds nothing to the count.
    pub async fn end(&self, id: &str, now: i64) -> Result<EndReport, SessionError> {
        let _ = now; // The store reports newly-revoked rows; an already-dead row counts 0,
        // which is the exact audit answer for "how many live sessions ended".
        let report = self
            .store
            .revoke(id, SessionEndReason::Ended.to_string())
            .await?;
        Ok(EndReport {
            ended: report.revoked,
        })
    }

    /// Ends every live session of `user` except `keep` ("log out everywhere else",
    /// ADR-070 §5.4, and the password-change rule §5.5 goes through
    /// [`SessionManager::revoke_all_for_user`] with reason `password_changed`).
    pub async fn end_others(
        &self,
        user: &UserKey,
        keep: &str,
        now: i64,
    ) -> Result<EndReport, SessionError> {
        // Revoke only rows that are live at `now`: a row already expired or revoked is
        // dead to every reader, and the audit count must mean *live* sessions ended.
        let listed = self.store.list_for_user(user).await?;
        let mut ended = 0;
        for record in listed {
            if record.id == keep || !record.alive_at(now) {
                continue;
            }
            let report = self
                .store
                .revoke(&record.id, SessionEndReason::Ended.to_string())
                .await?;
            ended += report.revoked;
        }
        Ok(EndReport { ended })
    }

    /// Revokes everything of `user`, optionally sparing one session, with the given end
    /// reason — the deactivation (§6.3) and password-change (§5.5) path.
    pub async fn revoke_all_for_user(
        &self,
        user: &UserKey,
        except: Option<&str>,
        reason: SessionEndReason,
    ) -> Result<EndReport, SessionError> {
        let report = self
            .store
            .revoke_all_for_user(user, except, reason.to_string())
            .await?;
        Ok(EndReport {
            ended: report.revoked,
        })
    }

    /// Lists a user's sessions, oldest first, with derived liveness at `now`
    /// (ADR-070 §5.4: a user lists their sessions; the API layer shows derived status).
    pub async fn list(&self, user: &UserKey, now: i64) -> Result<Vec<ListedSession>, SessionError> {
        let listed = self.store.list_for_user(user).await?;
        Ok(listed
            .into_iter()
            .map(|record| {
                let live = record.alive_at(now) && record.state == SessionState::Active;
                let source = record.source.clone();
                ListedSession {
                    record,
                    live,
                    source,
                }
            })
            .collect())
    }

    /// The policy's absolute lifetime in seconds (the store clamps touches against it).
    pub fn absolute_seconds(&self) -> i64 {
        duration_secs(self.policy.absolute)
    }
}

/// One row of [`SessionManager::list`]: the stored record plus the derived live flag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedSession {
    /// The stored record.
    pub record: SessionRecord,
    /// Whether the session is usable at the caller's `now` (active, not expired).
    pub live: bool,
    /// The salted source digest, for the user's "signed in from" display.
    pub source: SourceHash,
}

/// `Duration` → i64 seconds (policy values are realistic; the conversion saturates rather
/// than wrapping).
fn duration_secs(value: std::time::Duration) -> i64 {
    i64::try_from(value.as_secs()).unwrap_or(i64::MAX)
}
