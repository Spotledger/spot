//! The operations of the event stream: consumers, leases, reading, acknowledging, restarting
//! and opening a stream. Every operation first resolves and checks the caller.

use super::*;

impl Shared {
    pub(super) fn lease_ttl(&self) -> Duration {
        Duration::from_secs(self.settings.lease_seconds)
    }

    pub(super) fn poll(&self) -> Duration {
        Duration::from_millis(self.settings.poll_ms)
    }

    pub(super) fn error(&self, error: StreamError) -> ApiError {
        api_error(error, self.settings.lease_seconds)
    }

    /// Who is calling, and whether they may use the event stream at all (default deny).
    pub(super) async fn caller(&self, ctx: &RequestCtx) -> EventsResult<Caller> {
        if !self.ready.load(Ordering::Acquire) {
            return Err(unavailable());
        }
        if ctx.site.as_str() != self.site {
            return Err(ApiError::from_code(Code::EventstreamConsumerNotFound));
        }
        let auth = self.live.auth().ok_or_else(unavailable)?;
        let who = auth.authenticate_on(ctx, PathClass::Api).await?;
        if auth.is_restricted(&who) {
            return Err(denied());
        }
        let principals = self.live.principals().ok_or_else(unavailable)?;
        let principal = DocPrincipals::resolve(&*principals, &who.user_id).await;
        if !holds(&principal, &self.settings.consumer_roles) {
            return Err(denied());
        }
        let stream_reserved = holds(&principal, &self.settings.stream_roles);
        Ok(Caller {
            user: who.user_id.to_string(),
            principal,
            stream_reserved,
        })
    }

    /// The consumer, if the caller owns it (or administers the site): else `404`.
    pub(super) async fn visible(&self, caller: &Caller, name: &str) -> EventsResult<ConsumerRecord> {
        self.engine
            .get(name, &caller.principal)
            .await
            .map_err(|e| self.error(e))
    }

    /// Loads the owner's principal fresh: an owner who is gone, disabled or never resolved has
    /// no assignments, so its consumers read nothing.
    pub(super) async fn refresh_owner(&self, owner: &str) -> Principal {
        let principal = self.load_owner(owner).await;
        lock(&self.principals.map).insert(owner.to_owned(), principal.clone());
        principal
    }

    pub(super) async fn load_owner(&self, owner: &str) -> Principal {
        let none = Principal::user(owner, Vec::new());
        let (Some(id), Some(engine), Some(principals)) = (
            parse_record_id(owner),
            self.live.engine(),
            self.live.principals(),
        ) else {
            return none;
        };
        let (Ok(site), Ok(component)) = (SiteId::new(&self.site), Name::new("events")) else {
            return none;
        };
        let invocation = Invocation {
            site,
            actor: Actor::System { component },
            command_id: None,
            language: None,
            now: self.clock.now_utc(),
        };
        let Ok(user) = engine.load(&invocation, "user", id).await else {
            return none;
        };
        let active = matches!(
            user.value.fields().iter().find(|(n, _)| n.as_str() == "state"),
            Some((_, Value::Text(state))) if state == "active"
        );
        if !active {
            return none;
        }
        DocPrincipals::resolve(&*principals, &id).await
    }

    /// A consumer of a reserved topic class is served only while its owner still holds the
    /// right to stream that class.
    pub(super) fn check_owner_rights(&self, spec: &ConsumerSpec, owner: &Principal) -> EventsResult<()> {
        let reserved = spec.topics.iter().any(|t| t.reserved_class().is_some());
        if reserved && !holds(owner, &self.settings.stream_roles) {
            return Err(denied());
        }
        Ok(())
    }

    pub(super) fn partition_of(record: &ConsumerRecord, partition: u32) -> EventsResult<()> {
        if partition >= record.spec.partitions {
            return Err(spec_invalid(format!(
                "partition {partition} is outside 0..{}",
                record.spec.partitions
            )));
        }
        Ok(())
    }

