//! WP-IAM-05.T16: the directory snapshot and the application of a sign-in resolution, over the
//! real platform DocTypes and the in-memory backend (no database needed).
//!
//! What is proved here: an already linked identity signs its user in; an e-mail match alone never
//! links (it is refused, and nothing is created); a verified, unexpired invitation links once and
//! then the identity is simply linked; an unverified address, an expired invitation or an identity
//! already linked to another user never links; a just-in-time user is created by a rule of the
//! policy, active, with **no role at all**, and a domain rule needs a verified address; a consumer
//! is never turned into a user; a disabled user is not signed in. The PostgreSQL parts (the
//! hand-over table, the SAML store, the policy store, the session index, restart-safe replay
//! protection) are in `sso_providers_pg.rs`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_cli::services::sso::IdentityFailure;
use spot_cli::services::sso_identity::apply::{ApplyEnv, apply_in};
use spot_cli::services::sso_identity::directory::{SnapshotEnv, snapshot_in};
use spot_core::Audience;
use spot_meta::{Definition, Registry};
use spot_sso::{
    JitPolicy, JitRule, ProviderId, Resolution, SignInMode, SignInPolicy, SsoIdentity, codes,
    resolve,
};
use spot_storage_contract::{
    Catalog, ChangeFeed, Collation, DocumentData, DocumentStore, Expr, FeedPosition, Fields,
    Isolation, Name, Query, QueryExecutor, QueryPath, RecordId, RowLock, Selection, StorageBackend,
    Transaction, TxOptions, Value,
};
use spot_storage_fake::FakeBackend;
use time::OffsetDateTime;
use time::macros::datetime;

const NOW: OffsetDateTime = datetime!(2026-11-13 08:00:00 UTC);
const ISSUER: &str = "https://idp.acme.example";
const INVITATION: &str = "invitation";

/// The invitation DocType as `apps/platform/doctypes/invitation.toml` defines it (WP-IAM-12.T5),
/// repeated here so this file does not depend on the platform app listing it yet.
const INVITATION_TOML: &str = r#"
name = "invitation"
label = "Invitation"
app = "platform"
module = "identity"

[[fields]]
name = "user"
label = "User"
type = "link"
options = "user"
required = true

[[fields]]
name = "email"
label = "E-mail"
type = "text"
required = true

[[fields]]
name = "state"
label = "State"
type = "select"
options = "pending\naccepted\nexpired\nrevoked"
required = true
default = "pending"

[[fields]]
name = "expires_at"
label = "Expires At"
type = "datetime"
required = true

[[fields]]
name = "accepted_at"
label = "Accepted At"
type = "datetime"

[[fields]]
name = "digest"
label = "Token Digest"
type = "text"
required = true
unique = true
"#;

fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

struct World {
    backend: FakeBackend,
    catalog: Arc<Catalog>,
    defs: Vec<Definition>,
}

fn definitions() -> Vec<Definition> {
    let mut defs = spot_platform_app::definitions().unwrap();
    if !defs.iter().any(|d| d.name == INVITATION) {
        defs.push(Definition::parse("invitation.toml", INVITATION_TOML).unwrap());
    }
    defs
}

async fn world() -> World {
    let meta = Registry::new(definitions()).unwrap().effective();
    let catalog = Arc::new(meta.catalog(Collation::new("und").unwrap()).unwrap());
    let backend = FakeBackend::new();
    backend.prepare(&catalog).await.unwrap();
    World {
        backend,
        catalog,
        defs: definitions(),
    }
}

impl World {
    async fn begin(&self) -> <FakeBackend as StorageBackend>::Tx {
        self.backend
            .begin(
                self.catalog.clone(),
                TxOptions::new(n("sso_identity_test"), Isolation::ReadCommitted),
            )
            .await
            .unwrap()
    }

