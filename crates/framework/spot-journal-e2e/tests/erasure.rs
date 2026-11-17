//! Scenario 3: personal fields are protected on the way in (relay and snapshot), a subject is
//! erased by destroying its key, a rebuild shows the placeholder and nothing else changed, and no
//! stored object (raw, decoded segment entry or decoded snapshot image) holds a clear value.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use spot_events_contract::envelope::Position;
use spot_journal_e2e::audit::{catalog, stored_manifest};
use spot_journal_e2e::events::{Change, SOURCE, TENANT, domain_line, state_line, uuid};
use spot_journal_e2e::keys::{Signer, manifest_keys, snapshot_keys, snapshot_signer};
use spot_journal_e2e::store::TestStore;
use spot_journal_pii::{
    CounterNonces, FieldSpec, KeyHandle, MemoryKeys, PersonalFields, SubjectKeys,
};
use spot_journal_protect::{
    ProtectTransform, ProtectedParts, RevealSink, SharedFields, SharedKeys, SharedNonces,
    StaticFields,
};
use spot_journal_rebuild::{ApplySink, RebuildKeys, RebuildOptions, RecordingSink, execute};
use spot_journal_rebuild_plan::{Target, plan};
use spot_journal_relay::{Batch, Durability, Relay, RelayConfig};
use spot_journal_segment::SegmentReader;
use spot_journal_snapshot::{ImageRecord, SnapshotParams, SnapshotReader, SnapshotWriter};
use spot_journal_store::TenantId;

const TAGS: [&str; 5] = ["alice", "bruno", "carol", "dmitri", "gwen"];

fn contact_fields() -> PersonalFields {
    PersonalFields::new("contact")
        .unwrap()
        .with_field(FieldSpec::new("email", "person", "notifications", "contact_5y").unwrap())
        .with_field(
            FieldSpec::new("phones.*.number", "person", "notifications", "contact_5y").unwrap(),
        )
        .with_field(
            FieldSpec::new("tax_id", "person", "tax_filing", "tax_10y")
                .unwrap()
                .with_erasable(false),
        )
}

fn fields() -> SharedFields {
    Arc::new(StaticFields::new().with(contact_fields()))
}

fn tenant_keys() -> Arc<MemoryKeys> {
    let keys = MemoryKeys::new(KeyHandle::new([10; 32]));
    keys.insert("p1", KeyHandle::new([11; 32]));
    keys.insert("p2", KeyHandle::new([12; 32]));
    Arc::new(keys)
}

fn shared(keys: &Arc<MemoryKeys>) -> SharedKeys {
    Arc::<MemoryKeys>::clone(keys)
}

fn nonces() -> SharedNonces {
    Arc::new(Mutex::new(CounterNonces::new()))
}

fn contact_body(person: &str, tag: &str) -> Value {
    json!({
        "person": person,
        "name": format!("Name {tag}"),
        "email": format!("{tag}@mail.example"),
        "phones": [
            {"label": "home", "number": format!("+44 20 7946 {tag}1")},
            {"label": "work", "number": format!("+44 20 7946 {tag}2")},
        ],
        "tax_id": format!("TAX-{tag}"),
        "city": "Leeds",
    })
}

fn clear_values(tag: &str) -> Vec<String> {
    vec![
        format!("{tag}@mail.example"),
        format!("+44 20 7946 {tag}1"),
        format!("+44 20 7946 {tag}2"),
        format!("TAX-{tag}"),
    ]
}

fn change(n: u64, version: u64, person: &str, tag: &str) -> Change {
    Change {
        entity: "contact",
        n,
        version,
        body: contact_body(person, tag),
    }
}

