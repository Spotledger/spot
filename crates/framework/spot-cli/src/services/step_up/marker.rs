//! The step-up marker on a session, and the events of a step-up.
//!
//! The marker is `session.step_up_at` (an optional datetime of the platform app's `session`
//! DocType). A step-up writes it on the caller's **own** session and nowhere else: the record id
//! comes from the authenticated principal, never from the request, and the row is checked
//! (active, and linked to the same user) under a row lock before it is written. A session that
//! never stepped up has no marker, and a session that was only just created is not stepped up
//! either: the marker, not the session's age, is what "recent" means.
//!
//! # One reader (WP-IAM-12.T9)
//!
//! [`within_window`] is the only place the window logic lives, and [`session_is_current`] is the
//! only place a session's marker is read to answer "is this step-up recent". The identity
//! administration (`IdentityStepUpPort::is_recent`), the single sign-on administration
//! (`SsoAdminPort::step_up_current`) and the support-access caller (`SupportCaller`) all call
//! them, so none of them can decide recency from the age of a session. The window is
//! `[services.identity_admin] step_up_window_secs`; [`set_window`] and [`window_of`] carry it to
//! a reader that is built without the site's settings in hand.
//!
//! # Events
//!
//! A step-up is a sign-in event of the identity event registry with `step_up: true`: success is
//! one `identity_sign_in_succeeded`, written to the outbox **in the same transaction as the
//! marker** (a marker without its event cannot exist, and an event that cannot be written leaves
//! no marker). A refusal is one `identity_sign_in_failed` in its own transaction, with the
//! registry's reason class. Neither carries a password, a code, a token or a digest: the
//! payloads are the fixed shapes below, and the data of a refusal is the user id, the method and
//! a reason word.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Value as Json, json};
use spot_events_contract::payload::{EventMeta, PayloadDocument};
use spot_events_contract::{Actor, Topic};
use spot_http::api::registry::Code;
use spot_http::api::service::ApiError;
use spot_platform_app::doctype;
use spot_postgres::PgBackend;
use spot_storage_contract::{
    Catalog, DocumentStore, Isolation, Name, NodeRef, Outbox, RecordId, RowLock, StorageBackend,
    StorageError, Transaction, TxOptions, Value, Wait,
};
use time::OffsetDateTime;

type Tx = <PgBackend as StorageBackend>::Tx;

/// The event of a successful step-up (a sign-in event with `step_up: true`).
const EVENT_SUCCEEDED: &str = "identity_sign_in_succeeded";
/// The event of a refused step-up.
const EVENT_FAILED: &str = "identity_sign_in_failed";

/// How far ahead of this node's clock a marker written by another node may be and still count.
const STEP_UP_SKEW_SECS: i64 = 5;

/// The window a step-up lasts when no `[services.identity_admin] step_up_window_secs` is set, in
/// seconds. The same default the identity administration setting has.
pub const DEFAULT_WINDOW_SECS: i64 = 900;

/// The window each site registered, by site name.
fn windows() -> &'static Mutex<BTreeMap<String, i64>> {
    static WINDOWS: OnceLock<Mutex<BTreeMap<String, i64>>> = OnceLock::new();
    WINDOWS.get_or_init(Mutex::default)
}

/// Records the step-up window of `site` (the `step_up_window_secs` setting, already validated),
/// for readers that are built without the site's settings. Registered again on a reload.
pub fn set_window(site: &str, secs: i64) {
    windows()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(site.to_owned(), secs);
}

/// The step-up window of `site` in seconds: the registered one, else [`DEFAULT_WINDOW_SECS`].
#[must_use]
pub fn window_of(site: &str) -> i64 {
    windows()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(site)
        .copied()
        .unwrap_or(DEFAULT_WINDOW_SECS)
}

/// Whether a marker written at `marked` is within `window_secs` of `now` (the injected clock). A
/// session that never stepped up (`None`) is not recent, whatever its age. A marker a few seconds
/// ahead of this node's clock (another node wrote it) still counts; one far in the future (a
/// clock set back) does not.
#[must_use]
pub fn within_window(
    marked: Option<OffsetDateTime>,
    now: OffsetDateTime,
    window_secs: i64,
) -> bool {
    marked.is_some_and(|marked| {
        let age = (now - marked).whole_seconds();
        (-STEP_UP_SKEW_SECS..=window_secs).contains(&age)
    })
}

/// Whether the marker of `session` is within `window_secs` of `now`: the answer every operation
/// that needs a recent step-up reads. The session must exist and be `active`; its age is never
/// looked at.
///
/// # Errors
///
/// `503 server.unavailable` when the record cannot be read: an error is a refusal, never an
/// allow.
pub async fn session_is_current(
    backend: &PgBackend,
    catalog: &Arc<Catalog>,
    session: RecordId,
    now: OffsetDateTime,
    window_secs: i64,
) -> Result<bool, ApiError> {
    let marker = Marker::new(backend.clone(), Arc::clone(catalog));
    let facts = marker.facts(session).await?;
    Ok(facts
        .filter(|facts| facts.active)
        .is_some_and(|facts| within_window(facts.step_up_at, now, window_secs)))
}

