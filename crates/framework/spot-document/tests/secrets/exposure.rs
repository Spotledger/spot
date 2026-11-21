//! Never exposed: the plain value and its sealed bytes are in no stored column, document, event, audit entry or error.

use crate::support::*;

async fn the_plain_value_is_nowhere_in_the_table<B: StorageBackend + ChangeFeed>(r: &Rig<B>) {
    let (id, v1) = r.account("cy").await;
    let access = r.access();
    let v2 = r
        .engine
        .set_secret(&access, "account", id, "password", s(PASSWORD), v1)
        .await
        .unwrap();
    r.engine
        .set_secret(&access, "account", id, "token", s(TOKEN), v2)
        .await
        .unwrap();
    let mut needles = forms_of_plain(PASSWORD);
    needles.extend(forms_of_plain(TOKEN));
    let sealed_pw = r.sealed(id, "password").await.expect("sealed");
    let sealed_tok = r.sealed(id, "token").await.expect("sealed");
    assert!(
        sealed_pw.len() > PASSWORD.len(),
        "it is an envelope, not the text"
    );
    let all = r.everything(&[id], &[]).await;
    assert_absent(
        &all,
        &needles,
        "the stored row, documents, events or audit log",
    );
    // The index is a keyed MAC: it is not the value either.
    let bidx = r
        .bidx(id, "token")
        .await
        .expect("a lookup field has an index");
    assert_eq!(bidx.len(), 33);
    assert!(
        r.bidx(id, "password").await.is_none(),
        "no index on a non-lookup field"
    );
    assert_absent(&bidx, &needles, "the blind index");
    // The stored form of the sealed value is not shown in events, documents or audit log either.
    let mut sealed = forms_of_sealed(&sealed_pw);
    sealed.extend(forms_of_sealed(&sealed_tok));
    let shown = r.everything(&[id], &[]).await;
    // The stored row legitimately holds the sealed bytes; everything else must not. Rebuild the
    // haystack without the raw row.
    let events = r
        .engine
        .backend()
        .read_outbox(FeedPosition::START, 10_000)
        .await
        .unwrap();
    let mut outside = Vec::new();
    for record in events {
        outside.extend_from_slice(format!("{:?}", record.event).as_bytes());
        outside.extend_from_slice(record.event.payload.as_bytes());
    }
    outside.extend_from_slice(format!("{:?}", r.engine.secret_audit_log()).as_bytes());
    outside.extend_from_slice(
        format!("{:?}", r.engine.load(&inv(), "account", id).await.unwrap()).as_bytes(),
    );
    outside
        .extend_from_slice(format!("{:?}", r.engine.peek("account", id).await.unwrap()).as_bytes());
    assert_absent(&outside, &sealed, "an event, the audit log or a document");
    // The scan is not vacuous: the full haystack does hold the stored sealed bytes (the row is
    // read), and the one that excludes the row does hold the events and the audit log.
    assert!(
        contains(&shown, &sealed_pw) && contains(&shown, &sealed_tok),
        "the scan sees the stored sealed bytes"
    );
    assert!(
        contains(&outside, b"changed_fields") && contains(&outside, b"password"),
        "the scan sees the events and the audit log"
    );
}

