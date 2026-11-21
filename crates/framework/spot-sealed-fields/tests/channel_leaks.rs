#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

//! Channel leak inventory, the channels the crate builds itself.
//!
//! A recognisable marker value is driven through the crate's real paths (node write, encrypted
//! store, reveal service, sync filter) and every output the crate builds is searched for the
//! marker, its base64 (all three alignments), its hex and its length: error `Display` and
//! `Debug`, the audit record, the log line, the sync batch for a node without the key and the
//! stored forms. The channels the crate does not own are in `channel_doubles.rs`; the tenant
//! and key binding is in `tenant_key_binding.rs`.
//!
//! The file compiles under both feature sets: nothing here needs the hub's private key.

mod common;

use std::sync::Arc;

use common::*;
use spot_audit_chain::AuditRecord;
use spot_permissions::Principal;
use spot_protected_keys::{Duration, FieldAd, GroupName, MonoTime, TenantId};
use spot_sealed_fields::{
    AuditSink, BulkRequest, Destination, EncryptedStore, MemoryAuditSink, NodeIdentity,
    OutgoingField, OutgoingValue, RemoteReveal, SealTarget, Sealed, SealedAd, ServiceManifest,
    SyncClass, filter_outgoing_batch, offline_reveal, read_at_branch,
};
use spot_secrets::SecretBytes;

#[test]
fn the_detector_finds_each_form_it_claims_to_find() {
    // A positive control: a detector that finds nothing would make every test below vacuous.
    let n = marker().len();
    assert!(!leaks_in(&format!("x {MARKER} y"), marker()).is_empty());
    assert!(!leaks_in(&format!("x {} y", hex_encode(marker(), false)), marker()).is_empty());
    assert!(!leaks_in(&format!("x {} y", hex_encode(marker(), true)), marker()).is_empty());
    for shift in 0..3usize {
        let mut prefixed = vec![7u8; shift];
        prefixed.extend_from_slice(marker());
        prefixed.extend_from_slice(b"tail");
        let stream = base64_encode(&prefixed);
        assert!(
            !leaks_in(&stream, marker()).is_empty(),
            "base64 at shift {shift} not found: {stream}"
        );
    }
    assert!(!leaks_in(&format!("Thing {{ len: {n} }}"), marker()).is_empty());
    assert!(!leaks_in(&format!("{{\"length\":{n}}}"), marker()).is_empty());
    assert!(!leaks_in(&format!("{n} bytes"), marker()).is_empty());
    assert!(!leaks_in(&format!("field={:?} x", marker()), marker()).is_empty());
    // And it does not cry wolf at an unrelated number or text.
    assert!(leaks_in("len: 1234 and [redacted]", marker()).is_empty());
    assert!(leaks_in(&format!("len: {n}0"), marker()).is_empty());
}

// -------------------------------------------------------------- channel: error Display/Debug

#[test]
fn error_display_and_debug_never_carry_the_value_from_a_node_write_or_read() {
    let mut store = node_store(some_hub_key(9));
    let mut shown = Vec::new();

    // A write of the marker bound to the wrong tenant, wrong DocType, no place at all.
    for ad in [
        SealedAd::of_field(TenantId::new("other").expect("t"), DOCTYPE, "iban", "BA-1"),
        SealedAd::of_field(tenant(), "Other", "iban", "BA-1"),
        SealedAd::unbound(),
    ] {
        shown.push(show(
            &write_marker(&mut store, &ad).expect_err("wrong place"),
        ));
    }
    // A write of the marker with a class the store does not hold.
    let (g, key) = (group("notes"), store.hub_key());
    shown.push(show(
        &store
            .write(
                &SealTarget {
                    group: &g,
                    class: SyncClass::Everywhere,
                    field: "iban",
                    document: "BA-1",
                    hub_key: &key,
                },
                marker(),
                &iban_ad(),
            )
            .expect_err("wrong class"),
    ));
    // A write of the marker against a stale hub key.
    let stale = some_hub_key(8);
    shown.push(show(
        &store
            .write(
                &SealTarget {
                    group: &g,
                    class: SyncClass::Sealed,
                    field: "iban",
                    document: "BA-1",
                    hub_key: &stale,
                },
                marker(),
                &iban_ad(),
            )
            .expect_err("stale key"),
    ));
    // The read a node always refuses, after a real write of the marker.
    write_marker(&mut store, &iban_ad()).expect("matching write");
    shown.push(show(&store.read("BA-1", "iban").expect_err("unreadable")));
    // A blob parsed from the marker's own bytes, truncated and with a bad header.
    let blob = store.blob("BA-1", "iban").expect("blob").to_bytes();
    shown.push(show(&Sealed::from_bytes(&blob[..8]).expect_err("short")));
    let mut bad_magic = blob.clone();
    bad_magic[0] ^= 0xff;
    shown.push(show(&Sealed::from_bytes(&bad_magic).expect_err("magic")));

    assert!(shown.len() >= 8);
    for text in shown {
        assert_clean("error", &text);
    }
}

