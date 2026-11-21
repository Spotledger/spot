//! The audit journal (WP-SEC-03.T4): every secret operation is an event in the outbox, written
//! with the operation or not at all; a rotation does not log anyone out; a rewrap is visible to
//! replicas.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicU32;

use crate::flaky::{Fault, FlakyBackend};
use crate::support::*;
use spot_document::secrets::{SecretVault, VaultFuture};
use spot_secrets::SecretError;

const TOPIC: &str = "sys_secret_audit";

/// The outbox events of the audit topic, as JSON text (payload and all).
async fn audit_events<B: StorageBackend + ChangeFeed>(r: &Rig<B>) -> Vec<String> {
    r.engine
        .backend()
        .read_outbox(FeedPosition::START, 100_000)
        .await
        .unwrap()
        .into_iter()
        .filter(|record| record.event.topic.as_str() == TOPIC)
        .map(|record| record.event.payload)
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The journal
// ---------------------------------------------------------------------------------------------

async fn every_secret_operation_is_journalled_and_shows_no_value<B: StorageBackend + ChangeFeed>(
    r: &Rig<B>,
) {
    let access = r.access();
    let (id, v1) = r.account("j").await;
    let v2 = r
        .engine
        .set_secret(&access, "account", id, "token", s(TOKEN), v1)
        .await
        .unwrap();
    let _ = v2;
    let mut created = spot_app_api::Document::new(n("account"), RecordId::new_v7());
    created.set(n("label"), "created");
    let created_id = created.id();
    r.engine
        .create_with_secrets(
            &access,
            created,
            vec![
                ("password".into(), s(PASSWORD)),
                ("api_key".into(), s("k-1")),
            ],
        )
        .await
        .unwrap();
    r.engine
        .open_secret(&access, "account", id, "token")
        .await
        .unwrap();
    r.engine
        .open_secret(&access, "account", id, "password")
        .await
        .unwrap();
    r.engine
        .find_by_secret(&access, "account", "token", TOKEN.as_bytes())
        .await
        .unwrap();
    // A refused find and a refused set are journalled with their codes.
    let _ = r
        .engine
        .find_by_secret(&access, "account", "password", b"x")
        .await;
    let _ = r
        .engine
        .set_secret(
            &access,
            "account",
            RecordId::new_v7(),
            "password",
            s("x"),
            Version::FIRST,
        )
        .await;
    r.engine
        .rewrap_secrets(&access, "account", RewrapBatch::first(10))
        .await
        .unwrap();

    let journal = r.engine.secret_audit_journal(&access).await.unwrap();
    let shape: Vec<_> = journal
        .iter()
        .map(|e| {
            (
                e.op,
                e.field.as_str(),
                e.id.is_some(),
                e.error.as_deref(),
                e.state,
            )
        })
        .collect();
    assert_eq!(
        shape,
        [
            (SecretOp::Set, "token", true, None, "<set>"),
            (SecretOp::Set, "password", true, None, "<set>"),
            (SecretOp::Set, "api_key", true, None, "<set>"),
            (SecretOp::Open, "token", true, None, "<set>"),
            (SecretOp::Open, "password", true, None, "<unset>"),
            (SecretOp::Find, "token", false, None, "<set>"),
            (
                SecretOp::Find,
                "password",
                false,
                Some("secretfield.not_lookup"),
                "<none>"
            ),
            (
                SecretOp::Set,
                "password",
                true,
                Some("document.not_found"),
                "<none>"
            ),
            (SecretOp::Rewrap, "*", false, None, "<set>"),
        ]
    );
    for entry in &journal {
        assert_eq!(entry.caller, SecretCaller::Test);
        assert_eq!(entry.actor, "system:login_service");
        assert_eq!(entry.site, SITE);
        assert_eq!(entry.doctype, "account");
    }
    assert_eq!(journal[1].id, Some(created_id));

    // No value, sealed blob or index in any audit event, whatever their form.
    let text = audit_events(r).await.join("\n").into_bytes();
    let mut forms = forms_of_plain(PASSWORD);
    forms.extend(forms_of_plain(TOKEN));
    forms.extend(forms_of_plain("k-1"));
    for (rec, field) in [(id, "token"), (created_id, "password")] {
        forms.extend(forms_of_sealed(&r.sealed(rec, field).await.unwrap()));
    }
    let index = r.bidx(id, "token").await.unwrap();
    forms.push(index.clone());
    forms.push(hex(&index).into_bytes());
    assert_absent(&text, &forms, "the audit journal");
    assert!(
        contains(&text, b"login_service"),
        "the scan sees the events"
    );
}

/// The journal is append-only for a capability holder: no number of cheap calls pushes an
/// earlier entry out.
#[tokio::test]
async fn five_thousand_finds_leave_every_earlier_entry_intact() {
    let r = rig_fake().await.unwrap();
    let access = r.access();
    let (id, v) = r.account("evict").await;
    r.engine
        .set_secret(&access, "account", id, "token", s(TOKEN), v)
        .await
        .unwrap();
    r.engine
        .open_secret(&access, "account", id, "token")
        .await
        .unwrap();
    let before = r.engine.secret_audit_journal(&access).await.unwrap();
    assert_eq!(before.len(), 2);

    for i in 0..5_000 {
        let _ = r
            .engine
            .find_by_secret(&access, "account", "token", format!("probe-{i}").as_bytes())
            .await
            .unwrap();
    }

    let after = r.engine.secret_audit_journal(&access).await.unwrap();
    assert_eq!(after.len(), 5_002, "every call has its row");
    assert_eq!(
        &after[..2],
        &before[..],
        "the earlier entries are untouched"
    );
    assert!(after[2..].iter().all(|e| e.op == SecretOp::Find));
    // The in-process view is a bounded diagnostic; the journal is the record.
    assert_eq!(r.engine.secret_audit_log().len(), 1024);
}

/// A call whose audit row cannot be written does not happen: the value is not returned.
#[tokio::test]
async fn a_failed_audit_write_fails_the_call_closed() {
    use spot_storage_contract::ErrorKind;

    let keyring = keyring();
    let r = build(FlakyBackend::new(FakeBackend::new()), keyring, None);
    r.engine.prepare().await.unwrap();
    let access = r.access();
    let (id, v) = r.account("closed").await;
    r.engine
        .set_secret(&access, "account", id, "token", s(TOKEN), v)
        .await
        .unwrap();

    // The commit of the audit row fails, past the retry budget.
    for _ in 0..4 {
        r.engine
            .backend()
            .inject(Fault::rolled_back(ErrorKind::Unavailable));
    }
    let opened = r.engine.open_secret(&access, "account", id, "token").await;
    assert_eq!(code_of(opened), "secretfield.audit_failed");

    for _ in 0..4 {
        r.engine
            .backend()
            .inject(Fault::rolled_back(ErrorKind::Unavailable));
    }
    let found = r
        .engine
        .find_by_secret(&access, "account", "token", TOKEN.as_bytes())
        .await;
    assert_eq!(code_of(found), "secretfield.audit_failed");

    // A refusal that cannot be recorded is not passed off as a plain refusal either.
    for _ in 0..4 {
        r.engine
            .backend()
            .inject(Fault::rolled_back(ErrorKind::Unavailable));
    }
    let refused = r
        .engine
        .find_by_secret(&access, "account", "password", b"x")
        .await;
    assert_eq!(code_of(refused), "secretfield.audit_failed");

    // No row was written for any of them; the calls after the outage work and are journalled.
    let before = r.engine.secret_audit_journal(&access).await.unwrap().len();
    assert_eq!(before, 1, "only the set");
    r.engine
        .open_secret(&access, "account", id, "token")
        .await
        .unwrap();
    assert_eq!(
        r.engine.secret_audit_journal(&access).await.unwrap().len(),
        2
    );

    // A set whose transaction cannot commit leaves neither the secret nor its row.
    let (other, vo) = r.account("closed-set").await;
    for _ in 0..4 {
        r.engine
            .backend()
            .inject(Fault::rolled_back(ErrorKind::Unavailable));
    }
    let set = r
        .engine
        .set_secret(&access, "account", other, "password", s(PASSWORD), vo)
        .await;
    assert!(set.is_err());
    assert!(r.sealed(other, "password").await.is_none());
    // The attempt that did not happen is journalled as a refusal.
    let journal = r.engine.secret_audit_journal(&access).await.unwrap();
    assert_eq!(journal.len(), 3);
    let last = journal.last().unwrap();
    assert_eq!((last.op, last.state), (SecretOp::Set, "<none>"));
    assert!(last.error.is_some());
}

// ---------------------------------------------------------------------------------------------
// The lookup window of a rotation
// ---------------------------------------------------------------------------------------------

/// The real vault, counting how often the previous key version is consulted.
struct Counting {
    inner: SecretBox<Arc<MemoryKeyring>>,
    previous: Arc<AtomicU32>,
}

impl SecretVault for Counting {
    fn seal<'a>(&'a self, plain: &'a SecretString, aad: &'a [u8]) -> VaultFuture<'a, Vec<u8>> {
        SecretVault::seal(&self.inner, plain, aad)
    }

    fn open<'a>(&'a self, sealed: &'a [u8], aad: &'a [u8]) -> VaultFuture<'a, SecretString> {
        SecretVault::open(&self.inner, sealed, aad)
    }

    fn rewrap<'a>(&'a self, sealed: &'a [u8], aad: &'a [u8]) -> VaultFuture<'a, Vec<u8>> {
        SecretVault::rewrap(&self.inner, sealed, aad)
    }

    fn needs_rewrap(&self, sealed: &[u8]) -> Result<bool, SecretError> {
        SecretVault::needs_rewrap(&self.inner, sealed)
    }

    fn blind_index<'a>(&'a self, scope: &'a str, input: &'a [u8]) -> VaultFuture<'a, Vec<u8>> {
        SecretVault::blind_index(&self.inner, scope, input)
    }

    fn needs_reindex(&self, index: &[u8]) -> Result<bool, SecretError> {
        SecretVault::needs_reindex(&self.inner, index)
    }

    fn current_key_version(&self) -> u32 {
        SecretVault::current_key_version(&self.inner)
    }

    fn blind_index_previous<'a>(
        &'a self,
        scope: &'a str,
        input: &'a [u8],
    ) -> VaultFuture<'a, Option<Vec<u8>>> {
        self.previous.fetch_add(1, Ordering::SeqCst);
        SecretVault::blind_index_previous(&self.inner, scope, input)
    }
}

