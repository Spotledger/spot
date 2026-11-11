//! The fake's transaction.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spot_storage_contract::{
    Access, Catalog, ChangeOp, ChildRow, Document, DocumentData, DocumentStore, EntityDef,
    ErrorKind, Fields, GraphTraversal, LockKey, LockMode, Locks, Name, NodeRef, Outbox,
    OutboxEvent, OutboxRecord, Query, QueryExecutor, RecordId, RowLock, Rows, Savepoint, Sequences,
    SeriesKey, StorageError, StorageResult, Transaction, Traversal, TraversalResult, TxOptions,
    Value, Version, Wait,
};

use crate::constraints::{check_links, check_unique};
use crate::locks::{LockTarget, TxRowLocker, acquire, row_lock_mode, wait_until_free};
use crate::store::{Data, DocOp, Overlay, Record, RecordKind, View};
use crate::validate::{already_exists, check_document, child_entity, document_entity, stored};
use crate::{Shared, State, concurrent_update, graph, query};

/// Guard-lock namespace that serializes allocation per numbering series.
const SEQUENCE_NAMESPACE: &str = "sequence";

/// Everything a savepoint restores.
#[derive(Clone, Debug, Default)]
struct Work {
    overlay: Overlay,
    outbox: Vec<OutboxEvent>,
    /// Next number of each series this transaction allocated from or set.
    sequences: BTreeMap<SeriesKey, u64>,
    /// Counter for [`DocOp::order`].
    ops: u64,
}

/// A record to write (`Some`) or delete (`None`).
type Put = (Name, RecordId, Option<Record>);

/// A transaction of [`FakeBackend`](crate::FakeBackend). Dropping it rolls it back.
pub struct FakeTx {
    shared: Arc<Shared>,
    catalog: Arc<Catalog>,
    options: TxOptions,
    id: RecordId,
    /// Snapshot and its commit number, for repeatable-read transactions.
    snapshot: Option<(Arc<Data>, u64)>,
    work: Work,
    savepoints: Vec<(Savepoint, Work)>,
    next_savepoint: u32,
    finished: bool,
}

impl std::fmt::Debug for FakeTx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeTx")
            .field("id", &self.id)
            .field("label", &self.options.label)
            .finish_non_exhaustive()
    }
}

impl FakeTx {
    pub(crate) fn new(
        shared: Arc<Shared>,
        catalog: Arc<Catalog>,
        options: TxOptions,
        snapshot: Option<(Arc<Data>, u64)>,
    ) -> Self {
        Self {
            shared,
            catalog,
            options,
            id: RecordId::new_v7(),
            snapshot,
            work: Work::default(),
            savepoints: Vec::new(),
            next_savepoint: 1,
            finished: false,
        }
    }

    fn writable(&self) -> StorageResult<()> {
        if self.options.access == Access::ReadOnly {
            return Err(StorageError::new(
                ErrorKind::ReadOnlyTransaction,
                format!(
                    "transaction `{}` is read-only and cannot write or lock rows",
                    self.options.label
                ),
            ));
        }
        Ok(())
    }

    /// Fails unless `n` more writes fit the declared limit.
    fn check_writes(&self, n: u32) -> StorageResult<()> {
        if self.work.overlay.writes.saturating_add(n) > self.options.max_writes {
            return Err(StorageError::new(
                ErrorKind::TransactionTooLarge,
                format!(
                    "transaction `{}` would write more than its declared {} records and events; \
                     chunk the work into several transactions",
                    self.options.label, self.options.max_writes
                ),
            ));
        }
        Ok(())
    }

    /// The latest committed data.
    fn latest(&self) -> Arc<Data> {
        self.shared.state().data.clone()
    }

    /// The committed data this transaction reads: its snapshot, or the latest.
    fn base(&self) -> Arc<Data> {
        match &self.snapshot {
            Some((data, _)) => data.clone(),
            None => self.latest(),
        }
    }

    /// Fails with `SerializationFailure` if another transaction committed the record after
    /// this repeatable-read transaction's snapshot.
    fn check_snapshot(&self, entity: &Name, id: RecordId) -> StorageResult<()> {
        if let Some((_, seq)) = &self.snapshot
            && self
                .shared
                .state()
                .written_at
                .get(&(entity.clone(), id))
                .is_some_and(|c| c > seq)
        {
            return Err(concurrent_update(entity));
        }
        Ok(())
    }

