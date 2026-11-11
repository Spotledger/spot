//! [`PgKeyStore`]: the PostgreSQL `KeyStore`, through the shared bridge.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use deadpool_postgres::Pool;
use spot_pg_bridge::{BoxFuture, Bridge, PooledConn};
use spot_protected_keys::store::{
    MAX_EVENT_PAGE, MAX_ROTATIONS, check_epoch, check_root, check_wrapped,
};
use spot_protected_keys::{
    Audit, GroupName, KeyEpoch, KeyEvent, KeyEventKind, KeyStore, KeyStoreBackup, KeyStoreError,
    NodeId, NodeStatus, ProtectedKeyError, RefusalReason, ReleaseRecord, RevocationReport, RootRef,
    RotatedGroup, Rotation, StoredNode, TenantId, WrappedGroupKey, WrappedNodeKey,
};
use tokio_postgres::Transaction;

use crate::codec::{self, epoch_out, time_out};
use crate::error::{SetupError, failed, from_bridge, pg};
use crate::{schema, sql};

/// Deadline of a read.
pub const READ_DEADLINE: Duration = Duration::from_secs(5);
/// Deadline of a write (one statement or one short transaction).
pub const WRITE_DEADLINE: Duration = Duration::from_secs(10);
/// Deadline of the table creation.
pub const PREPARE_DEADLINE: Duration = Duration::from_secs(30);

/// The key store in PostgreSQL. Build it with [`PgKeyStore::new`], create the tables once with
/// [`PgKeyStore::prepare`], then use it as the `KeyStore` of the hub.
///
/// The store is bound to **one tenant** for its whole life: the tenant is stamped on every row
/// and filtered on in every statement, and no method takes one.
///
/// **Call it from a plain thread, never from inside a tokio runtime** (see the crate docs).
pub struct PgKeyStore {
    bridge: Arc<Bridge>,
    tenant: TenantId,
}

impl std::fmt::Debug for PgKeyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgKeyStore")
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

impl PgKeyStore {
    /// Builds the store for `tenant` on the process's shared `bridge` over `pool`.
    ///
    /// # Errors
    /// `protected_keys_pg.bad_setup` when the tenant is empty, longer than 128 characters or
    /// holds a control character, or when the bridge configuration cannot hold against the pool.
    pub fn new(
        bridge: Arc<Bridge>,
        pool: &Pool,
        tenant: &str,
    ) -> std::result::Result<Self, SetupError> {
        let tenant = TenantId::new(tenant)
            .map_err(|_| SetupError::bad_setup("the tenant name is not usable"))?;
        bridge
            .config()
            .validate(pool.status().max_size)
            .map_err(|_| {
                SetupError::bad_setup("the bridge configuration does not hold against the pool")
            })?;
        Ok(Self { bridge, tenant })
    }

    /// Creates the tables, triggers and functions if missing, in one transaction under an
    /// advisory lock. Idempotent; safe from several nodes at once.
    ///
    /// # Errors
    /// `protected_keys_pg.migration_failed` with a fixed sentence.
    pub fn prepare(&self) -> std::result::Result<(), SetupError> {
        self.bridge
            .run(PREPARE_DEADLINE, |mut conn: PooledConn| {
                Box::pin(async move { schema::prepare(&mut conn).await.map_err(|_| ()) })
            })
            .map_err(|_| SetupError::migration("the key tables could not be created"))
    }

    /// Leaves `role` with the least privilege the store needs: `SELECT` and `INSERT` on the
    /// history and the revocation records, `SELECT`, `INSERT` and `UPDATE` on the keys, heads
    /// and nodes, and nothing that deletes or edits history. The role name is bound data, quoted
    /// by the database. Call it as the table owner after [`PgKeyStore::prepare`].
    ///
    /// # Errors
    /// `protected_keys_pg.bad_input` for an empty, long or control-character role name;
    /// `protected_keys_pg.store_failed` when the role does not exist or the caller may not grant.
    pub fn restrict_role(&self, role: &str) -> Result<(), KeyStoreError> {
        if role.is_empty() || role.len() > 63 || role.chars().any(char::is_control) {
            return Err(KeyStoreError::BadInput("the role name is not usable"));
        }
        let role = role.to_owned();
        self.write(move |conn| {
            Box::pin(async move {
                conn.execute(sql::RESTRICT_ROLE, &[&role])
                    .await
                    .map_err(|e| pg(&e))?;
                Ok(())
            })
        })
    }

