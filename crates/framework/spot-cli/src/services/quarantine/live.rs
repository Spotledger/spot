//! The live pieces the quarantine module assembles on a site (WP-AI-07.T8): the platform audit
//! sink for quarantine decisions ([`PlatformAudit`]), the principal source over the site's
//! role assignments ([`SitePrincipals`]), the assembly of the backend ([`backend_over`]) and the
//! per-site record of what was provided, so a configuration reload provides the same backend
//! again instead of building a second one.
//!
//! # The audit sink
//!
//! [`PlatformAudit`] appends each decision (retry, release, discard) to the site's tamper-evident
//! audit chain (`spot-audit-chain` over `spot-audit-store`, the chain the other platform services
//! append to) under the scope [`SCOPE_PREFIX`] and the site name. An entry names the actor, the
//! item id, the record's apply key and the reason code. It never carries the held image or a
//! field value. A failed append is an error to the backend, which then changes nothing (the
//! decision is audited before the state moves, see [`super::backend`]).
//!
//! # The principal source
//!
//! The permission engine needs a [`Principal`] (a user and the assignments it holds). The site's
//! own source ([`spot_document::engine::PrincipalSource`], the storage-backed one) is asynchronous;
//! the quarantine seam is synchronous, so the lookup goes through `block_in_place` on a
//! multi-thread runtime (every `spot serve` process has one). The actor is a user record id, in
//! one of three spellings: `<id>`, `user:<id>`, or `agent:<id>` (the remedy host's actor; an
//! agent is registered under the id of the user record it signs in as, so it acts with its own
//! role set, never the role set of its owner). Anything else, an id the site holds no active
//! assignment for, and a runtime that cannot block are all **unknown** (`None`), and every check
//! then refuses.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

use spot_audit_chain::{
    AuditChain, AuditError as ChainError, AuditRecord, CheckpointPolicy, CheckpointSigner,
    PayloadValue, Subject,
};
use spot_audit_store::PgAuditStore;
use spot_core::Clock;
use spot_document::engine::PrincipalSource as SitePrincipalSource;
use spot_permissions::{PermissionEngine, Principal};
use spot_quarantine::audit::AuditFuture;
use spot_quarantine::{AuditEntry, QuarantineAudit, QuarantineError, QuarantineStore};

use super::QuarantineBackend;
use super::backend::StoreBackend;
use super::guards::PrincipalSource;

/// The fixed prefix of the quarantine chain's scope; the site name follows.
pub const SCOPE_PREFIX: &str = "quarantine:";

/// The longest reason code kept in an audit entry (characters).
const MAX_REASON_CHARS: usize = 200;

/// What the audit sink answers when the chain did not take the entry. A fixed sentence: a store
/// message can quote a value.
const AUDIT_REFUSED: &str = "the audit trail did not take the entry";

/// The chain scope of `site`'s quarantine decisions.
#[must_use]
pub fn scope_of(site: &str) -> String {
    format!("{SCOPE_PREFIX}{site}")
}

/// A signer that never signs: the checkpoint policy of this chain is "never automatically", and
/// the key arrives with the platform's audit key work (the identity chain does the same).
struct NoSigner;

impl CheckpointSigner for NoSigner {
    fn key_id(&self) -> String {
        String::new()
    }

    fn sign(&self, _message: &[u8]) -> Result<Vec<u8>, ChainError> {
        Err(ChainError::Signer("no checkpoint key is configured".into()))
    }
}

/// The actor as the audit chain spells it: `system`, `user:<id>` or `agent:<id>`.
#[must_use]
pub fn chain_actor(actor: &str) -> String {
    if actor.is_empty() || actor == "system" {
        return "system".to_owned();
    }
    if actor.starts_with("user:") || actor.starts_with("agent:") {
        return actor.to_owned();
    }
    format!("user:{actor}")
}

