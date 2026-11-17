//! The `webhooks` caller of the secret path (WP-INT-01.T5): the webhook signing-secret service
//! seals a secret and opens it for the delivery handler under `SecretCaller::Webhooks`, the audit
//! journal names that caller, and nothing about a value reaches the journal, an error or a row.
//!
//! The suite's own `account` DocType stands in for the `webhook` DocType: the engine's secret path
//! is the same for every DocType, so what is proved here holds for `webhook.secret`.
//!
//! To be run, this file needs `#[path = "secrets/webhooks.rs"] mod webhooks;` in
//! `tests/secrets.rs` (an integrator edit: that file is not owned by this task).

use crate::support::*;

const SIGNING_SECRET: &str = "whsec_9f2c41d7b8a3e0561c4d77aa02be83f1";
const NEXT_SIGNING_SECRET: &str = "whsec_0b7e55c2d19a4f83a6c1e2d4f7390a58";

fn webhooks<B: StorageBackend + ChangeFeed>(r: &Rig<B>) -> SecretAccess {
    r.engine.secret_access(SecretCaller::Webhooks, &inv())
}

fn webhooks_at<B: StorageBackend + ChangeFeed>(r: &Rig<B>, site: &str) -> SecretAccess {
    r.engine
        .secret_access(SecretCaller::Webhooks, &inv_at(site))
}

#[test]
fn the_webhooks_caller_has_a_stable_name_that_round_trips() {
    assert_eq!(SecretCaller::Webhooks.as_str(), "webhooks");
    assert_eq!(
        SecretCaller::from_name("webhooks"),
        Some(SecretCaller::Webhooks)
    );
    for caller in [
        SecretCaller::LoginService,
        SecretCaller::Seed,
        SecretCaller::Webhooks,
        SecretCaller::Test,
    ] {
        assert_eq!(SecretCaller::from_name(caller.as_str()), Some(caller));
    }
    assert_eq!(SecretCaller::from_name("webhook"), None);
    assert_eq!(SecretCaller::from_name("Webhooks"), None);
}

async fn a_secret_is_set_opened_and_rotated_under_the_webhooks_caller<
    B: StorageBackend + ChangeFeed,
>(
    r: &Rig<B>,
) {
    let access = webhooks(r);
    assert_eq!(access.caller(), SecretCaller::Webhooks);
    let (id, v1) = r.account("hook").await;

    // Unset: the read path answers nothing, never an empty key.
    assert!(
        r.engine
            .open_secret(&access, "account", id, "password")
            .await
            .unwrap()
            .is_none()
    );

    let v2 = r
        .engine
        .set_secret(&access, "account", id, "password", s(SIGNING_SECRET), v1)
        .await
        .unwrap();
    let first = r.sealed(id, "password").await.unwrap();
    assert_eq!(
        r.engine
            .open_secret(&access, "account", id, "password")
            .await
            .unwrap()
            .unwrap()
            .expose(),
        SIGNING_SECRET
    );

    // A rotation replaces the value: the resolver now returns the new one and the stored form
    // changed.
    r.engine
        .set_secret(
            &access,
            "account",
            id,
            "password",
            s(NEXT_SIGNING_SECRET),
            v2,
        )
        .await
        .unwrap();
    let second = r.sealed(id, "password").await.unwrap();
    assert_ne!(first, second);
    assert_eq!(
        r.engine
            .open_secret(&access, "account", id, "password")
            .await
            .unwrap()
            .unwrap()
            .expose(),
        NEXT_SIGNING_SECRET
    );
}

async fn the_journal_names_webhooks_and_holds_no_value<B: StorageBackend + ChangeFeed>(r: &Rig<B>) {
    let access = webhooks(r);
    let (id, v1) = r.account("hook").await;
    r.engine
        .set_secret(&access, "account", id, "password", s(SIGNING_SECRET), v1)
        .await
        .unwrap();
    r.engine
        .open_secret(&access, "account", id, "password")
        .await
        .unwrap();

    let journal = r.engine.secret_audit_journal(&access).await.unwrap();
    let ours: Vec<_> = journal.iter().filter(|e| e.id == Some(id)).collect();
    assert!(
        ours.iter().any(|e| e.op == SecretOp::Set),
        "the set is journalled: {ours:?}"
    );
    assert!(
        ours.iter().any(|e| e.op == SecretOp::Open),
        "the read of the signing key is journalled: {ours:?}"
    );
    for entry in &ours {
        assert_eq!(entry.caller, SecretCaller::Webhooks, "{entry:?}");
        assert_eq!(entry.field, "password");
        assert_eq!(entry.error, None);
    }

    // The journal, the documents, the rows, the outbox and the in-process log hold no form of
    // the value or of its sealed bytes.
    let sealed = r.sealed(id, "password").await.unwrap();
    let everything = r.everything(&[id], &[format!("{journal:?}")]).await;
    assert_absent(&everything, &forms_of_plain(SIGNING_SECRET), "the journal");
    assert_absent(&everything, &forms_of_sealed(&sealed), "the journal");
}