    fn tenant_text(&self) -> String {
        self.tenant.as_str().to_owned()
    }

    fn read<T, F>(&self, op: F) -> Result<T, KeyStoreError>
    where
        T: Send + 'static,
        F: FnOnce(PooledConn) -> BoxFuture<'static, Result<T, KeyStoreError>> + Send + 'static,
    {
        from_bridge(self.bridge.run(READ_DEADLINE, op))
    }

    fn write<T, F>(&self, op: F) -> Result<T, KeyStoreError>
    where
        T: Send + 'static,
        F: FnOnce(PooledConn) -> BoxFuture<'static, Result<T, KeyStoreError>> + Send + 'static,
    {
        from_bridge(self.bridge.run(WRITE_DEADLINE, op))
    }
}

/// What one history row carries besides the audit stamp.
struct Ev {
    kind: KeyEventKind,
    group: Option<String>,
    node: Option<String>,
    epoch: Option<i64>,
    to_epoch: Option<i64>,
    detail: Option<String>,
}

impl Ev {
    fn new(kind: KeyEventKind) -> Self {
        Self {
            kind,
            group: None,
            node: None,
            epoch: None,
            to_epoch: None,
            detail: None,
        }
    }
}

async fn append(
    tx: &Transaction<'_>,
    tenant: &str,
    ev: Ev,
    audit: &Audit,
) -> Result<(), KeyStoreError> {
    let at = time_out(audit.at())?;
    tx.execute(
        sql::INSERT_EVENT,
        &[
            &tenant,
            &ev.kind.as_str(),
            &ev.group,
            &ev.node,
            &ev.epoch,
            &ev.to_epoch,
            &ev.detail,
            &audit.actor(),
            &at,
        ],
    )
    .await
    .map_err(|e| pg(&e))?;
    Ok(())
}

fn unknown_group(group: &GroupName) -> KeyStoreError {
    ProtectedKeyError::UnknownGroup {
        group: group.clone(),
    }
    .into()
}

fn unknown_node(node: &NodeId) -> KeyStoreError {
    ProtectedKeyError::UnknownNode { node: node.clone() }.into()
}

fn node_revoked(node: &NodeId) -> KeyStoreError {
    ProtectedKeyError::NodeRevoked { node: node.clone() }.into()
}

/// The head of `group`, if it exists.
async fn head_of(
    tx: &Transaction<'_>,
    tenant: &str,
    group: &GroupName,
) -> Result<Option<KeyEpoch>, KeyStoreError> {
    tx.query_opt(sql::HEAD, &[&tenant, &group.as_str()])
        .await
        .map_err(|e| pg(&e))?
        .as_ref()
        .map(codec::head)
        .transpose()
}

fn check_next(from: KeyEpoch, next: &WrappedGroupKey) -> Result<(), KeyStoreError> {
    check_epoch(from)?;
    check_epoch(next.epoch)?;
    check_wrapped(&next.bytes)?;
    check_root(&next.root)?;
    if from.checked_next() != Some(next.epoch) {
        return Err(KeyStoreError::BadInput(
            "the next epoch is not the one after the epoch read",
        ));
    }
    Ok(())
}

/// Inserts one epoch's wrapped key (the head row exists already).
async fn insert_epoch(
    tx: &Transaction<'_>,
    tenant: &str,
    key: &WrappedGroupKey,
    supersedes: Option<i64>,
) -> Result<(), KeyStoreError> {
    let epoch = epoch_out(key.epoch)?;
    tx.execute(
        sql::INSERT_EPOCH,
        &[
            &tenant,
            &key.group.as_str(),
            &epoch,
            &supersedes,
            &key.root.id,
            &i64::from(key.root.version),
            &key.bytes,
        ],
    )
    .await
    .map_err(|e| pg(&e))?;
    Ok(())
}

/// Moves a group head by compare-and-set; on a lost race says why.
async fn bump_head(
    tx: &Transaction<'_>,
    tenant: &str,
    group: &GroupName,
    from: KeyEpoch,
    to: KeyEpoch,
) -> Result<(), KeyStoreError> {
    let (from_i, to_i) = (epoch_out(from)?, epoch_out(to)?);
    let moved = tx
        .query_opt(sql::BUMP_HEAD, &[&tenant, &group.as_str(), &from_i, &to_i])
        .await
        .map_err(|e| pg(&e))?;
    if moved.is_some() {
        return Ok(());
    }
    match head_of(tx, tenant, group).await? {
        None => Err(unknown_group(group)),
        Some(_) => Err(KeyStoreError::EpochConflict {
            group: group.clone(),
        }),
    }
}