async fn counting_rig() -> (Rig<FakeBackend>, Arc<AtomicU32>) {
    let keyring = keyring();
    let previous = Arc::new(AtomicU32::new(0));
    let vault = Arc::new(Counting {
        inner: SecretBox::new(keyring.clone()),
        previous: previous.clone(),
    });
    let rig = build_with(FakeBackend::new(), keyring, vault, None);
    rig.engine.prepare().await.unwrap();
    (rig, previous)
}

async fn find_all(r: &Rig<FakeBackend>, ids: &[RecordId]) -> Vec<Option<RecordId>> {
    let access = r.access();
    let mut out = Vec::new();
    for i in 0..ids.len() {
        out.push(
            r.engine
                .find_by_secret(
                    &access,
                    "account",
                    "token",
                    format!("{TOKEN}{i}").as_bytes(),
                )
                .await
                .unwrap(),
        );
    }
    out
}

async fn four_tokens(r: &Rig<FakeBackend>) -> Vec<RecordId> {
    let access = r.access();
    let mut ids = Vec::new();
    for i in 0..4 {
        let (id, v) = r.account(&format!("w{i}")).await;
        r.engine
            .set_secret(
                &access,
                "account",
                id,
                "token",
                s(&format!("{TOKEN}{i}")),
                v,
            )
            .await
            .unwrap();
        ids.push(id);
    }
    ids
}

