//! Rows to and from the port's types. A read that cannot decode a row, or finds a value the
//! writer could never have produced, is `Corrupt`: never skipped and never repaired.

use spot_protected_keys::store::check_epoch;
use spot_protected_keys::{
    GroupName, KeyEpoch, KeyEvent, KeyEventKind, KeyStoreError, MonoTime, NodeId, NodeStatus,
    ReleaseRecord, RootRef, StoredNode, WrappedGroupKey, WrappedNodeKey,
};
use tokio_postgres::Row;

fn corrupt(what: &'static str) -> KeyStoreError {
    KeyStoreError::Corrupt(what)
}

/// An epoch as a `bigint`; zero and values beyond the signed range are refused.
pub(crate) fn epoch_out(epoch: KeyEpoch) -> Result<i64, KeyStoreError> {
    check_epoch(epoch)?;
    i64::try_from(epoch.get()).map_err(|_| KeyStoreError::BadInput("the epoch is out of range"))
}

/// An instant as a `bigint`.
pub(crate) fn time_out(at: MonoTime) -> Result<i64, KeyStoreError> {
    i64::try_from(at.millis()).map_err(|_| KeyStoreError::BadInput("the time is out of range"))
}

fn epoch_in(value: i64) -> Result<KeyEpoch, KeyStoreError> {
    match u64::try_from(value) {
        Ok(raw) if raw >= 1 => Ok(KeyEpoch::new(raw)),
        _ => Err(corrupt("epoch")),
    }
}

fn time_in(value: i64) -> Result<MonoTime, KeyStoreError> {
    u64::try_from(value)
        .map(MonoTime::from_millis)
        .map_err(|_| corrupt("time"))
}

fn group_in(raw: &str) -> Result<GroupName, KeyStoreError> {
    GroupName::new(raw).map_err(|_| corrupt("group name"))
}

fn node_in(raw: &str) -> Result<NodeId, KeyStoreError> {
    NodeId::new(raw).map_err(|_| corrupt("node id"))
}

fn root_in(id: String, version: i64) -> Result<RootRef, KeyStoreError> {
    let version = u32::try_from(version).map_err(|_| corrupt("root version"))?;
    Ok(RootRef { id, version })
}

/// A group name read from a one-column row.
pub(crate) fn group_name(row: &Row) -> Result<GroupName, KeyStoreError> {
    let raw: String = row
        .try_get("group_name")
        .map_err(|_| corrupt("group_name"))?;
    group_in(&raw)
}

/// A head epoch from a one-column row.
pub(crate) fn head(row: &Row) -> Result<KeyEpoch, KeyStoreError> {
    let value: i64 = row
        .try_get("current_epoch")
        .map_err(|_| corrupt("current_epoch"))?;
    epoch_in(value)
}

/// A `from_epoch` from a revocation row.
pub(crate) fn from_epoch(row: &Row) -> Result<KeyEpoch, KeyStoreError> {
    let value: i64 = row
        .try_get("from_epoch")
        .map_err(|_| corrupt("from_epoch"))?;
    epoch_in(value)
}

/// A group key row; `None` when the epoch was retired (its `wrapped` is erased).
pub(crate) fn group_key(row: &Row) -> Result<Option<WrappedGroupKey>, KeyStoreError> {
    let group: String = row
        .try_get("group_name")
        .map_err(|_| corrupt("group_name"))?;
    let epoch: i64 = row.try_get("epoch").map_err(|_| corrupt("epoch"))?;
    let id: String = row.try_get("root_id").map_err(|_| corrupt("root_id"))?;
    let version: i64 = row
        .try_get("root_version")
        .map_err(|_| corrupt("root_version"))?;
    let bytes: Option<Vec<u8>> = row.try_get("wrapped").map_err(|_| corrupt("wrapped"))?;
    let retired: Option<i64> = row
        .try_get("retired_at_ms")
        .map_err(|_| corrupt("retired_at_ms"))?;
    match (bytes, retired) {
        (Some(bytes), None) => Ok(Some(WrappedGroupKey {
            group: group_in(&group)?,
            epoch: epoch_in(epoch)?,
            root: root_in(id, version)?,
            bytes,
        })),
        (None, Some(_)) => Ok(None),
        _ => Err(corrupt("retirement")),
    }
}