async fn create_group_op(
    mut conn: PooledConn,
    tenant: String,
    key: WrappedGroupKey,
    audit: Audit,
) -> Result<(), KeyStoreError> {
    let tx = conn.transaction().await.map_err(|e| pg(&e))?;
    let created = tx
        .query_opt(sql::INSERT_GROUP, &[&tenant, &key.group.as_str()])
        .await
        .map_err(|e| pg(&e))?;
    if created.is_none() {
        return Err(ProtectedKeyError::GroupExists { group: key.group }.into());
    }
    insert_epoch(&tx, &tenant, &key, None).await?;
    let mut ev = Ev::new(KeyEventKind::Issue);
    ev.group = Some(key.group.as_str().to_owned());
    ev.epoch = Some(epoch_out(key.epoch)?);
    append(&tx, &tenant, ev, &audit).await?;
    tx.commit().await.map_err(|e| pg(&e))
}

async fn rotate_group_op(
    mut conn: PooledConn,
    tenant: String,
    from: KeyEpoch,
    next: WrappedGroupKey,
    audit: Audit,
) -> Result<(), KeyStoreError> {
    let tx = conn.transaction().await.map_err(|e| pg(&e))?;
    bump_head(&tx, &tenant, &next.group, from, next.epoch).await?;
    let from_i = epoch_out(from)?;
    insert_epoch(&tx, &tenant, &next, Some(from_i)).await?;
    let mut ev = Ev::new(KeyEventKind::Rotate);
    ev.group = Some(next.group.as_str().to_owned());
    ev.epoch = Some(from_i);
    ev.to_epoch = Some(epoch_out(next.epoch)?);
    append(&tx, &tenant, ev, &audit).await?;
    tx.commit().await.map_err(|e| pg(&e))
}

async fn retire_op(
    mut conn: PooledConn,
    tenant: String,
    group: GroupName,
    epoch: KeyEpoch,
    audit: Audit,
) -> Result<(), KeyStoreError> {
    let tx = conn.transaction().await.map_err(|e| pg(&e))?;
    let epoch_i = epoch_out(epoch)?;
    let at = time_out(audit.at())?;
    let done = tx
        .query_opt(sql::RETIRE, &[&tenant, &group.as_str(), &epoch_i, &at])
        .await
        .map_err(|e| pg(&e))?;
    if done.is_none() {
        let Some(current) = head_of(&tx, &tenant, &group).await? else {
            return Err(unknown_group(&group));
        };
        if current == epoch {
            return Err(KeyStoreError::BadInput(
                "the current epoch cannot be retired",
            ));
        }
        return Err(match read_key(&tx, &tenant, &group, epoch).await? {
            Some(None) => ProtectedKeyError::EpochRetired { group, epoch }.into(),
            _ => ProtectedKeyError::UnknownEpoch { group, epoch }.into(),
        });
    }
    let mut ev = Ev::new(KeyEventKind::Retire);
    ev.group = Some(group.as_str().to_owned());
    ev.epoch = Some(epoch_i);
    append(&tx, &tenant, ev, &audit).await?;
    tx.commit().await.map_err(|e| pg(&e))
}

/// One epoch's row: `None` if absent, `Some(None)` if retired.
async fn read_key(
    tx: &Transaction<'_>,
    tenant: &str,
    group: &GroupName,
    epoch: KeyEpoch,
) -> Result<Option<Option<WrappedGroupKey>>, KeyStoreError> {
    let epoch_i = epoch_out(epoch)?;
    tx.query_opt(sql::GROUP_KEY, &[&tenant, &group.as_str(), &epoch_i])
        .await
        .map_err(|e| pg(&e))?
        .as_ref()
        .map(codec::group_key)
        .transpose()
}

async fn group_key_op(
    mut conn: PooledConn,
    tenant: String,
    group: GroupName,
    epoch: KeyEpoch,
) -> Result<WrappedGroupKey, KeyStoreError> {
    let tx = conn.transaction().await.map_err(|e| pg(&e))?;
    match read_key(&tx, &tenant, &group, epoch).await? {
        Some(Some(key)) => Ok(key),
        Some(None) => Err(ProtectedKeyError::EpochRetired { group, epoch }.into()),
        None => {
            if head_of(&tx, &tenant, &group).await?.is_none() {
                Err(unknown_group(&group))
            } else {
                Err(ProtectedKeyError::UnknownEpoch { group, epoch }.into())
            }
        }
    }
}

