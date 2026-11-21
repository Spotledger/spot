//! Catalogue search: PostgreSQL narrows the candidates, the contract ranks them.
//!
//! The in-memory index defines the ranking: one latest eligible release per app (chosen after the
//! filters, so a host filter may select an older compatible version), ordered by
//! `(exact name, name prefix, overlap of query tokens with the app name, title and description)`,
//! then app name, with a cursor bound to the query, the caller and the index revision. Semantic
//! version order and host-requirement matching cannot be done in SQL, so the SQL side applies only
//! the filters that can (visibility, publisher, category, capability flags, and "shares a token
//! with the query, or is named like it") using the GIN index on `search_tokens`, and this module
//! applies the rest on the returned rows exactly as the reference does. Ties are broken by app
//! name, so the order is stable.

use sha2::{Digest as _, Sha256};
use spot_registry::{RegistryError, Release, Result, SearchPage, SearchQuery};
use tokio_postgres::Transaction;
use tokio_postgres::types::ToSql;

use crate::codec::{RELEASE_COLUMNS, corpus, release_from_row, tokens};
use crate::error::query_failed;

/// The SQL clause for one capability name; an unknown name matches nothing.
fn capability_clause(capability: Option<&str>) -> &'static str {
    match capability {
        None => "TRUE",
        Some("network") => "cap_network",
        Some("email") => "cap_email",
        Some("files") => "cap_files <> 'none'",
        Some(c) if c.split_once('.') == Some(("doctypes", "read")) => "cap_doc_read",
        Some(c) if c.split_once('.') == Some(("doctypes", "write")) => "cap_doc_write",
        Some(_) => "FALSE",
    }
}

/// Checks the query the way the reference does, before anything is read.
pub(crate) fn validate(query: &SearchQuery) -> Result<()> {
    if query.limit == 0 || query.limit > 100 {
        return Err(RegistryError::new(
            "registry.invalid_query",
            "limit must be 1..=100",
        ));
    }
    if query.visibility.include_inactive && query.caller.is_none() {
        return Err(RegistryError::new(
            "registry.unauthorized",
            "inactive listings require authenticated ownership",
        ));
    }
    Ok(())
}

/// Reads the candidate releases and the revision in one repeatable-read transaction.
pub(crate) async fn candidates(
    tx: &Transaction<'_>,
    query: &SearchQuery,
) -> Result<(Vec<Release>, u64)> {
    let text = query.text.to_lowercase();
    let wanted: Vec<String> = tokens(&text).into_iter().collect();
    let no_text = wanted.is_empty();
    let caller = query.caller.as_ref().map(|c| c.publisher().to_owned());
    let category = query.category.as_ref().map(|c| c.to_ascii_lowercase());
    let sql = format!(
        "SELECT {RELEASE_COLUMNS} FROM spot_control.spot_registry_release \
         WHERE ($1 OR NOT is_pre) \
           AND ((yanked_at IS NULL AND withdrawn_at IS NULL) OR ($2 AND publisher = $3)) \
           AND ($4::text IS NULL OR publisher = $4) \
           AND ($5::text IS NULL OR category_key = $5) \
           AND ({}) \
           AND ($6 OR app = $7 OR ($7 <> '' AND starts_with(app, $7)) OR search_tokens && $8::text[])",
        capability_clause(query.capability.as_deref())
    );
    let params: [&(dyn ToSql + Sync); 8] = [
        &query.visibility.include_pre,
        &query.visibility.include_inactive,
        &caller,
        &query.publisher,
        &category,
        &no_text,
        &text,
        &wanted,
    ];
    let rows = tx.query(&sql, &params).await.map_err(query_failed)?;
    let releases = rows
        .iter()
        .map(release_from_row)
        .collect::<Result<Vec<_>>>()?;
    let revision = crate::ops::revision(tx).await?;
    Ok((releases, revision))
}

fn cursor_error() -> RegistryError {
    RegistryError::new(
        "registry.invalid_cursor",
        "cursor is invalid or its query snapshot changed",
    )
}

/// Picks the latest eligible release per app, ranks and pages, as the reference does.
pub(crate) fn rank_and_page(
    mut rows: Vec<Release>,
    q: &SearchQuery,
    revision: u64,
) -> Result<SearchPage> {
    rows.retain(|r| {
        q.compatible_host
            .as_ref()
            .is_none_or(|v| r.requires_host.as_ref().is_none_or(|req| req.matches(v)))
    });
    rows.sort_by(|a, b| a.app.cmp(&b.app).then(b.version.cmp(&a.version)));
    rows.dedup_by(|a, b| a.app == b.app);
    let text = q.text.to_lowercase();
    let wanted = tokens(&text);
    let rank = |r: &Release| {
        let exact = r.app.as_str() == text;
        let prefix = !text.is_empty() && r.app.as_str().starts_with(&text);
        let overlap = wanted
            .intersection(&corpus(&r.app, &r.title, &r.description))
            .count();
        (exact, prefix, overlap)
    };
    rows.retain(|r| {
        let (e, p, n) = rank(r);
        wanted.is_empty() || e || p || n > 0
    });
    rows.sort_by(|a, b| rank(b).cmp(&rank(a)).then(a.app.cmp(&b.app)));
    let fields = [
        q.text.clone(),
        format!("{:?}", q.publisher),
        format!("{:?}", q.category),
        format!("{:?}", q.compatible_host),
        format!("{:?}", q.capability),
        format!("{:?}", q.visibility),
        q.caller
            .as_ref()
            .map_or(String::new(), |c| c.publisher().to_owned()),
        q.limit.to_string(),
        revision.to_string(),
    ];
    let mut hash = Sha256::new();
    for field in fields {
        hash.update((field.len() as u64).to_le_bytes());
        hash.update(field.as_bytes());
    }
    let fingerprint = hash
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let start = if let Some(cursor) = &q.cursor {
        let (key, offset) = cursor.split_once(':').ok_or_else(cursor_error)?;
        if key != fingerprint {
            return Err(cursor_error());
        }
        let offset = usize::from_str_radix(offset, 16).map_err(|_| cursor_error())?;
        if offset == 0 || offset >= rows.len() || !offset.is_multiple_of(q.limit) {
            return Err(cursor_error());
        }
        offset
    } else {
        0
    };
    let end = start.saturating_add(q.limit).min(rows.len());
    let next_cursor = (end < rows.len()).then(|| format!("{fingerprint}:{end:x}"));
    Ok(SearchPage {
        releases: rows[start..end].to_vec(),
        next_cursor,
    })
}