/// The eight clear lines: positions 1..=8 (position 4 is a domain event).
fn script() -> Vec<Vec<u8>> {
    let lines = [
        change(1, 1, "p1", "alice"),
        change(2, 1, "p2", "bruno"),
        change(3, 1, "p1", "carol"),
    ];
    let mut out: Vec<Vec<u8>> = lines
        .iter()
        .enumerate()
        .map(|(i, c)| state_line(SOURCE, 1, i as u64 + 1, c))
        .collect();
    out.push(domain_line(SOURCE, 1, 4));
    out.push(state_line(SOURCE, 1, 5, &change(1, 2, "p1", "dmitri")));
    out.push(state_line(SOURCE, 1, 6, &change(4, 1, "p2", "hiro")));
    out.push(state_line(SOURCE, 1, 7, &change(2, 2, "p2", "bruno")));
    out.push(state_line(SOURCE, 1, 8, &change(1, 3, "p1", "gwen")));
    out
}

fn position(seq: u64) -> Position {
    Position { generation: 1, seq }
}

struct Built {
    store: TestStore,
    snapshot: spot_journal_snapshot::SnapshotInfo,
}

async fn build(keys: &Arc<MemoryKeys>) -> Built {
    let store = TestStore::default();
    let mut relay = Relay::open(
        store.clone(),
        Box::new(Signer),
        manifest_keys(),
        RelayConfig::new(TENANT, SOURCE, Durability::Local),
        1_000,
    )
    .await
    .unwrap();
    let nonces = nonces();
    let transform = ProtectTransform::new(fields(), shared(keys), Arc::clone(&nonces));
    let parts = ProtectedParts::new(fields(), shared(keys), Arc::clone(&nonces));
    let lines = script();
    for (batch_id, range) in [(1_u64, 1..=5_u64), (2, 6..=8)] {
        let entries = range
            .map(|seq| {
                (
                    position(seq),
                    lines[usize::try_from(seq).unwrap() - 1].clone(),
                )
            })
            .collect();
        transform
            .append_batch(&mut relay, Batch { batch_id, entries }, 2_000)
            .await
            .unwrap();
        assert!(relay.seal_now(2_000).await.unwrap());
    }
    // The database as of position 5, protected before it reaches the snapshot writer.
    let record = |n: u64, version: u64, person: &str, tag: &str| ImageRecord {
        entity: "contact".to_owned(),
        id: uuid(n),
        version,
        entity_schema: 1,
        image: contact_body(person, tag),
    };
    let records = parts
        .protect_records(vec![
            record(1, 2, "p1", "dmitri"),
            record(2, 1, "p2", "bruno"),
            record(3, 1, "p1", "carol"),
        ])
        .unwrap();
    let params = SnapshotParams {
        tenant: TENANT.to_owned(),
        generation: 1,
        position: 5,
        manifest_hash: "ab".repeat(32),
        taken_at_millis: 1_700_000_000_000,
    };
    let signer = snapshot_signer();
    let mut writer = SnapshotWriter::new(&store, params, &signer)
        .and_then(|w| w.with_part_bytes(400))
        .unwrap();
    writer.write_part(records).await.unwrap();
    let mut snapshot = writer.finish().await.unwrap();
    snapshot.verified = true;
    Built { store, snapshot }
}

async fn rebuild(
    built: &Built,
    with_snapshot: bool,
    keys: SharedKeys,
) -> BTreeMap<(String, String), (u64, Position, Value)> {
    let manifest = stored_manifest(&built.store, 1).await;
    let snapshots = if with_snapshot {
        vec![built.snapshot]
    } else {
        Vec::new()
    };
    let planned = plan(&catalog(&[&manifest], snapshots), None, Target::End).unwrap();
    let mk = manifest_keys();
    let sk = snapshot_keys();
    let rebuild_keys = RebuildKeys {
        manifest: &mk,
        snapshot: &sk,
    };
    let mut sink = RevealSink::new(RecordingSink::new(), fields(), keys);
    let sink_ref: &mut dyn ApplySink = &mut sink;
    execute(
        &built.store,
        &rebuild_keys,
        &spot_image_upcast::Registry::new(),
        &planned,
        sink_ref,
        &RebuildOptions::new(TenantId::new(TENANT).unwrap(), SOURCE, 1),
    )
    .await
    .unwrap();
    let inner = sink.into_inner();
    inner
        .state()
        .expect("committed")
        .iter()
        .map(|(k, r)| {
            (
                k.clone(),
                (r.version, r.position, Value::Object(r.image.body().clone())),
            )
        })
        .collect()
}