async fn register_node_op(
    mut conn: PooledConn,
    tenant: String,
    node: NodeId,
    wrap: WrappedNodeKey,
    audit: Audit,
) -> Result<(), KeyStoreError> {
    let tx = conn.transaction().await.map_err(|e| pg(&e))?;
    let at = time_out(audit.at())?;
    let done = tx
        .query_opt(
            sql::UPSERT_NODE,
            &[
                &tenant,
                &node.as_str(),
                &wrap.root.id,
                &i64::from(wrap.root.version),
                &wrap.bytes,
                &at,
            ],
        )
        .await
        .map_err(|e| pg(&e))?;
    if done.is_none() {
        return Err(node_revoked(&node));
    }
    let mut ev = Ev::new(KeyEventKind::Register);
    ev.node = Some(node.as_str().to_owned());
    append(&tx, &tenant, ev, &audit).await?;
    tx.commit().await.map_err(|e| pg(&e))
}

async fn set_status_op(
    mut conn: PooledConn,
    tenant: String,
    node: NodeId,
    status: NodeStatus,
    audit: Audit,
) -> Result<(), KeyStoreError> {
    let tx = conn.transaction().await.map_err(|e| pg(&e))?;
    let done = tx
        .query_opt(
            sql::SET_STATUS,
            &[&tenant, &node.as_str(), &status.as_str()],
        )
        .await
        .map_err(|e| pg(&e))?;
    if done.is_none() {
        let exists = tx
            .query_opt(sql::NODE, &[&tenant, &node.as_str()])
            .await
            .map_err(|e| pg(&e))?
            .is_some();
        return Err(if exists {
            node_revoked(&node)
        } else {
            unknown_node(&node)
        });
    }
    let mut ev = Ev::new(KeyEventKind::Status);
    ev.node = Some(node.as_str().to_owned());
    ev.detail = Some(status.as_str().to_owned());
    append(&tx, &tenant, ev, &audit).await?;
    tx.commit().await.map_err(|e| pg(&e))
}

/// Why a release statement recorded nothing, from the stored rows, in the order the port
/// documents: the node, then its revocation epoch, then the group and epoch.
async fn diagnose_release(
    tx: &Transaction<'_>,
    tenant: &str,
    release: &ReleaseRecord,
) -> Result<KeyStoreError, KeyStoreError> {
    let node = tx
        .query_opt(sql::NODE, &[&tenant, &release.node.as_str()])
        .await
        .map_err(|e| pg(&e))?
        .as_ref()
        .map(codec::node)
        .transpose()?;
    let Some(node) = node else {
        return Ok(unknown_node(&release.node));
    };
    match node.status {
        NodeStatus::Active => {}
        NodeStatus::Suspended => {
            return Ok(ProtectedKeyError::NodeNotActive {
                node: release.node.clone(),
                status: "suspended",
            }
            .into());
        }
        NodeStatus::Revoked => return Ok(node_revoked(&release.node)),
    }
    let revoked = tx
        .query_opt(
            sql::REVOCATION_EPOCH,
            &[&tenant, &release.node.as_str(), &release.group.as_str()],
        )
        .await
        .map_err(|e| pg(&e))?
        .as_ref()
        .map(codec::from_epoch)
        .transpose()?;
    if revoked.is_some_and(|r| release.epoch >= r) {
        return Ok(node_revoked(&release.node));
    }
    if head_of(tx, tenant, &release.group).await?.is_none() {
        return Ok(unknown_group(&release.group));
    }
    Ok(
        match read_key(tx, tenant, &release.group, release.epoch).await? {
            None => ProtectedKeyError::UnknownEpoch {
                group: release.group.clone(),
                epoch: release.epoch,
            }
            .into(),
            Some(None) => ProtectedKeyError::EpochRetired {
                group: release.group.clone(),
                epoch: release.epoch,
            }
            .into(),
            Some(Some(_)) => failed("the release was not recorded"),
        },
    )
}