    async fn lock(&self, target: &LockTarget, mode: LockMode, wait: Wait) -> StorageResult<()> {
        acquire(
            &self.shared,
            self.id,
            target,
            mode,
            wait,
            self.options.lock_timeout,
        )
        .await
    }

    /// The exclusive row lock every write takes on its document.
    async fn lock_for_write(&self, entity: &Name, id: RecordId) -> StorageResult<()> {
        let target = LockTarget::Row(entity.clone(), id);
        self.lock(&target, LockMode::Exclusive, Wait::Block).await
    }

    /// Writes `puts` into the overlay, then checks unique indexes of every written record; on
    /// failure the overlay is restored.
    fn apply(&mut self, data: &Data, puts: Vec<Put>) -> StorageResult<()> {
        let mut undo = Vec::with_capacity(puts.len());
        for (entity, id, record) in &puts {
            let previous = self.work.overlay.put(entity, *id, record.clone());
            undo.push((entity, *id, previous));
        }
        let view = View::new(data, &self.work.overlay);
        let checked = puts
            .iter()
            .try_for_each(|(entity, id, record)| match record {
                Some(r) => check_unique(&view, self.catalog.entity(entity)?, *id, r),
                None => Ok(()),
            });
        if checked.is_err() {
            for (entity, id, previous) in undo.into_iter().rev() {
                self.work.overlay.restore(entity, id, previous);
            }
        }
        checked
    }

    /// Records a document write for version re-checks and the change feed.
    fn note_doc(&mut self, data: &Data, node: NodeRef, deleted: bool, version: Version) {
        let base = data.get(&node.entity, node.id).and_then(Record::version);
        let order = self.work.ops;
        self.work.ops += 1;
        self.work
            .overlay
            .docs
            .entry(node)
            .and_modify(|op| {
                op.deleted = deleted;
                op.version = version;
            })
            .or_insert(DocOp {
                order,
                base,
                deleted,
                version,
            });
    }

    /// The parent record and child rows of `doc` at `version`.
    fn records(
        &self,
        def: &EntityDef,
        doc: &DocumentData,
        version: Version,
    ) -> StorageResult<Vec<Put>> {
        let mut puts = Vec::new();
        let mut children = BTreeMap::new();
        for (table, rows) in &doc.children {
            let child_def = child_entity(&self.catalog, def, table)?;
            let mut ids = Vec::with_capacity(rows.len());
            for (i, row) in rows.iter().enumerate() {
                ids.push(row.id);
                puts.push((
                    child_def.name.clone(),
                    row.id,
                    Some(Record {
                        fields: stored(&row.fields),
                        kind: RecordKind::Child {
                            parent: doc.id,
                            parent_field: table.clone(),
                            idx: u32::try_from(i + 1).unwrap_or(u32::MAX),
                        },
                    }),
                ));
            }
            children.insert(table.clone(), ids);
        }
        puts.insert(
            0,
            (
                doc.entity.clone(),
                doc.id,
                Some(Record {
                    fields: stored(&doc.fields),
                    kind: RecordKind::Document { version, children },
                }),
            ),
        );
        Ok(puts)
    }

    /// Child rows of a stored document as (child entity, id).
    fn child_rows(&self, def: &EntityDef, record: &Record) -> StorageResult<Vec<(Name, RecordId)>> {
        record
            .child_ids()
            .map(|(table, id)| Ok((child_entity(&self.catalog, def, table)?.name.clone(), id)))
            .collect()
    }

