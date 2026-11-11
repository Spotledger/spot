#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

//! Channel leak inventory: the log line.
//!
//! This file holds exactly one test on purpose. A scoped `tracing` subscriber only sees a
//! callsite reliably when no other thread of the same process hits that callsite while the
//! subscriber is being installed, so the log capture runs in its own test binary.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use spot_protected_keys::{FieldAd, MonoTime};
use spot_sealed_fields::{EncryptedStore, MemoryAuditSink, SealedAd};

// ---------------------------------------------------------------------------- channel: log line

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<String>>>);

struct Fields(String);

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        let _ = write!(self.0, "{}={value:?} ", field.name());
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        if let Ok(mut lines) = self.0.lock() {
            lines.push(format!(
                "{} {} {}",
                event.metadata().level(),
                event.metadata().target(),
                fields.0
            ));
        }
    }
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
}

#[test]
fn log_lines_of_every_crate_path_never_carry_the_value() {
    let capture = Capture::default();
    tracing::subscriber::with_default(capture.clone(), || {
        // The node write logs a line; drive it with the marker, accepted and refused.
        let mut store = node_store(some_hub_key(9));
        write_marker(&mut store, &iban_ad()).expect("write");
        let _ = write_marker(&mut store, &SealedAd::unbound());
        let _ = store.read("BA-1", "iban");

        // The encrypted store and the reveal path, in case either logs.
        let now = MonoTime::from_millis(1_000);
        let (mut keys, g, epoch) = leased_node(now);
        let ad = FieldAd::new(tenant(), DOCTYPE, "iban", "BA-1", epoch);
        let mut encrypted = EncryptedStore::new(&mut keys);
        if let Ok(field) = encrypted.write(&g, epoch, &ad, marker(), now) {
            let _ = encrypted.read(&field, &ad, now);
        }
        let service = service_with(Arc::new(MemoryAuditSink::new()));
        let _ = service.reveal(&clerk(), DOCTYPE, "BA-1", "iban", 1);
        let _ = service.reveal(&masked_reader(), DOCTYPE, "BA-1", "iban", 2);
        let _ = service.explain(&masked_reader(), DOCTYPE, "BA-1", "iban");
    });
    let lines = capture.0.lock().expect("lock").clone();
    assert!(
        lines.iter().any(|line| line.contains("sealed a value")),
        "the node write log line was not captured, so this test would be vacuous: {lines:?}"
    );
    for line in lines {
        assert_clean("log", &line);
    }
}