/// What the session record says about itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SessionFacts {
    /// The user the session belongs to.
    pub user: Option<RecordId>,
    /// How it was established (`password`, `oidc`, `saml`, ...).
    pub method: Option<String>,
    /// Whether the session is still `active`.
    pub active: bool,
    /// The step-up marker (`step_up_at`), when the session has stepped up.
    pub step_up_at: Option<OffsetDateTime>,
    /// The salted digest of the client's address, as the session stored it (base16, never the
    /// address). It is what a step-up audit record names as its source.
    pub source: Option<String>,
}

impl SessionFacts {
    /// Whether a single sign-on provider established the session.
    pub(super) fn is_sso(&self) -> bool {
        matches!(self.method.as_deref(), Some("oidc" | "saml"))
    }
}

fn unavailable() -> ApiError {
    ApiError::from_code(Code::ServerUnavailable).detail("retry_after", Json::from(5))
}

/// A storage failure as the answer. The text is dropped: it can name a value.
fn failed(_error: &StorageError) -> ApiError {
    unavailable()
}

fn name(text: &str) -> Result<Name, ApiError> {
    Name::new(text).map_err(|_| unavailable())
}

/// The session marker store of one site.
pub(super) struct Marker {
    backend: PgBackend,
    catalog: Arc<Catalog>,
}

impl Marker {
    pub(super) fn new(backend: PgBackend, catalog: Arc<Catalog>) -> Self {
        Self { backend, catalog }
    }

    async fn begin(&self, label: &str, read_only: bool) -> Result<Tx, ApiError> {
        let options = TxOptions::new(name(label)?, Isolation::ReadCommitted);
        let options = if read_only {
            options.read_only()
        } else {
            options
        };
        self.backend
            .begin(Arc::clone(&self.catalog), options)
            .await
            .map_err(|e| failed(&e))
    }

    /// What the record of `session` says about it, or `None` when there is no such record.
    pub(super) async fn facts(&self, session: RecordId) -> Result<Option<SessionFacts>, ApiError> {
        let mut tx = self.begin("step_up_read", true).await?;
        let loaded = tx
            .load(&name(doctype::SESSION)?, session, RowLock::None)
            .await;
        let _ = tx.rollback().await;
        let Some(document) = loaded.map_err(|e| failed(&e))? else {
            return Ok(None);
        };
        let fields = &document.data.fields;
        let user = match fields.get(&name("user")?) {
            Some(Value::Id(id)) => Some(*id),
            _ => None,
        };
        let text = |column: &str| -> Result<Option<String>, ApiError> {
            Ok(match fields.get(&name(column)?) {
                Some(Value::Text(text)) => Some(text.clone()),
                _ => None,
            })
        };
        Ok(Some(SessionFacts {
            user,
            method: text("method")?,
            active: text("state")?.as_deref() == Some("active"),
            step_up_at: match fields.get(&name("step_up_at")?) {
                Some(Value::DateTime(at)) => Some(*at),
                _ => None,
            },
            source: text("source_digest")?,
        }))
    }

    /// Writes `step_up_at = now` on `session` and queues the success event, in one transaction.
    /// Nothing is written when the session is gone, ended, or not `user`'s.
    pub(super) async fn mark(
        &self,
        session: RecordId,
        user: RecordId,
        now: OffsetDateTime,
        method: &str,
    ) -> Result<(), ApiError> {
        let mut tx = self.begin("step_up_mark", false).await?;
        let result = Self::write(&mut tx, session, user, now, method).await;
        match result {
            Ok(()) => tx.commit().await.map_err(|e| failed(&e)),
            Err(error) => {
                let _ = tx.rollback().await;
                Err(error)
            }
        }
    }

    async fn write(
        tx: &mut Tx,
        session: RecordId,
        user: RecordId,
        now: OffsetDateTime,
        method: &str,
    ) -> Result<(), ApiError> {
        let entity = name(doctype::SESSION)?;
        let loaded = tx
            .load(&entity, session, RowLock::Update(Wait::Block))
            .await
            .map_err(|e| failed(&e))?;
        let Some(mut loaded) = loaded else {
            return Err(ApiError::from_code(Code::AuthSessionExpired));
        };
        let owner = match loaded.data.fields.get(&name("user")?) {
            Some(Value::Id(id)) => Some(*id),
            _ => None,
        };
        let active = matches!(
            loaded.data.fields.get(&name("state")?),
            Some(Value::Text(state)) if state == "active"
        );
        if !active {
            return Err(ApiError::from_code(Code::AuthSessionExpired));
        }
        // The marker goes on the caller's own session only.
        if owner != Some(user) {
            return Err(ApiError::from_code(Code::PermissionDenied));
        }
        loaded
            .data
            .fields
            .insert(name("step_up_at")?, Value::DateTime(now));
        tx.update(&loaded.data, loaded.version)
            .await
            .map_err(|e| failed(&e))?;
        let data = json!({
            "user": user.to_string(),
            "method": method,
            "session": session.to_string(),
            "step_up": true,
        });
        Self::enqueue(tx, EVENT_SUCCEEDED, user, now, data).await
    }