    /// Loads the stored document `id` as the transaction currently sees it, checking the
    /// expected version. Used by `update` and `delete` after the row lock is held.
    fn current(
        &self,
        data: &Data,
        def: &EntityDef,
        id: RecordId,
        expected: Version,
    ) -> StorageResult<Record> {
        self.check_snapshot(&def.name, id)?;
        let view = View::new(data, &self.work.overlay);
        let Some(record) = view.get(&def.name, id) else {
            return Err(StorageError::new(
                ErrorKind::NotFound,
                format!("`{}` document {id} does not exist", def.name),
            )
            .with_entity(&def.name));
        };
        let version = record.version().unwrap_or(Version::FIRST);
        if version != expected {
            return Err(StorageError::new(
                ErrorKind::StaleVersion,
                format!(
                    "`{}` document {id} is at version {}, not {}: it changed since it was read; \
                     reload it and try again",
                    def.name, version.0, expected.0
                ),
            )
            .with_entity(&def.name));
        }
        Ok(record.clone())
    }

    fn document(
        &self,
        view: &View<'_>,
        def: &EntityDef,
        id: RecordId,
        record: &Record,
    ) -> StorageResult<Document> {
        let project = |def: &EntityDef, fields: &Fields| -> Fields {
            def.fields
                .iter()
                .map(|f| {
                    let v = fields.get(&f.name).cloned().unwrap_or(Value::Null);
                    (f.name.clone(), v)
                })
                .collect()
        };
        let stored = match &record.kind {
            RecordKind::Document { children, .. } => Some(children),
            RecordKind::Child { .. } => None,
        };
        let mut children = BTreeMap::new();
        for table in &def.children {
            let child_def = self.catalog.entity(&table.entity)?;
            let ids = stored
                .and_then(|c| c.get(&table.field))
                .map_or(&[][..], Vec::as_slice);
            let mut rows = Vec::with_capacity(ids.len());
            for row_id in ids {
                let row = view.get(&child_def.name, *row_id).ok_or_else(|| {
                    StorageError::new(
                        ErrorKind::Internal,
                        format!("child row {row_id} of `{}` {id} is missing", def.name),
                    )
                })?;
                rows.push(ChildRow {
                    id: *row_id,
                    fields: project(child_def, &row.fields),
                });
            }
            children.insert(table.field.clone(), rows);
        }
        Ok(Document {
            data: DocumentData {
                entity: def.name.clone(),
                id,
                fields: project(def, &record.fields),
                children,
            },
            version: record.version().unwrap_or(Version::FIRST),
        })
    }

    /// Takes the sequence guard lock and returns the series' next number as this
    /// transaction sees it.
    async fn series_next(&mut self, series: &SeriesKey) -> StorageResult<u64> {
        self.writable()?;
        let target = LockTarget::Guard(Name::new(SEQUENCE_NAMESPACE)?, series.as_str().to_owned());
        self.lock(&target, LockMode::Exclusive, Wait::Block).await?;
        if let Some(next) = self.work.sequences.get(series) {
            return Ok(*next);
        }
        Ok(self
            .shared
            .state()
            .sequences
            .get(series)
            .copied()
            .unwrap_or(1))
    }

