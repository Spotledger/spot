//! WP-IAM-12.T5: invitations. The invitation and its mail are one transaction, accepting is
//! single use with one answer for every bad token, expiry follows the injected clock, a weak
//! password leaves the token usable, and no secret is stored or echoed.
//!
//! The logic is generic over the storage transaction, so these tests run the real functions over
//! the in-memory backend with the real DocType files (no database needed). The PostgreSQL
//! specifics (row-lock serialisation of two accepts of one token) belong to the integration run
//! with `SPOT_TEST_PG_URL` and are listed in the task report.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use serde_json::Value as Json;
use spot_auth::{HashParams, OpaqueToken, Password, PasswordHash, PasswordPolicy, verify_password};
use spot_cli::services::invitations::{
    AcceptEnv, Decision, INVITATION, InvitationCredentials, MailSpec, Unavailable, Verdict,
    accept_in, decide, enqueue_for_committed, invitation_message, issue_with_mail, mail_event,
    token_digest_hex,
};
use spot_core::SiteId;
use spot_http::api::ports::invitation_accept::{AcceptError, AcceptIn};
use spot_http::api::service::BoxFuture;
use spot_meta::{Definition, Registry};
use spot_storage_contract::{
    Catalog, ChangeFeed, Collation, DocumentStore, FeedPosition, Isolation, Name, OutboxRecord,
    RecordId, RowLock, StorageBackend, Transaction, TxOptions, Value,
};
use spot_storage_fake::FakeBackend;
use time::OffsetDateTime;
use time::macros::datetime;

const STRONG: &str = "violet-lantern-staple-9-Quartz";
const EMAIL: &str = "ada@example.com";
const NOW: OffsetDateTime = datetime!(2026-11-13 08:00:00 UTC);

fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

#[derive(Default)]
struct Creds {
    calls: Mutex<Vec<(RecordId, PasswordHash)>>,
    fail: Mutex<bool>,
}

impl Creds {
    fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
    fn failing(&self, on: bool) {
        *self.fail.lock().unwrap() = on;
    }
}

impl InvitationCredentials for Creds {
    fn set_password<'a>(
        &'a self,
        user: RecordId,
        hash: PasswordHash,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if *self.fail.lock().unwrap() {
                return Err("the credential store is down".to_owned());
            }
            self.calls.lock().unwrap().push((user, hash));
            Ok(())
        })
    }
}

struct World {
    backend: FakeBackend,
    catalog: Arc<Catalog>,
    defs: Vec<Definition>,
    spec: MailSpec,
    user: RecordId,
    creds: Arc<Creds>,
    policy: PasswordPolicy,
    params: HashParams,
}

fn definitions() -> Vec<Definition> {
    vec![
        Definition::parse(
            "user.toml",
            include_str!("../../../../apps/platform/doctypes/user.toml"),
        )
        .unwrap(),
        Definition::parse(
            "user_recovery_digest.toml",
            include_str!("../../../../apps/platform/doctypes/user_recovery_digest.toml"),
        )
        .unwrap(),
        Definition::parse(
            "invitation.toml",
            include_str!("../../../../apps/platform/doctypes/invitation.toml"),
        )
        .unwrap(),
    ]
}

async fn begin(w: &World) -> <FakeBackend as StorageBackend>::Tx {
    w.backend
        .begin(
            w.catalog.clone(),
            TxOptions::new(n("invitation_test"), Isolation::ReadCommitted),
        )
        .await
        .unwrap()
}

