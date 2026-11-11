//! The seams the engine asks the host for: the outbox head, the owners' principals, the
//! committed facts of a document, the field guard's sinks, the read filter, the stream rights and
//! the audit sink.

use super::*;

// ---------------------------------------------------------------------------------------------
// The seams the engine asks the host for
// ---------------------------------------------------------------------------------------------

/// The cached head of the outbox: where a `tail` consumer starts.
#[derive(Default)]
pub(super) struct HeadCache {
    pub(super) value: Mutex<Option<FeedPosition>>,
}

impl FeedHead for HeadCache {
    fn head(&self) -> Option<FeedPosition> {
        *lock(&self.value)
    }
}

impl HeadCache {
    /// Reads the outbox from the last known head to its end. A generation change (a restore)
    /// restarts the scan from the feed's start.
    pub(super) async fn refresh(&self, backend: &PgBackend) -> EventsResult<()> {
        let mut after = (*lock(&self.value)).unwrap_or(FeedPosition::START);
        loop {
            match backend.read_outbox(after, HEAD_PAGE).await {
                Ok(records) => match records.last() {
                    Some(last) => after = last.position,
                    None => break,
                },
                Err(error) if error.kind == ErrorKind::ResyncRequired && after != FeedPosition::START => {
                    after = FeedPosition::START;
                }
                Err(error) => return Err(storage_error_to_api(&error, None)),
            }
        }
        *lock(&self.value) = Some(after);
        Ok(())
    }
}

/// The principals of consumer owners, resolved before a pull so the engine's synchronous read
/// filter can use them. An owner never resolved has no assignments, so it reads nothing.
#[derive(Default)]
pub(super) struct Principals {
    pub(super) map: Mutex<HashMap<String, Principal>>,
}

impl spot_event_stream::PrincipalSource for Principals {
    fn principal(&self, owner: &str) -> Principal {
        lock(&self.map)
            .get(owner)
            .cloned()
            .unwrap_or_else(|| Principal::user(owner, Vec::new()))
    }
}

/// The committed facts of a document, loaded as the platform for the read filter.
pub(super) struct LiveFacts {
    pub(super) live: Arc<SiteLive>,
    pub(super) clock: Arc<dyn Clock>,
    pub(super) site: SiteId,
}

thread_local! {
    /// Set by [`LiveFacts::facts`] when the document could not be read (deleted, store error,
    /// wrong runtime): the permission engine reads empty facts as "no value to restrict on", so
    /// a caller holding a scoped (user-permission) role must not be served on that decision.
    static FACTS_MISSING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl SubjectFacts for LiveFacts {
    fn facts(&self, doctype: &str, id: &spot_storage_contract::RecordId) -> DocFacts {
        FACTS_MISSING.with(|flag| flag.set(true));
        let none = DocFacts::default();
        // The engine's filter is synchronous: reading a document means waiting on the runtime.
        // That is only sound on the multi-thread runtime; anywhere else the facts are empty and
        // a rule that needs them does not match (fail closed).
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return none;
        };
        if handle.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
            return none;
        }
        let (Some(engine), Ok(component)) = (self.live.engine(), Name::new("events")) else {
            return none;
        };
        let invocation = Invocation {
            site: self.site.clone(),
            actor: Actor::System { component },
            command_id: None,
            language: None,
            now: self.clock.now_utc(),
        };
        let doctype = doctype.to_owned();
        let id = *id;
        let loaded = tokio::task::block_in_place(|| {
            handle.block_on(async move { engine.load(&invocation, &doctype, id).await })
        });
        let Ok(loaded) = loaded else {
            return none;
        };
        let doc = loaded.value;
        FACTS_MISSING.with(|flag| flag.set(false));
        DocFacts {
            doctype: doc.doctype().as_str().to_owned(),
            id: (!doc.is_new()).then(|| doc.id().to_string()),
            created_by: doc
                .fields()
                .iter()
                .find(|(name, _)| name.as_str() == "created_by")
                .and_then(|(_, value)| match value {
                    Value::Text(text) => Some(text.clone()),
                    _ => None,
                }),
            state: doc.state().map(|s| s.as_str().to_owned()),
            values: doc
                .fields()
                .iter()
                .map(|(name, value)| (name.as_str().to_owned(), value.clone()))
                .collect(),
        }
    }
}

/// The guard's sinks: a record names who, what and through which door, never a value.
pub(super) struct GuardSinks;

impl TraceSink for GuardSinks {
    fn offer(&self, record: FieldAccessRecord) -> Offer {
        tracing::info!(
            target: "spot::audit::field_access",
            who = %record.who,
            doctype = %record.doctype,
            field = %record.field,
            via = record.via.as_str(),
            authorised = record.authorised,
            "field access"
        );
        Offer::Accepted
    }
}

impl spot_field_guard::AuditSink for GuardSinks {
    fn offer(&self, _record: GuardAudit) -> Offer {
        tracing::info!(target: "spot::audit::field_guard", "field guard record");
        Offer::Accepted
    }
}