    /// Checks and applies the transaction under the state mutex.
    fn commit_locked(&self, state: &mut State) -> StorageResult<()> {
        let work = &self.work;
        let overlay = &work.overlay;
        if overlay.is_empty() && work.outbox.is_empty() && work.sequences.is_empty() {
            return Ok(());
        }
        let written = || {
            overlay
                .records
                .iter()
                .flat_map(|(e, t)| t.iter().map(move |(id, r)| (e, *id, r.as_ref())))
        };
        if let Some((_, seq)) = &self.snapshot {
            for (entity, id, _) in written() {
                if state
                    .written_at
                    .get(&(entity.clone(), id))
                    .is_some_and(|c| c > seq)
                {
                    return Err(concurrent_update(entity));
                }
            }
        }
        for (node, op) in &overlay.docs {
            let committed = state
                .data
                .get(&node.entity, node.id)
                .and_then(Record::version);
            if committed != op.base {
                return Err(match op.base {
                    None => {
                        already_exists(&node.entity, node.id, "was created by another transaction")
                    }
                    Some(_) => StorageError::new(
                        ErrorKind::StaleVersion,
                        format!(
                            "`{}` document {} was changed by another transaction",
                            node.entity, node.id
                        ),
                    )
                    .with_entity(&node.entity),
                });
            }
        }
        {
            let view = View::new(&state.data, overlay);
            for (entity, id, record) in written() {
                let Some(record) = record else { continue };
                let def = self.catalog.entity(entity)?;
                if let (Some(parent), Some(stored)) = (record.parent(), state.data.get(entity, id))
                    && let Some(owner) = stored.parent()
                    && owner != parent
                    && let spot_storage_contract::EntityKind::Child { parent: pe } = &def.kind
                    && !overlay.docs.contains_key(&NodeRef {
                        entity: pe.clone(),
                        id: owner,
                    })
                {
                    return Err(already_exists(entity, id, "belongs to another document"));
                }
                check_unique(&view, def, id, record)?;
            }
            check_links(&self.catalog, &state.data, overlay, &view)?;
        }
        for e in &work.outbox {
            if state.feeds.event_ids.contains(&e.id) {
                return Err(duplicate_event(e.id));
            }
        }

        state.commits += 1;
        let commit = state.commits;
        let data = Arc::make_mut(&mut state.data);
        for (entity, records) in &overlay.records {
            let table = data.table_mut(entity);
            for (id, record) in records {
                match record {
                    Some(r) => table.insert(*id, r.clone()),
                    None => table.remove(id),
                };
                state.written_at.insert((entity.clone(), *id), commit);
            }
        }
        for (series, next) in &work.sequences {
            state.sequences.insert(series.clone(), *next);
        }
        for (ordinal, event) in work.outbox.iter().enumerate() {
            let position = state.feeds.next_outbox_position();
            state.feeds.event_ids.insert(event.id);
            state.feeds.outbox.push(OutboxRecord {
                position,
                transaction: self.id,
                ordinal: u32::try_from(ordinal).unwrap_or(u32::MAX),
                event: event.clone(),
            });
        }
        let mut docs: Vec<(&NodeRef, &DocOp)> = overlay.docs.iter().collect();
        docs.sort_by_key(|(_, op)| op.order);
        for (node, op) in docs {
            let change = match (op.base.is_some(), op.deleted) {
                (false, true) => continue,
                (false, false) => ChangeOp::Insert,
                (true, true) => ChangeOp::Delete,
                (true, false) => ChangeOp::Update,
            };
            let position = state.feeds.next_change_position();
            state.feeds.changes.push(spot_storage_contract::Change {
                position,
                transaction: self.id,
                document: node.clone(),
                op: change,
                version: op.version,
            });
        }
        Ok(())
    }

    /// Releases every lock and wakes waiting transactions. Idempotent.
    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let released = self.shared.state().locks.release_all(self.id);
        if released {
            self.shared.released.notify_waiters();
        }
    }
}

impl Drop for FakeTx {
    fn drop(&mut self) {
        self.finish();
    }
}

fn duplicate_event(id: RecordId) -> StorageError {
    StorageError::new(
        ErrorKind::AlreadyExists,
        format!("outbox event {id} was already enqueued; event ids are deduplication keys"),
    )
}

impl DocumentStore for FakeTx {
    async fn insert(&mut self, doc: &DocumentData) -> StorageResult<Version> {
        self.writable()?;
        let catalog = self.catalog.clone();
        let def = document_entity(&catalog, &doc.entity)?;
        let rows = check_document(&catalog, def, doc)?;
        let writes = rows.saturating_add(1);
        self.check_writes(writes)?;
        self.lock_for_write(&doc.entity, doc.id).await?;
        let data = self.latest();
        let puts = self.records(def, doc, Version::FIRST)?;
        {
            let view = View::new(&data, &self.work.overlay);
            for (entity, id, _) in &puts {
                if view.get(entity, *id).is_some() {
                    return Err(already_exists(entity, *id, "already exists"));
                }
            }
        }
        self.apply(&data, puts)?;
        let node = NodeRef {
            entity: doc.entity.clone(),
            id: doc.id,
        };
        self.note_doc(&data, node, false, Version::FIRST);
        self.work.overlay.writes += writes;
        Ok(Version::FIRST)
    }

    async fn load(
        &mut self,
        entity: &Name,
        id: RecordId,
        lock: RowLock,
    ) -> StorageResult<Option<Document>> {
        let catalog = self.catalog.clone();
        let def = document_entity(&catalog, entity)?;
        if let Some((mode, wait)) = row_lock_mode(lock) {
            self.writable()?;
            self.lock(&LockTarget::Row(entity.clone(), id), mode, wait)
                .await?;
            self.check_snapshot(entity, id)?;
        }
        let data = self.base();
        let view = View::new(&data, &self.work.overlay);
        match view.get(entity, id) {
            Some(record) => self.document(&view, def, id, record).map(Some),
            None => Ok(None),
        }
    }