    /// Every declared field of `doctype` set to null, as storage demands full writes.
    fn blank(&self, doctype: &str) -> Fields {
        let def = self.defs.iter().find(|d| d.name == doctype).unwrap();
        let mut fields = Fields::new();
        for f in def.fields.iter().filter(|f| !f.ty.is_table()) {
            if f.ty == spot_meta::FieldType::Secret {
                fields.insert(n(&spot_meta::sealed_column(&f.name)), Value::Null);
                if f.lookup_index() {
                    fields.insert(n(&spot_meta::blind_index_column(&f.name)), Value::Null);
                }
            } else {
                fields.insert(n(&f.name), Value::Null);
            }
        }
        fields
    }

    async fn insert(&self, doctype: &str, pairs: Vec<(&str, Value)>) -> RecordId {
        let mut fields = self.blank(doctype);
        for (key, value) in pairs {
            fields.insert(n(key), value);
        }
        let id = RecordId::new_v7();
        let mut tx = self.begin().await;
        tx.insert(&DocumentData {
            entity: n(doctype),
            id,
            fields,
            children: Default::default(),
        })
        .await
        .unwrap();
        tx.commit().await.unwrap();
        id
    }

    async fn user(&self, email: &str, state: &str, kind: &str) -> RecordId {
        self.insert(
            "user",
            vec![
                ("user_name", Value::from(email)),
                ("email", Value::from(email)),
                ("full_name", Value::from("Someone")),
                ("user_type", Value::from(kind)),
                ("state", Value::from(state)),
                ("language", Value::from("en")),
                ("time_zone", Value::from("UTC")),
                ("failed_attempts", Value::Int(0)),
            ],
        )
        .await
    }

    async fn link(&self, user: RecordId, subject: &str) {
        self.insert(
            "linked_identity",
            vec![
                ("user", Value::Id(user)),
                ("provider", Value::from(ISSUER)),
                ("subject", Value::from(subject)),
            ],
        )
        .await;
    }

    async fn invitation(
        &self,
        user: RecordId,
        email: &str,
        state: &str,
        expires: OffsetDateTime,
    ) -> RecordId {
        self.insert(
            INVITATION,
            vec![
                ("user", Value::Id(user)),
                ("email", Value::from(email)),
                ("state", Value::from(state)),
                ("expires_at", Value::DateTime(expires)),
                ("digest", Value::from(RecordId::new_v7().to_string())),
            ],
        )
        .await
    }

    async fn count(&self, doctype: &str) -> usize {
        let (entity, alias) = (n(doctype), n("t"));
        let mut q = Query::from(&entity, &alias, QueryPath::Report);
        q.select = vec![Selection {
            expr: Expr::field(&alias, &n("id")),
            alias: n("v"),
        }];
        q.limit = Some(500);
        let mut tx = self.begin().await;
        let rows = tx.query(&q).await.unwrap();
        tx.rollback().await.unwrap();
        rows.rows.len()
    }

    async fn text(&self, doctype: &str, id: RecordId, field: &str) -> Option<String> {
        let mut tx = self.begin().await;
        let loaded = tx
            .load(&n(doctype), id, RowLock::None)
            .await
            .unwrap()
            .unwrap();
        tx.rollback().await.unwrap();
        match loaded.data.fields.get(&n(field)) {
            Some(Value::Text(t)) => Some(t.clone()),
            _ => None,
        }
    }

    async fn topics(&self) -> Vec<String> {
        self.backend
            .read_outbox(FeedPosition::START, 1000)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.event.topic.as_str().to_owned())
            .collect()
    }

    /// The snapshot of `identity` under `policy`.
    async fn snapshot(
        &self,
        policy: &SignInPolicy,
        identity: &SsoIdentity,
    ) -> spot_cli::services::sso::DirectorySnapshot {
        let providers: BTreeMap<String, (bool, Audience)> =
            BTreeMap::from([("okta".to_owned(), (true, Audience::Staff))]);
        let env = SnapshotEnv {
            now: NOW,
            policy,
            providers: &providers,
        };
        let mut tx = self.begin().await;
        let snapshot = snapshot_in(&mut tx, &env, identity).await.unwrap();
        tx.rollback().await.unwrap();
        snapshot
    }

    /// Applies `resolution` in its own transaction, committing on success.
    async fn apply(
        &self,
        policy: &SignInPolicy,
        identity: &SsoIdentity,
        resolution: &Resolution,
    ) -> Result<RecordId, IdentityFailure> {
        let env = ApplyEnv {
            now: NOW,
            defs: &self.defs,
            policy,
        };
        let mut tx = self.begin().await;
        match apply_in(&mut tx, &env, identity, resolution).await {
            Ok(user) => {
                tx.commit().await.unwrap();
                Ok(user)
            }
            Err(failure) => {
                tx.rollback().await.unwrap();
                Err(failure)
            }
        }
    }
}

