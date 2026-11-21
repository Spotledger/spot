#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

//! Channel leak inventory, the channels the crate does not own.
//!
//! Search, export, cache, the outbox/journal and the other `ADR_CHANNELS` are built by other
//! crates. Each is covered here by a documented test double that renders through
//! `ChannelGuard` exactly as the README contract says a channel must, one test per name in
//! `ADR_CHANNELS`. The stored forms fed to the double are a real sealed blob and a real
//! ciphertext holding the marker.

mod common;

use common::*;
use spot_protected_keys::{FieldAd, MonoTime};
use spot_sealed_fields::{
    ADR_CHANNELS, ChannelGuard, Destination, Emission, EncryptedStore, SyncClass, builtin_registry,
};

// ----------------------------------------------- channels outside the crate: documented doubles

/// A channel the crate does not own, as the README contract says it must behave: it asks
/// `ChannelGuard` through the registry, writes the masked form or nothing, and only the
/// hub-side channels may carry the value (where the audited reveal decides).
///
/// `plain` is what the channel holds in memory; `stored` is the stored form it would write on
/// an `Emit` that is not at the hub.
fn render_through_double(
    channel: &str,
    class: SyncClass,
    plain: &[u8],
    stored: &[u8],
) -> Option<String> {
    let registry = builtin_registry();
    let destination = registry.get(channel).expect("registered").destination();
    match registry.emission(channel, class).expect("registered") {
        Emission::Drop => None,
        Emission::Mask => Some(format!("{channel}|key=acme/{DOCTYPE}/BA-1/iban|[masked]")),
        Emission::Emit if destination == Destination::Hub => Some(format!(
            "{channel}|key=acme/{DOCTYPE}/BA-1/iban|{}",
            String::from_utf8_lossy(plain)
        )),
        Emission::Emit => Some(format!(
            "{channel}|key=acme/{DOCTYPE}/BA-1/iban|{}",
            hex_encode(stored, false)
        )),
    }
}

fn channel_double_never_leaks(channel: &str) {
    let registry = builtin_registry();
    let destination = registry.get(channel).expect("registered").destination();
    // A real sealed blob and a real ciphertext are the stored forms.
    let mut store = node_store(some_hub_key(9));
    let sealed = write_marker(&mut store, &iban_ad())
        .expect("write")
        .to_bytes();
    let now = MonoTime::from_millis(1_000);
    let (mut keys, g, epoch) = leased_node(now);
    let ad = FieldAd::new(tenant(), DOCTYPE, "iban", "BA-1", epoch);
    let cipher = EncryptedStore::new(&mut keys)
        .write(&g, epoch, &ad, marker(), now)
        .expect("encrypted")
        .ciphertext;

    for (class, stored) in [
        (SyncClass::Encrypted, cipher.as_slice()),
        (SyncClass::Sealed, sealed.as_slice()),
        (SyncClass::HomeOnly, b"".as_slice()),
    ] {
        let output = render_through_double(channel, class, marker(), stored);
        let emission = ChannelGuard::decide(class, destination);
        match output {
            None => assert_eq!(emission, Emission::Drop, "{channel} {class:?}"),
            Some(text) => {
                if destination == Destination::Hub {
                    // The hub is the value's home: the value is governed by the audited reveal
                    // and the caller's rights, not by this guard.
                    assert_eq!(emission, Emission::Emit, "{channel} {class:?}");
                } else {
                    assert_clean(channel, &text);
                }
            }
        }
    }
    // `everywhere` is the one class that emits as itself outside the hub, by design.
    if destination != Destination::Hub {
        let text = render_through_double(channel, SyncClass::Everywhere, marker(), marker());
        assert_eq!(
            text.as_deref()
                .map(|t| t.contains("4d4152") || !t.contains("MARKER")),
            Some(true)
        );
    }
}

/// One test per name in `ADR_CHANNELS`: the macro lists the names, and the last test fails if
/// the list and the constant ever differ.
macro_rules! channel_tests {
    ($($test:ident => $name:literal),+ $(,)?) => {
        $(
            #[test]
            fn $test() {
                channel_double_never_leaks($name);
            }
        )+

        const TESTED_CHANNELS: &[&str] = &[$($name),+];
    };
}

channel_tests! {
    document_read_channel_follows_the_rule => "document_read",
    list_channel_follows_the_rule => "list",
    report_channel_follows_the_rule => "report",
    export_channel_follows_the_rule => "export",
    print_channel_follows_the_rule => "print",
    realtime_event_channel_follows_the_rule => "realtime_event",
    journal_stream_outbox_channel_follows_the_rule => "journal_stream",
    link_lookup_channel_follows_the_rule => "link_lookup",
    search_index_channel_follows_the_rule => "search_index",
    embedding_channel_follows_the_rule => "embedding",
    version_history_channel_follows_the_rule => "version_history",
    audit_diff_channel_follows_the_rule => "audit_diff",
    api_output_channel_follows_the_rule => "api_output",
    scim_output_channel_follows_the_rule => "scim_output",
    notification_channel_follows_the_rule => "notification",
    bootstrap_snapshot_channel_follows_the_rule => "bootstrap_snapshot",
    cache_channel_follows_the_rule => "cache",
    backup_channel_follows_the_rule => "backup",
}

#[test]
fn every_named_channel_has_its_own_test_and_a_registration() {
    let mut tested: Vec<&str> = TESTED_CHANNELS.to_vec();
    let mut named: Vec<&str> = ADR_CHANNELS.to_vec();
    tested.sort_unstable();
    named.sort_unstable();
    assert_eq!(
        tested, named,
        "a channel in ADR_CHANNELS has no test (or the reverse)"
    );
    let registry = builtin_registry();
    for name in ADR_CHANNELS {
        assert!(
            registry.get(name).is_some(),
            "channel `{name}` unregistered"
        );
    }
}

#[test]
fn the_double_is_not_vacuous_a_leaky_channel_is_caught() {
    // A channel that wrote the plaintext outside the hub would be flagged by the detector.
    let leaky = format!("export|key=acme/{DOCTYPE}/BA-1/iban|{MARKER}");
    assert!(!leaks_in(&leaky, marker()).is_empty());
    // And the double itself emits the value only at the hub: a sealed value on an external
    // channel is masked, on a branch channel it is dropped.
    assert_eq!(
        render_through_double("export", SyncClass::Sealed, marker(), b"x").as_deref(),
        Some("export|key=acme/bank_account/BA-1/iban|[masked]")
    );
    assert_eq!(
        render_through_double("journal_stream", SyncClass::Sealed, marker(), b"x"),
        None
    );
    assert_eq!(
        render_through_double("backup", SyncClass::HomeOnly, marker(), b""),
        None
    );
}