#[test]
fn error_display_and_debug_never_carry_the_value_from_the_encrypted_store() {
    let now = MonoTime::from_millis(1_000);
    let (mut keys, g, epoch) = leased_node(now);
    let ad = FieldAd::new(tenant(), DOCTYPE, "iban", "BA-1", epoch);
    let mut store = EncryptedStore::new(&mut keys);
    let field = store.write(&g, epoch, &ad, marker(), now).expect("write");

    let mut shown = Vec::new();
    // Moved place and tampered bytes: the cipher fails on the marker's ciphertext.
    let moved = FieldAd::new(tenant(), DOCTYPE, "iban", "BA-2", epoch);
    shown.push(show(&store.read(&field, &moved, now).expect_err("moved")));
    let mut tampered = field.clone();
    tampered.ciphertext[0] ^= 0xff;
    shown.push(show(
        &store.read(&tampered, &ad, now).expect_err("tampered"),
    ));
    // Past the lease, on both read and write of the marker.
    let after = now.plus(Duration::from_hours(1));
    shown.push(show(&store.read(&field, &ad, after).expect_err("expired")));
    shown.push(show(
        &store
            .write(&g, epoch, &ad, marker(), after)
            .expect_err("expired write"),
    ));
    // A key version the ad does not name, and a group the node holds no key for.
    let other_epoch = epoch.next();
    shown.push(show(
        &store
            .write(&g, other_epoch, &ad, marker(), now)
            .expect_err("epoch"),
    ));
    shown.push(show(
        &store
            .write(&group("ghost"), epoch, &ad.with_epoch(epoch), marker(), now)
            .expect_err("ghost group"),
    ));
    for text in shown {
        assert_clean("error", &text);
    }
}

#[test]
fn error_display_and_debug_never_carry_the_value_from_the_reveal_paths() {
    let mut shown = Vec::new();
    let sink = Arc::new(MemoryAuditSink::new());
    let service = service_with(sink.clone());

    // Denied: the source holds the marker, the principal may only see it masked.
    shown.push(show(
        &service
            .reveal(&masked_reader(), DOCTYPE, "BA-1", "iban", 1)
            .expect_err("denied"),
    ));
    // A person asking for a bulk reveal, and a service without the group in its manifest.
    let ids = vec!["BA-1".to_owned()];
    let person = clerk();
    let manifest = ServiceManifest::new(vec![group("bank")]);
    let bulk = |principal: &Principal, manifest: &ServiceManifest, grp: &GroupName| {
        service.reveal_bulk(BulkRequest {
            principal,
            manifest,
            group: grp,
            doctype: DOCTYPE,
            ids: &ids,
            field: "iban",
            at: 2,
        })
    };
    shown.push(show(
        &bulk(&person, &manifest, &group("bank")).expect_err("person"),
    ));
    let system = Principal::system();
    shown.push(show(
        &bulk(&system, &ServiceManifest::new(vec![]), &group("bank")).expect_err("undeclared"),
    ));
    // A revoked node.
    let revoked = service_with(Arc::new(MemoryAuditSink::new()))
        .at_node(NodeIdentity::revoked(node("branch-1")));
    shown.push(show(
        &revoked
            .reveal(&clerk(), DOCTYPE, "BA-1", "iban", 3)
            .expect_err("revoked"),
    ));
    // An audit sink that refuses: nothing is released and the error does not carry the value.
    struct Refusing;
    impl AuditSink for Refusing {
        fn offer(&self, _record: AuditRecord) -> bool {
            false
        }
    }
    shown.push(show(
        &service_with(Arc::new(Refusing))
            .reveal(&clerk(), DOCTYPE, "BA-1", "iban", 4)
            .expect_err("audit unavailable"),
    ));
    // Offline.
    shown.push(show(&offline_reveal().expect_err("offline")));
    shown.push(show(
        &RemoteReveal::offline()
            .reveal(&clerk(), DOCTYPE, "BA-1", "iban", 5)
            .expect_err("offline"),
    ));
    assert_eq!(shown.len(), 7);
    for text in shown {
        assert_clean("error", &text);
    }
    // The support explanation is a text channel too.
    assert_clean(
        "error",
        &service.explain(&masked_reader(), DOCTYPE, "BA-1", "iban"),
    );
}

// ---------------------------------------------------------------------- channel: audit record

