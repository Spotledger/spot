//! [`DocumentServiceFacts`]: the real [`DocFactsSource`], over the document engine.
//!
//! The inbox judges "may this user see the task?" on the subject document's real state and
//! values. This source loads the document through the site-bound [`DocumentService`] **as the
//! platform of the task's tenant** (an `Actor::System` of this component), not as the user: the
//! load only gathers facts for the permission decision, which then runs for the user. The
//! values mapping is the same as the realtime bridge's and the trigger source's.
//!
//! Fail closed: a subject that is not a record id, a document that is not found or cannot be
//! read, a document of another DocType than the task names, or a posted-type document without a
//! lifecycle state all answer `None`, and the access rule then denies. An empty fact set is
//! never built.
//!
//! # Async
//!
//! [`DocFactsSource::facts`] returns a boxed future that awaits the document service directly:
//! nothing blocks a thread and there is no poll bound, so a slow but healthy read still answers.
//! Dropping the future cancels the load. A host that wants a deadline wraps the call in its
//! runtime's timeout and treats the expiry as "cannot load" (the access rule then denies).

use std::sync::Arc;

use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::prelude::{Name, OffsetDateTime, RecordId, SiteId, Value};
use spot_permissions::DocFacts;

use crate::access::{AccessFuture, DocFactsSource};
use crate::task::Task;

/// Supplies the instant an invocation carries; time is never read from the wall clock here.
pub type NowFn = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

/// The component name the system actor carries.
const COMPONENT: &str = "bpm_tasks";

/// A [`DocFactsSource`] over a site-bound [`DocumentService`].
pub struct DocumentServiceFacts {
    service: Arc<dyn DocumentService>,
    site: SiteId,
    now: NowFn,
}

impl DocumentServiceFacts {
    /// A source reading `site`'s documents through `service`; `now` supplies the invocation
    /// instant.
    #[must_use]
    pub fn new(service: Arc<dyn DocumentService>, site: SiteId, now: NowFn) -> Self {
        Self { service, site, now }
    }

    fn invocation(&self) -> Option<Invocation> {
        Some(Invocation {
            site: self.site.clone(),
            actor: Actor::System {
                component: Name::new(COMPONENT).ok()?,
            },
            command_id: None,
            language: None,
            now: (self.now)(),
        })
    }

    async fn load(&self, doctype: &str, id: RecordId) -> Option<DocFacts> {
        let inv = self.invocation()?;
        let meta = self.service.meta(doctype).await.ok()?;
        let doc = self.service.load(&inv, doctype, id).await.ok()?.value;
        if doc.doctype().as_str() != doctype || doc.is_new() {
            return None;
        }
        let state = doc.state().map(|s| s.as_str().to_owned());
        if meta.is_posted && state.is_none() {
            return None;
        }
        let created_by = match doc.get("created_by") {
            Value::Text(text) => Some(text.clone()),
            _ => None,
        };
        Some(DocFacts {
            doctype: doctype.to_owned(),
            id: Some(id.to_string()),
            created_by,
            state,
            values: doc
                .fields()
                .iter()
                .map(|(name, value)| (name.as_str().to_owned(), value.clone()))
                .collect(),
        })
    }
}

impl DocFactsSource for DocumentServiceFacts {
    fn facts<'a>(&'a self, task: &'a Task) -> AccessFuture<'a, Option<DocFacts>> {
        Box::pin(async move {
            let doctype = task.doctype.as_deref()?;
            let id = parse_record_id(task.subject.as_deref()?)?;
            self.load(doctype, id).await
        })
    }
}

/// A record id from its text form, hyphenated or not.
fn parse_record_id(text: &str) -> Option<RecordId> {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        let part = hex.get(index * 2..index * 2 + 2)?;
        *byte = u8::from_str_radix(part, 16).ok()?;
    }
    RecordId::from_bytes(bytes).ok()
}
