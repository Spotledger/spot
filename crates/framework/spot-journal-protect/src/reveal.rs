//! The rebuild-side wrapper: reveal each image before it reaches the inner sink.

use serde_json::Value;
use spot_events_contract::envelope::Position;
use spot_image_upcast::Image;
use spot_journal_rebuild::{
    AppliedAudit, AppliedDelete, AppliedImage, ApplySink, PlanSummary, SinkError, SinkFuture,
};

use crate::image::{Identity, SharedFields, SharedKeys, reveal_image};

/// Wraps a rebuild sink so every state image is revealed before the inner sink sees it.
///
/// * An image whose subject's key is destroyed arrives with the documented placeholder in place of
///   each erasable personal field; the rest of the record, its version and its position are as
///   stored.
/// * A key store outage, or an envelope that is tampered with or moved, is a [`SinkError`]: the
///   executor aborts the rebuild. Nothing is replaced by a placeholder on that path, because a
///   rebuilt database that quietly lost data it could have read is worse than a stopped rebuild.
/// * Tombstones and audit entries are handed over untouched (an audit entry carries no personal
///   field of this mechanism's kind).
pub struct RevealSink<S: ApplySink> {
    inner: S,
    fields: SharedFields,
    keys: SharedKeys,
}

impl<S: ApplySink> RevealSink<S> {
    /// A sink that reveals with `keys` and then applies to `inner`.
    #[must_use]
    pub fn new(inner: S, fields: SharedFields, keys: SharedKeys) -> Self {
        Self {
            inner,
            fields,
            keys,
        }
    }

    /// The wrapped sink.
    #[must_use]
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// Gives the wrapped sink back.
    #[must_use]
    pub fn into_inner(self) -> S {
        self.inner
    }

    fn revealed(&self, applied: &AppliedImage) -> Result<Option<Image>, SinkError> {
        let identity = Identity {
            entity: &applied.entity,
            id: &applied.id,
            version: applied.version,
        };
        let body = Value::Object(applied.image.body().clone());
        let revealed =
            reveal_image(&self.fields, &self.keys, &identity, &body).map_err(|error| {
                SinkError::new(format!(
                    "{} ({}) for {} {}",
                    error.code(),
                    error.pii_code().unwrap_or("none"),
                    applied.entity,
                    applied.id
                ))
            })?;
        let Some(Value::Object(body)) = revealed else {
            return Ok(None);
        };
        Ok(Some(Image::new(
            applied.image.entity(),
            applied.image.entity_schema(),
            body,
        )))
    }
}

impl<S: ApplySink> ApplySink for RevealSink<S> {
    fn begin<'a>(&'a mut self, summary: &'a PlanSummary) -> SinkFuture<'a> {
        self.inner.begin(summary)
    }

    fn apply_image(&mut self, mut image: AppliedImage) -> SinkFuture<'_> {
        match self.revealed(&image) {
            Ok(Some(revealed)) => image.image = revealed,
            Ok(None) => {}
            Err(error) => return Box::pin(std::future::ready(Err(error))),
        }
        self.inner.apply_image(image)
    }

    fn apply_delete(&mut self, delete: AppliedDelete) -> SinkFuture<'_> {
        self.inner.apply_delete(delete)
    }

    fn apply_audit(&mut self, audit: AppliedAudit) -> SinkFuture<'_> {
        self.inner.apply_audit(audit)
    }

    fn commit(&mut self, last_position: Position) -> SinkFuture<'_> {
        self.inner.commit(last_position)
    }

    fn abort<'a>(&'a mut self, reason: &'a str) -> SinkFuture<'a> {
        self.inner.abort(reason)
    }
}