async fn world() -> World {
    let defs = definitions();
    let meta = Registry::new(definitions()).unwrap().effective();
    let catalog = Arc::new(meta.catalog(Collation::new("und").unwrap()).unwrap());
    let backend = FakeBackend::new();
    backend.prepare(&catalog).await.unwrap();
    let user = RecordId::new_v7();
    let w = World {
        backend,
        catalog,
        defs,
        spec: MailSpec {
            site: SiteId::new("acme").unwrap(),
            from: "noreply@acme.example".to_owned(),
        },
        user,
        creds: Arc::new(Creds::default()),
        policy: PasswordPolicy::default(),
        params: HashParams::for_tests(),
    };
    // An invited user, as the administrator's create leaves it.
    let mut fields = spot_cli_fields(&w.defs, "user");
    for (key, value) in [
        ("user_name", Value::from(EMAIL)),
        ("email", Value::from(EMAIL)),
        ("full_name", Value::from("Ada")),
        ("user_type", Value::from("internal")),
        ("state", Value::from("invited")),
        ("language", Value::from("en")),
        ("time_zone", Value::from("UTC")),
        ("failed_attempts", Value::Int(0)),
    ] {
        fields.insert(n(key), value);
    }
    let mut tx = begin(&w).await;
    tx.insert(&spot_storage_contract::DocumentData {
        entity: n("user"),
        id: user,
        fields,
        children: Default::default(),
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();
    w
}

/// Every declared field of `doctype` set to null, as storage demands full writes.
fn spot_cli_fields(defs: &[Definition], doctype: &str) -> spot_storage_contract::Fields {
    let def = defs.iter().find(|d| d.name == doctype).unwrap();
    let mut fields = spot_storage_contract::Fields::new();
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

async fn invite_at(w: &World, now: OffsetDateTime) -> (RecordId, String, OffsetDateTime) {
    let mut tx = begin(w).await;
    let issued = issue_with_mail(
        &mut tx,
        &w.defs,
        &w.spec,
        w.user,
        EMAIL,
        now,
        time::Duration::hours(72),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    (issued.id, issued.token, issued.expires_at)
}

async fn accept(w: &World, token: &str, password: &str, now: OffsetDateTime) -> Verdict {
    accept_named(w, token, password, None, now).await
}

async fn accept_named(
    w: &World,
    token: &str,
    password: &str,
    full_name: Option<&str>,
    now: OffsetDateTime,
) -> Verdict {
    let mut tx = begin(w).await;
    let env = AcceptEnv {
        site: "acme",
        now,
        credentials: w.creds.as_ref(),
        policy: &w.policy,
        params: &w.params,
    };
    let input = AcceptIn {
        token: token.to_owned(),
        password: password.to_owned(),
        full_name: full_name.map(str::to_owned),
    };
    let verdict = accept_in(&mut tx, &env, &input).await;
    if verdict.commits() {
        tx.commit().await.unwrap();
    } else {
        tx.rollback().await.unwrap();
    }
    verdict
}

async fn outbox(w: &World) -> Vec<OutboxRecord> {
    w.backend
        .read_outbox(FeedPosition::START, 1000)
        .await
        .unwrap()
}

async fn text_of(w: &World, doctype: &str, id: RecordId, field: &str) -> Option<String> {
    let mut tx = begin(w).await;
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

fn topics(records: &[OutboxRecord]) -> Vec<String> {
    records
        .iter()
        .map(|r| r.event.topic.as_str().to_owned())
        .collect()
}

#[test]
fn the_clock_decides_the_state() {
    let later = NOW + time::Duration::hours(1);
    assert_eq!(decide("pending", Some(later), NOW), Decision::Accept);
    // The expiry instant itself is past.
    assert_eq!(decide("pending", Some(NOW), NOW), Decision::Expired);
    assert_eq!(decide("pending", Some(NOW), later), Decision::Expired);
    for final_state in ["accepted", "expired", "revoked", "", "other"] {
        assert_eq!(decide(final_state, Some(later), NOW), Decision::Refuse);
    }
    // No expiry on a pending record: fail closed.
    assert_eq!(decide("pending", None, NOW), Decision::Refuse);
}

#[test]
fn only_a_token_shaped_text_has_a_digest() {
    let token = OpaqueToken::generate();
    let digest = token_digest_hex(token.as_str()).unwrap();
    assert_eq!(digest.len(), 64);
    assert!(
        digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
    assert!(!digest.contains(token.as_str()));
    for junk in ["", "short", "not a token at all, with spaces", "\u{0}"] {
        assert!(token_digest_hex(junk).is_none(), "{junk:?}");
    }
}

#[test]
fn the_message_carries_the_token_in_the_body_only() {
    let token = OpaqueToken::generate();
    let message = invitation_message("noreply@acme.example", EMAIL, "acme", token.as_str(), NOW);
    assert!(!message.subject.contains(token.as_str()));
    assert!(message.headers.is_empty());
    let body = message.text.as_deref().unwrap();
    assert!(body.contains(token.as_str()));
    assert!(body.contains("2026-11-13T08:00:00Z"));
    assert_eq!(message.to.len(), 1);
    assert_eq!(message.to[0].address, EMAIL);
}

#[test]
fn a_mail_the_library_refuses_is_not_an_event() {
    let spec = MailSpec {
        site: SiteId::new("acme").unwrap(),
        from: "noreply@acme.example".to_owned(),
    };
    let token = OpaqueToken::generate();
    let id = RecordId::new_v7();
    let ok = mail_event(&spec, id, EMAIL, token.as_str(), NOW, NOW);
    assert!(ok.is_ok(), "{:?}", ok.err());
    for bad in ["not an address", "", "a@b@c"] {
        assert_eq!(
            mail_event(&spec, id, bad, token.as_str(), NOW, NOW).err(),
            Some(Unavailable),
            "{bad:?}"
        );
    }
    // A text that is not a token has no digest, so no message id: refused.
    assert_eq!(
        mail_event(&spec, id, EMAIL, "short", NOW, NOW).err(),
        Some(Unavailable)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_invitation_and_its_mail_are_committed_together() {
    let w = world().await;
    let (id, token, expires) = invite_at(&w, NOW).await;
    assert_eq!(expires, NOW + time::Duration::hours(72));

    // The record: pending, the digest and nothing of the token.
    assert_eq!(
        text_of(&w, INVITATION, id, "state").await.as_deref(),
        Some("pending")
    );
    let digest = token_digest_hex(&token).unwrap();
    assert_eq!(text_of(&w, INVITATION, id, "digest").await, Some(digest));
    let mut tx = begin(&w).await;
    let loaded = tx
        .load(&n(INVITATION), id, RowLock::None)
        .await
        .unwrap()
        .unwrap();
    tx.rollback().await.unwrap();
    assert!(
        !format!("{:?}", loaded.data).contains(&token),
        "the token is never stored"
    );

    // The mail: one job event, about this invitation, whose message holds the token in its text.
    let records = outbox(&w).await;
    assert_eq!(topics(&records), ["sys_job_enqueued"]);
    let event = &records[0].event;
    assert_eq!(event.subject.id, id);
    let payload: Json = serde_json::from_str(&event.payload).unwrap();
    assert_eq!(payload["data"]["job"], "platform:send_mail");
    let message = &payload["data"]["args"]["message"];
    assert_eq!(message["to"][0]["address"], EMAIL);
    assert!(message["text"].as_str().unwrap().contains(&token));
    assert!(!message["subject"].as_str().unwrap().contains(&token));
    assert!(
        payload["data"]["idempotency_key"]
            .as_str()
            .unwrap()
            .starts_with("invitation-")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kill_between_the_invitation_and_its_mail_leaves_both_or_neither() {
    let w = world().await;

    // Killed before the commit: no invitation, no mail.
    let mut tx = begin(&w).await;
    let issued = issue_with_mail(
        &mut tx,
        &w.defs,
        &w.spec,
        w.user,
        EMAIL,
        NOW,
        time::Duration::hours(72),
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    assert!(
        outbox(&w).await.is_empty(),
        "no mail without its invitation"
    );
    let mut tx = begin(&w).await;
    let digest = token_digest_hex(&issued.token).unwrap();
    let found = spot_cli_find(&mut tx, &digest).await;
    tx.rollback().await.unwrap();
    assert!(found.is_none(), "no invitation without its mail");

    // A mail the library refuses fails the whole issue: the write of the record is rolled back
    // with the transaction, never left pending with nobody told.
    let mut tx = begin(&w).await;
    let refused = issue_with_mail(
        &mut tx,
        &w.defs,
        &w.spec,
        w.user,
        "not an address",
        NOW,
        time::Duration::hours(72),
    )
    .await;
    assert_eq!(refused.err(), Some(Unavailable));
    tx.rollback().await.unwrap();
    assert!(outbox(&w).await.is_empty());

    // Committed: both exist.
    let (id, token, _) = invite_at(&w, NOW).await;
    assert_eq!(outbox(&w).await.len(), 1);
    assert_eq!(
        text_of(&w, INVITATION, id, "state").await.as_deref(),
        Some("pending")
    );
    assert!(token_digest_hex(&token).is_some());
}

async fn spot_cli_find(
    tx: &mut <FakeBackend as StorageBackend>::Tx,
    digest: &str,
) -> Option<RecordId> {
    use spot_storage_contract::{
        Expr, Query, QueryExecutor, QueryPath, Selection, TextMatch, TextMode,
    };
    let (entity, alias) = (n(INVITATION), n("t"));
    let mut q = Query::from(&entity, &alias, QueryPath::Report);
    q.select = vec![Selection {
        expr: Expr::field(&alias, &n("id")),
        alias: n("v"),
    }];
    q.filter = Some(Expr::Text(
        Box::new(Expr::field(&alias, &n("digest"))),
        TextMatch {
            mode: TextMode::Equals,
            pattern: digest.to_owned(),
            case_insensitive: false,
        },
    ));
    q.limit = Some(1);
    let rows = tx.query(&q).await.unwrap();
    rows.rows.into_iter().flatten().find_map(|v| match v {
        Value::Id(id) => Some(id),
        _ => None,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepting_sets_the_password_activates_the_user_and_works_once() {
    let w = world().await;
    let (id, token, _) = invite_at(&w, NOW).await;

    let verdict = accept_named(&w, &token, STRONG, Some("Ada Lovelace"), NOW).await;
    assert_eq!(verdict, Verdict::Accepted(w.user));
    assert!(verdict.answer().is_ok());

    // The verifier went to the credential port, hashed: the password is in no store.
    assert_eq!(w.creds.count(), 1);
    {
        let calls = w.creds.calls.lock().unwrap();
        let (user, hash) = &calls[0];
        assert_eq!(*user, w.user);
        assert!(!hash.phc_string().contains(STRONG));
        assert!(
            verify_password(&Password::new(STRONG), hash, &w.params).is_ok(),
            "the verifier is the password's"
        );
    }
    // The user is active and keeps its type; the invitation is accepted.
    assert_eq!(
        text_of(&w, "user", w.user, "state").await.as_deref(),
        Some("active")
    );
    assert_eq!(
        text_of(&w, "user", w.user, "user_type").await.as_deref(),
        Some("internal")
    );
    assert_eq!(
        text_of(&w, "user", w.user, "full_name").await.as_deref(),
        Some("Ada Lovelace")
    );
    assert_eq!(
        text_of(&w, INVITATION, id, "state").await.as_deref(),
        Some("accepted")
    );
    // The facts: the user was enabled and the password changed (and the mail job from the issue).
    let topics = topics(&outbox(&w).await);
    assert!(
        topics.contains(&"identity_user_enabled".to_owned()),
        "{topics:?}"
    );
    assert!(
        topics.contains(&"identity_password_changed".to_owned()),
        "{topics:?}"
    );
    let blob = format!("{:?}", outbox(&w).await);
    assert!(!blob.contains(STRONG), "no event holds the password");

    // The second use of the same token is the one refusal, and changes nothing.
    let again = accept(&w, &token, "another-Strong-passphrase-77", NOW).await;
    assert_eq!(again, Verdict::Invalid);
    assert_eq!(again.answer(), Err(AcceptError::Invalid));
    assert_eq!(w.creds.count(), 1, "the password is not set twice");
    assert_eq!(
        text_of(&w, "user", w.user, "full_name").await.as_deref(),
        Some("Ada Lovelace")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_invitation_is_refused_by_the_injected_clock_and_marked() {
    let w = world().await;
    let (id, token, expires) = invite_at(&w, NOW).await;

    // One second before the end it still works for the policy check (a weak password is the
    // policy's answer, which only a valid token reaches).
    let almost = expires - time::Duration::seconds(1);
    assert_eq!(
        accept(&w, &token, "short", almost).await,
        Verdict::PasswordPolicy
    );

    // At the expiry instant it is expired: the answer is the one refusal, the record says so,
    // and the password port was never asked.
    let verdict = accept(&w, &token, STRONG, expires).await;
    assert_eq!(verdict, Verdict::Expired);
    assert_eq!(verdict.answer(), Err(AcceptError::Invalid));
    assert_eq!(
        text_of(&w, INVITATION, id, "state").await.as_deref(),
        Some("expired")
    );
    assert_eq!(w.creds.count(), 0);

    // Still refused when the clock is somehow earlier again: expired is final.
    assert_eq!(accept(&w, &token, STRONG, NOW).await, Verdict::Invalid);
    assert_eq!(
        text_of(&w, "user", w.user, "state").await.as_deref(),
        Some("invited")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_bad_token_is_the_same_answer() {
    let w = world().await;
    let (id, token, _) = invite_at(&w, NOW).await;
    let (other_id, revoked, _) = invite_at(&w, NOW).await;

    // Revoke the second invitation as an administrator does.
    let mut tx = begin(&w).await;
    let loaded = tx
        .load(
            &n(INVITATION),
            other_id,
            RowLock::Update(spot_storage_contract::Wait::Block),
        )
        .await
        .unwrap()
        .unwrap();
    let mut data = loaded.data;
    data.fields.insert(n("state"), Value::from("revoked"));
    tx.update(&data, loaded.version).await.unwrap();
    tx.commit().await.unwrap();

    let unknown = OpaqueToken::generate();
    let used = {
        assert_eq!(
            accept(&w, &token, STRONG, NOW).await,
            Verdict::Accepted(w.user)
        );
        token.clone()
    };
    let answers = [
        accept(&w, unknown.as_str(), STRONG, NOW).await,
        accept(&w, "short", STRONG, NOW).await,
        accept(&w, "", STRONG, NOW).await,
        accept(&w, &revoked, STRONG, NOW).await,
        accept(&w, &used, STRONG, NOW).await,
    ];
    for answer in answers {
        assert_eq!(answer.answer(), Err(AcceptError::Invalid), "{answer:?}");
        assert!(!answer.commits());
    }
    // Nothing of the above reached the password port beyond the one real acceptance.
    assert_eq!(w.creds.count(), 1);
    let _ = id;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_weak_password_is_its_own_answer_and_leaves_the_token_usable() {
    let w = world().await;
    let (id, token, _) = invite_at(&w, NOW).await;

    let weak = accept(&w, &token, "short", NOW).await;
    assert_eq!(weak, Verdict::PasswordPolicy);
    assert_eq!(weak.answer(), Err(AcceptError::PasswordPolicy));
    assert_eq!(w.creds.count(), 0);
    assert_eq!(
        text_of(&w, INVITATION, id, "state").await.as_deref(),
        Some("pending")
    );
    assert_eq!(
        text_of(&w, "user", w.user, "state").await.as_deref(),
        Some("invited")
    );

    // The words the policy forbids: the user's own address.
    assert_eq!(
        accept(&w, &token, "ada@example.com-ada@example.com", NOW).await,
        Verdict::PasswordPolicy
    );

    // On a token that is not valid, a weak password is the token's answer, never the policy's.
    let unknown = OpaqueToken::generate();
    assert_eq!(
        accept(&w, unknown.as_str(), "short", NOW).await,
        Verdict::Invalid
    );

    // The same token with a good password then works.
    assert_eq!(
        accept(&w, &token, STRONG, NOW).await,
        Verdict::Accepted(w.user)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_an_invited_user_can_accept() {
    let w = world().await;
    let (_, token, _) = invite_at(&w, NOW).await;

    // The user was disabled after the invitation was issued.
    let mut tx = begin(&w).await;
    let loaded = tx
        .load(
            &n("user"),
            w.user,
            RowLock::Update(spot_storage_contract::Wait::Block),
        )
        .await
        .unwrap()
        .unwrap();
    let mut data = loaded.data;
    data.fields.insert(n("state"), Value::from("disabled"));
    tx.update(&data, loaded.version).await.unwrap();
    tx.commit().await.unwrap();

    assert_eq!(accept(&w, &token, STRONG, NOW).await, Verdict::Invalid);
    assert_eq!(w.creds.count(), 0);
    assert_eq!(
        text_of(&w, "user", w.user, "state").await.as_deref(),
        Some("disabled")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_credential_store_changes_nothing_and_the_token_still_works() {
    let w = world().await;
    let (id, token, _) = invite_at(&w, NOW).await;

    w.creds.failing(true);
    let down = accept(&w, &token, STRONG, NOW).await;
    assert_eq!(down, Verdict::Unavailable);
    assert_eq!(down.answer(), Err(AcceptError::Unavailable));
    assert_eq!(
        text_of(&w, INVITATION, id, "state").await.as_deref(),
        Some("pending")
    );
    assert_eq!(
        text_of(&w, "user", w.user, "state").await.as_deref(),
        Some("invited")
    );
    assert_eq!(outbox(&w).await.len(), 1, "only the mail job of the issue");

    w.creds.failing(false);
    assert_eq!(
        accept(&w, &token, STRONG, NOW).await,
        Verdict::Accepted(w.user)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mail_of_a_committed_invitation_is_queued_in_the_callers_transaction() {
    let w = world().await;
    // An invitation the user administration wrote on its own (no mail yet).
    let token = OpaqueToken::generate();
    let id = RecordId::new_v7();
    let mut fields = spot_cli_fields(&w.defs, INVITATION);
    for (key, value) in [
        ("user", Value::Id(w.user)),
        ("email", Value::from(EMAIL)),
        ("state", Value::from("pending")),
        ("expires_at", spot_cli_dt(NOW + time::Duration::hours(72))),
        ("sent_at", spot_cli_dt(NOW)),
        (
            "digest",
            Value::from(token_digest_hex(token.as_str()).unwrap()),
        ),
    ] {
        fields.insert(n(key), value);
    }
    let mut tx = begin(&w).await;
    tx.insert(&spot_storage_contract::DocumentData {
        entity: n(INVITATION),
        id,
        fields,
        children: Default::default(),
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(outbox(&w).await.is_empty());

    // The after-commit adapter queues its mail; killed before the commit: nothing.
    let mut tx = begin(&w).await;
    let found = enqueue_for_committed(&mut tx, &w.spec, EMAIL, token.as_str(), NOW, NOW)
        .await
        .unwrap();
    assert_eq!(found, id);
    tx.rollback().await.unwrap();
    assert!(outbox(&w).await.is_empty());

    let mut tx = begin(&w).await;
    enqueue_for_committed(&mut tx, &w.spec, EMAIL, token.as_str(), NOW, NOW)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(topics(&outbox(&w).await), ["sys_job_enqueued"]);

    // A token no invitation holds queues nothing.
    let stranger = OpaqueToken::generate();
    let mut tx = begin(&w).await;
    let refused = enqueue_for_committed(&mut tx, &w.spec, EMAIL, stranger.as_str(), NOW, NOW).await;
    assert_eq!(refused.err(), Some(Unavailable));
    tx.rollback().await.unwrap();
    assert_eq!(outbox(&w).await.len(), 1);
}

fn spot_cli_dt(t: OffsetDateTime) -> Value {
    Value::DateTime(t)
}
