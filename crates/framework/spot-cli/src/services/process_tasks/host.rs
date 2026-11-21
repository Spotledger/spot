//! Joining the parts of one site's inbox: [`Wiring`] and the [`TaskHost`] it builds.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use spot_bpm_engine::{InMemoryTimerPort, NodeHandler};
use spot_bpm_tasks::{
    ApproverResolver, BusinessCalendar, DecisionNodeHandler, Directory, DocFactsSource,
    EngineSlaTimers, InboxAccess, PrincipalSource, SlaTimerPort, TaskEngine, TaskService,
    TaskStore,
};
use spot_bpm_tasks_pg::PgTaskStore;
use spot_core::Clock;
use spot_http::api::ports::task::TaskPort;
use time::OffsetDateTime;
use tokio::runtime::Handle;

use super::adapters::{DirectoryOf, HostAccess, HostNotifier, HostResolver, HostSegregation};
use super::inbox::Inbox;
use super::node::HostDecisionHandler;
use super::ops::Core;
use super::seams::{
    Callers, DecisionSource, HostMetrics, Stats, StatsSnapshot, TaskDirectory, TaskNoticeSink,
};
use super::threads::{Hop, plain};

/// How a site's inbox is joined from its parts: the production wiring fills every field from the
/// live site, a test fills them with fakes (the store is always real PostgreSQL).
pub struct Wiring {
    /// The site's name (also the tenant of the task store).
    pub site: String,
    /// The source of `now`.
    pub clock: Arc<dyn Clock>,
    /// The runtime the hops run on.
    pub runtime: Handle,
    /// The durable task store.
    pub store: Arc<PgTaskStore>,
    /// Who a request's credential names.
    pub callers: Arc<dyn Callers>,
    /// Role members, managers, reports and active users.
    pub directory: Arc<dyn TaskDirectory>,
    /// A user's principal, for reading the guarded document.
    pub principals: Arc<dyn PrincipalSource>,
    /// The guarded document's facts.
    pub facts: Arc<dyn DocFactsSource>,
    /// The permission engine served now.
    pub decisions: Arc<dyn DecisionSource>,
    /// Approvers named by a rule (the rule graph's resolver when present).
    pub resolver: Arc<dyn ApproverResolver>,
    /// Resumes the Case.
    pub engine: Arc<dyn TaskEngine>,
    /// Where notices go, when anything takes them.
    pub sink: Option<Arc<dyn TaskNoticeSink>>,
    /// The calendar SLA time is counted on.
    pub calendar: Arc<dyn BusinessCalendar>,
}

/// One site's hosted inbox: the service, the port, the Decision node handler and the SLA loop's
/// operations.
pub struct TaskHost {
    pub(super) core: Arc<Core>,
    handler: Arc<HostDecisionHandler>,
    inbox: Arc<Inbox>,
}

impl Wiring {
    /// Joins the parts into one hosted inbox.
    #[must_use]
    pub fn build(self) -> TaskHost {
        let stats = Arc::new(Stats::default());
        let hop = Hop::new(self.runtime.clone());
        let directory = Arc::new(DirectoryOf(Arc::clone(&self.directory)));
        let access: Arc<dyn InboxAccess> = Arc::new(HostAccess {
            decisions: self.decisions,
            principals: self.principals,
            directory: Arc::clone(&directory) as Arc<dyn Directory>,
            facts: Arc::clone(&self.facts),
        });
        let timers = Arc::new(EngineSlaTimers::new(Arc::new(InMemoryTimerPort::new())));
        let store_dyn: Arc<dyn TaskStore> = Arc::clone(&self.store) as Arc<dyn TaskStore>;
        let service = Arc::new(
            TaskService::new(Arc::clone(&store_dyn), self.engine)
                .with_directory(Arc::clone(&directory) as Arc<dyn Directory>)
                .with_access(Arc::clone(&access))
                .with_segregation(Arc::new(HostSegregation {
                    store: Arc::clone(&store_dyn),
                    facts: self.facts,
                }))
                .with_notifier(Arc::new(HostNotifier {
                    sink: self.sink,
                    hop,
                    stats: Arc::clone(&stats),
                }))
                .with_calendar(Arc::clone(&self.calendar))
                .with_timers(Arc::clone(&timers) as Arc<dyn SlaTimerPort>)
                .with_metrics(Arc::new(HostMetrics {
                    site: self.site.clone(),
                    stats: Arc::clone(&stats),
                })),
        );
        let resolver = Arc::new(HostResolver {
            rules: self.resolver,
            directory: Arc::clone(&self.directory),
        });
        let handler = Arc::new(HostDecisionHandler {
            inner: DecisionNodeHandler::new(
                Arc::clone(&service),
                directory as Arc<dyn Directory>,
                resolver,
            ),
        });
        let core = Arc::new(Core {
            site: self.site,
            clock: self.clock,
            callers: self.callers,
            directory: self.directory,
            store: self.store,
            service,
            access,
            timers,
            calendar: self.calendar,
            replays: Mutex::new(BTreeMap::new()),
            schedule: Mutex::new(()),
            stats,
        });
        let inbox = Arc::new(Inbox {
            core: Arc::clone(&core),
        });
        TaskHost {
            core,
            handler,
            inbox,
        }
    }
}