#[tokio::test]
async fn a_rotation_does_not_log_anyone_out_and_the_window_closes_with_the_rewrap() {
    let (r, previous) = counting_rig().await;
    let access = r.access();
    let ids = four_tokens(&r).await;
    let found: Vec<_> = ids.iter().copied().map(Some).collect();
    assert_eq!(find_all(&r, &ids).await, found);
    previous.store(0, Ordering::SeqCst);

    // Rotate: every stored index is now under the previous version.
    assert_eq!(r.keyring.rotate().unwrap(), 2);
    assert_eq!(find_all(&r, &ids).await, found, "found before any rewrap");
    assert!(
        previous.load(Ordering::SeqCst) > 0,
        "through the previous version"
    );
    // A value nobody holds is still not found.
    assert_eq!(
        r.engine
            .find_by_secret(&access, "account", "token", b"nobody")
            .await
            .unwrap(),
        None
    );

    // Rewrap half: two records are on the new version, two are not; all four are found.
    let half = r
        .engine
        .rewrap_secrets(&access, "account", RewrapBatch::first(2))
        .await
        .unwrap();
    assert_eq!((half.reindexed, half.next.is_some()), (2, true));
    assert_eq!(find_all(&r, &ids).await, found, "found while half-way");

    // Finish.
    let rest = r
        .engine
        .rewrap_secrets(
            &access,
            "account",
            RewrapBatch {
                size: 10,
                after: half.next,
            },
        )
        .await
        .unwrap();
    assert_eq!((rest.reindexed, rest.next), (2, None));
    previous.store(0, Ordering::SeqCst);
    assert_eq!(find_all(&r, &ids).await, found, "found after the rewrap");
    assert_eq!(
        previous.load(Ordering::SeqCst),
        0,
        "the previous version is no longer consulted"
    );
    // So the old key can be retired without a lookup noticing.
    r.keyring.retire(1, false).unwrap();
    assert_eq!(find_all(&r, &ids).await, found);

    // A second rotation opens the window again, for the new previous version.
    assert_eq!(r.keyring.rotate().unwrap(), 3);
    assert_eq!(find_all(&r, &ids).await, found);
    assert!(previous.load(Ordering::SeqCst) > 0);
}