async fn record_release_op(
    mut conn: PooledConn,
    tenant: String,
    release: ReleaseRecord,
    audit: Audit,
) -> Result<(), KeyStoreError> {
    let tx = conn.transaction().await.map_err(|e| pg(&e))?;
    let epoch = epoch_out(release.epoch)?;
    let lease = time_out(release.lease_until)?;
    let done = tx
        .query_opt(
            sql::RECORD_RELEASE,
            &[
                &tenant,
                &release.node.as_str(),
                &release.group.as_str(),
                &epoch,
                &lease,
            ],
        )
        .await
        .map_err(|e| pg(&e))?;
    if done.is_none() {
        return Err(diagnose_release(&tx, &tenant, &release).await?);
    }
    let mut ev = Ev::new(KeyEventKind::Release);
    ev.group = Some(release.group.as_str().to_owned());
    ev.node = Some(release.node.as_str().to_owned());
    ev.epoch = Some(epoch);
    append(&tx, &tenant, ev, &audit).await?;
    tx.commit().await.map_err(|e| pg(&e))
}

async fn revoke_op(
    mut conn: PooledConn,
    tenant: String,
    node: NodeId,
    rotations: Vec<Rotation>,
    audit: Audit,
) -> Result<RevocationReport, KeyStoreError> {
    let tx = conn.transaction().await.map_err(|e| pg(&e))?;
    let at = time_out(audit.at())?;
    // The row lock orders this against every release to the node.
    let revoked = tx
        .query_opt(sql::REVOKE_NODE, &[&tenant, &node.as_str(), &at])
        .await
        .map_err(|e| pg(&e))?;
    if revoked.is_none() {
        let exists = tx
            .query_opt(sql::NODE, &[&tenant, &node.as_str()])
            .await
            .map_err(|e| pg(&e))?
            .is_some();
        return Err(if exists {
            node_revoked(&node)
        } else {
            unknown_node(&node)
        });
    }
    let held: BTreeSet<GroupName> = tx
        .query(sql::HOLD_GROUPS, &[&tenant, &node.as_str()])
        .await
        .map_err(|e| pg(&e))?
        .iter()
        .map(codec::group_name)
        .collect::<Result<_, _>>()?;
    let named: BTreeSet<GroupName> = rotations.iter().map(|r| r.next.group.clone()).collect();
    if named.iter().any(|g| !held.contains(g)) {
        return Err(KeyStoreError::BadInput(
            "a rotation names a group the node does not hold",
        ));
    }
    if let Some(missed) = held.iter().find(|g| !named.contains(*g)) {
        return Err(KeyStoreError::EpochConflict {
            group: missed.clone(),
        });
    }
    let mut rotated = Vec::with_capacity(rotations.len());
    for rotation in &rotations {
        let group = &rotation.next.group;
        bump_head(&tx, &tenant, group, rotation.from, rotation.next.epoch).await?;
        let (from_i, to_i) = (epoch_out(rotation.from)?, epoch_out(rotation.next.epoch)?);
        insert_epoch(&tx, &tenant, &rotation.next, Some(from_i)).await?;
        tx.execute(
            sql::INSERT_REVOCATION,
            &[
                &tenant,
                &node.as_str(),
                &group.as_str(),
                &from_i,
                &to_i,
                &at,
            ],
        )
        .await
        .map_err(|e| pg(&e))?;
        let mut ev = Ev::new(KeyEventKind::Rotate);
        ev.group = Some(group.as_str().to_owned());
        ev.node = Some(node.as_str().to_owned());
        ev.epoch = Some(from_i);
        ev.to_epoch = Some(to_i);
        append(&tx, &tenant, ev, &audit).await?;
        rotated.push(RotatedGroup {
            group: group.clone(),
            from: rotation.from,
            to: rotation.next.epoch,
        });
    }
    tx.execute(sql::DELETE_HOLDS, &[&tenant, &node.as_str()])
        .await
        .map_err(|e| pg(&e))?;
    let mut ev = Ev::new(KeyEventKind::Revoke);
    ev.node = Some(node.as_str().to_owned());
    append(&tx, &tenant, ev, &audit).await?;
    tx.commit().await.map_err(|e| pg(&e))?;
    Ok(RevocationReport {
        node,
        revoked_at: audit.at(),
        rotated,
    })
}

impl KeyStore for PgKeyStore {
    fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    fn create_group(&self, key: &WrappedGroupKey, audit: &Audit) -> Result<(), KeyStoreError> {
        check_epoch(key.epoch)?;
        check_wrapped(&key.bytes)?;
        check_root(&key.root)?;
        if key.epoch != KeyEpoch::FIRST {
            return Err(KeyStoreError::BadInput("a group starts at its first epoch"));
        }
        let (tenant, key, audit) = (self.tenant_text(), key.clone(), audit.clone());
        self.write(move |conn| Box::pin(create_group_op(conn, tenant, key, audit)))
    }