/// Every view of every stored object: raw bytes, each decoded segment entry, each snapshot image.
async fn views(built: &Built) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for (key, bytes) in built.store.dump("").await {
        out.push((format!("raw {key}"), bytes.clone()));
        if let Ok(reader) = SegmentReader::open(bytes) {
            for entry in reader.entries() {
                out.push((
                    format!("decoded entry of {key}"),
                    entry.unwrap().line.into_bytes(),
                ));
            }
        }
    }
    let sk = snapshot_keys();
    let reader = SnapshotReader::open(&built.store, TENANT, 1, 5, &sk)
        .await
        .unwrap();
    let mut images = reader.images();
    while let Some(record) = images.next().await.unwrap() {
        out.push((
            "decoded snapshot image".to_owned(),
            serde_json::to_vec(&record).unwrap(),
        ));
    }
    out
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    let needle = needle.as_bytes();
    haystack.windows(needle.len()).any(|w| w == needle)
}

async fn assert_no_clear_value(built: &Built, when: &str) {
    let views = views(built).await;
    let decoded = views
        .iter()
        .filter(|(what, _)| what.starts_with("decoded entry"))
        .count();
    assert_eq!(decoded, 8, "every one of the 8 entries was decoded");
    assert!(
        views
            .iter()
            .any(|(what, _)| what == "decoded snapshot image")
    );
    for (what, bytes) in &views {
        for tag in TAGS {
            for needle in clear_values(tag) {
                assert!(
                    !contains(bytes, &needle),
                    "{when}: a clear value is in {what}"
                );
            }
        }
    }
}

#[tokio::test]
async fn no_clear_value_is_stored_and_a_rebuild_with_reveal_restores_the_values() {
    let keys = tenant_keys();
    let built = build(&keys).await;
    assert_no_clear_value(&built, "before erasure").await;
    for with_snapshot in [true, false] {
        let state = rebuild(&built, with_snapshot, shared(&keys)).await;
        let (version, at, body) = &state[&("contact".to_owned(), uuid(1))];
        assert_eq!((*version, *at), (3, position(8)));
        assert_eq!(*body, contact_body("p1", "gwen"));
    }
}

#[tokio::test]
async fn erasing_a_subject_changes_nothing_but_its_erasable_values() {
    let keys = tenant_keys();
    let built = build(&keys).await;
    let before = rebuild(&built, true, shared(&keys)).await;

    keys.destroy("p1").unwrap();

    assert_no_clear_value(&built, "after erasure").await;
    let after = rebuild(&built, true, shared(&keys)).await;
    assert_eq!(before.len(), after.len());
    let placeholder = json!({"$erased": 1, "purpose": "notifications", "erased_at": null});
    for (key, (version, at, body)) in &before {
        let (after_version, after_at, after_body) = &after[key];
        assert_eq!((version, at), (after_version, after_at));
        let mut expected = body.clone();
        if body["person"] == "p1" {
            expected["email"] = placeholder.clone();
            expected["phones"][0]["number"] = placeholder.clone();
            expected["phones"][1]["number"] = placeholder.clone();
        }
        assert_eq!(*after_body, expected, "{key:?}");
    }
    // The legal-retention tax id and the other subject's values survive in full.
    assert_eq!(
        after[&("contact".to_owned(), uuid(1))].2["tax_id"],
        "TAX-gwen"
    );
    assert_eq!(
        after[&("contact".to_owned(), uuid(4))].2,
        contact_body("p2", "hiro")
    );
    // The same holds for a rebuild from the stream alone: the same versions and values. (A record
    // the snapshot carries is positioned at the snapshot, the stream positions it at its own
    // entry, so the positions are not compared.)
    let stream = rebuild(&built, false, shared(&keys)).await;
    let without_positions = |state: &BTreeMap<(String, String), (u64, Position, Value)>| {
        state
            .iter()
            .map(|(k, (version, _, body))| (k.clone(), (*version, body.clone())))
            .collect::<BTreeMap<_, _>>()
    };
    assert_eq!(without_positions(&stream), without_positions(&after));
}
