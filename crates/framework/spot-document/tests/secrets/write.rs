//! Sealing on write: the value is sealed into its column in the same transaction as its record, and an ordinary write never touches or unsets it.

use crate::support::*;

async fn a_secret_round_trips<B: StorageBackend + ChangeFeed>(r: &Rig<B>) {
    let (id, v1) = r.account("ada").await;
    let access = r.access();
    assert_eq!(
        r.engine
            .open_secret(&access, "account", id, "password")
            .await
            .unwrap()
            .map(|s| s.expose().to_owned()),
        None,
        "never set"
    );
    assert!(
        !r.engine
            .secret_is_set(&inv(), "account", id, "password")
            .await
            .unwrap()
    );
    let v2 = r
        .engine
        .set_secret(&access, "account", id, "password", s(PASSWORD), v1)
        .await
        .unwrap();
    assert_eq!(v2, v1.next(), "the write bumps the version like any other");
    let opened = r
        .engine
        .open_secret(&access, "account", id, "password")
        .await
        .unwrap();
    assert_eq!(opened.unwrap().expose(), PASSWORD);
    assert!(
        r.engine
            .secret_is_set(&inv(), "account", id, "password")
            .await
            .unwrap()
    );
    // The other secrets are still unset.
    assert!(
        !r.engine
            .secret_is_set(&inv(), "account", id, "token")
            .await
            .unwrap()
    );
    assert!(
        r.engine
            .open_secret(&access, "account", id, "token")
            .await
            .unwrap()
            .is_none()
    );
    // And the engine's own load shows a document with no secret in it at all.
    let doc = r.engine.load(&inv(), "account", id).await.unwrap().value;
    assert_eq!(doc.version(), Some(v2));
    assert!(
        doc.fields()
            .keys()
            .all(|k| !k.as_str().contains("password"))
    );
}

async fn create_with_secrets_writes_the_record_and_its_secrets_together<
    B: StorageBackend + ChangeFeed,
>(
    r: &Rig<B>,
) {
    let access = r.access();
    let mut doc = spot_app_api::Document::new(n("account"), RecordId::new_v7());
    doc.set(n("label"), "bob");
    let id = doc.id();
    let saved = r
        .engine
        .create_with_secrets(
            &access,
            doc,
            vec![("password".into(), s(PASSWORD)), ("token".into(), s(TOKEN))],
        )
        .await
        .unwrap();
    assert_eq!(saved.version(), Some(Version::FIRST));
    assert_eq!(
        r.engine
            .open_secret(&access, "account", id, "password")
            .await
            .unwrap()
            .unwrap()
            .expose(),
        PASSWORD
    );
    assert_eq!(
        r.engine
            .find_by_secret(&access, "account", "token", TOKEN.as_bytes())
            .await
            .unwrap(),
        Some(id)
    );
    // A field given twice, and one the DocType does not have, are refused and write nothing.
    let doc = spot_app_api::Document::new(n("account"), RecordId::new_v7());
    let twice = vec![("password".into(), s("a")), ("password".into(), s("b"))];
    assert_eq!(
        code_of(r.engine.create_with_secrets(&access, doc, twice).await),
        "secretfield.unknown_field"
    );
    let doc = spot_app_api::Document::new(n("account"), RecordId::new_v7());
    let ghost = doc.id();
    assert_eq!(
        code_of(
            r.engine
                .create_with_secrets(&access, doc, vec![("label".into(), s("x"))])
                .await
        ),
        "secretfield.unknown_field",
        "a text field is not a secret field"
    );
    assert!(r.engine.peek("account", ghost).await.unwrap().is_none());
}

async fn the_same_plaintext_seals_differently_each_time<B: StorageBackend + ChangeFeed>(
    r: &Rig<B>,
) {
    let (a, va) = r.account("a").await;
    let (b, vb) = r.account("b").await;
    let access = r.access();
    r.engine
        .set_secret(&access, "account", a, "password", s(PASSWORD), va)
        .await
        .unwrap();
    r.engine
        .set_secret(&access, "account", b, "password", s(PASSWORD), vb)
        .await
        .unwrap();
    let (sa, sb) = (
        r.sealed(a, "password").await.unwrap(),
        r.sealed(b, "password").await.unwrap(),
    );
    assert_ne!(sa, sb, "randomised encryption");
    // The same record rewritten with the same text also changes.
    let va2 = r
        .engine
        .peek("account", a)
        .await
        .unwrap()
        .unwrap()
        .version()
        .unwrap();
    r.engine
        .set_secret(&access, "account", a, "password", s(PASSWORD), va2)
        .await
        .unwrap();
    assert_ne!(sa, r.sealed(a, "password").await.unwrap());
    // The index of equal values is equal: that is what makes it searchable.
    let (ta, tb) = (r.account("c").await, r.account("d").await);
    r.engine
        .set_secret(&access, "account", ta.0, "api_key", s(TOKEN), ta.1)
        .await
        .unwrap();
    r.engine
        .set_secret(&access, "account", tb.0, "api_key", s(TOKEN), tb.1)
        .await
        .unwrap();
    assert_eq!(r.bidx(ta.0, "api_key").await, r.bidx(tb.0, "api_key").await);
}

