use crate::{Caller, RegistryError, Release, Result, Version, Visibility};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;
/// Tokenized catalogue query with conjunctive filters.
#[derive(Clone, Debug)]
pub struct SearchQuery {
    /// Case-insensitive words; punctuation and underscores delimit tokens.
    pub text: String,
    /// Exact publisher ID.
    pub publisher: Option<String>,
    /// Exact category, case-insensitive.
    pub category: Option<String>,
    /// Host version that must satisfy requires_host.
    pub compatible_host: Option<Version>,
    /// One capability: network, email, files, doctypes.read or doctypes.write.
    pub capability: Option<String>,
    /// Pre-release/inactive visibility.
    pub visibility: Visibility,
    /// Authenticated caller for inactive access, rechecked by Registry.
    pub caller: Option<Caller>,
    /// Page size, 1..=100.
    pub limit: usize,
    /// Opaque query-bound, snapshot-bound cursor, returned by the previous page.
    pub cursor: Option<String>,
}
impl Default for SearchQuery {
    fn default() -> Self {
        Self {
            text: String::new(),
            publisher: None,
            category: None,
            compatible_host: None,
            capability: None,
            visibility: Visibility::default(),
            caller: None,
            limit: 20,
            cursor: None,
        }
    }
}
/// Deterministic page of matching releases.
#[derive(Clone, Debug)]
pub struct SearchPage {
    /// One latest eligible release per app, in rank order.
    pub releases: Vec<Release>,
    /// Absent at end of results.
    pub next_cursor: Option<String>,
}
fn tokens(s: &str) -> BTreeSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect()
}
pub(crate) fn authorized(
    release: &Release,
    visibility: &Visibility,
    caller: Option<&Caller>,
) -> bool {
    if !visibility.include_pre && !release.version.pre.is_empty() {
        return false;
    }
    release.active()
        || (visibility.include_inactive && caller.is_some_and(|c| c.publisher == release.publisher))
}
pub(crate) fn page(mut rows: Vec<Release>, q: &SearchQuery, revision: u64) -> Result<SearchPage> {
    if q.limit == 0 || q.limit > 100 {
        return Err(RegistryError::new(
            "registry.invalid_query",
            "limit must be 1..=100",
        ));
    }
    if q.visibility.include_inactive && q.caller.is_none() {
        return Err(RegistryError::new(
            "registry.unauthorized",
            "inactive listings require authenticated ownership",
        ));
    }
    let text = q.text.to_lowercase();
    let wanted = tokens(&text);
    rows.retain(|r| {
        authorized(r, &q.visibility, q.caller.as_ref())
            && q.publisher.as_ref().is_none_or(|p| p == &r.publisher)
            && q.category
                .as_ref()
                .is_none_or(|c| c.eq_ignore_ascii_case(&r.category))
            && q.compatible_host
                .as_ref()
                .is_none_or(|v| r.requires_host.as_ref().is_none_or(|req| req.matches(v)))
            && q.capability.as_ref().is_none_or(|c| match c.as_str() {
                "network" => !r.capabilities.network.is_empty(),
                "email" => r.capabilities.email,
                "files" => r.capabilities.files != spot_manifest::FileAccess::None,
                // Resource-scoped capabilities use the resource.operation vocabulary;
                // neither segment is an error-code namespace.
                _ => match c.split_once('.') {
                    Some(("doctypes", "read")) => !r.capabilities.doctypes.read.is_empty(),
                    Some(("doctypes", "write")) => !r.capabilities.doctypes.write.is_empty(),
                    _ => false,
                },
            })
    });
    // Pick latest eligible release per app before ranking; filters may select an older
    // compatible version, which is useful to an installer on an older host.
    rows.sort_by(|a, b| a.app.cmp(&b.app).then(b.version.cmp(&a.version)));
    rows.dedup_by(|a, b| a.app == b.app);
    let rank = |r: &Release| {
        let exact = r.app.as_str() == text;
        let prefix = !text.is_empty() && r.app.as_str().starts_with(&text);
        let corpus = tokens(&format!("{} {} {}", r.app.as_str(), r.title, r.description));
        let overlap = wanted.intersection(&corpus).count();
        (exact, prefix, overlap)
    };
    rows.retain(|r| {
        let (e, p, n) = rank(r);
        wanted.is_empty() || e || p || n > 0
    });
    rows.sort_by(|a, b| rank(b).cmp(&rank(a)).then(a.app.cmp(&b.app)));
    // Hash a length-framed query, authenticated identity and index revision. Cursors
    // cannot cross query/visibility changes or silently skip rows after mutation.
    let fields = [
        q.text.clone(),
        format!("{:?}", q.publisher),
        format!("{:?}", q.category),
        format!("{:?}", q.compatible_host),
        format!("{:?}", q.capability),
        format!("{:?}", q.visibility),
        q.caller
            .as_ref()
            .map_or(String::new(), |c| c.publisher.clone()),
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
        if offset == 0 || offset >= rows.len() || offset % q.limit != 0 {
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
fn cursor_error() -> RegistryError {
    RegistryError::new(
        "registry.invalid_cursor",
        "cursor is invalid or its query snapshot changed",
    )
}
