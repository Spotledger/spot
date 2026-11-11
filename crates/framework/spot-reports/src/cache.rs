//! The compiled-report cache: reports keyed by `(name, metadata identity)`, in a bounded map.
//!
//! A report is compiled once per metadata **identity** — a [`MetaIdentity`], which is the
//! metadata version together with a content digest of the DocTypes. The version alone would be
//! wrong: a rollback or a snapshot adopted from another node serves different DocTypes at a
//! version this process has already used, and a cache keyed by the version alone would hand back
//! a report compiled against DocTypes the site no longer has. The identity is always an argument
//! — the cache holds no global state and no clock — so a caller that keeps the set it read can
//! prove it used the metadata it meant to. When the identity changes, the entry for the new
//! identity is compiled afresh; the old identity's entries age out.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use crate::compile::Compiler;
use crate::compiled::CompiledReport;
use crate::definition::ReportDef;
use crate::error::ReportError;
use crate::meta::{EffectiveMetaSet, MetaIdentity};

/// A bounded map of compiled reports, keyed by `(name, metadata identity)`.
#[derive(Debug)]
pub struct ReportCache {
    capacity: usize,
    entries: BTreeMap<(String, MetaIdentity), Arc<CompiledReport>>,
    /// Least-recently-used first.
    recency: VecDeque<(String, MetaIdentity)>,
}

impl ReportCache {
    /// A cache holding at most `capacity` compiled reports.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        ReportCache {
            capacity: capacity.max(1),
            entries: BTreeMap::new(),
            recency: VecDeque::new(),
        }
    }

    /// The number of cached reports.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The compiled report for `name` against `meta`, if cached. Touches its recency.
    pub fn get(&mut self, name: &str, meta: &EffectiveMetaSet) -> Option<Arc<CompiledReport>> {
        let key = (name.to_owned(), MetaIdentity::of(meta));
        let hit = self.entries.get(&key).cloned();
        if hit.is_some() {
            self.touch(&key);
        }
        hit
    }

    /// Inserts a compiled report for `meta`, evicting the least-recently-used entry when full.
    pub fn insert(&mut self, name: &str, meta: &EffectiveMetaSet, report: Arc<CompiledReport>) {
        let key = (name.to_owned(), MetaIdentity::of(meta));
        self.entries.insert(key.clone(), report);
        self.touch(&key);
        while self.entries.len() > self.capacity {
            if let Some(oldest) = self.recency.pop_front() {
                self.entries.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /// Compiles `def` against the compiler's metadata, returning the cached report when it is
    /// already there.
    ///
    /// The key is the compiler's own [`MetaIdentity`], so a cached report is only ever returned
    /// for the metadata view it was compiled against.
    ///
    /// # Errors
    ///
    /// Every [`ReportError`] the compiler reports.
    pub fn get_or_compile(
        &mut self,
        def: &ReportDef,
        compiler: &Compiler<'_>,
    ) -> Result<Arc<CompiledReport>, Vec<ReportError>> {
        let meta = compiler.meta();
        if let Some(hit) = self.get(&def.name, meta) {
            return Ok(hit);
        }
        let compiled = Arc::new(compiler.compile(def)?);
        self.insert(&def.name, meta, Arc::clone(&compiled));
        Ok(compiled)
    }

    fn touch(&mut self, key: &(String, MetaIdentity)) {
        if let Some(pos) = self.recency.iter().position(|k| k == key) {
            self.recency.remove(pos);
        }
        self.recency.push_back(key.clone());
    }
}