/// The chain record of one decision. Names the actor, the item, the apply key and the reason
/// code; never the held image.
#[must_use]
pub fn audit_record(entry: &AuditEntry) -> AuditRecord {
    let mut payload: BTreeMap<String, PayloadValue> = BTreeMap::new();
    payload.insert(
        "item".to_owned(),
        PayloadValue::Int(i64::try_from(entry.item.0).unwrap_or(i64::MAX)),
    );
    payload.insert(
        "name".to_owned(),
        PayloadValue::Text(entry.record_ref.name.as_str().to_owned()),
    );
    payload.insert(
        "version".to_owned(),
        PayloadValue::Int(i64::try_from(entry.record_ref.version).unwrap_or(i64::MAX)),
    );
    if !entry.reason.is_empty() {
        let reason: String = entry.reason.chars().take(MAX_REASON_CHARS).collect();
        payload.insert("reason".to_owned(), PayloadValue::Text(reason));
    }
    AuditRecord {
        kind: format!("quarantine_{}", entry.action.as_str()),
        actor: chain_actor(&entry.actor),
        on_behalf_of: None,
        subject: Some(Subject {
            doctype: entry.record_ref.entity.clone(),
            id: entry.record_ref.id.clone(),
        }),
        at: entry.at,
        payload,
        masked: true,
    }
}

/// The platform's audit sink for quarantine decisions: one chain per site.
pub struct PlatformAudit {
    chain: AuditChain<PgAuditStore>,
}

impl PlatformAudit {
    /// A sink for `site` over the site's audit store.
    #[must_use]
    pub fn new(store: PgAuditStore, site: &str) -> Self {
        Self {
            chain: AuditChain::new(
                store,
                scope_of(site),
                Arc::new(NoSigner),
                CheckpointPolicy { every: 0 },
            ),
        }
    }

    /// The chain scope this sink writes.
    #[must_use]
    pub fn scope(&self) -> &str {
        self.chain.site()
    }

    /// The chain, for a verifier or a test.
    #[must_use]
    pub fn chain(&self) -> &AuditChain<PgAuditStore> {
        &self.chain
    }
}

impl QuarantineAudit for PlatformAudit {
    fn record<'a>(&'a self, entry: &'a AuditEntry) -> AuditFuture<'a> {
        Box::pin(async move {
            let record = audit_record(entry);
            match self.chain.append(record).await {
                Ok(_) => Ok(()),
                Err(error) => {
                    // The cause is a store or validation error: the log keeps its class only.
                    tracing::warn!(
                        scope = self.chain.site(),
                        class = audit_error_class(&error),
                        "quarantine: a decision could not be appended to the audit trail"
                    );
                    Err(QuarantineError::Audit {
                        why: AUDIT_REFUSED.to_owned(),
                    })
                }
            }
        })
    }
}

fn audit_error_class(error: &ChainError) -> &'static str {
    match error {
        ChainError::PayloadTooLarge { .. } => "payload_too_large",
        ChainError::FloatRefused => "float_refused",
        ChainError::RecordInvalid(_) => "record_invalid",
        ChainError::HeadConflict => "head_conflict",
        ChainError::Store(_) => "store",
        ChainError::Signer(_) => "signer",
        _ => "other",
    }
}

/// The user record an actor names: `<id>`, `user:<id>` or `agent:<id>`, where `<id>` is a record
/// id (hyphenated or compact UUID). `None` for anything else (including `system`).
#[must_use]
pub fn user_of(actor: &str) -> Option<spot_app_api::prelude::RecordId> {
    let id = actor
        .strip_prefix("user:")
        .or_else(|| actor.strip_prefix("agent:"))
        .unwrap_or(actor);
    spot_bpm_documents::ids::parse_record_id(id)
}

/// The principal source of the quarantine module over the site's own source.
pub struct SitePrincipals {
    source: Arc<dyn SitePrincipalSource>,
}

impl SitePrincipals {
    /// A source reading assignments through `source`.
    #[must_use]
    pub fn new(source: Arc<dyn SitePrincipalSource>) -> Self {
        Self { source }
    }
}

impl PrincipalSource for SitePrincipals {
    fn principal(&self, actor: &str) -> Option<Principal> {
        let user = user_of(actor)?;
        let handle = tokio::runtime::Handle::try_current().ok()?;
        if handle.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
            // `block_in_place` panics on a current-thread runtime. Fail closed instead.
            tracing::warn!("quarantine: no principal could be resolved on this runtime");
            return None;
        }
        let source = Arc::clone(&self.source);
        let principal = tokio::task::block_in_place(move || {
            handle.block_on(async move { source.resolve(&user).await })
        });
        // The site's source answers a principal with no assignments for an id it does not know
        // (so a check never reveals whether the user exists). Here that is "unknown".
        if principal.is_system || principal.assignments.is_empty() {
            return None;
        }
        Some(principal)
    }
}