/// The site is part of the blind-index input, so a clean sweep under one site proves nothing
/// about another's records: it must not close the other site's window (WP-SEC-03.T7, F2).
#[tokio::test]
async fn a_clean_sweep_on_one_site_does_not_close_the_window_of_another() {
    const OTHER: &str = "other.test";
    let (r, previous) = counting_rig().await;
    let a = r.access();
    let b = r.access_at(OTHER);
    let ids = four_tokens(&r).await;
    r.keyring.rotate().unwrap();
    // The other site's record is written under the new version, so site A's sweep leaves it
    // alone (it needs no move) and the sweep is clean.
    let (b_id, b_version) = r.account("other-site").await;
    r.engine
        .set_secret(&b, "account", b_id, "token", s("tok-of-b"), b_version)
        .await
        .unwrap();

    let full = r
        .engine
        .rewrap_secrets(&a, "account", RewrapBatch::first(10))
        .await
        .unwrap();
    assert_eq!((full.next, full.failed), (None, 0), "a clean sweep on A");

    // Site A's window is closed: its lookups no longer ask for the previous version.
    previous.store(0, Ordering::SeqCst);
    let found: Vec<_> = ids.iter().copied().map(Some).collect();
    assert_eq!(find_all(&r, &ids).await, found);
    assert_eq!(previous.load(Ordering::SeqCst), 0, "A's window is closed");

    // Site B has not swept: its window is still open.
    previous.store(0, Ordering::SeqCst);
    let hit = r
        .engine
        .find_by_secret(&b, "account", "token", b"tok-of-b")
        .await
        .unwrap();
    assert_eq!(hit, Some(b_id));
    assert!(
        previous.load(Ordering::SeqCst) > 0,
        "A's sweep must not close B's window"
    );

    // B's own clean sweep closes it.
    let swept = r
        .engine
        .rewrap_secrets(&b, "account", RewrapBatch::first(10))
        .await
        .unwrap();
    assert_eq!((swept.next, swept.failed), (None, 0));
    previous.store(0, Ordering::SeqCst);
    r.engine
        .find_by_secret(&b, "account", "token", b"tok-of-b")
        .await
        .unwrap();
    assert_eq!(previous.load(Ordering::SeqCst), 0, "B's window is closed");
}

