//! [`PgOutboxImages`]: the `ImageSource` of the hub's closure builder over the tenant's outbox.
//!
//! Every captured change of a record is a full-state `doc_state` event (ADR-061): its payload is
//! `{"op", "entity_schema", "image"}` with the whole record, parent and child rows. The current
//! image of a record is therefore the image of its newest published `doc_state` event in the
//! stream's generation, and a newest event whose `op` is `delete` is a tombstone. Like the stream
//! this reads the tenant's **own database**, whose outbox has no tenant column.

use serde_json::Value;
use spot_sync_closure::{ClosureError, ImageSource, RecordKey, SourceImage};

use crate::db::Db;
use crate::error::OpError;

/// The topic of a document's committed image.
const TOPIC_DOC_STATE: &str = "doc_state";

/// Deadline of one image read.
pub const IMAGE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);

const NEWEST: &str = "SELECT p.seq, o.payload \
    FROM spot_outbox_pub p JOIN spot_outbox o ON o.id = p.event_id \
    WHERE p.generation = $1 AND o.topic = $2 AND o.subject_entity = $3 \
    AND o.subject_id = $4::text::uuid ORDER BY p.seq DESC LIMIT 1";

/// Reads the current image of a record from the newest `doc_state` event of the outbox.
pub struct PgOutboxImages {
    db: Db,
    generation: u64,
}

impl std::fmt::Debug for PgOutboxImages {
    /// Names the tenant and the generation.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgOutboxImages")
            .field("tenant", &self.db.tenant)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl PgOutboxImages {
    /// Images of generation `generation` of the tenant's outbox.
    #[must_use]
    pub fn new(db: Db, generation: u64) -> Self {
        Self { db, generation }
    }
}

fn failed(reason: &str) -> ClosureError {
    ClosureError::SourceFailed {
        reason: reason.to_owned(),
    }
}

fn is_uuid(text: &str) -> bool {
    text.len() == 36
        && text.bytes().enumerate().all(|(at, b)| match at {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

impl ImageSource for PgOutboxImages {
    fn image(&self, key: &RecordKey) -> Result<Option<SourceImage>, ClosureError> {
        // A record id that is not an id names no record: nothing to read, nothing to fail.
        if !is_uuid(&key.id) {
            return Ok(None);
        }
        let generation = i64::try_from(self.generation).map_err(|_| failed("the generation"))?;
        let (entity, id) = (key.entity.clone(), key.id.clone());
        let row: Option<(u64, String)> = self
            .db
            .run(IMAGE_DEADLINE, move |conn| {
                Box::pin(async move {
                    let found = conn
                        .query_opt(NEWEST, &[&generation, &TOPIC_DOC_STATE, &entity, &id])
                        .await?;
                    found
                        .map(|row| {
                            let seq =
                                u64::try_from(row.get::<_, i64>(0)).map_err(|_| OpError::Decode)?;
                            Ok((seq, row.get::<_, String>(1)))
                        })
                        .transpose()
                })
            })
            .map_err(|_| failed("the outbox could not be read"))?;
        let Some((position, payload)) = row else {
            return Ok(None);
        };
        let event: Value =
            serde_json::from_str(&payload).map_err(|_| failed("an event did not decode"))?;
        if event.get("op").and_then(Value::as_str) == Some("delete") {
            return Ok(Some(SourceImage {
                key: key.clone(),
                image: Value::Null,
                position,
                deleted: true,
            }));
        }
        let image = event
            .get("image")
            .cloned()
            .ok_or_else(|| failed("an event carries no image"))?;
        Ok(Some(SourceImage::new(key.clone(), image, position)))
    }
}

const GENERATION: &str = "SELECT COALESCE((SELECT value::bigint FROM spot_meta \
    WHERE key = 'feed_generation'), 1)";

/// The generation of the tenant's outbox stream now (1 before any was started).
///
/// # Errors
/// The store's failure.
pub fn current_generation(db: &Db) -> Result<u64, crate::error::PgHubError> {
    db.run(IMAGE_DEADLINE, move |conn| {
        Box::pin(async move {
            let row = conn.query_one(GENERATION, &[]).await?;
            u64::try_from(row.get::<_, i64>(0)).map_err(|_| OpError::Decode)
        })
    })
    .map_err(crate::error::map)
}