    async fn update(&mut self, doc: &DocumentData, expected: Version) -> StorageResult<Version> {
        self.writable()?;
        let catalog = self.catalog.clone();
        let def = document_entity(&catalog, &doc.entity)?;
        check_document(&catalog, def, doc)?;
        self.lock_for_write(&doc.entity, doc.id).await?;
        let data = self.latest();
        let current = self.current(&data, def, doc.id, expected)?;
        let version = expected.next();
        let mut puts = self.records(def, doc, version)?;
        let kept: BTreeSet<RecordId> = puts.iter().map(|(_, id, _)| *id).collect();
        let old = self.child_rows(def, &current)?;
        let old_ids: BTreeSet<RecordId> = old.iter().map(|(_, id)| *id).collect();
        {
            let view = View::new(&data, &self.work.overlay);
            for (entity, id, _) in puts.iter().skip(1) {
                if !old_ids.contains(id) && view.get(entity, *id).is_some() {
                    return Err(already_exists(entity, *id, "belongs to another document"));
                }
            }
        }
        let removed: Vec<Put> = old
            .into_iter()
            .filter(|(_, id)| !kept.contains(id))
            .map(|(entity, id)| (entity, id, None))
            .collect();
        let writes = u32::try_from(puts.len() + removed.len()).unwrap_or(u32::MAX);
        self.check_writes(writes)?;
        puts.extend(removed);
        self.apply(&data, puts)?;
        let node = NodeRef {
            entity: doc.entity.clone(),
            id: doc.id,
        };
        self.note_doc(&data, node, false, version);
        self.work.overlay.writes += writes;
        Ok(version)
    }

    async fn delete(
        &mut self,
        entity: &Name,
        id: RecordId,
        expected: Version,
    ) -> StorageResult<()> {
        self.writable()?;
        let catalog = self.catalog.clone();
        let def = document_entity(&catalog, entity)?;
        self.lock_for_write(entity, id).await?;
        let data = self.latest();
        let current = self.current(&data, def, id, expected)?;
        let mut puts: Vec<Put> = vec![(entity.clone(), id, None)];
        puts.extend(
            self.child_rows(def, &current)?
                .into_iter()
                .map(|(e, row)| (e, row, None)),
        );
        let writes = u32::try_from(puts.len()).unwrap_or(u32::MAX);
        self.check_writes(writes)?;
        self.apply(&data, puts)?;
        let node = NodeRef {
            entity: entity.clone(),
            id,
        };
        self.note_doc(&data, node, true, expected);
        self.work.overlay.writes += writes;
        Ok(())
    }
}

impl QueryExecutor for FakeTx {
    async fn query(&mut self, q: &Query) -> StorageResult<Rows> {
        q.validate(&self.catalog)?;
        crate::capabilities().check_query(q)?;
        if q.lock != RowLock::None {
            self.writable()?;
        }
        let mut deadline = None;
        loop {
            let blocked = {
                let mut guard = self.shared.state();
                let State {
                    data,
                    locks,
                    written_at,
                    ..
                } = &mut *guard;
                let base = match &self.snapshot {
                    Some((snapshot, _)) => snapshot.clone(),
                    None => data.clone(),
                };
                let view = View::new(&base, &self.work.overlay);
                let mut locker = TxRowLocker {
                    locks,
                    written_at,
                    tx: self.id,
                    lock: q.lock,
                    snapshot: self.snapshot.as_ref().map(|(_, seq)| *seq),
                    blocked: None,
                };
                let result = query::execute(&self.catalog, &view, q, &mut locker);
                match (result, locker.blocked) {
                    (Err(_), Some(blocked)) => blocked,
                    (result, _) => return result,
                }
            };
            let (target, mode) = blocked;
            let timeout = self.options.lock_timeout;
            let until = *deadline.get_or_insert_with(|| tokio::time::Instant::now() + timeout);
            wait_until_free(&self.shared, self.id, &target, mode, until, timeout).await?;
        }
    }
}