impl TaskHost {
    /// The task service (the Decision node and the SLA loop act through it).
    #[must_use]
    pub fn service(&self) -> Arc<TaskService> {
        Arc::clone(&self.core.service)
    }

    /// The production `TaskPort`.
    #[must_use]
    pub fn port(&self) -> Arc<dyn TaskPort> {
        Arc::clone(&self.inbox) as Arc<dyn TaskPort>
    }

    /// The durable task store, for the modules that ask who is tied to a run (the process API).
    #[must_use]
    pub fn store(&self) -> Arc<dyn TaskStore> {
        Arc::clone(&self.core.store) as Arc<dyn TaskStore>
    }

    /// Cancels the tasks of a cancelled Case, for the process API.
    #[must_use]
    pub fn canceller(&self) -> Arc<dyn crate::services::process_api::CaseTaskCanceller> {
        Arc::new(CaseCanceller {
            core: Arc::clone(&self.core),
        })
    }

    /// The Decision node handler, for the process engine's registry.
    #[must_use]
    pub fn handler(&self) -> Arc<dyn NodeHandler> {
        Arc::clone(&self.handler) as Arc<dyn NodeHandler>
    }

    /// A snapshot of the counters.
    #[must_use]
    pub fn stats(&self) -> StatsSnapshot {
        self.core.stats.snapshot()
    }

    /// Fires the SLA steps due at `now`, on a plain thread. The ids of the tasks that moved.
    ///
    /// # Errors
    /// The task service's refusal (its code), or that the thread could not run.
    pub async fn tick(&self, now: OffsetDateTime) -> std::result::Result<Vec<String>, String> {
        let core = Arc::clone(&self.core);
        match plain(move || core.tick(now)).await {
            Some(Ok(fired)) => Ok(fired.iter().map(ToString::to_string).collect()),
            Some(Err(error)) => Err(error.code().to_owned()),
            None => Err("the tick thread did not run".to_owned()),
        }
    }

    /// Re-arms every open task's SLA timer from the store, on a plain thread. How many.
    ///
    /// # Errors
    /// The task service's refusal (its code), or that the thread could not run.
    pub async fn rearm(&self) -> std::result::Result<usize, String> {
        let core = Arc::clone(&self.core);
        match plain(move || core.rearm()).await {
            Some(Ok(armed)) => Ok(armed),
            Some(Err(error)) => Err(error.code().to_owned()),
            None => Err("the re-arm thread did not run".to_owned()),
        }
    }
}

/// The tasks of a cancelled Case, cancelled on a plain thread (the task store's bridge refuses a
/// runtime thread).
struct CaseCanceller {
    core: Arc<Core>,
}

impl crate::services::process_api::CaseTaskCanceller for CaseCanceller {
    fn cancel_case<'a>(
        &'a self,
        case: &'a str,
    ) -> spot_http::api::service::BoxFuture<'a, ()> {
        Box::pin(async move {
            let core = Arc::clone(&self.core);
            let filter = spot_bpm_tasks::TaskFilter {
                case: Some(case.to_owned()),
                ..spot_bpm_tasks::TaskFilter::default()
            };
            let done = super::threads::plain(move || {
                let now = core.clock.now_utc();
                let tasks = TaskStore::list(&*core.store, &filter)?;
                for task in tasks.iter().filter(|task| task.is_open()) {
                    core.service.cancel(&task.id, now)?;
                }
                Ok::<(), spot_bpm_tasks::TasksError>(())
            })
            .await;
            if !matches!(done, Some(Ok(()))) {
                tracing::warn!("the tasks of a cancelled run could not all be cancelled");
            }
        })
    }
}