async fn a_rolled_back_transaction_leaves_no_secret_and_no_index<B: StorageBackend + ChangeFeed>(
    r: &Rig<B>,
) {
    let access = r.access();
    // set_secret: the write happens, then a step after it fails, so everything rolls back.
    let (id, v1) = r.account("rb").await;
    r.tripwire.store(true, Ordering::SeqCst);
    let err = r
        .engine
        .set_secret(&access, "account", id, "token", s(TOKEN), v1)
        .await
        .unwrap_err();
    assert_eq!(err.code, "probe.tripwire");
    r.tripwire.store(false, Ordering::SeqCst);
    assert!(r.sealed(id, "token").await.is_none(), "no sealed value");
    assert!(r.bidx(id, "token").await.is_none(), "no index");
    assert_eq!(
        r.engine
            .find_by_secret(&access, "account", "token", TOKEN.as_bytes())
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        r.engine
            .peek("account", id)
            .await
            .unwrap()
            .unwrap()
            .version(),
        Some(v1),
        "the version did not move"
    );
    // create_with_secrets: no record at all.
    let doc = spot_app_api::Document::new(n("account"), RecordId::new_v7());
    let ghost = doc.id();
    r.tripwire.store(true, Ordering::SeqCst);
    let err = r
        .engine
        .create_with_secrets(&access, doc, vec![("token".into(), s(TOKEN))])
        .await
        .unwrap_err();
    assert_eq!(err.code, "probe.tripwire");
    r.tripwire.store(false, Ordering::SeqCst);
    assert!(r.engine.peek("account", ghost).await.unwrap().is_none());
    assert_eq!(
        r.engine
            .find_by_secret(&access, "account", "token", TOKEN.as_bytes())
            .await
            .unwrap(),
        None
    );
    // And the audit log recorded both refusals.
    let failed = r
        .engine
        .secret_audit_log()
        .into_iter()
        .filter(|e| e.error.is_some())
        .count();
    assert_eq!(failed, 2);
}

async fn an_ordinary_write_keeps_every_secret<B: StorageBackend + ChangeFeed>(r: &Rig<B>) {
    let access = r.access();
    let (id, v1) = r.account("ed").await;
    let v2 = r
        .engine
        .set_secret(&access, "account", id, "password", s(PASSWORD), v1)
        .await
        .unwrap();
    let v3 = r
        .engine
        .set_secret(&access, "account", id, "token", s(TOKEN), v2)
        .await
        .unwrap();
    let before_pw = r.sealed(id, "password").await.unwrap();
    let before_tok = r.bidx(id, "token").await.unwrap();
    // An ordinary update, a second one, and a rename-like edit.
    let mut patch = DocumentPatch::default();
    patch
        .fields
        .insert(n("label"), Value::Text("edited".into()));
    let doc = r
        .engine
        .update(&inv(), "account", id, patch, v3)
        .await
        .unwrap()
        .value;
    let v4 = doc.version().unwrap();
    assert_eq!(v4, v3.next());
    assert_eq!(
        r.sealed(id, "password").await.unwrap(),
        before_pw,
        "the bytes did not move"
    );
    assert_eq!(r.bidx(id, "token").await.unwrap(), before_tok);
    assert_eq!(
        r.engine
            .open_secret(&access, "account", id, "password")
            .await
            .unwrap()
            .unwrap()
            .expose(),
        PASSWORD
    );
    assert_eq!(
        r.engine
            .find_by_secret(&access, "account", "token", TOKEN.as_bytes())
            .await
            .unwrap(),
        Some(id)
    );
}

async fn an_ordinary_post_and_cancel_keep_every_secret<B: StorageBackend + ChangeFeed>(r: &Rig<B>) {
    let access = r.access();
    let (id, v1) = r.account("pc").await;
    let v2 = r
        .engine
        .set_secret(&access, "account", id, "password", s(PASSWORD), v1)
        .await
        .unwrap();
    let v3 = r
        .engine
        .set_secret(&access, "account", id, "token", s(TOKEN), v2)
        .await
        .unwrap();
    let sealed_pw = r.sealed(id, "password").await.unwrap();
    let sealed_tok = r.sealed(id, "token").await.unwrap();
    let bidx_tok = r.bidx(id, "token").await.unwrap();
    // The write of a posting and the write of a cancellation each carry the sealed columns
    // over unchanged (a full-row write must not unset a credential).
    let posted = r
        .engine
        .post(&inv(), "account", id, v3)
        .await
        .unwrap()
        .value;
    assert_eq!(posted.state(), Some(spot_app_api::DocState::Posted));
    let v4 = posted.version().unwrap();
    for stage in ["posted", "cancelled"] {
        assert_eq!(
            r.sealed(id, "password").await.unwrap(),
            sealed_pw,
            "{stage}"
        );
        assert_eq!(r.sealed(id, "token").await.unwrap(), sealed_tok, "{stage}");
        assert_eq!(r.bidx(id, "token").await.unwrap(), bidx_tok, "{stage}");
        assert_eq!(
            r.engine
                .open_secret(&access, "account", id, "password")
                .await
                .unwrap()
                .unwrap()
                .expose(),
            PASSWORD,
            "{stage}"
        );
        assert_eq!(
            r.engine
                .find_by_secret(&access, "account", "token", TOKEN.as_bytes())
                .await
                .unwrap(),
            Some(id),
            "{stage}"
        );
        if stage == "posted" {
            let cancelled = r.engine.cancel(&inv(), "account", id, v4).await.unwrap();
            assert_eq!(
                cancelled.value.state(),
                Some(spot_app_api::DocState::Cancelled)
            );
        }
    }
}