#[test]
fn audit_records_of_a_real_reveal_never_carry_the_value() {
    let sink = Arc::new(MemoryAuditSink::new());
    let service = service_with(sink.clone());

    // The allowed reveal really releases the marker (so the test sees it), and the audit
    // record written for it does not carry it.
    let revealed = service
        .reveal(&clerk(), DOCTYPE, "BA-1", "iban", 10)
        .expect("allowed");
    assert_eq!(revealed.expose(), marker(), "the control: the value moved");
    // A refused reveal, a bulk reveal and a revoked node's attempt are audited too.
    service
        .reveal(&masked_reader(), DOCTYPE, "BA-1", "iban", 11)
        .expect_err("denied");
    let ids = vec!["BA-1".to_owned(), "BA-2".to_owned()];
    let released = service
        .reveal_bulk(BulkRequest {
            principal: &Principal::system(),
            manifest: &ServiceManifest::new(vec![group("bank")]),
            group: &group("bank"),
            doctype: DOCTYPE,
            ids: &ids,
            field: "iban",
            at: 12,
        })
        .expect("bulk");
    assert!(released.iter().all(|value| value.expose() == marker()));
    let revoked_sink = Arc::new(MemoryAuditSink::new());
    service_with(revoked_sink.clone())
        .at_node(NodeIdentity::revoked(node("branch-1")))
        .reveal(&clerk(), DOCTYPE, "BA-1", "iban", 13)
        .expect_err("revoked");

    let mut records = sink.records();
    records.extend(revoked_sink.records());
    assert_eq!(records.len(), 4, "allowed, denied, bulk, revoked");
    for record in &records {
        assert_clean("audit_record", &format!("{record:?}"));
        assert_clean(
            "audit_record",
            &serde_json::to_string(record).expect("serialise"),
        );
    }
}

// -------------------------------------------------- channel: sync batch for a node without keys

#[test]
fn a_sync_batch_for_a_node_without_the_key_carries_no_value_in_any_form() {
    let mut store = node_store(some_hub_key(9));
    let blob = write_marker(&mut store, &iban_ad())
        .expect("write")
        .to_bytes();

    let now = MonoTime::from_millis(1_000);
    let (mut keys, g, epoch) = leased_node(now);
    let ad = FieldAd::new(tenant(), DOCTYPE, "iban", "BA-1", epoch);
    let encrypted = EncryptedStore::new(&mut keys)
        .write(&g, epoch, &ad, marker(), now)
        .expect("encrypted");

    // The hub's batch: the sealed blob, the encrypted ciphertext (both really holding the
    // marker), a home-only field still in plaintext, and a plain field.
    let batch = vec![
        OutgoingField::plain("number", OutgoingValue::Plain(b"BA-1".to_vec())),
        OutgoingField::protected(
            "iban",
            group("bank_details"),
            SyncClass::Sealed,
            OutgoingValue::Sealed(blob.clone()),
        ),
        OutgoingField::protected(
            "iban_enc",
            group("bank_enc"),
            SyncClass::Encrypted,
            OutgoingValue::Encrypted(encrypted.ciphertext.clone()),
        ),
        OutgoingField::protected(
            "iban_home",
            group("bank_home"),
            SyncClass::HomeOnly,
            OutgoingValue::Plain(marker().to_vec()),
        ),
    ];
    let to_branch = filter_outgoing_batch(batch.clone(), Destination::Branch);
    let names: Vec<&str> = to_branch.iter().map(|f| f.field.as_str()).collect();
    assert_eq!(
        names,
        ["number", "iban_enc"],
        "sealed and home-only dropped"
    );
    // Every byte the branch receives, as bytes and as text, and the batch's own Debug.
    assert_clean("sync_batch", &format!("{to_branch:?}"));
    for field in &to_branch {
        match &field.value {
            OutgoingValue::Plain(bytes)
            | OutgoingValue::Sealed(bytes)
            | OutgoingValue::Encrypted(bytes) => {
                assert_clean_bytes("sync_batch", bytes);
                assert_clean("sync_batch", &hex_encode(bytes, false));
                assert_clean("sync_batch", &base64_encode(bytes));
            }
            OutgoingValue::Placeholder => {}
        }
    }
    // The hub's own copy, even with the sealed blob, carries no plaintext either.
    let to_hub = filter_outgoing_batch(batch, Destination::Hub);
    let sealed_field = to_hub.iter().find(|f| f.field == "iban").expect("sealed");
    assert_eq!(sealed_field.value, OutgoingValue::Sealed(blob.clone()));
    assert_clean_bytes("sync_batch", &blob);
    assert_clean("sync_batch", &hex_encode(&blob, false));
    assert_clean("sync_batch", &base64_encode(&blob));
    // The stored blob is longer than the value: its length does not reveal the value's.
    assert_ne!(blob.len(), marker().len());

    // What a branch reads of a home-only or sealed field it was never sent.
    for class in [SyncClass::HomeOnly, SyncClass::Sealed] {
        let seen = read_at_branch(class, Some(marker().to_vec()));
        assert_clean("sync_batch", &format!("{seen:?}"));
    }
}

#[test]
fn stored_forms_on_the_node_carry_no_value_in_any_form() {
    let mut store = node_store(some_hub_key(9));
    let sealed = write_marker(&mut store, &iban_ad()).expect("write");
    let bytes = sealed.to_bytes();
    assert_clean_bytes("stored_blob", &bytes);
    assert_clean("stored_blob", &hex_encode(&bytes, false));
    assert_clean("stored_blob", &base64_encode(&bytes));
    assert_clean("stored_blob", &format!("{sealed:?}"));
    assert!(format!("{sealed:?}").contains("[sealed]"));
    let stored = store.stored();
    assert_eq!(stored.len(), 1);
    assert_clean("stored_blob", &format!("{:?}", stored[0]));
    let revealed = SecretBytes::new(marker().to_vec());
    assert_clean("secret_debug", &format!("{revealed:?}"));
}
