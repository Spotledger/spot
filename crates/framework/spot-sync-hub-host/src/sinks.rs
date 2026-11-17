//! The push sink of the hub: one sink per tenant, routed by the batch's source site.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_sync_http::PushSink;
use spot_sync_protocol::{Batch, Error, Outcome};

/// Routes a pushed batch to the sink of its tenant.
///
/// The tenant is the **site of the batch's source**, and the guard has already checked that the
/// authenticated node owns that source (same site and partition) before the batch reaches this
/// sink, so a node cannot reach another tenant's sink. A tenant this hub does not serve is a
/// failure that applies nothing.
#[derive(Default)]
pub struct TenantSinks {
    sinks: BTreeMap<String, Arc<dyn PushSink>>,
}

impl TenantSinks {
    /// No tenant yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Serves `site` through `sink`.
    #[must_use]
    pub fn with(mut self, site: impl Into<String>, sink: Arc<dyn PushSink>) -> Self {
        self.sinks.insert(site.into(), sink);
        self
    }

    /// The tenants served.
    #[must_use]
    pub fn sites(&self) -> Vec<String> {
        self.sinks.keys().cloned().collect()
    }
}

impl PushSink for TenantSinks {
    fn on_batch(&self, batch: &Batch) -> Outcome {
        match self.sinks.get(batch.source.site()) {
            Some(sink) => sink.on_batch(batch),
            None => Outcome::Failed(Error::invalid_argument("this hub serves no such tenant")),
        }
    }
}
