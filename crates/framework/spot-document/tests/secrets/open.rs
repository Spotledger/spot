//! The explicit open capability: a value opens only in the place it was sealed, under its key, and every call fails closed without a vault.

use crate::support::*;

async fn a_sealed_value_moved_elsewhere_does_not_open<B: StorageBackend + ChangeFeed>(r: &Rig<B>) {
    let (a, va) = r.account("a").await;
    let (b, vb) = r.account("b").await;
    let access = r.access();
    r.engine
        .set_secret(&access, "account", a, "password", s(PASSWORD), va)
        .await
        .unwrap();
    r.engine
        .set_secret(&access, "account", b, "password", s("another"), vb)
        .await
        .unwrap();
    let blob = r.sealed(a, "password").await.unwrap();

    // To another record.
    r.poke(b, "password__sealed", Value::Bytes(blob.clone()))
        .await;
    let err = r
        .engine
        .open_secret(&access, "account", b, "password")
        .await
        .unwrap_err();
    assert_eq!(err.code, "secretfield.open_failed");
    // To another field of the same record.
    r.poke(a, "token__sealed", Value::Bytes(blob.clone())).await;
    let err = r
        .engine
        .open_secret(&access, "account", a, "token")
        .await
        .unwrap_err();
    assert_eq!(err.code, "secretfield.open_failed");
    // The right place still opens.
    assert_eq!(
        r.engine
            .open_secret(&access, "account", a, "password")
            .await
            .unwrap()
            .unwrap()
            .expose(),
        PASSWORD
    );
    // Another site (tenant) cannot open it either, even with the same key.
    let other = r.access_at("evil.test");
    let err = r
        .engine
        .open_secret(&other, "account", a, "password")
        .await
        .unwrap_err();
    assert_eq!(err.code, "secretfield.open_failed");
    // No error shows the value or its sealed form.
    let shown = format!("{err:?}{err}");
    assert_absent(shown.as_bytes(), &forms_of_plain(PASSWORD), "an error");
    assert_absent(shown.as_bytes(), &forms_of_sealed(&blob), "an error");
    // A flipped bit is caught.
    let mut tampered = blob.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 1;
    r.poke(a, "password__sealed", Value::Bytes(tampered)).await;
    let err = r
        .engine
        .open_secret(&access, "account", a, "password")
        .await
        .unwrap_err();
    assert_eq!(err.code, "secretfield.open_failed");
    // Garbage that is not an envelope at all.
    r.poke(
        a,
        "password__sealed",
        Value::Bytes(b"not an envelope".to_vec()),
    )
    .await;
    let err = r
        .engine
        .open_secret(&access, "account", a, "password")
        .await
        .unwrap_err();
    assert_eq!(err.code, "secretfield.open_failed");
}

async fn a_value_sealed_under_another_key_does_not_open<B: StorageBackend + ChangeFeed>(
    r: &Rig<B>,
) {
    let (id, v1) = r.account("k").await;
    let access = r.access();
    r.engine
        .set_secret(&access, "account", id, "password", s(PASSWORD), v1)
        .await
        .unwrap();
    // The associated data the engine really uses: site, DocType, record and field, NUL-framed
    // after the version label (this must stay in step with `secrets::aad`).
    let mut aad = b"spot-document/secret/v1".to_vec();
    for part in [SITE, "account", &id.to_string(), "password"] {
        aad.push(0);
        aad.extend_from_slice(part.as_bytes());
    }
    // Control: a blob sealed under the engine's own key with the real associated data opens, so
    // the failure below is down to the key and not to a wrong AAD.
    let own = SecretBox::new(r.keyring.clone());
    let sealed = own.seal(&s(TOKEN), &aad).await.unwrap().to_bytes();
    r.poke(id, "password__sealed", Value::Bytes(sealed)).await;
    let opened = r
        .engine
        .open_secret(&access, "account", id, "password")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(opened.expose(), TOKEN, "the control opens");
    // A box with the same key id but other key material, the same AAD.
    let foreign = SecretBox::new(MemoryKeyring::new("test-kek").unwrap());
    let sealed = foreign.seal(&s(PASSWORD), &aad).await.unwrap().to_bytes();
    r.poke(id, "password__sealed", Value::Bytes(sealed)).await;
    let err = r
        .engine
        .open_secret(&access, "account", id, "password")
        .await
        .unwrap_err();
    assert_eq!(err.code, "secretfield.open_failed");
}

async fn without_a_vault_every_secret_call_fails_closed<B: StorageBackend + ChangeFeed>(
    _r: &Rig<B>,
) {
    let backend = FakeBackend::new();
    let mut builder = RegistryBuilder::new();
    builder.add_controller(
        "probe",
        Arc::new(Tripwire {
            armed: Arc::new(AtomicBool::new(false)),
        }),
    );
    let engine = Engine::new(
        backend,
        meta(),
        Arc::new(builder.build().unwrap()),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap();
    engine.prepare().await.unwrap();
    let access = engine.secret_access(SecretCaller::Test, &inv());
    let id = RecordId::new_v7();
    assert_eq!(
        code_of(
            engine
                .set_secret(&access, "account", id, "password", s("x"), Version::FIRST)
                .await
        ),
        "secretfield.not_configured"
    );
    assert_eq!(
        code_of(engine.open_secret(&access, "account", id, "password").await),
        "secretfield.not_configured"
    );
    assert_eq!(
        code_of(
            engine
                .find_by_secret(&access, "account", "token", b"x")
                .await
        ),
        "secretfield.not_configured"
    );
    assert_eq!(
        code_of(
            engine
                .rewrap_secrets(&access, "account", RewrapBatch::first(1))
                .await
        ),
        "secretfield.not_configured"
    );
    let doc = spot_app_api::Document::new(n("account"), id);
    assert_eq!(
        code_of(
            engine
                .create_with_secrets(&access, doc, vec![("token".into(), s("x"))])
                .await
        ),
        "secretfield.not_configured"
    );
    assert!(
        engine.peek("account", id).await.unwrap().is_none(),
        "nothing was created"
    );
}

cases!(
    a_sealed_value_moved_elsewhere_does_not_open,
    a_value_sealed_under_another_key_does_not_open,
    without_a_vault_every_secret_call_fails_closed,
);