    pub(super) fn op_lock(&self, name: &str, partition: u32) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(
            lock(&self.locks)
                .entry((name.to_owned(), partition))
                .or_default(),
        )
    }

    pub(super) fn view(record: &ConsumerRecord) -> ConsumerOut {
        let name = record.spec.name.as_str();
        ConsumerOut {
            name: name.to_owned(),
            owner: record.spec.owner.clone(),
            topics: record.spec.topics.iter().map(TopicSelector::as_str).collect(),
            filter: record.spec.filter.clone(),
            partitions: record.spec.partitions,
            payload_mode: match record.spec.payload_mode {
                PayloadMode::IdsOnly => PayloadModeIn::IdsOnly,
                PayloadMode::Fields => PayloadModeIn::Fields,
            },
            partitions_state: record
                .checkpoints
                .iter()
                .enumerate()
                .map(|(index, checkpoint)| PartitionOut {
                    partition: u32::try_from(index).unwrap_or(0),
                    checkpoint: wire(name, *checkpoint),
                    parked: record.parked.get(index).copied().unwrap_or(false),
                })
                .collect(),
        }
    }

    // ---- consumers ----------------------------------------------------------------------

    pub(super) async fn list(&self, ctx: &RequestCtx) -> EventsResult<Vec<ConsumerOut>> {
        let caller = self.caller(ctx).await?;
        let records = self
            .engine
            .list(&caller.principal)
            .await
            .map_err(|e| self.error(e))?;
        Ok(records.iter().map(Self::view).collect())
    }

    pub(super) async fn put(
        &self,
        ctx: &RequestCtx,
        name: &str,
        spec: ConsumerSpecIn,
    ) -> EventsResult<(ConsumerOut, bool)> {
        let caller = self.caller(ctx).await?;
        let topics = spec
            .topics
            .iter()
            .map(|t| TopicSelector::parse(t))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| self.error(e))?;
        let start = match spec.start {
            None | Some(StartIn::Tail) => Start::Tail,
            Some(StartIn::FromBeginning) => Start::FromBeginning,
            Some(StartIn::FromPosition(text)) => Start::FromPosition(unwire(name, &text)?),
        };
        let consumer = ConsumerSpec {
            name: name.to_owned(),
            owner: caller.user.clone(),
            topics,
            filter: spec.filter,
            partitions: spec.partitions,
            payload_mode: match spec.payload_mode {
                PayloadModeIn::IdsOnly => PayloadMode::IdsOnly,
                PayloadModeIn::Fields => PayloadMode::Fields,
            },
            start,
        };
        consumer.validate().map_err(|e| self.error(e))?;
        let reserved = consumer.topics.iter().any(|t| t.reserved_class().is_some());
        if reserved && !caller.stream_reserved {
            return Err(denied());
        }
        self.rights.set(&caller.user, caller.stream_reserved);
        if matches!(consumer.start, Start::Tail) {
            self.head.refresh(&self.backend).await?;
        }
        let began = self.clock.now_utc();
        let existed = self.engine.get(name, &caller.principal).await.is_ok();
        let record = self
            .engine
            .register(consumer, &caller.principal)
            .await
            .map_err(|e| self.error(e))?;
        lock(&self.principals.map).insert(caller.user.clone(), caller.principal.clone());
        let created = !existed && record.registered_at >= began;
        Ok((Self::view(&record), created))
    }

    pub(super) async fn get(&self, ctx: &RequestCtx, name: &str) -> EventsResult<ConsumerOut> {
        let caller = self.caller(ctx).await?;
        Ok(Self::view(&self.visible(&caller, name).await?))
    }

    pub(super) async fn delete(&self, ctx: &RequestCtx, name: &str) -> EventsResult<()> {
        let caller = self.caller(ctx).await?;
        self.visible(&caller, name).await?;
        self.engine
            .delete(name, &caller.principal)
            .await
            .map_err(|e| self.error(e))?;
        let held = lock(&self.held).remove(name);
        if let Some(held) = held {
            let _ = self.coordinator.release(&held.lease).await;
        }
        lock(&self.outstanding).retain(|(c, _)| c != name);
        lock(&self.sent).retain(|(c, _), _| c != name);
        lock(&self.locks).retain(|(c, _), _| c != name);
        Ok(())
    }

    // ---- leases -------------------------------------------------------------------------

    /// The lease for a pull: the one the header names (renewed), else a fresh one. A consumer
    /// someone else holds, or that a stream holds, is `409 eventstream.consumer_busy`.
    pub(super) async fn take_lease(
        &self,
        name: &str,
        header: Option<&str>,
    ) -> EventsResult<(String, ConsumerLease)> {
        let known = lock(&self.held).get(name).cloned();
        if let Some(held) = known
            && header == Some(held.token.as_str())
        {
            if held.streaming {
                return Err(self.error(StreamError::ConsumerBusy {
                    name: name.to_owned(),
                    reason: "a stream holds this consumer".to_owned(),
                }));
            }
            match self.coordinator.renew(&held.lease, self.lease_ttl()).await {
                Ok(renewed) => {
                    if let Some(entry) = lock(&self.held).get_mut(name) {
                        entry.lease = renewed.clone();
                    }
                    return Ok((
                        held.token,
                        ConsumerLease {
                            consumer: name.to_owned(),
                            lease: renewed,
                        },
                    ));
                }
                Err(_) => {
                    lock(&self.held).remove(name);
                }
            }
        }
        self.new_lease(name, false).await
    }

    pub(super) async fn new_lease(&self, name: &str, streaming: bool) -> EventsResult<(String, ConsumerLease)> {
        let lease = self.engine.acquire(name).await.map_err(|e| self.error(e))?;
        let token = token_of(&format!(
            "{}{}",
            spot_http::request::RequestId::generate().as_str(),
            spot_http::request::RequestId::generate().as_str()
        ));
        lock(&self.held).insert(
            name.to_owned(),
            Held {
                token: token.clone(),
                lease: lease.lease.clone(),
                streaming,
            },
        );
        Ok((token, lease))
    }

    /// The lease the header names, if this process handed it out and it still holds.
    pub(super) async fn held_lease(&self, name: &str, header: Option<&str>) -> Option<ConsumerLease> {
        let held = lock(&self.held).get(name).cloned()?;
        if header != Some(held.token.as_str()) {
            return None;
        }
        match self.coordinator.renew(&held.lease, self.lease_ttl()).await {
            Ok(renewed) => {
                if let Some(entry) = lock(&self.held).get_mut(name) {
                    entry.lease = renewed.clone();
                }
                Some(ConsumerLease {
                    consumer: name.to_owned(),
                    lease: renewed,
                })
            }
            Err(_) => {
                lock(&self.held).remove(name);
                None
            }
        }
    }

    // ---- reading ------------------------------------------------------------------------

    /// One engine pull, serialised with the other operations on the partition. With `repeat`,
    /// a batch delivered earlier and not acknowledged is delivered again first: the partition
    /// is rewound to its checkpoint (the engine would read on past it).
    pub(super) async fn pull_once(
        &self,
        lease: &ConsumerLease,
        partition: u32,
        limit: u32,
        repeat: bool,
    ) -> EventsResult<spot_event_stream::Batch> {
        let name = lease.consumer.as_str();
        let key = (name.to_owned(), partition);
        let gate = self.op_lock(name, partition);
        let _turn = gate.lock().await;
        if repeat && lock(&self.outstanding).contains(&key) {
            let checkpoint = self
                .engine
                .checkpoint(name, partition)
                .await
                .map_err(|e| self.error(e))?;
            self.audit.quiet(&key, true);
            let rewound = self.engine.restart(lease, partition, checkpoint).await;
            self.audit.quiet(&key, false);
            rewound.map_err(|e| self.error(e))?;
            lock(&self.outstanding).remove(&key);
        }
        let batch = self
            .engine
            .pull(lease, partition, Some(limit), false)
            .await
            .map_err(|e| self.error(e))?;
        if let Some(next) = batch.next_position {
            if repeat {
                lock(&self.outstanding).insert(key.clone());
            }
            let mut sent = lock(&self.sent);
            let top = sent.get(&key).copied();
            if top.is_none_or(|t| order(next) > order(t)) {
                sent.insert(key, next);
            }
        }
        Ok(batch)
    }

    pub(super) async fn pull(
        &self,
        ctx: &RequestCtx,
        name: &str,
        header: Option<&str>,
        request: PullIn,
    ) -> EventsResult<PullOut> {
        let caller = self.caller(ctx).await?;
        let record = self.visible(&caller, name).await?;
        Self::partition_of(&record, request.partition)?;
        let owner = self.refresh_owner(&record.spec.owner).await;
        self.check_owner_rights(&record.spec, &owner)?;
        let (token, mut lease) = self.take_lease(name, header).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(u64::from(request.wait_ms));
        let renew_every = self.lease_ttl() / 3;
        let mut renewed_at = tokio::time::Instant::now();
        let mut stop = self.stop.subscribe();
        loop {
            let batch = self
                .pull_once(&lease, request.partition, request.limit, true)
                .await?;
            let now = tokio::time::Instant::now();
            if !batch.events.is_empty()
                || batch.window_full
                || batch.parked
                || now >= deadline
                || *stop.borrow()
            {
                let mut events = Vec::with_capacity(batch.events.len());
                for envelope in &batch.events {
                    events.push(serde_json::to_value(envelope).map_err(|_| {
                        tracing::error!("an event envelope could not be serialised");
                        ApiError::from_code(Code::HttpInternal)
                    })?);
                }
                let next = match batch.next_position {
                    Some(position) => position,
                    None => self
                        .engine
                        .checkpoint(name, request.partition)
                        .await
                        .map_err(|e| self.error(e))?,
                };
                return Ok(PullOut {
                    lease: token,
                    events,
                    next_position: wire(name, next),
                    skipped: batch.skipped,
                    window_full: batch.window_full,
                });
            }
            if now.duration_since(renewed_at) >= renew_every {
                let renewed = self
                    .coordinator
                    .renew(&lease.lease, self.lease_ttl())
                    .await
                    .map_err(|_| {
                        self.error(StreamError::ConsumerBusy {
                            name: name.to_owned(),
                            reason: "the lease lapsed".to_owned(),
                        })
                    })?;
                lease.lease = renewed.clone();
                if let Some(entry) = lock(&self.held).get_mut(name) {
                    entry.lease = renewed;
                }
                renewed_at = now;
            }
            let left = deadline.saturating_duration_since(now);
            tokio::select! {
                () = tokio::time::sleep(self.poll().min(left)) => {}
                _ = stop.changed() => {}
            }
        }
    }

    pub(super) async fn ack(
        &self,
        ctx: &RequestCtx,
        name: &str,
        header: Option<&str>,
        partition: u32,
        text: &str,
    ) -> EventsResult<CheckpointOut> {
        let caller = self.caller(ctx).await?;
        let record = self.visible(&caller, name).await?;
        Self::partition_of(&record, partition)?;
        let position = unwire(name, text)?;
        let refused = |why: &str| ApiError::new(Code::EventstreamAckRefused, why);
        let lease = self
            .held_lease(name, header)
            .await
            .ok_or_else(|| refused("The lease sent is not the one this consumer holds."))?;
        let key = (name.to_owned(), partition);
        let gate = self.op_lock(name, partition);
        let _turn = gate.lock().await;
        let checkpoint = self
            .engine
            .checkpoint(name, partition)
            .await
            .map_err(|e| self.error(e))?;
        let done = |at: Position| CheckpointOut {
            partition,
            position: wire(name, at),
        };
        if order(position) == order(checkpoint) {
            return Ok(done(checkpoint));
        }
        if order(position) < order(checkpoint) {
            return Err(refused("The position is behind the checkpoint."));
        }
        let delivered = lock(&self.sent).get(&key).copied();
        match self.engine.ack(&lease, partition, position).await {
            Ok(()) => {}
            // An event of a stream is acknowledged by its own `id`. The engine acknowledges the
            // last position of a batch, so an earlier delivered position moves the checkpoint
            // by a restart there (not audited as one; it is an acknowledgement). Only a position
            // of the generation that was delivered, at or before the last delivered one: a
            // well-formed token of another generation compares lower and is never accepted.
            Err(StreamError::AckRefused { .. })
                if delivered.is_some_and(|top| {
                    position.generation == top.generation && order(position) <= order(top)
                }) =>
            {
                self.audit.quiet(&key, true);
                let moved = self.engine.restart(&lease, partition, position).await;
                self.audit.quiet(&key, false);
                moved.map_err(|e| self.error(e))?;
                self.audit.record(AuditRecord::Acked {
                    actor: record.spec.owner.clone(),
                    consumer: name.to_owned(),
                    partition,
                    position: position.seq,
                    generation: position.generation,
                    at: self.clock.now_utc(),
                });
            }
            Err(error) => return Err(self.error(error)),
        }
        lock(&self.outstanding).remove(&key);
        Ok(done(position))
    }

    pub(super) async fn restart(
        &self,
        ctx: &RequestCtx,
        name: &str,
        header: Option<&str>,
        partition: u32,
        from: StartIn,
    ) -> EventsResult<CheckpointOut> {
        let caller = self.caller(ctx).await?;
        let record = self.visible(&caller, name).await?;
        Self::partition_of(&record, partition)?;
        let position = match from {
            StartIn::FromBeginning => Position {
                generation: 0,
                seq: 0,
            },
            StartIn::FromPosition(text) => unwire(name, &text)?,
            StartIn::Tail => {
                self.head.refresh(&self.backend).await?;
                self.head
                    .head()
                    .map_or(Position { generation: 0, seq: 0 }, spot_event_stream::position_from_feed)
            }
        };
        let lease = self.held_lease(name, header).await.ok_or_else(|| {
            self.error(StreamError::ConsumerBusy {
                name: name.to_owned(),
                reason: "the lease sent is not the one this consumer holds".to_owned(),
            })
        })?;
        let key = (name.to_owned(), partition);
        let gate = self.op_lock(name, partition);
        let _turn = gate.lock().await;
        self.engine
            .restart(&lease, partition, position)
            .await
            .map_err(|e| self.error(e))?;
        lock(&self.outstanding).remove(&key);
        lock(&self.sent).remove(&key);
        Ok(CheckpointOut {
            partition,
            position: wire(name, position),
        })
    }

    // ---- streams ------------------------------------------------------------------------

    pub(super) fn slot(self: &Arc<Self>, user: &str) -> EventsResult<StreamSlot> {
        let mut streams = lock(&self.streams);
        let mine = streams.per_user.get(user).copied().unwrap_or(0);
        if streams.total >= self.settings.max_streams || mine >= self.settings.max_streams_per_user {
            return Err(ApiError::from_code(Code::ApiRateLimited)
                .detail("retry_after", Json::from(5u64)));
        }
        streams.total += 1;
        *streams.per_user.entry(user.to_owned()).or_insert(0) += 1;
        Ok(StreamSlot {
            shared: Arc::clone(self),
            user: user.to_owned(),
        })
    }

    pub(super) async fn open_stream(
        self: &Arc<Self>,
        ctx: &RequestCtx,
        request: StreamIn,
    ) -> EventsResult<Box<dyn EventFeed>> {
        let caller = self.caller(ctx).await?;
        let record = self.visible(&caller, &request.consumer).await?;
        Self::partition_of(&record, request.partition)?;
        let owner = self.refresh_owner(&record.spec.owner).await;
        self.check_owner_rights(&record.spec, &owner)?;
        let name = request.consumer.as_str();
        let after = request.after.as_deref().map(|t| unwire(name, t)).transpose()?;
        let slot = self.slot(&caller.user)?;
        let (token, lease) = self.new_lease(name, true).await?;
        let checkpoint = self
            .engine
            .checkpoint(name, request.partition)
            .await
            .map_err(|e| self.error(e))?;
        // A resume position from another stream generation than the checkpoint is a lost
        // position: one `resync_required` frame, then the stream ends.
        let resync = after.is_some_and(|p| p.generation != checkpoint.generation);
        Ok(Box::new(StreamFeed {
            shared: Arc::clone(self),
            ctx: ctx.clone(),
            consumer: request.consumer,
            partition: request.partition,
            lease,
            token,
            after,
            resync: resync.then_some(checkpoint),
            queued: VecDeque::new(),
            revalidated: tokio::time::Instant::now(),
            renewed: tokio::time::Instant::now(),
            stop: self.stop.subscribe(),
            closed: false,
            _slot: slot,
        }))
    }
}

