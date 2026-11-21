//! Canonical associated data for every AEAD use in the crate.
//!
//! Each layer has its own domain tag, and every variable-length part is length-prefixed, so no
//! two bindings can collide by concatenation. A ciphertext therefore cannot be moved between
//! layers, groups, epochs or nodes.

use crate::ids::{GroupName, KeyEpoch, NodeId, TenantId};

fn push(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

fn prefix(tag: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(tag.len() + 32);
    out.extend_from_slice(tag);
    out
}

/// The root layer binds the tenant, the group and the epoch.
pub(crate) fn root_ad(tenant: &TenantId, group: &GroupName, epoch: KeyEpoch) -> Vec<u8> {
    let mut out = prefix(b"spot-protected-keys/root/v1");
    push(&mut out, tenant.as_str().as_bytes());
    push(&mut out, group.as_str().as_bytes());
    out.extend_from_slice(&epoch.get().to_be_bytes());
    out
}

/// The node layer binds the tenant, the group, the epoch and the node.
pub(crate) fn release_ad(
    tenant: &TenantId,
    group: &GroupName,
    epoch: KeyEpoch,
    node: &NodeId,
) -> Vec<u8> {
    let mut out = prefix(b"spot-protected-keys/release/v1");
    push(&mut out, tenant.as_str().as_bytes());
    push(&mut out, group.as_str().as_bytes());
    out.extend_from_slice(&epoch.get().to_be_bytes());
    push(&mut out, node.as_str().as_bytes());
    out
}

/// The field layer binds the tenant, the doctype, the field, the document and the epoch.
pub(crate) fn field_ad(
    tenant: &TenantId,
    doctype: &str,
    field: &str,
    document: &str,
    epoch: KeyEpoch,
) -> Vec<u8> {
    let mut out = prefix(b"spot-protected-keys/field/v1");
    push(&mut out, tenant.as_str().as_bytes());
    push(&mut out, doctype.as_bytes());
    push(&mut out, field.as_bytes());
    push(&mut out, document.as_bytes());
    out.extend_from_slice(&epoch.get().to_be_bytes());
    out
}

/// The node-key layer binds a node's wrapping key, wrapped under the tenant root, to the tenant
/// and the node.
pub(crate) fn node_wrap_ad(tenant: &TenantId, node: &NodeId) -> Vec<u8> {
    let mut out = prefix(b"spot-protected-keys/node-wrap/v1");
    push(&mut out, tenant.as_str().as_bytes());
    push(&mut out, node.as_str().as_bytes());
    out
}
