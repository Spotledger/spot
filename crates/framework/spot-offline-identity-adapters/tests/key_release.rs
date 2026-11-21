//! The key-release adapter: a wrapped key or a refusal, and never anything else.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_offline_identity::{KeyReleasePort, KeyReleaseRequest, NodeId, OfflineError, TenantId};
use spot_offline_identity_adapters::ProtectedKeysRelease;
use spot_protected_keys::{
    GroupName, Hub, KeyRelease, MemoryRootKeyProvider, MonoTime, NodeId as KeyNodeId, NodeKeyStore,
    NodeStatus, NodeWrapKey, TenantId as KeyTenantId, Tier, TierTable,
};

fn tenant() -> KeyTenantId {
    KeyTenantId::new("acme").unwrap()
}

fn group() -> GroupName {
    GroupName::new("bank_details").unwrap()
}

fn key_node() -> KeyNodeId {
    KeyNodeId::new("branch-1").unwrap()
}

fn offline_node() -> NodeId {
    NodeId::new("branch-1").unwrap()
}

fn request() -> KeyReleaseRequest {
    KeyReleaseRequest {
        tenant: TenantId::new("acme").unwrap(),
        claimed_node: offline_node(),
        authenticated_node: offline_node(),
        group: "bank_details".to_owned(),
        at_micros: 1,
    }
}

/// A hub with one group and one active, tier-2 node.
fn fixture() -> (Hub<MemoryRootKeyProvider>, TierTable, NodeWrapKey) {
    let mut hub = Hub::new(MemoryRootKeyProvider::new(tenant()).unwrap());
    hub.create_group(&group(), MonoTime::ZERO).unwrap();
    let wrap = NodeWrapKey::generate(key_node()).unwrap();
    let node_copy = wrap.duplicate().unwrap();
    hub.register_node(&key_node(), wrap).unwrap();
    let mut tiers = TierTable::new();
    tiers.grant(key_node(), group(), Tier::two());
    (hub, tiers, node_copy)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn the_adapter_returns_exactly_the_wrapped_key_and_nothing_else() {
    let (mut hub, tiers, node_copy) = fixture();
    let now = MonoTime::from_millis(1_000);
    let direct = hub
        .release(&key_node(), &group(), now, &tiers)
        .unwrap()
        .wrapped_key;
    assert!(!direct.is_empty());

    let mut adapter = ProtectedKeysRelease::new(&mut hub, &tiers, now);
    let wrapped = adapter.release(&request()).unwrap();
    assert!(
        !wrapped.is_empty(),
        "only the wrapped ciphertext is returned, never the bare key"
    );
    assert_eq!(
        wrapped.len(),
        direct.len(),
        "the same wrapped-key shape as a direct release"
    );
    assert_ne!(
        wrapped, direct,
        "each release is freshly wrapped, so the bytes differ"
    );

    // The bytes are the key the node can actually use: it opens on the node's own key store.
    let release = KeyRelease {
        tenant: tenant(),
        group: group(),
        epoch: spot_protected_keys::KeyEpoch::FIRST,
        node: key_node(),
        wrapped_key: wrapped,
        lease_until: MonoTime::from_millis(60_000),
    };
    let mut store = NodeKeyStore::new(key_node(), node_copy);
    store.accept(&release, now).unwrap();
    assert!(store.is_held(&group(), spot_protected_keys::KeyEpoch::FIRST));
}

#[test]
fn a_suspended_node_gets_no_key_and_the_refusal_carries_no_key_material() {
    let (mut hub, tiers, _node) = fixture();
    let now = MonoTime::from_millis(1_000);
    let wrapped = hub
        .release(&key_node(), &group(), now, &tiers)
        .unwrap()
        .wrapped_key;
    hub.set_status(&key_node(), NodeStatus::Suspended).unwrap();

    let mut adapter = ProtectedKeysRelease::new(&mut hub, &tiers, now);
    let error = adapter.release(&request()).unwrap_err();
    assert_eq!(
        error,
        OfflineError::KeyReleaseRefused {
            node: "branch-1".to_owned(),
            reason: "the node is suspended",
        }
    );
    let text = error.to_string();
    assert!(
        !text.contains(&hex(&wrapped)),
        "an error never carries the wrapped key: {text}"
    );
    assert!(text.contains("branch-1"));
}

#[test]
fn a_tenant_mismatch_or_an_impersonating_node_is_refused_before_the_hierarchy() {
    let (mut hub, tiers, _node) = fixture();
    let now = MonoTime::from_millis(1_000);
    let mut adapter = ProtectedKeysRelease::new(&mut hub, &tiers, now);

    let mut other_tenant = request();
    other_tenant.tenant = TenantId::new("other").unwrap();
    assert_eq!(
        adapter.release(&other_tenant).unwrap_err(),
        OfflineError::KeyReleaseRefused {
            node: "branch-1".to_owned(),
            reason: "another tenant",
        }
    );

    let mut impersonating = request();
    impersonating.authenticated_node = NodeId::new("branch-2").unwrap();
    assert_eq!(
        adapter.release(&impersonating).unwrap_err(),
        OfflineError::KeyReleaseRefused {
            node: "branch-1".to_owned(),
            reason: "the node is not the one that authenticated",
        }
    );
}

#[test]
fn a_node_without_a_tier_for_the_group_is_refused() {
    let (mut hub, _tiers, _node) = fixture();
    let empty = TierTable::new();
    let mut adapter = ProtectedKeysRelease::new(&mut hub, &empty, MonoTime::ZERO);
    assert_eq!(
        adapter.release(&request()).unwrap_err(),
        OfflineError::KeyReleaseRefused {
            node: "branch-1".to_owned(),
            reason: "the node has no tier for that protection group",
        }
    );
}

#[test]
fn an_unknown_group_or_node_is_refused_with_a_plain_reason() {
    let (mut hub, tiers, _node) = fixture();
    let mut adapter = ProtectedKeysRelease::new(&mut hub, &tiers, MonoTime::ZERO);

    let mut unknown_group = request();
    unknown_group.group = "no_such_group".to_owned();
    assert_eq!(
        adapter.release(&unknown_group).unwrap_err(),
        OfflineError::KeyReleaseRefused {
            node: "branch-1".to_owned(),
            reason: "the protection group has no key",
        }
    );

    let mut unknown_node = request();
    unknown_node.claimed_node = NodeId::new("branch-9").unwrap();
    unknown_node.authenticated_node = NodeId::new("branch-9").unwrap();
    assert_eq!(
        adapter.release(&unknown_node).unwrap_err(),
        OfflineError::KeyReleaseRefused {
            node: "branch-9".to_owned(),
            reason: "the node is not registered",
        }
    );
}

#[test]
fn the_hub_clock_the_adapter_uses_is_an_argument() {
    let (mut hub, tiers, _node) = fixture();
    let mut adapter = ProtectedKeysRelease::new(&mut hub, &tiers, MonoTime::ZERO);
    assert_eq!(adapter.now(), MonoTime::ZERO);
    adapter.set_now(MonoTime::from_millis(5_000));
    assert_eq!(adapter.now(), MonoTime::from_millis(5_000));
    assert!(adapter.release(&request()).is_ok());
}