    /// Queues the refusal event in its own transaction. Best effort by design: the refusal is
    /// the answer whether or not the event could be written, so a failure here is logged (a
    /// fixed word, no value) and not returned.
    pub(super) async fn refused(
        &self,
        user: RecordId,
        now: OffsetDateTime,
        method: &str,
        reason: &str,
    ) {
        let written = async {
            let mut tx = self.begin("step_up_refused", false).await?;
            let data = json!({
                "user": user.to_string(),
                "method": method,
                "reason": reason,
            });
            match Self::enqueue(&mut tx, EVENT_FAILED, user, now, data).await {
                Ok(()) => tx.commit().await.map_err(|e| failed(&e)),
                Err(error) => {
                    let _ = tx.rollback().await;
                    Err(error)
                }
            }
        }
        .await;
        if written.is_err() {
            tracing::warn!(
                code = super::CODE_AUDIT_FAILED,
                "a refused step-up could not be written to the event outbox"
            );
        }
    }

    async fn enqueue(
        tx: &mut Tx,
        topic: &str,
        user: RecordId,
        now: OffsetDateTime,
        data: Json,
    ) -> Result<(), ApiError> {
        let id = RecordId::new_v7();
        let meta = EventMeta {
            occurred_at: now,
            actor: Actor::User {
                id: user,
                via: None,
            },
            command_id: None,
            causation_id: None,
            correlation_id: id,
            trace_id: None,
        };
        let topic = Topic::any(topic).map_err(|_| unavailable())?;
        let subject = NodeRef {
            entity: name(doctype::USER)?,
            id: user,
        };
        let event = PayloadDocument { meta, data }
            .to_outbox_event(id, &topic, subject, None, 1)
            .map_err(|_| unavailable())?;
        tx.enqueue(&event).await.map_err(|e| failed(&e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(method: Option<&str>) -> SessionFacts {
        SessionFacts {
            user: None,
            method: method.map(str::to_owned),
            active: true,
            step_up_at: None,
            source: None,
        }
    }

    #[test]
    fn only_a_provider_method_is_a_single_sign_on_session() {
        assert!(facts(Some("oidc")).is_sso());
        assert!(facts(Some("saml")).is_sso());
        for method in [Some("password"), Some("totp"), Some("api_key"), None] {
            assert!(!facts(method).is_sso(), "{method:?}");
        }
    }

    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-11-21 09:00:00 UTC);

    #[test]
    fn a_session_that_never_stepped_up_is_not_within_any_window() {
        assert!(!within_window(None, NOW, 900));
        assert!(!within_window(None, NOW, 86_400));
    }

    #[test]
    fn the_window_is_inclusive_at_its_end_and_closed_just_after() {
        let marked = NOW - time::Duration::seconds(60);
        assert!(within_window(Some(marked), NOW, 60));
        assert!(!within_window(Some(marked), NOW, 59));
        assert!(within_window(Some(NOW), NOW, 30));
        // A marker from before the window is not recent.
        let old = NOW - time::Duration::seconds(901);
        assert!(!within_window(Some(old), NOW, 900));
    }

    #[test]
    fn a_marker_a_few_seconds_ahead_counts_and_one_far_ahead_does_not() {
        assert!(within_window(
            Some(NOW + time::Duration::seconds(STEP_UP_SKEW_SECS)),
            NOW,
            900
        ));
        assert!(!within_window(
            Some(NOW + time::Duration::seconds(STEP_UP_SKEW_SECS + 1)),
            NOW,
            900
        ));
        assert!(!within_window(
            Some(NOW + time::Duration::seconds(3600)),
            NOW,
            900
        ));
    }

    #[test]
    fn the_window_of_a_site_is_the_registered_one_else_the_default() {
        assert_eq!(window_of("marker-test-unregistered"), DEFAULT_WINDOW_SECS);
        set_window("marker-test-registered", 60);
        assert_eq!(window_of("marker-test-registered"), 60);
        // Another site is not affected.
        assert_eq!(window_of("marker-test-other"), DEFAULT_WINDOW_SECS);
        set_window("marker-test-registered", 120);
        assert_eq!(window_of("marker-test-registered"), 120);
    }

    #[test]
    fn the_topics_are_registry_topics() {
        assert!(Topic::any(EVENT_SUCCEEDED).is_ok());
        assert!(Topic::any(EVENT_FAILED).is_ok());
    }
}