async fn a_refused_set_changes_nothing_and_is_journalled_with_its_code<
    B: StorageBackend + ChangeFeed,
>(
    r: &Rig<B>,
) {
    let access = webhooks(r);
    let (id, v1) = r.account("hook").await;
    r.engine
        .set_secret(
            &access,
            "account",
            id,
            "password",
            s("first-value-1234567"),
            v1,
        )
        .await
        .unwrap();
    // `v1` is stale now: the write is refused and changes nothing.
    let err = r
        .engine
        .set_secret(&access, "account", id, "password", s(SIGNING_SECRET), v1)
        .await
        .unwrap_err();
    let shown = format!("{err:?}{err}");
    assert_absent(
        shown.as_bytes(),
        &forms_of_plain(SIGNING_SECRET),
        "an error",
    );

    // The stored value is still the first one.
    assert_eq!(
        r.engine
            .open_secret(&access, "account", id, "password")
            .await
            .unwrap()
            .unwrap()
            .expose(),
        "first-value-1234567"
    );

    // The refusal is in the journal with its code, under the same caller.
    let journal = r.engine.secret_audit_journal(&access).await.unwrap();
    let refused: Vec<_> = journal
        .iter()
        .filter(|e| e.op == SecretOp::Set && e.error.is_some())
        .collect();
    assert_eq!(refused.len(), 1, "{journal:?}");
    assert_eq!(refused[0].caller, SecretCaller::Webhooks);
    assert_eq!(refused[0].error.as_deref(), Some(err.code.as_str()));
    let text = format!("{journal:?}");
    assert_absent(
        text.as_bytes(),
        &forms_of_plain(SIGNING_SECRET),
        "the journal",
    );
}

async fn another_sites_resolver_cannot_open_this_sites_secret<B: StorageBackend + ChangeFeed>(
    r: &Rig<B>,
) {
    let access = webhooks(r);
    let (id, v1) = r.account("hook").await;
    r.engine
        .set_secret(&access, "account", id, "password", s(SIGNING_SECRET), v1)
        .await
        .unwrap();
    let other = webhooks_at(r, "evil.test");
    let err = r
        .engine
        .open_secret(&other, "account", id, "password")
        .await
        .unwrap_err();
    assert_eq!(err.code, "secretfield.open_failed");
    let shown = format!("{err:?}{err}");
    assert_absent(
        shown.as_bytes(),
        &forms_of_plain(SIGNING_SECRET),
        "an error",
    );
    // The owner still opens it.
    assert_eq!(
        r.engine
            .open_secret(&access, "account", id, "password")
            .await
            .unwrap()
            .unwrap()
            .expose(),
        SIGNING_SECRET
    );
}

cases!(
    a_secret_is_set_opened_and_rotated_under_the_webhooks_caller,
    the_journal_names_webhooks_and_holds_no_value,
    a_refused_set_changes_nothing_and_is_journalled_with_its_code,
    another_sites_resolver_cannot_open_this_sites_secret,
);

/// With no vault the engine answers `secretfield.not_configured` to the read and the write: the
/// resolver gets an error, never an empty key, and nothing is written in the clear.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_engine_with_no_vault_fails_closed_for_the_webhooks_caller() {
    let mut builder = RegistryBuilder::new();
    builder.add_controller(
        "probe",
        Arc::new(Tripwire {
            armed: Arc::new(AtomicBool::new(false)),
        }),
    );
    let engine = Engine::new(
        FakeBackend::new(),
        meta(),
        Arc::new(builder.build().unwrap()),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap();
    engine.prepare().await.unwrap();
    let mut doc = spot_app_api::Document::new(n("account"), RecordId::new_v7());
    doc.set(n("label"), "hook");
    let saved = engine.create(&inv(), doc).await.unwrap().value;
    let access = engine.secret_access(SecretCaller::Webhooks, &inv());
    let read = engine
        .open_secret(&access, "account", saved.id(), "password")
        .await;
    assert_eq!(code_of(read), "secretfield.not_configured");
    let write = engine
        .set_secret(
            &access,
            "account",
            saved.id(),
            "password",
            s(SIGNING_SECRET),
            saved.version().unwrap(),
        )
        .await;
    assert_eq!(code_of(write), "secretfield.not_configured");
}