/// The backend over its parts, for `tenant`.
#[must_use]
pub fn backend_over(
    tenant: &str,
    store: Arc<dyn QuarantineStore>,
    audit: Arc<dyn QuarantineAudit>,
    engine: Arc<PermissionEngine>,
    principals: Arc<dyn PrincipalSource>,
    clock: Arc<dyn Clock>,
) -> Arc<dyn QuarantineBackend> {
    Arc::new(StoreBackend::new(
        tenant, store, audit, engine, principals, clock,
    ))
}

type Provided = Arc<dyn QuarantineBackend>;

fn provided_map() -> &'static Mutex<HashMap<String, Provided>> {
    static PROVIDED: OnceLock<Mutex<HashMap<String, Provided>>> = OnceLock::new();
    PROVIDED.get_or_init(Mutex::default)
}

/// Records the backend a site provides, so a reload provides the same one.
pub fn remember(site: &str, backend: Arc<dyn QuarantineBackend>) {
    provided_map()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(site.to_owned(), backend);
}

/// Forgets what `site` provided, the backend and the store (a build that provides nothing must
/// not leave an old one).
pub fn forget(site: &str) {
    provided_map()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(site);
    stores_map()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(site);
}

type ProvidedStore = Arc<dyn QuarantineStore>;

fn stores_map() -> &'static Mutex<HashMap<String, ProvidedStore>> {
    static STORES: OnceLock<Mutex<HashMap<String, ProvidedStore>>> = OnceLock::new();
    STORES.get_or_init(Mutex::default)
}

/// Records the store a site provided as the `QuarantineStore` port (WP-AI-07.T17), so a reload
/// provides the same instance the backend was built over.
pub fn remember_store(site: &str, store: Arc<dyn QuarantineStore>) {
    stores_map()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(site.to_owned(), store);
}

/// The store `site` provided at its first build, if any.
#[must_use]
pub fn recall_store(site: &str) -> Option<Arc<dyn QuarantineStore>> {
    stores_map()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(site)
        .cloned()
}

