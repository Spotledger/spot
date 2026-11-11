//! The read-only presence check for the appender tables (WP-AI-11.T5).

use deadpool_postgres::Pool;

use crate::appender_sql::{
    APPENDER_COLUMNS, BOOKMARK_TABLE, COLUMNS_OF, DOC_INDEX_TABLE, EVENT_INDEX_TABLE,
    SITE_CHECKPOINT_TABLE,
};
use crate::error::{APPENDER_TABLES_MISSING, Error, Result, db, pool};

/// What [`verify_appender`] found: each table of the appender state is present with the columns
/// this crate expects. The state is usable only when [`AppenderTables::is_complete`] is true.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppenderTables {
    /// `spot_audit_bookmark`.
    pub bookmark: bool,
    /// `spot_audit_event_index`.
    pub event_index: bool,
    /// `spot_audit_doc_index`.
    pub doc_index: bool,
    /// `spot_audit_site_checkpoint`.
    pub site_checkpoint: bool,
}

impl AppenderTables {
    /// Whether every table is present with the right columns.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.missing().is_empty()
    }

    /// The names of the tables that are missing or have other columns, in a fixed order.
    #[must_use]
    pub fn missing(&self) -> Vec<&'static str> {
        [
            (self.bookmark, BOOKMARK_TABLE),
            (self.event_index, EVENT_INDEX_TABLE),
            (self.doc_index, DOC_INDEX_TABLE),
            (self.site_checkpoint, SITE_CHECKPOINT_TABLE),
        ]
        .into_iter()
        .filter_map(|(present, name)| (!present).then_some(name))
        .collect()
    }

    /// `Ok` when complete.
    ///
    /// # Errors
    ///
    /// [`crate::APPENDER_TABLES_MISSING`], naming the tables (fixed names, never data).
    pub fn require_complete(&self) -> Result<()> {
        let missing = self.missing();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(Error::new(
                APPENDER_TABLES_MISSING,
                format!(
                    "the audit appender state is not installed; missing or changed: {}",
                    missing.join(", ")
                ),
            ))
        }
    }
}

/// Reads, without changing anything, whether the four appender tables are present with the
/// columns this crate expects. A host calls it at start-up, beside [`crate::verify`], and refuses
/// to serve a site that lacks them.
///
/// # Errors
///
/// [`crate::UNAVAILABLE`] when the database or its pool fails.
pub async fn verify_appender(db_pool: &Pool) -> Result<AppenderTables> {
    let client = db_pool
        .get()
        .await
        .map_err(|e| pool("verify appender", &e))?;
    let mut found = [false; 4];
    for (slot, (table, columns)) in found.iter_mut().zip(APPENDER_COLUMNS) {
        let rows = client
            .query(COLUMNS_OF, &[&table])
            .await
            .map_err(|e| db("verify appender", &e))?;
        let mut names = Vec::with_capacity(rows.len());
        for row in &rows {
            let name: String = row.try_get(0).map_err(|e| db("verify appender", &e))?;
            names.push(name);
        }
        *slot = names.iter().map(String::as_str).eq(columns.iter().copied());
    }
    Ok(AppenderTables {
        bookmark: found[0],
        event_index: found[1],
        doc_index: found[2],
        site_checkpoint: found[3],
    })
}