    fn rotate_group(
        &self,
        from: KeyEpoch,
        next: &WrappedGroupKey,
        audit: &Audit,
    ) -> Result<(), KeyStoreError> {
        check_next(from, next)?;
        let (tenant, next, audit) = (self.tenant_text(), next.clone(), audit.clone());
        self.write(move |conn| Box::pin(rotate_group_op(conn, tenant, from, next, audit)))
    }

    fn group_epoch(&self, group: &GroupName) -> Result<Option<KeyEpoch>, KeyStoreError> {
        let (tenant, group) = (self.tenant_text(), group.clone());
        self.read(move |conn| {
            Box::pin(async move {
                conn.query_opt(sql::HEAD, &[&tenant, &group.as_str()])
                    .await
                    .map_err(|e| pg(&e))?
                    .as_ref()
                    .map(codec::head)
                    .transpose()
            })
        })
    }

    fn groups(&self) -> Result<Vec<GroupName>, KeyStoreError> {
        let tenant = self.tenant_text();
        self.read(move |conn| {
            Box::pin(async move {
                conn.query(sql::GROUPS, &[&tenant])
                    .await
                    .map_err(|e| pg(&e))?
                    .iter()
                    .map(codec::group_name)
                    .collect()
            })
        })
    }

    fn group_key(
        &self,
        group: &GroupName,
        epoch: KeyEpoch,
    ) -> Result<WrappedGroupKey, KeyStoreError> {
        let (tenant, group) = (self.tenant_text(), group.clone());
        self.read(move |conn| Box::pin(group_key_op(conn, tenant, group, epoch)))
    }

    fn retire_epoch(
        &self,
        group: &GroupName,
        epoch: KeyEpoch,
        audit: &Audit,
    ) -> Result<(), KeyStoreError> {
        let (tenant, group, audit) = (self.tenant_text(), group.clone(), audit.clone());
        self.write(move |conn| Box::pin(retire_op(conn, tenant, group, epoch, audit)))
    }

    fn backup(&self, root: &RootRef) -> Result<KeyStoreBackup, KeyStoreError> {
        check_root(root)?;
        let tenant = self.tenant_text();
        let tenant_id = self.tenant.clone();
        let root = root.clone();
        self.read(move |conn| {
            Box::pin(async move {
                let rows = conn
                    .query(sql::BACKUP, &[&tenant])
                    .await
                    .map_err(|e| pg(&e))?;
                let mut groups = Vec::with_capacity(rows.len());
                for row in &rows {
                    match codec::group_key(row)? {
                        Some(key) => groups.push(key),
                        None => return Err(KeyStoreError::Corrupt("retirement")),
                    }
                }
                Ok(KeyStoreBackup {
                    tenant: tenant_id,
                    root,
                    groups,
                })
            })
        })
    }

    fn register_node(
        &self,
        node: &NodeId,
        wrap: &WrappedNodeKey,
        audit: &Audit,
    ) -> Result<(), KeyStoreError> {
        check_wrapped(&wrap.bytes)?;
        check_root(&wrap.root)?;
        let (tenant, node, wrap, audit) = (
            self.tenant_text(),
            node.clone(),
            wrap.clone(),
            audit.clone(),
        );
        self.write(move |conn| Box::pin(register_node_op(conn, tenant, node, wrap, audit)))
    }

    fn node(&self, node: &NodeId) -> Result<Option<StoredNode>, KeyStoreError> {
        let (tenant, node) = (self.tenant_text(), node.clone());
        self.read(move |conn| {
            Box::pin(async move {
                conn.query_opt(sql::NODE, &[&tenant, &node.as_str()])
                    .await
                    .map_err(|e| pg(&e))?
                    .as_ref()
                    .map(codec::node)
                    .transpose()
            })
        })
    }

    fn set_node_status(
        &self,
        node: &NodeId,
        status: NodeStatus,
        audit: &Audit,
    ) -> Result<(), KeyStoreError> {
        if status == NodeStatus::Revoked {
            return Err(KeyStoreError::BadInput("revoke a node with revoke_node"));
        }
        let (tenant, node, audit) = (self.tenant_text(), node.clone(), audit.clone());
        self.write(move |conn| Box::pin(set_status_op(conn, tenant, node, status, audit)))
    }