/// The backend `site` provided at its first build, if any.
#[must_use]
pub fn recall(site: &str) -> Option<Arc<dyn QuarantineBackend>> {
    provided_map()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(site)
        .cloned()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use spot_http::api::service::BoxFuture;
    use spot_quarantine::{AuditAction, ItemId, RecordRef};
    use spot_replica_apply::ReceivedRecord;
    use spot_sync_protocol::{RecordName, SourceId};

    fn record_ref() -> RecordRef {
        let id = spot_storage_contract::RecordId::new_v7();
        let payload = serde_json::json!({
            "meta_version": 1,
            "document": {
                "entity": "note",
                "id": id.to_string(),
                "fields": { "title": { "t": "text", "v": "secret title" } }
            }
        })
        .to_string();
        let record = ReceivedRecord::from_payload(
            &SourceId::new("edge-1#b1").unwrap(),
            RecordName::new(&format!("note:{id}")).unwrap(),
            1,
            1,
            &payload,
        )
        .unwrap();
        RecordRef::of(&record)
    }

    fn entry(action: AuditAction, actor: &str, reason: &str) -> AuditEntry {
        AuditEntry {
            action,
            item: ItemId(7),
            record_ref: record_ref(),
            source: SourceId::new("edge-1#b1").unwrap(),
            actor: actor.to_owned(),
            reason: reason.to_owned(),
            at: 1_790_000_000_000_000,
        }
    }

    #[test]
    fn the_scope_names_the_site() {
        assert_eq!(scope_of("acme"), "quarantine:acme");
    }

    #[test]
    fn actors_are_spelled_the_way_the_chain_accepts() {
        assert_eq!(chain_actor(""), "system");
        assert_eq!(chain_actor("system"), "system");
        assert_eq!(chain_actor("user:u1"), "user:u1");
        assert_eq!(chain_actor("agent:a1"), "agent:a1");
        assert_eq!(chain_actor("u1"), "user:u1");
    }

    #[test]
    fn a_record_names_the_decision_and_never_the_payload() {
        let record = audit_record(&entry(AuditAction::Discarded, "agent:a1", "duplicate"));
        assert_eq!(record.kind, "quarantine_discarded");
        assert_eq!(record.actor, "agent:a1");
        assert!(record.masked);
        assert!(record.validate().is_ok());
        assert_eq!(
            record.payload.get("item"),
            Some(&PayloadValue::Int(7)),
            "the item id"
        );
        assert_eq!(
            record.payload.get("reason"),
            Some(&PayloadValue::Text("duplicate".to_owned()))
        );
        let text = format!("{record:?}");
        assert!(!text.contains("secret title"), "{text}");
    }

    #[test]
    fn a_decision_without_a_reason_has_no_reason_entry_and_a_long_one_is_cut() {
        let none = audit_record(&entry(AuditAction::Retried, "u1", ""));
        assert!(!none.payload.contains_key("reason"));
        assert_eq!(none.actor, "user:u1");
        let long = "x".repeat(1_000);
        let cut = audit_record(&entry(AuditAction::Retried, "u1", &long));
        match cut.payload.get("reason") {
            Some(PayloadValue::Text(text)) => assert_eq!(text.chars().count(), MAX_REASON_CHARS),
            other => panic!("unexpected reason entry: {other:?}"),
        }
        assert!(cut.validate().is_ok());
    }

    #[test]
    fn an_actor_names_a_user_record_in_three_spellings_or_nothing() {
        let id = spot_storage_contract::RecordId::new_v7();
        let text = id.to_string();
        assert_eq!(user_of(&text), Some(id));
        assert_eq!(user_of(&format!("user:{text}")), Some(id));
        assert_eq!(user_of(&format!("agent:{text}")), Some(id));
        assert_eq!(user_of("system"), None);
        assert_eq!(user_of("agent:not-an-id"), None);
        assert_eq!(user_of(""), None);
    }

    struct Fixed(Principal);

    impl SitePrincipalSource for Fixed {
        fn resolve<'a>(
            &'a self,
            _user: &'a spot_app_api::prelude::RecordId,
        ) -> BoxFuture<'a, Principal> {
            Box::pin(async move { self.0.clone() })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_user_with_an_assignment_resolves_and_one_without_is_unknown() {
        let id = spot_storage_contract::RecordId::new_v7().to_string();
        let with = SitePrincipals::new(Arc::new(Fixed(Principal::with_roles("u", &["Reviewer"]))));
        let without = SitePrincipals::new(Arc::new(Fixed(Principal::user("u", vec![]))));
        let (a, b, c) = (
            with.principal(&format!("agent:{id}")),
            without.principal(&id),
            with.principal("nobody"),
        );
        assert!(a.is_some());
        assert!(b.is_none(), "no assignment is unknown");
        assert!(c.is_none(), "an actor that is not a record id is unknown");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_runtime_that_cannot_block_resolves_nobody() {
        let id = spot_storage_contract::RecordId::new_v7().to_string();
        let with = SitePrincipals::new(Arc::new(Fixed(Principal::with_roles("u", &["Reviewer"]))));
        assert!(with.principal(&id).is_none());
    }

    #[test]
    fn what_a_site_provided_is_remembered_and_forgotten() {
        struct Nothing;
        #[async_trait::async_trait]
        impl QuarantineBackend for Nothing {
            async fn list(
                &self,
                _t: &str,
                _a: Option<&str>,
                _l: usize,
            ) -> Result<Vec<super::super::Item>, super::super::BackendError> {
                Ok(Vec::new())
            }
            async fn get(
                &self,
                _t: &str,
                _i: &str,
            ) -> Result<Option<super::super::Item>, super::super::BackendError> {
                Ok(None)
            }
            async fn image(
                &self,
                _t: &str,
                _i: &str,
            ) -> Result<Option<String>, super::super::BackendError> {
                Ok(None)
            }
            async fn transition(
                &self,
                _t: &str,
                _i: &str,
                _to: super::super::State,
                _d: super::super::Decision,
                _a: &str,
                _r: Option<&str>,
            ) -> Result<(), super::super::BackendError> {
                Ok(())
            }
            async fn summary(
                &self,
                _t: &str,
            ) -> Result<Vec<super::super::Item>, super::super::BackendError> {
                Ok(Vec::new())
            }
        }
        let site = "live-test-remember";
        assert!(recall(site).is_none());
        remember(site, Arc::new(Nothing));
        assert!(recall(site).is_some());
        forget(site);
        assert!(recall(site).is_none());
    }
}