#[tokio::test]
async fn a_sweep_that_skipped_records_or_failed_does_not_close_the_window() {
    let (r, previous) = counting_rig().await;
    let access = r.access();
    let ids = four_tokens(&r).await;
    r.keyring.rotate().unwrap();

    // Starting after the first record is not a sweep of the DocType.
    let tail = r
        .engine
        .rewrap_secrets(
            &access,
            "account",
            RewrapBatch {
                size: 10,
                after: Some(ids.iter().copied().min().unwrap()),
            },
        )
        .await
        .unwrap();
    assert_eq!(tail.next, None);
    previous.store(0, Ordering::SeqCst);
    let found: Vec<_> = ids.iter().copied().map(Some).collect();
    assert_eq!(find_all(&r, &ids).await, found);
    assert!(
        previous.load(Ordering::SeqCst) > 0,
        "the window stays open until a sweep from the start completes"
    );

    // One that completes from the start closes it.
    let full = r
        .engine
        .rewrap_secrets(&access, "account", RewrapBatch::first(10))
        .await
        .unwrap();
    assert_eq!((full.next, full.failed), (None, 0));
    previous.store(0, Ordering::SeqCst);
    assert_eq!(find_all(&r, &ids).await, found);
    assert_eq!(previous.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_sweep_with_a_failed_value_leaves_the_window_open() {
    let (r, previous) = counting_rig().await;
    let access = r.access();
    let ids = four_tokens(&r).await;
    r.keyring.rotate().unwrap();
    r.poke(
        ids[0],
        "token__sealed",
        Value::Bytes(b"SPS1 garbage".to_vec()),
    )
    .await;
    let report = r
        .engine
        .rewrap_secrets(&access, "account", RewrapBatch::first(10))
        .await
        .unwrap();
    assert_eq!((report.next, report.failed), (None, 1));
    previous.store(0, Ordering::SeqCst);
    // The three good records are found on the current version; the window stays open for the
    // one that could not be moved.
    let found = find_all(&r, &ids).await;
    assert_eq!(found[1..], [Some(ids[1]), Some(ids[2]), Some(ids[3])]);
    assert!(previous.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn an_index_two_versions_old_is_not_found() {
    let (r, _previous) = counting_rig().await;
    let ids = four_tokens(&r).await;
    r.keyring.rotate().unwrap();
    r.keyring.rotate().unwrap();
    assert_eq!(
        find_all(&r, &ids).await,
        [None, None, None, None],
        "only the current and the previous version are asked: the lookup fails closed"
    );
}

// ---------------------------------------------------------------------------------------------
// A rewrap is an update for replicas
// ---------------------------------------------------------------------------------------------

async fn a_rewrap_emits_the_update_event_and_keeps_the_child_rows<
    B: StorageBackend + ChangeFeed,
>(
    r: &Rig<B>,
) {
    let access = r.access();
    let id = RecordId::new_v7();
    let mut doc = spot_app_api::Document::new(n("vault_box"), id);
    doc.set(n("label"), "box");
    for what in ["first", "second"] {
        doc.rows_mut(n("entries")).push(spot_app_api::ChildRow {
            id: RecordId::new_v7(),
            fields: BTreeMap::from([(n("what"), Value::Text(what.into()))]),
        });
    }
    let saved = r
        .engine
        .create_with_secrets(&access, doc, vec![("token".into(), s(TOKEN))])
        .await
        .unwrap();
    let v1 = saved.version().unwrap();

    r.keyring.rotate().unwrap();
    let report = r
        .engine
        .rewrap_secrets(&access, "vault_box", RewrapBatch::first(10))
        .await
        .unwrap();
    assert_eq!(
        (report.rewrapped, report.reindexed, report.failed),
        (1, 1, 0)
    );

    // The record is one version on, its rows are intact, and the secret still opens.
    let after = r.engine.peek("vault_box", id).await.unwrap().unwrap();
    let v2 = after.version().unwrap();
    assert_ne!(v2, v1);
    let rows: Vec<_> = after
        .rows("entries")
        .iter()
        .map(|row| row.fields.get(&n("what")).cloned())
        .collect();
    assert_eq!(
        rows,
        [
            Some(Value::Text("first".into())),
            Some(Value::Text("second".into()))
        ],
        "the child rows survive a rewrap"
    );
    assert_eq!(
        r.engine
            .open_secret(&access, "vault_box", id, "token")
            .await
            .unwrap()
            .unwrap()
            .expose(),
        TOKEN
    );

    // The lifecycle event of an update of the sealed column: names only, at the new version.
    let events = r
        .engine
        .backend()
        .read_outbox(FeedPosition::START, 1000)
        .await
        .unwrap();
    let updates: Vec<_> = events
        .iter()
        .map(|record| &record.event)
        .filter(|e| e.subject.id == id && e.topic.as_str() == Operation::Update.topic_name())
        .collect();
    assert_eq!(
        updates.len(),
        1,
        "the rewrap's update (the create is another topic)"
    );
    assert_eq!(updates[0].subject_version, Some(v2));
    let payload: serde_json::Value = serde_json::from_str(&updates[0].payload).unwrap();
    assert_eq!(
        payload["data"]["changed_fields"],
        serde_json::json!(["token"])
    );
    let shown = updates[0].payload.as_bytes();
    assert_absent(shown, &forms_of_plain(TOKEN), "the rewrap event");
    assert_absent(
        shown,
        &forms_of_sealed(&sealed_of(r, id).await),
        "the rewrap event",
    );
    // And it is in the same transaction as the batch's audit row.
    let in_tx: Vec<_> = events
        .iter()
        .filter(|record| {
            record.event.subject.id == id
                && record.event.topic.as_str() == Operation::Update.topic_name()
        })
        .map(|record| record.transaction)
        .collect();
    let audit_tx: Vec<_> = events
        .iter()
        .filter(|record| record.event.topic.as_str() == TOPIC)
        .filter(|record| record.event.payload.contains("\"rewrap\""))
        .map(|record| record.transaction)
        .collect();
    assert_eq!(in_tx.len(), 1);
    assert!(audit_tx.contains(&in_tx[0]));

    // A second pass has nothing to do, so emits nothing.
    let again = r
        .engine
        .rewrap_secrets(&access, "vault_box", RewrapBatch::first(10))
        .await
        .unwrap();
    assert_eq!((again.rewrapped, again.reindexed), (0, 0));
    let count = r
        .engine
        .backend()
        .read_outbox(FeedPosition::START, 1000)
        .await
        .unwrap()
        .iter()
        .filter(|record| record.event.topic.as_str() == Operation::Update.topic_name())
        .count();
    assert_eq!(count, 1);
}

async fn sealed_of<B: StorageBackend + ChangeFeed>(r: &Rig<B>, id: RecordId) -> Vec<u8> {
    let mut tx = r
        .engine
        .backend()
        .begin(
            catalog(),
            TxOptions::new(n("raw_read"), Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    let row = tx
        .load(&n("vault_box"), id, RowLock::None)
        .await
        .unwrap()
        .unwrap();
    tx.rollback().await.unwrap();
    match row.data.fields.get(&n("token__sealed")) {
        Some(Value::Bytes(b)) => b.clone(),
        other => panic!("no sealed column: {other:?}"),
    }
}

/// The journal is read with the capability and shows only its own site's rows (WP-SEC-03.T7,
/// F4). A caller with no capability cannot call it at all: `secret_audit_journal` takes a
/// `SecretAccess`, which only `Engine::secret_access` makes.
async fn the_journal_shows_a_capability_only_its_own_sites_rows<B: StorageBackend + ChangeFeed>(
    r: &Rig<B>,
) {
    const OTHER: &str = "other.test";
    const THIRD: &str = "third.test";
    let a = r.access();
    let b = r.access_at(OTHER);
    let (id, v) = r.account("sites").await;
    r.engine
        .set_secret(&a, "account", id, "token", s(TOKEN), v)
        .await
        .unwrap();
    r.engine
        .open_secret(&a, "account", id, "token")
        .await
        .unwrap();
    // The other site's calls: a refused find, an open of the same record (it cannot open A's
    // value: wrong site) and a rewrap batch.
    let _ = r
        .engine
        .find_by_secret(&b, "account", "password", b"x")
        .await;
    let _ = r.engine.open_secret(&b, "account", id, "token").await;
    r.engine
        .rewrap_secrets(&b, "account", RewrapBatch::first(10))
        .await
        .unwrap();

    let seen_by_a = r.engine.secret_audit_journal(&a).await.unwrap();
    let seen_by_b = r.engine.secret_audit_journal(&b).await.unwrap();
    assert_eq!(seen_by_a.len(), 2, "A made two calls");
    assert!(seen_by_a.iter().all(|e| e.site == SITE));
    assert_eq!(seen_by_b.len(), 3, "B made three calls");
    assert!(seen_by_b.iter().all(|e| e.site == OTHER));
    // A site with no calls sees nothing.
    let third = r.access_at(THIRD);
    assert!(
        r.engine
            .secret_audit_journal(&third)
            .await
            .unwrap()
            .is_empty()
    );

    // Nothing of either site's journal shows a value, a sealed blob, an index or a hex form.
    let text = format!("{seen_by_a:?}{seen_by_b:?}").into_bytes();
    let mut forms = forms_of_plain(TOKEN);
    forms.extend(forms_of_sealed(&r.sealed(id, "token").await.unwrap()));
    let index = r.bidx(id, "token").await.unwrap();
    forms.push(index.clone());
    forms.push(hex(&index).into_bytes());
    assert_absent(&text, &forms, "the journal read back");
}

/// The keys the engine writes into a `sys_secret_audit` event are exactly the allow-list the
/// audit appender chains (`spot_doc_audit::SECRET_EVENT_KEYS`): a key added to the writer
/// without the chain's knowledge makes the appender refuse the event, so this pins the two.
async fn the_audit_events_carry_only_the_keys_the_chain_accepts<B: StorageBackend + ChangeFeed>(
    r: &Rig<B>,
) {
    const KEYS: [&str; 9] = [
        "op", "doctype", "field", "record", "caller", "site", "outcome", "error", "state",
    ];
    let access = r.access();
    let (id, v) = r.account("keys").await;
    r.engine
        .set_secret(&access, "account", id, "token", s(TOKEN), v)
        .await
        .unwrap();
    r.engine
        .open_secret(&access, "account", id, "token")
        .await
        .unwrap();
    let _ = r
        .engine
        .find_by_secret(&access, "account", "password", b"x")
        .await;
    r.engine
        .rewrap_secrets(&access, "account", RewrapBatch::first(10))
        .await
        .unwrap();
    let events = audit_events(r).await;
    assert!(events.len() >= 4);
    for payload in events {
        let doc: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let data = doc["data"].as_object().unwrap();
        for key in data.keys() {
            assert!(KEYS.contains(&key.as_str()), "unexpected key `{key}`");
        }
        assert!(data.values().all(serde_json::Value::is_string));
    }
}

cases!(
    every_secret_operation_is_journalled_and_shows_no_value,
    a_rewrap_emits_the_update_event_and_keeps_the_child_rows,
    the_journal_shows_a_capability_only_its_own_sites_rows,
    the_audit_events_carry_only_the_keys_the_chain_accepts,
);
