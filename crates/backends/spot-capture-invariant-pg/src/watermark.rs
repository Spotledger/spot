//! The persisted watermark (`spot_capture_watermark`): where the scan resumes after a restart.

use deadpool_postgres::Pool;

use crate::error::CapturePgError;
use crate::sql;

/// Where the capture invariant has settled, per stream.
///
/// `0` means "nothing settled yet". A watermark of another generation than the stream's current
/// one is discarded by the scan (a fork or restore starts a new generation at 0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Watermarks {
    /// The generation both positions belong to.
    pub generation: i64,
    /// The last settled change-feed position.
    pub feed: i64,
    /// The last settled `doc_state` event position.
    pub events: i64,
}

impl Watermarks {
    /// Nothing settled.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Reads the stored watermarks. An absent row reads as 0.
    ///
    /// # Errors
    ///
    /// [`CapturePgError::WatermarkFailed`] when the table cannot be read (it is created by
    /// `spot migrate`).
    pub async fn load(pool: &Pool) -> Result<Self, CapturePgError> {
        let conn = pool
            .get()
            .await
            .map_err(|_| CapturePgError::WatermarkFailed)?;
        let rows = conn
            .query(sql::WATERMARKS, &[])
            .await
            .map_err(|_| CapturePgError::WatermarkFailed)?;
        let mut out = Self::none();
        for row in &rows {
            let source: String = row
                .try_get(0)
                .map_err(|_| CapturePgError::WatermarkFailed)?;
            let generation: i64 = row
                .try_get(1)
                .map_err(|_| CapturePgError::WatermarkFailed)?;
            let seq: i64 = row
                .try_get(2)
                .map_err(|_| CapturePgError::WatermarkFailed)?;
            // The two rows are written in one transaction, so they carry one generation.
            out.generation = generation;
            match source.as_str() {
                sql::SOURCE_FEED => out.feed = seq,
                sql::SOURCE_EVENTS => out.events = seq,
                _ => {}
            }
        }
        Ok(out)
    }

    /// Stores the watermarks, both rows in one transaction.
    ///
    /// # Errors
    ///
    /// [`CapturePgError::WatermarkFailed`] when the write fails.
    pub async fn save(&self, pool: &Pool) -> Result<(), CapturePgError> {
        let mut conn = pool
            .get()
            .await
            .map_err(|_| CapturePgError::WatermarkFailed)?;
        let tx = conn
            .transaction()
            .await
            .map_err(|_| CapturePgError::WatermarkFailed)?;
        for (source, seq) in [
            (sql::SOURCE_FEED, self.feed),
            (sql::SOURCE_EVENTS, self.events),
        ] {
            tx.execute(sql::SAVE_WATERMARK, &[&source, &self.generation, &seq])
                .await
                .map_err(|_| CapturePgError::WatermarkFailed)?;
        }
        tx.commit()
            .await
            .map_err(|_| CapturePgError::WatermarkFailed)
    }
}
