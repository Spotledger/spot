//! Multi-node and load helpers over [`StackHost`] (WP-TST-11.T3): a follower node's convergence,
//! the write-through check (a write through the engine that uses the new field), and a cloneable
//! writer handle for the load test.

use std::sync::Arc;
use std::time::{Duration, Instant};

use spot_app_api::Document;
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::prelude::{Name, RecordId, SiteId, Value};
use spot_core::SystemClock;
use spot_customise::Caller;
use spot_document::engine::Engine;
use spot_meta::{FieldSpec, FieldType};
use spot_postgres::PgBackend;
use tokio::runtime::Runtime;

use crate::stack::StackHost;

/// The DocType the checks write: the suite's standard `party`, whose only required field is `code`.
pub const PARTY: &str = "party";

fn name(text: &str) -> Result<Name, String> {
    Name::new(text).map_err(|e| format!("{text}: {e:?}"))
}

fn invocation() -> Result<Invocation, String> {
    use spot_core::Clock as _;
    Ok(Invocation {
        site: SiteId::new("l10-stack").map_err(|e| format!("{e:?}"))?,
        actor: Actor::System {
            component: name("l10_stack")?,
        },
        command_id: None,
        language: None,
        now: SystemClock::new().now_utc(),
    })
}

/// A handle that writes through one node's engine from any thread.
#[derive(Clone)]
pub struct Writer {
    rt: Arc<Runtime>,
    engine: Arc<Engine<PgBackend>>,
}

impl Writer {
    /// Creates a `party` with `code`, setting `field` (a text field) when given, then loads it
    /// back and checks the field came back. `Err` names what failed, never a value.
    ///
    /// # Errors
    ///
    /// The engine's refusal, or a loaded document that lacks the field.
    pub fn write_through(&self, code: &str, field: Option<&str>) -> Result<(), String> {
        let engine = self.engine.clone();
        let code = code.to_owned();
        let field = field.map(str::to_owned);
        self.rt.block_on(async move {
            let inv = invocation()?;
            let id = RecordId::new_v7();
            let mut doc = Document::new(name(PARTY)?, id);
            doc.set(name("code")?, Value::Text(code));
            if let Some(f) = &field {
                doc.set(name(f)?, Value::Text("x".to_owned()));
            }
            let saved = engine
                .create(&inv, doc)
                .await
                .map_err(|e| format!("create refused: {}", e.code))?
                .value;
            let loaded = engine
                .load(&inv, PARTY, saved.id())
                .await
                .map_err(|e| format!("load refused: {}", e.code))?
                .value;
            if let Some(f) = &field
                && !loaded.fields().keys().any(|k| k.as_str() == f)
            {
                return Err("the loaded document lacks the new field".to_owned());
            }
            Ok(())
        })
    }
}

impl StackHost {
    /// A writer over this node's engine.
    #[must_use]
    pub fn writer(&self) -> Writer {
        Writer {
            rt: self.rt.clone(),
            engine: self.engine.clone(),
        }
    }

    /// The metadata version this node serves.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.live.current().meta().version()
    }

    /// The version the engine's swap listener applied (the plan layer).
    #[must_use]
    pub fn plan_version(&self) -> u64 {
        self.applied.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The content hash of the metadata this node serves, as text.
    #[must_use]
    pub fn hash(&self) -> String {
        format!("{:?}", self.live.current().hash())
    }

    /// Adds the text custom field `name` to `party` through the service; returns the new version.
    ///
    /// # Errors
    ///
    /// The service's refusal, as text (the code and message, never a value).
    pub fn add_field(&self, field: &str) -> Result<u64, String> {
        let service = self.service.clone();
        let spec = FieldSpec::new(field, FieldType::Text);
        let who = Caller::new("l10-stack", &["System Manager"]);
        self.rt
            .block_on(async move {
                service
                    .add_custom_field(&who, PARTY, &spec, None)
                    .await
                    .map(|applied| applied.version.get())
            })
            .map_err(|e| format!("{}: {e}", e.code()))
    }

    /// Brings this node in line with the shared records (what the poller does on a tick).
    /// Returns the version it swapped to, if it was behind.
    ///
    /// # Errors
    ///
    /// The service's failure, as text.
    pub fn recover(&self) -> Result<Option<u64>, String> {
        let service = self.service.clone();
        self.rt
            .block_on(async move { service.recover().await })
            .map(|r| r.swapped.map(|v| v.get()))
            .map_err(|e| format!("{}: {e}", e.code()))
    }

    /// Recovers until this node serves at least `version`, polling with a deadline, and returns
    /// how long it took. No sleep is spent after the version arrives.
    ///
    /// # Errors
    ///
    /// The deadline passing, or a recovery failure.
    pub fn converge_to(&self, version: u64, within: Duration) -> Result<Duration, String> {
        let started = Instant::now();
        loop {
            self.recover()?;
            if self.version() >= version && self.plan_version() >= version {
                return Ok(started.elapsed());
            }
            if started.elapsed() > within {
                return Err(format!(
                    "still at version {} (plan {}) after {within:?}, wanted {version}",
                    self.version(),
                    self.plan_version()
                ));
            }
            std::thread::yield_now();
        }
    }
}