/// A node row.
pub(crate) fn node(row: &Row) -> Result<StoredNode, KeyStoreError> {
    let node: String = row.try_get("node").map_err(|_| corrupt("node"))?;
    let status: String = row.try_get("status").map_err(|_| corrupt("status"))?;
    let id: Option<String> = row.try_get("root_id").map_err(|_| corrupt("root_id"))?;
    let version: Option<i64> = row
        .try_get("root_version")
        .map_err(|_| corrupt("root_version"))?;
    let bytes: Option<Vec<u8>> = row.try_get("wrapped").map_err(|_| corrupt("wrapped"))?;
    let registered: i64 = row
        .try_get("registered_at_ms")
        .map_err(|_| corrupt("registered_at_ms"))?;
    let status = match status.as_str() {
        "active" => NodeStatus::Active,
        "suspended" => NodeStatus::Suspended,
        "revoked" => NodeStatus::Revoked,
        _ => return Err(corrupt("status")),
    };
    let wrap = match (bytes, id, version) {
        (Some(bytes), Some(id), Some(version)) => Some(WrappedNodeKey {
            root: root_in(id, version)?,
            bytes,
        }),
        (None, None, None) => None,
        _ => return Err(corrupt("node key")),
    };
    if (status == NodeStatus::Revoked) != wrap.is_none() {
        return Err(corrupt("node key and status"));
    }
    Ok(StoredNode {
        node: node_in(&node)?,
        status,
        wrap,
        registered_at: time_in(registered)?,
    })
}

/// A hold row.
pub(crate) fn hold(row: &Row) -> Result<ReleaseRecord, KeyStoreError> {
    let node: String = row.try_get("node").map_err(|_| corrupt("node"))?;
    let group: String = row
        .try_get("group_name")
        .map_err(|_| corrupt("group_name"))?;
    let epoch: i64 = row.try_get("epoch").map_err(|_| corrupt("epoch"))?;
    let lease: i64 = row
        .try_get("lease_until_ms")
        .map_err(|_| corrupt("lease_until_ms"))?;
    Ok(ReleaseRecord {
        node: node_in(&node)?,
        group: group_in(&group)?,
        epoch: epoch_in(epoch)?,
        lease_until: time_in(lease)?,
    })
}

/// A history row.
pub(crate) fn event(row: &Row) -> Result<KeyEvent, KeyStoreError> {
    let seq: i64 = row.try_get("seq").map_err(|_| corrupt("seq"))?;
    let kind: String = row.try_get("kind").map_err(|_| corrupt("kind"))?;
    let group: Option<String> = row
        .try_get("group_name")
        .map_err(|_| corrupt("group_name"))?;
    let node: Option<String> = row.try_get("node").map_err(|_| corrupt("node"))?;
    let epoch: Option<i64> = row.try_get("epoch").map_err(|_| corrupt("epoch"))?;
    let to_epoch: Option<i64> = row.try_get("to_epoch").map_err(|_| corrupt("to_epoch"))?;
    let detail: Option<String> = row.try_get("detail").map_err(|_| corrupt("detail"))?;
    let actor: String = row.try_get("actor").map_err(|_| corrupt("actor"))?;
    let at: i64 = row.try_get("at_ms").map_err(|_| corrupt("at_ms"))?;
    Ok(KeyEvent {
        seq: u64::try_from(seq).map_err(|_| corrupt("seq"))?,
        kind: KeyEventKind::parse(&kind).ok_or_else(|| corrupt("kind"))?,
        group: group.as_deref().map(group_in).transpose()?,
        node: node.as_deref().map(node_in).transpose()?,
        epoch: epoch.map(epoch_in).transpose()?,
        to_epoch: to_epoch.map(epoch_in).transpose()?,
        detail,
        actor,
        at: time_in(at)?,
    })
}
