//! The read cache: entries keyed by what was read, kept with the `ETag` they were read at.

use std::collections::BTreeMap;

/// The most entries held; past it the oldest key (in key order) is dropped.
const MAX_ENTRIES: usize = 512;

/// What a cache entry is the answer to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CacheKey {
    /// One document.
    Doc {
        /// The DocType.
        doctype: String,
        /// The document id.
        id: String,
    },
    /// A list of documents of one DocType under one query.
    List {
        /// The DocType.
        doctype: String,
        /// The query string.
        query: String,
    },
}

impl CacheKey {
    fn doctype(&self) -> &str {
        match self {
            Self::Doc { doctype, .. } | Self::List { doctype, .. } => doctype,
        }
    }
}

/// One cached answer.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    /// The `ETag` header value, quotes included, as sent back in `If-None-Match`.
    pub(crate) etag: String,
    /// The body.
    pub(crate) body: String,
}

/// The cache.
#[derive(Debug, Default)]
pub(crate) struct Cache {
    entries: BTreeMap<CacheKey, Entry>,
}

impl Cache {
    pub(crate) fn get(&self, key: &CacheKey) -> Option<&Entry> {
        self.entries.get(key)
    }

    pub(crate) fn put(&mut self, key: CacheKey, entry: Entry) {
        if self.entries.len() >= MAX_ENTRIES
            && !self.entries.contains_key(&key)
            && let Some(first) = self.entries.keys().next().cloned()
        {
            self.entries.remove(&first);
        }
        self.entries.insert(key, entry);
    }

    /// The version a cached document was read at (its numeric `ETag`).
    pub(crate) fn doc_version(&self, doctype: &str, id: &str) -> Option<u64> {
        let key = CacheKey::Doc {
            doctype: doctype.to_owned(),
            id: id.to_owned(),
        };
        self.entries.get(&key)?.etag.trim_matches('"').parse().ok()
    }

    /// Drops one document and every list of its DocType (a list may contain it).
    pub(crate) fn invalidate_doc(&mut self, doctype: &str, id: &str) {
        self.entries.remove(&CacheKey::Doc {
            doctype: doctype.to_owned(),
            id: id.to_owned(),
        });
        self.invalidate_lists(doctype);
    }

    /// Drops every list of a DocType.
    pub(crate) fn invalidate_lists(&mut self, doctype: &str) {
        self.entries
            .retain(|k, _| !(matches!(k, CacheKey::List { .. }) && k.doctype() == doctype));
    }

    /// Drops everything of a DocType.
    pub(crate) fn invalidate_doctype(&mut self, doctype: &str) {
        self.entries.retain(|k, _| k.doctype() != doctype);
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
}