async fn the_event_and_audit_name_the_field_and_never_the_value<B: StorageBackend + ChangeFeed>(
    r: &Rig<B>,
) {
    let access = r.access();
    let (id, v1) = r.account("ev").await;
    let v2 = r
        .engine
        .set_secret(&access, "account", id, "password", s(PASSWORD), v1)
        .await
        .unwrap();
    let events = r
        .engine
        .backend()
        .read_outbox(FeedPosition::START, 100)
        .await
        .unwrap();
    // The lifecycle event, not the audit event written after it in the same transaction.
    let last = events
        .iter()
        .rev()
        .map(|record| record.event.clone())
        .find(|e| !matches!(e.topic.as_str(), "sys_secret_audit" | "doc_state"))
        .unwrap();
    assert_eq!(last.subject_version, Some(v2));
    let payload: serde_json::Value = serde_json::from_str(&last.payload).unwrap();
    let changed = &payload["data"]["changed_fields"];
    assert_eq!(
        changed,
        &serde_json::json!(["password"]),
        "the name, never the value"
    );
    let audit = r.engine.secret_audit_log();
    let entry = audit.last().unwrap();
    assert_eq!((entry.op, entry.error.clone()), (SecretOp::Set, None));
    assert_eq!((entry.field.as_str(), entry.state), ("password", "<set>"));
    assert_eq!(entry.caller, SecretCaller::Test);
    assert_eq!(entry.site, SITE);
    // A refused use is on the log too, with its code.
    let _ = r
        .engine
        .find_by_secret(&access, "account", "password", b"x")
        .await;
    let audit = r.engine.secret_audit_log();
    assert_eq!(
        audit.last().unwrap().error.as_deref(),
        Some("secretfield.not_lookup")
    );
    // The Debug of the capability and of the vault errors shows no secret.
    assert!(!format!("{access:?}").contains(PASSWORD));
}

cases!(
    the_plain_value_is_nowhere_in_the_table,
    the_event_and_audit_name_the_field_and_never_the_value,
);

// ---------------------------------------------------------------------------------------------
// Property: any non-empty text round-trips and is found
// ---------------------------------------------------------------------------------------------

mod props {
    use super::*;
    use proptest::prelude::*;

    /// Sets `text` as a lookup secret, reads it back, finds it, and (when `scan`) checks that no
    /// form of it is in anything the engine and storage could show.
    async fn round_trip<B: StorageBackend + ChangeFeed>(rig: &Rig<B>, text: &str, scan: bool) {
        let access = rig.access();
        let (id, v) = rig.account("prop").await;
        rig.engine
            .set_secret(
                &access,
                "account",
                id,
                "api_key",
                SecretString::new(text),
                v,
            )
            .await
            .unwrap();
        let opened = rig
            .engine
            .open_secret(&access, "account", id, "api_key")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(opened.expose(), text);
        let found = rig
            .engine
            .find_by_secret(&access, "account", "api_key", text.as_bytes())
            .await
            .unwrap();
        assert_eq!(found, Some(id));
        if scan {
            let all = rig.everything(&[id], &[]).await;
            assert_absent(&all, &forms_of_plain(text), "the row, events or audit");
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]

        #[test]
        fn any_text_round_trips_and_is_found_and_never_shows(text in "\\PC{1,200}") {
            runtime().block_on(async {
                let rig = rig_fake().await.unwrap();
                // A one-character text can occur in a field name by chance, so only texts of a
                // length that cannot are scanned for here; the short ones follow below.
                round_trip(&rig, &text, text.len() >= 12).await;
            });
        }

        /// Values of 4 to 11 characters, which the test above cannot scan for. They are drawn
        /// from Cyrillic letters, which no field name, event topic or stored text of the suite
        /// holds, so a hit is the value itself.
        #[test]
        fn short_values_round_trip_and_never_show(text in "[\\u{0400}-\\u{04FF}]{4,11}") {
            runtime().block_on(async {
                let rig = rig_fake().await.unwrap();
                round_trip(&rig, &text, true).await;
            });
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(6))]

        /// The same on PostgreSQL, when `SPOT_TEST_PG_URL` is set.
        #[test]
        fn on_postgres_any_text_and_short_values_round_trip_and_never_show(
            long in "\\PC{12,80}",
            short in "[\\u{0400}-\\u{04FF}]{4,11}",
        ) {
            runtime().block_on(async {
                let Some(rig) = rig_pg().await else { return };
                round_trip(&rig, &long, true).await;
                round_trip(&rig, &short, true).await;
            });
        }
    }
}