    fn record_release(&self, release: &ReleaseRecord, audit: &Audit) -> Result<(), KeyStoreError> {
        check_epoch(release.epoch)?;
        time_out(release.lease_until)?;
        let (tenant, release, audit) = (self.tenant_text(), release.clone(), audit.clone());
        self.write(move |conn| Box::pin(record_release_op(conn, tenant, release, audit)))
    }

    fn record_refusal(
        &self,
        node: &NodeId,
        group: &GroupName,
        epoch: Option<KeyEpoch>,
        reason: RefusalReason,
        audit: &Audit,
    ) -> Result<(), KeyStoreError> {
        let epoch = epoch.map(epoch_out).transpose()?;
        let at = time_out(audit.at())?;
        let (tenant, node, group, audit) = (
            self.tenant_text(),
            node.as_str().to_owned(),
            group.as_str().to_owned(),
            audit.actor().to_owned(),
        );
        self.write(move |conn| {
            Box::pin(async move {
                conn.execute(
                    sql::INSERT_EVENT,
                    &[
                        &tenant,
                        &KeyEventKind::Refuse.as_str(),
                        &Some(group),
                        &Some(node),
                        &epoch,
                        &None::<i64>,
                        &Some(reason.code()),
                        &audit,
                        &at,
                    ],
                )
                .await
                .map_err(|e| pg(&e))?;
                Ok(())
            })
        })
    }

    fn held(&self, node: &NodeId) -> Result<Vec<ReleaseRecord>, KeyStoreError> {
        let (tenant, node) = (self.tenant_text(), node.clone());
        self.read(move |conn| {
            Box::pin(async move {
                conn.query(sql::HOLDS, &[&tenant, &node.as_str()])
                    .await
                    .map_err(|e| pg(&e))?
                    .iter()
                    .map(codec::hold)
                    .collect()
            })
        })
    }

    fn revoke_node(
        &self,
        node: &NodeId,
        rotations: &[Rotation],
        audit: &Audit,
    ) -> Result<RevocationReport, KeyStoreError> {
        if rotations.len() > MAX_ROTATIONS {
            return Err(KeyStoreError::BadInput("too many groups in one revocation"));
        }
        let mut sorted = rotations.to_vec();
        // One lock order for every revocation, so two revocations cannot deadlock on heads.
        sorted.sort_by(|a, b| a.next.group.cmp(&b.next.group));
        for pair in sorted.windows(2) {
            if pair[0].next.group == pair[1].next.group {
                return Err(KeyStoreError::BadInput("a group is rotated twice"));
            }
        }
        for rotation in &sorted {
            check_next(rotation.from, &rotation.next)?;
        }
        let (tenant, node, audit) = (self.tenant_text(), node.clone(), audit.clone());
        self.write(move |conn| Box::pin(revoke_op(conn, tenant, node, sorted, audit)))
    }

    fn revocation_epoch(
        &self,
        node: &NodeId,
        group: &GroupName,
    ) -> Result<Option<KeyEpoch>, KeyStoreError> {
        let (tenant, node, group) = (self.tenant_text(), node.clone(), group.clone());
        self.read(move |conn| {
            Box::pin(async move {
                conn.query_opt(
                    sql::REVOCATION_EPOCH,
                    &[&tenant, &node.as_str(), &group.as_str()],
                )
                .await
                .map_err(|e| pg(&e))?
                .as_ref()
                .map(codec::from_epoch)
                .transpose()
            })
        })
    }

    fn events(&self, after: u64, limit: usize) -> Result<Vec<KeyEvent>, KeyStoreError> {
        if limit == 0 || limit > MAX_EVENT_PAGE {
            return Err(KeyStoreError::BadInput("the page size is out of range"));
        }
        let after = i64::try_from(after)
            .map_err(|_| KeyStoreError::BadInput("the cursor is out of range"))?;
        let limit = i64::try_from(limit)
            .map_err(|_| KeyStoreError::BadInput("the page size is out of range"))?;
        let tenant = self.tenant_text();
        self.read(move |conn| {
            Box::pin(async move {
                conn.query(sql::EVENTS, &[&tenant, &after, &limit])
                    .await
                    .map_err(|e| pg(&e))?
                    .iter()
                    .map(codec::event)
                    .collect()
            })
        })
    }
}