fn identity(subject: &str, email: Option<&str>, verified: bool) -> SsoIdentity {
    SsoIdentity {
        provider: ProviderId::new("okta"),
        issuer: ISSUER.to_owned(),
        subject: subject.to_owned(),
        email: email.map(str::to_owned),
        email_verified: verified,
        display_name: Some("Ada Lovelace".to_owned()),
        groups: Vec::new(),
        acr: None,
        amr: Vec::new(),
        auth_time: None,
        raw_claims_hash: "h".to_owned(),
    }
}

fn optional() -> SignInPolicy {
    SignInPolicy {
        mode: SignInMode::SsoOptional,
        ..SignInPolicy::local()
    }
}

fn jit(domain: &str) -> SignInPolicy {
    SignInPolicy {
        jit: JitPolicy::RuleBased(vec![JitRule {
            email_domain: Some(domain.to_owned()),
            group: None,
            user_type: spot_iam_directory::UserType::Internal,
        }]),
        ..optional()
    }
}

fn refused(result: Result<RecordId, IdentityFailure>) -> &'static str {
    match result {
        Err(IdentityFailure::Refused(code)) => code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_already_linked_identity_signs_the_user_in() {
    let w = world().await;
    let user = w.user("ada@acme.example", "active", "internal").await;
    w.link(user, "sub-1").await;
    let who = identity("sub-1", Some("ada@acme.example"), true);
    let policy = optional();

    let snapshot = w.snapshot(&policy, &who).await;
    assert_eq!(snapshot.linked.len(), 1);
    let resolution = resolve(&who, &snapshot, &policy, NOW);
    assert!(
        matches!(resolution, Resolution::SignIn { user: u, .. } if u == user),
        "{resolution:?}"
    );

    assert_eq!(w.apply(&policy, &who, &resolution).await, Ok(user));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_email_match_alone_never_links() {
    let w = world().await;
    let user = w.user("ada@acme.example", "active", "internal").await;
    let who = identity("stranger", Some("ada@acme.example"), true);
    let policy = optional();

    let snapshot = w.snapshot(&policy, &who).await;
    assert!(snapshot.linked.is_empty());
    assert_eq!(snapshot.emails.len(), 1, "the match is reported ...");
    let resolution = resolve(&who, &snapshot, &policy, NOW);
    assert!(
        matches!(resolution, Resolution::RequireLocalConfirmation { .. }),
        "{resolution:?}"
    );

    // ... and refused, even if a caller applied it anyway. Nothing is created.
    assert_eq!(
        refused(w.apply(&policy, &who, &resolution).await),
        codes::LINK_REQUIRES_PROOF
    );
    assert_eq!(w.count("linked_identity").await, 0);
    assert_eq!(w.count("user").await, 1);
    let _ = user;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_verified_unexpired_invitation_links_once() {
    let w = world().await;
    let user = w.user("ada@acme.example", "invited", "internal").await;
    let invitation = w
        .invitation(
            user,
            "ada@acme.example",
            "pending",
            NOW + time::Duration::hours(1),
        )
        .await;
    let who = identity("sub-9", Some("Ada@Acme.example"), true);
    let policy = optional();

    let snapshot = w.snapshot(&policy, &who).await;
    assert_eq!(snapshot.invitations.len(), 1);
    let resolution = resolve(&who, &snapshot, &policy, NOW);
    assert!(
        matches!(&resolution, Resolution::LinkViaInvitation { invitation: i, .. } if *i == invitation),
        "{resolution:?}"
    );

    assert_eq!(w.apply(&policy, &who, &resolution).await, Ok(user));
    assert_eq!(
        w.text("user", user, "state").await.as_deref(),
        Some("active")
    );
    assert_eq!(
        w.text(INVITATION, invitation, "state").await.as_deref(),
        Some("accepted")
    );
    assert_eq!(w.count("linked_identity").await, 1);
    assert!(
        w.topics()
            .await
            .contains(&"identity_user_enabled".to_owned())
    );

    // The next sign-in is an ordinary linked one; the used invitation links nobody else.
    let again = w.snapshot(&policy, &who).await;
    assert!(matches!(
        resolve(&who, &again, &policy, NOW),
        Resolution::SignIn { .. }
    ));
    let other = identity("sub-other", Some("ada@acme.example"), true);
    let snapshot = w.snapshot(&policy, &other).await;
    assert!(
        snapshot.invitations.is_empty(),
        "an accepted invitation is not open"
    );
    let resolution = resolve(&other, &snapshot, &policy, NOW);
    assert_eq!(
        refused(w.apply(&policy, &other, &resolution).await),
        codes::LINK_REQUIRES_PROOF
    );
    assert_eq!(w.count("linked_identity").await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unverified_or_expired_or_foreign_invitation_never_links() {
    let w = world().await;
    let user = w.user("ada@acme.example", "invited", "internal").await;
    let invitation = w
        .invitation(
            user,
            "ada@acme.example",
            "pending",
            NOW + time::Duration::hours(1),
        )
        .await;
    let policy = optional();
    let forced = |email: &str| Resolution::LinkViaInvitation {
        invitation,
        email: email.to_owned(),
    };

    // The provider did not verify the address.
    let unverified = identity("sub-1", Some("ada@acme.example"), false);
    assert_eq!(
        refused(
            w.apply(&policy, &unverified, &forced("ada@acme.example"))
                .await
        ),
        codes::LINK_REQUIRES_PROOF
    );
    // The identity's address is not the invitation's.
    let wrong = identity("sub-2", Some("mallory@acme.example"), true);
    assert_eq!(
        refused(w.apply(&policy, &wrong, &forced("ada@acme.example")).await),
        codes::LINK_REQUIRES_PROOF
    );
    assert_eq!(w.count("linked_identity").await, 0);
    assert_eq!(
        w.text(INVITATION, invitation, "state").await.as_deref(),
        Some("pending")
    );

    // Expired: not open in the snapshot, and refused if forced.
    let late = w
        .invitation(
            user,
            "ada@acme.example",
            "pending",
            NOW - time::Duration::seconds(1),
        )
        .await;
    let good = identity("sub-3", Some("ada@acme.example"), true);
    let snapshot = w.snapshot(&policy, &good).await;
    assert_eq!(
        snapshot.invitations.len(),
        1,
        "only the unexpired one is open"
    );
    let expired = Resolution::LinkViaInvitation {
        invitation: late,
        email: "ada@acme.example".to_owned(),
    };
    assert_eq!(
        refused(w.apply(&policy, &good, &expired).await),
        codes::LINK_REQUIRES_PROOF
    );
    assert_eq!(w.count("linked_identity").await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_identity_linked_to_another_user_is_never_moved() {
    let w = world().await;
    let first = w.user("first@acme.example", "active", "internal").await;
    w.link(first, "sub-shared").await;
    let second = w.user("second@acme.example", "invited", "internal").await;
    let invitation = w
        .invitation(
            second,
            "second@acme.example",
            "pending",
            NOW + time::Duration::hours(1),
        )
        .await;
    let who = identity("sub-shared", Some("second@acme.example"), true);
    let forced = Resolution::LinkViaInvitation {
        invitation,
        email: "second@acme.example".to_owned(),
    };
    assert_eq!(
        refused(w.apply(&optional(), &who, &forced).await),
        codes::IDENTITY_OTHER_USER
    );
    assert_eq!(
        w.text(INVITATION, invitation, "state").await.as_deref(),
        Some("pending")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_just_in_time_user_has_no_role_and_a_verified_domain() {
    let w = world().await;
    let policy = jit("acme.example");
    let who = identity("sub-new", Some("new.hire@acme.example"), true);

    let snapshot = w.snapshot(&policy, &who).await;
    let resolution = resolve(&who, &snapshot, &policy, NOW);
    assert!(
        matches!(resolution, Resolution::CreateUser { .. }),
        "{resolution:?}"
    );
    let user = w.apply(&policy, &who, &resolution).await.unwrap();

    assert_eq!(
        w.text("user", user, "state").await.as_deref(),
        Some("active")
    );
    assert_eq!(
        w.text("user", user, "user_type").await.as_deref(),
        Some("internal")
    );
    assert_eq!(
        w.text("user", user, "email").await.as_deref(),
        Some("new.hire@acme.example")
    );
    assert_eq!(w.count("linked_identity").await, 1);
    assert_eq!(
        w.count("role_assignment").await,
        0,
        "a created user has no role at all"
    );
    assert!(
        w.topics()
            .await
            .contains(&"identity_user_created".to_owned())
    );

    // A second identity with the same address does not get a second account.
    let again = identity("sub-copy", Some("new.hire@acme.example"), true);
    let snapshot = w.snapshot(&policy, &again).await;
    let resolution = resolve(&again, &snapshot, &policy, NOW);
    assert_eq!(
        refused(w.apply(&policy, &again, &resolution).await),
        codes::LINK_REQUIRES_PROOF
    );
    assert_eq!(w.count("user").await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_domain_rule_needs_a_verified_address() {
    let w = world().await;
    let policy = jit("acme.example");
    let who = identity("sub-x", Some("anyone@acme.example"), false);
    let snapshot = w.snapshot(&policy, &who).await;
    // The library matches the rule on the claimed domain; creating the user is refused here.
    let resolution = resolve(&who, &snapshot, &policy, NOW);
    assert!(
        matches!(resolution, Resolution::CreateUser { .. }),
        "{resolution:?}"
    );
    assert_eq!(
        refused(w.apply(&policy, &who, &resolution).await),
        codes::JIT_NOT_ALLOWED
    );
    assert_eq!(w.count("user").await, 0);
    assert_eq!(w.count("linked_identity").await, 0);

    // No address at all: no user either, even when a group rule allows the creation.
    let by_group = SignInPolicy {
        jit: JitPolicy::RuleBased(vec![JitRule {
            email_domain: None,
            group: Some("staff".to_owned()),
            user_type: spot_iam_directory::UserType::Internal,
        }]),
        ..optional()
    };
    let mut nameless = identity("sub-y", None, false);
    nameless.groups = vec!["staff".to_owned()];
    let forced = Resolution::CreateUser {
        user_type: spot_iam_directory::UserType::Internal,
        identity: Box::new(nameless.clone()),
        audit: spot_sso::JitCreated {
            provider: ProviderId::new("okta"),
            subject: "sub-y".to_owned(),
            user_type: spot_iam_directory::UserType::Internal,
        },
    };
    assert_eq!(
        refused(w.apply(&by_group, &nameless, &forced).await),
        codes::CLAIM_MISSING
    );
    assert_eq!(w.count("user").await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_consumer_is_never_a_user_and_a_disabled_user_is_not_signed_in() {
    let w = world().await;
    let who = identity("sub-c", Some("c@x.example"), true);
    let consumer = Resolution::ConsumerSignIn {
        provider: ProviderId::new("okta"),
        subject: "sub-c".to_owned(),
        email_verified: true,
    };
    assert_eq!(
        refused(w.apply(&optional(), &who, &consumer).await),
        codes::SSO_NOT_ALLOWED
    );
    assert_eq!(w.count("user").await, 0);

    let user = w.user("off@acme.example", "disabled", "internal").await;
    w.link(user, "sub-off").await;
    let off = identity("sub-off", Some("off@acme.example"), true);
    let snapshot = w.snapshot(&optional(), &off).await;
    let resolution = resolve(&off, &snapshot, &optional(), NOW);
    assert!(
        matches!(resolution, Resolution::Denied(_)),
        "{resolution:?}"
    );
    // Forced through, the apply step refuses too (defence in depth).
    let forced = Resolution::SignIn {
        user,
        method: spot_sso::AuthMethod::Sso,
    };
    assert_eq!(
        refused(w.apply(&optional(), &off, &forced).await),
        codes::USER_DISABLED
    );
}