impl GraphTraversal for FakeTx {
    async fn traverse(&mut self, traversal: &Traversal) -> StorageResult<TraversalResult> {
        let data = self.base();
        let view = View::new(&data, &self.work.overlay);
        graph::traverse(&self.catalog, &view, traversal)
    }
}

impl Locks for FakeTx {
    async fn lock_key(&mut self, key: &LockKey, mode: LockMode, wait: Wait) -> StorageResult<()> {
        let target = LockTarget::Guard(key.namespace.clone(), key.key.clone());
        self.lock(&target, mode, wait).await
    }
}

impl Sequences for FakeTx {
    async fn allocate(&mut self, series: &SeriesKey, count: u32) -> StorageResult<u64> {
        self.writable()?;
        if count == 0 {
            return Err(StorageError::new(
                ErrorKind::InvalidValue,
                "allocate needs a count of at least 1",
            ));
        }
        let first = self.series_next(series).await?;
        let next = first.checked_add(u64::from(count)).ok_or_else(|| {
            StorageError::new(
                ErrorKind::InvalidValue,
                format!("series `{}` would overflow", series.as_str()),
            )
        })?;
        self.work.sequences.insert(series.clone(), next);
        Ok(first)
    }

    async fn set_next(&mut self, series: &SeriesKey, next: u64) -> StorageResult<()> {
        let current = self.series_next(series).await?;
        if next < current {
            return Err(StorageError::new(
                ErrorKind::InvalidValue,
                format!(
                    "series `{}` has handed out numbers up to {}; setting the next number to \
                     {next} would reuse them",
                    series.as_str(),
                    current - 1
                ),
            ));
        }
        self.work.sequences.insert(series.clone(), next);
        Ok(())
    }
}

impl Outbox for FakeTx {
    async fn enqueue(&mut self, event: &OutboxEvent) -> StorageResult<()> {
        self.writable()?;
        if let Err(e) = serde_json::from_str::<serde_json::Value>(&event.payload) {
            return Err(StorageError::new(
                ErrorKind::InvalidValue,
                format!("outbox event payload is not valid JSON: {e}"),
            ));
        }
        if self.work.outbox.iter().any(|e| e.id == event.id)
            || self.shared.state().feeds.event_ids.contains(&event.id)
        {
            return Err(duplicate_event(event.id));
        }
        self.check_writes(1)?;
        self.work.outbox.push(event.clone());
        self.work.overlay.writes += 1;
        Ok(())
    }
}

impl Transaction for FakeTx {
    fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    fn options(&self) -> &TxOptions {
        &self.options
    }

    async fn savepoint(&mut self) -> StorageResult<Savepoint> {
        let sp = Savepoint(self.next_savepoint);
        self.next_savepoint += 1;
        self.savepoints.push((sp, self.work.clone()));
        Ok(sp)
    }

    async fn rollback_to(&mut self, savepoint: Savepoint) -> StorageResult<()> {
        let at = self.savepoint_index(savepoint)?;
        self.savepoints.truncate(at + 1);
        if let Some((_, work)) = self.savepoints.last() {
            self.work = work.clone();
        }
        Ok(())
    }

    async fn release(&mut self, savepoint: Savepoint) -> StorageResult<()> {
        let at = self.savepoint_index(savepoint)?;
        self.savepoints.truncate(at);
        Ok(())
    }

    async fn commit(mut self) -> StorageResult<()> {
        let result = {
            let mut state = self.shared.state();
            self.commit_locked(&mut state)
        };
        self.finish();
        result
    }

    async fn rollback(mut self) -> StorageResult<()> {
        self.finish();
        Ok(())
    }
}

impl FakeTx {
    fn savepoint_index(&self, savepoint: Savepoint) -> StorageResult<usize> {
        self.savepoints
            .iter()
            .position(|(sp, _)| *sp == savepoint)
            .ok_or_else(|| {
                StorageError::new(
                    ErrorKind::InvalidValue,
                    format!(
                        "savepoint {} does not exist: it was released or rolled past",
                        savepoint.0
                    ),
                )
            })
    }
}