async fn a_secret_in_an_ordinary_patch_or_create_is_refused<B: StorageBackend + ChangeFeed>(
    r: &Rig<B>,
) {
    let (id, v1) = r.account("p").await;
    let mut patch = DocumentPatch::default();
    patch
        .fields
        .insert(n("password"), Value::Text(PASSWORD.into()));
    let err = r
        .engine
        .update(&inv(), "account", id, patch, v1)
        .await
        .unwrap_err();
    assert!(
        format!("{err:?}").contains("document.invalid_value"),
        "{err:?}"
    );
    let shown = format!("{err:?}");
    assert_absent(shown.as_bytes(), &forms_of_plain(PASSWORD), "the refusal");
    let mut doc = spot_app_api::Document::new(n("account"), RecordId::new_v7());
    doc.set(n("token"), TOKEN);
    let err = r.engine.create(&inv(), doc).await.unwrap_err();
    assert!(
        format!("{err:?}").contains("document.invalid_value"),
        "{err:?}"
    );
    let shown = format!("{err:?}");
    assert_absent(shown.as_bytes(), &forms_of_plain(TOKEN), "the refusal");
    assert!(r.sealed(id, "password").await.is_none());
}

async fn a_stale_version_is_refused_and_concurrent_sets_serialise<
    B: StorageBackend + ChangeFeed,
>(
    r: &Rig<B>,
) {
    let access = r.access();
    let (id, v1) = r.account("cc").await;
    let v2 = r
        .engine
        .set_secret(&access, "account", id, "password", s("one"), v1)
        .await
        .unwrap();
    let err = r
        .engine
        .set_secret(&access, "account", id, "password", s("two"), v1)
        .await
        .unwrap_err();
    assert_eq!(err.code, "document.stale_version");
    assert_eq!(
        r.engine
            .open_secret(&access, "account", id, "password")
            .await
            .unwrap()
            .unwrap()
            .expose(),
        "one",
        "the refused write changed nothing"
    );
    // Two writers at the same version: exactly one wins; a reader sees one value or the other,
    // never a mixture.
    let (a, b) = tokio::join!(
        r.engine
            .set_secret(&access, "account", id, "password", s("alpha"), v2),
        r.engine
            .set_secret(&access, "account", id, "password", s("beta"), v2),
    );
    let wins = [&a, &b].iter().filter(|x| x.is_ok()).count();
    assert_eq!(wins, 1, "{a:?} {b:?}");
    let loser = if a.is_err() { a } else { b };
    assert_eq!(loser.unwrap_err().code, "document.stale_version");
    let now = r
        .engine
        .open_secret(&access, "account", id, "password")
        .await
        .unwrap()
        .unwrap();
    assert!(["alpha", "beta"].contains(&now.expose()));
}

async fn a_posted_or_unknown_target_and_a_missing_record_are_refused<
    B: StorageBackend + ChangeFeed,
>(
    r: &Rig<B>,
) {
    let access = r.access();
    let ghost = RecordId::new_v7();
    assert_eq!(
        code_of(
            r.engine
                .set_secret(
                    &access,
                    "account",
                    ghost,
                    "password",
                    s("x"),
                    Version::FIRST
                )
                .await
        ),
        "document.not_found"
    );
    assert_eq!(
        code_of(
            r.engine
                .open_secret(&access, "account", ghost, "password")
                .await
        ),
        "document.not_found"
    );
    assert_eq!(
        code_of(
            r.engine
                .open_secret(&access, "ghost", ghost, "password")
                .await
        ),
        "meta.unknown_doctype"
    );
    let (id, v1) = r.account("x").await;
    assert_eq!(
        code_of(
            r.engine
                .set_secret(&access, "account", id, "label", s("x"), v1)
                .await
        ),
        "secretfield.unknown_field"
    );
    assert_eq!(
        code_of(
            r.engine
                .set_secret(&access, "crate_line", id, "what", s("x"), v1)
                .await
        ),
        "secretfield.unsupported_doctype"
    );
}

cases!(
    a_secret_round_trips,
    create_with_secrets_writes_the_record_and_its_secrets_together,
    the_same_plaintext_seals_differently_each_time,
    a_rolled_back_transaction_leaves_no_secret_and_no_index,
    an_ordinary_write_keeps_every_secret,
    an_ordinary_post_and_cancel_keep_every_secret,
    a_secret_in_an_ordinary_patch_or_create_is_refused,
    a_stale_version_is_refused_and_concurrent_sets_serialise,
    a_posted_or_unknown_target_and_a_missing_record_are_refused,
);
