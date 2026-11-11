// ---------------------------------------------------------------------------------------------
// A controller of `customer` built from closures
// ---------------------------------------------------------------------------------------------

struct Probe {
    steps: Vec<(StepDecl, StepFn)>,
}

impl Probe {
    fn new() -> Self {
        Self { steps: Vec::new() }
    }

    fn step<F>(mut self, name: &str, phase: Phase, op: Operation, f: F) -> Self
    where
        F: for<'a> Fn(&'a mut dyn HookContext) -> BoxFuture<'a, AppResult<()>>
            + Send
            + Sync
            + 'static,
    {
        let decl = StepDecl {
            name: StepName::new(name).unwrap(),
            point: HookPoint::of(phase, Target::Op(op)),
            priority: 0,
            budget: Duration::from_secs(5),
        };
        self.steps.push((decl, Arc::new(f)));
        self
    }
}

impl Controller for Probe {
    fn doctype(&self) -> &str {
        "customer"
    }

    fn steps(&self) -> Vec<StepDecl> {
        self.steps.iter().map(|(d, _)| d.clone()).collect()
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        match self.steps.iter().find(|(d, _)| &d.name == step) {
            Some((_, f)) => f(ctx),
            None => Box::pin(async { Ok(()) }),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// A backend that refuses the state event, to prove a failed flush rolls everything back
// ---------------------------------------------------------------------------------------------

struct RefusesState<B> {
    inner: B,
    armed: Arc<AtomicBool>,
}

struct RefusesStateTx<T> {
    inner: T,
    armed: Arc<AtomicBool>,
}

impl<B: ChangeFeed> ChangeFeed for RefusesState<B> {
    async fn read_outbox(
        &self,
        after: FeedPosition,
        limit: u32,
    ) -> StorageResult<Vec<OutboxRecord>> {
        self.inner.read_outbox(after, limit).await
    }

    async fn read_changes(
        &self,
        after: FeedPosition,
        limit: u32,
    ) -> StorageResult<Vec<spot_storage_contract::Change>> {
        self.inner.read_changes(after, limit).await
    }
}

impl<B: StorageBackend> StorageBackend for RefusesState<B> {
    type Tx = RefusesStateTx<B::Tx>;

    fn capabilities(&self) -> spot_storage_contract::Capabilities {
        self.inner.capabilities()
    }

    async fn prepare(&self, catalog: &Catalog) -> StorageResult<()> {
        self.inner.prepare(catalog).await
    }

    async fn begin(&self, catalog: Arc<Catalog>, options: TxOptions) -> StorageResult<Self::Tx> {
        Ok(RefusesStateTx {
            inner: self.inner.begin(catalog, options).await?,
            armed: self.armed.clone(),
        })
    }

    async fn read(
        &self,
        catalog: Arc<Catalog>,
        source: ReadSource,
        query: &Query,
    ) -> StorageResult<Rows> {
        self.inner.read(catalog, source, query).await
    }

    async fn health(&self) -> HealthReport {
        self.inner.health().await
    }
}

impl<T: Transaction> DocumentStore for RefusesStateTx<T> {
    async fn insert(&mut self, doc: &DocumentData) -> StorageResult<Version> {
        self.inner.insert(doc).await
    }

    async fn load(
        &mut self,
        entity: &spot_app_api::prelude::Name,
        id: RecordId,
        lock: RowLock,
    ) -> StorageResult<Option<spot_storage_contract::Document>> {
        self.inner.load(entity, id, lock).await
    }

    async fn update(&mut self, doc: &DocumentData, expected: Version) -> StorageResult<Version> {
        self.inner.update(doc, expected).await
    }

    async fn delete(
        &mut self,
        entity: &spot_app_api::prelude::Name,
        id: RecordId,
        expected: Version,
    ) -> StorageResult<()> {
        self.inner.delete(entity, id, expected).await
    }
}

impl<T: Transaction> QueryExecutor for RefusesStateTx<T> {
    async fn query(&mut self, query: &Query) -> StorageResult<Rows> {
        self.inner.query(query).await
    }
}

impl<T: Transaction> GraphTraversal for RefusesStateTx<T> {
    async fn traverse(
        &mut self,
        traversal: &spot_storage_contract::Traversal,
    ) -> StorageResult<spot_storage_contract::TraversalResult> {
        self.inner.traverse(traversal).await
    }
}

impl<T: Transaction> Locks for RefusesStateTx<T> {
    async fn lock_key(&mut self, key: &LockKey, mode: LockMode, wait: Wait) -> StorageResult<()> {
        self.inner.lock_key(key, mode, wait).await
    }
}

impl<T: Transaction> Sequences for RefusesStateTx<T> {
    async fn allocate(&mut self, series: &SeriesKey, count: u32) -> StorageResult<u64> {
        self.inner.allocate(series, count).await
    }

    async fn set_next(&mut self, series: &SeriesKey, next: u64) -> StorageResult<()> {
        self.inner.set_next(series, next).await
    }
}

impl<T: Transaction> Outbox for RefusesStateTx<T> {
    async fn enqueue(&mut self, event: &OutboxEvent) -> StorageResult<()> {
        if self.armed.load(Ordering::SeqCst) && event.topic.as_str() == TOPIC_DOC_STATE {
            return Err(spot_storage_contract::StorageError::new(
                spot_storage_contract::ErrorKind::Internal,
                "refused by the test",
            ));
        }
        self.inner.enqueue(event).await
    }
}

impl<T: Transaction> Transaction for RefusesStateTx<T> {
    fn catalog(&self) -> &Catalog {
        self.inner.catalog()
    }

    fn options(&self) -> &TxOptions {
        self.inner.options()
    }

    async fn savepoint(&mut self) -> StorageResult<Savepoint> {
        self.inner.savepoint().await
    }

    async fn rollback_to(&mut self, savepoint: Savepoint) -> StorageResult<()> {
        self.inner.rollback_to(savepoint).await
    }

    async fn release(&mut self, savepoint: Savepoint) -> StorageResult<()> {
        self.inner.release(savepoint).await
    }

    async fn commit(self) -> StorageResult<()> {
        self.inner.commit().await
    }

    async fn rollback(self) -> StorageResult<()> {
        self.inner.rollback().await
    }
}