/// The read filter and field projection over the permission snapshot served now. The
/// permission engine, the field guard and the metadata change together with each swap, so the
/// pair is rebuilt when the served snapshot is not the one it was built from.
pub(super) struct LiveVisibility {
    pub(super) live: Arc<SiteLive>,
    pub(super) facts: Arc<LiveFacts>,
    pub(super) principals: Arc<Principals>,
    pub(super) clock: Arc<dyn Clock>,
    pub(super) cached: Mutex<Option<(Arc<Served>, Arc<PermissionVisibility>)>>,
}

impl LiveVisibility {
    pub(super) fn current(&self) -> Option<Arc<PermissionVisibility>> {
        let served = self.live.served()?;
        let mut cached = lock(&self.cached);
        if let Some((from, visibility)) = cached.as_ref()
            && Arc::ptr_eq(from, &served)
        {
            return Some(Arc::clone(visibility));
        }
        let guard = FieldGuard::new(
            Arc::clone(&served.decision),
            Arc::new(MetaProtection::new(&served.meta)),
            Arc::new(GuardSinks),
            Arc::new(GuardSinks),
            Arc::clone(&self.clock),
        );
        let visibility = Arc::new(PermissionVisibility::new(
            Arc::clone(&served.decision),
            guard,
            Arc::clone(&self.facts) as Arc<dyn SubjectFacts>,
            Arc::clone(&self.principals) as Arc<dyn spot_event_stream::PrincipalSource>,
        ));
        *cached = Some((served, Arc::clone(&visibility)));
        Some(visibility)
    }
}

impl Visibility for LiveVisibility {
    fn may_read(&self, owner: &str, subject: &Subject) -> std::result::Result<bool, StreamError> {
        let Some(visibility) = self.current() else {
            return Ok(false);
        };
        FACTS_MISSING.with(|flag| flag.set(false));
        let allowed = visibility.may_read(owner, subject)?;
        // Fail closed: without the document's values a value restriction on the owner's role
        // cannot be applied, so an owner with any scoped assignment reads nothing here.
        let missing = FACTS_MISSING.with(|flag| flag.replace(false));
        if allowed && missing {
            let principal = spot_event_stream::PrincipalSource::principal(&*self.principals, owner);
            if principal.assignments.iter().any(|a| !a.scope.is_empty()) {
                return Ok(false);
            }
        }
        Ok(allowed)
    }

    fn project(
        &self,
        owner: &str,
        subject: &Subject,
        data: &mut serde_json::Map<String, Json>,
    ) -> std::result::Result<(), StreamError> {
        match self.current() {
            Some(visibility) => visibility.project(owner, subject, data),
            None => {
                data.clear();
                Err(StreamError::Permission {
                    code: spot_field_guard::error::UNKNOWN_DOCTYPE,
                    message: "no permission snapshot is served".to_owned(),
                })
            }
        }
    }
}

/// The owners that hold the stream right for a reserved topic class right now.
#[derive(Default)]
pub(super) struct Rights {
    pub(super) owners: Mutex<HashSet<String>>,
}

impl Rights {
    pub(super) fn set(&self, owner: &str, allowed: bool) {
        let mut owners = lock(&self.owners);
        if allowed {
            owners.insert(owner.to_owned());
        } else {
            owners.remove(owner);
        }
    }
}

impl StreamRights for Rights {
    fn may_stream_class(&self, owner: &str, _class: &str) -> bool {
        lock(&self.owners).contains(owner)
    }
}

/// The audit sink: one structured line per registered, restarted, acknowledged, parked or
/// deleted consumer. A restart the host made itself to repeat a batch is not audited.
pub(super) struct EventAudit {
    pub(super) site: String,
    pub(super) quiet: Mutex<HashSet<(String, u32)>>,
}

impl EventAudit {
    pub(super) fn quiet(&self, key: &(String, u32), on: bool) {
        let mut quiet = lock(&self.quiet);
        if on {
            quiet.insert(key.clone());
        } else {
            quiet.remove(key);
        }
    }
}

impl AuditSink for EventAudit {
    fn record(&self, record: AuditRecord) {
        let site = self.site.as_str();
        match record {
            AuditRecord::Registered {
                actor, consumer, ..
            } => tracing::info!(target: AUDIT_TARGET, site, action = "registered", %actor, %consumer),
            AuditRecord::Deleted {
                actor, consumer, ..
            } => tracing::info!(target: AUDIT_TARGET, site, action = "deleted", %actor, %consumer),
            AuditRecord::Acked {
                actor,
                consumer,
                partition,
                position,
                generation,
                ..
            } => tracing::info!(
                target: AUDIT_TARGET, site, action = "acknowledged", %actor, %consumer,
                partition, generation, position
            ),
            AuditRecord::Restarted {
                actor,
                consumer,
                partition,
                position,
                generation,
                ..
            } => {
                if lock(&self.quiet).remove(&(consumer.clone(), partition)) {
                    return;
                }
                tracing::info!(
                    target: AUDIT_TARGET, site, action = "restarted", %actor, %consumer,
                    partition, generation, position
                );
            }
            AuditRecord::Parked {
                consumer,
                partition,
                ..
            } => tracing::warn!(target: AUDIT_TARGET, site, action = "parked", %consumer, partition),
        }
    }
}

