#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

//! The tracing hygiene check, in its own test binary (WP-SEC-10.T1, item 6 of the brief).
//!
//! `tracing` caches each callsite's interest the first time it is reached; another test in the
//! same binary that reaches the hub's `debug!` callsites with no subscriber would cache them as
//! disabled. Keeping this test alone in a binary means every callsite is first reached under
//! the capture subscriber, so the whole operation log is captured.

use std::io::Write;
use std::sync::{Arc, Mutex};

use spot_protected_keys::{
    GroupName, Hub, MemoryRootKeyProvider, MonoTime, NodeId, NodeWrapKey, TenantId, Tier, TierTable,
};

/// The canary key value: 32 ASCII bytes, so it is visible in any text encoding.
const CANARY: &[u8; 32] = b"CANARY-KEY-MATERIAL-DO-NOT-LEAK!";
/// The same canary as text.
const CANARY_TEXT: &str = "CANARY-KEY-MATERIAL-DO-NOT-LEAK!";

fn canary_key() -> spot_protected_keys::LockedKey {
    spot_protected_keys::LockedKey::from_bytes(CANARY).expect("canary key")
}

/// A writer that collects tracing output.
#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("buf")).into_owned()
    }
}

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("buf").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn tracing_output_carries_no_key_material() {
    let buf = SharedBuf::default();
    let subscriber = tracing_subscriber::fmt::Subscriber::builder()
        .with_writer({
            let buf = buf.clone();
            move || buf.clone()
        })
        .with_max_level(tracing::Level::TRACE)
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        let tenant = TenantId::new("acme").expect("tenant");
        let provider = MemoryRootKeyProvider::from_key(tenant, &canary_key()).expect("provider");
        let mut hub = Hub::new(provider);
        let g = GroupName::new("bank_details").expect("group");
        hub.create_group(&g, MonoTime::ZERO).expect("create");
        hub.rotate_group(&g, MonoTime::from_millis(1))
            .expect("rotate");
        let n = NodeId::new("branch-1").expect("node");
        hub.register_node(&n, NodeWrapKey::from_key(n.clone(), canary_key()))
            .expect("register");
        let mut tiers = TierTable::new();
        tiers.grant(n.clone(), g.clone(), Tier::two());
        hub.release(&n, &g, MonoTime::ZERO, &tiers)
            .expect("release");
        hub.revoke_node(&n, MonoTime::from_millis(2))
            .expect("revoke");
    });

    let text = buf.text();
    assert!(
        text.contains("created protection group"),
        "the tracing capture is missing the create event: {text}"
    );
    assert!(text.contains("rotated protection group"), "{text}");
    assert!(text.contains("released group key"), "{text}");
    assert!(
        text.contains("revoked node and rotated its groups"),
        "{text}"
    );
    assert!(
        !text.contains(CANARY_TEXT),
        "tracing output leaked the canary: {text}"
    );
    let hex: String = CANARY.iter().map(|b| format!("{b:02x}")).collect();
    assert!(
        !text.contains(&hex),
        "tracing output leaked the canary as hex: {text}"
    );
}
