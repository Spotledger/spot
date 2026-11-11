//! What the designer knows about the site it is designing against: the definitions already
//! installed, the DocTypes that are protected, and how many rows a table holds.
//!
//! The context is plain data the caller builds (from a live snapshot, a fixture or a test);
//! the crate never reads a database, a clock or the environment. A draft on a **new** DocType
//! needs nothing but the site id; a draft on an **existing** DocType needs that definition so
//! validation and publish can tell a new field from an existing one.

use std::collections::{BTreeMap, BTreeSet};

use spot_core::SiteId;
use spot_meta::Definition;

/// The facts a draft is validated and published against.
#[derive(Clone, Debug)]
pub struct SiteContext {
    site: SiteId,
    definitions: Vec<Definition>,
    protected: BTreeSet<String>,
    rows: BTreeMap<String, u64>,
}

impl SiteContext {
    /// A site with no definitions, nothing protected and no rows.
    #[must_use]
    pub fn new(site: SiteId) -> Self {
        Self {
            site,
            definitions: Vec::new(),
            protected: BTreeSet::new(),
            rows: BTreeMap::new(),
        }
    }

    /// Adds an installed definition.
    #[must_use]
    pub fn with_definition(mut self, definition: Definition) -> Self {
        self.definitions.push(definition);
        self
    }

    /// Adds several installed definitions.
    #[must_use]
    pub fn with_definitions(mut self, definitions: Vec<Definition>) -> Self {
        self.definitions.extend(definitions);
        self
    }

    /// Marks a DocType as protected (a system DocType the designer may not edit).
    #[must_use]
    pub fn with_protected(mut self, doctype: &str) -> Self {
        self.protected.insert(doctype.to_owned());
        self
    }

    /// Records how many rows a DocType's table holds (the data-loss and backfill signal).
    #[must_use]
    pub fn with_rows(mut self, doctype: &str, rows: u64) -> Self {
        self.rows.insert(doctype.to_owned(), rows);
        self
    }

    /// The site.
    #[must_use]
    pub fn site(&self) -> &SiteId {
        &self.site
    }

    /// The installed definitions.
    #[must_use]
    pub fn definitions(&self) -> &[Definition] {
        &self.definitions
    }

    /// One installed definition, by name.
    #[must_use]
    pub fn definition(&self, name: &str) -> Option<&Definition> {
        self.definitions.iter().find(|d| d.name == name)
    }

    /// Whether the site already has the DocType.
    #[must_use]
    pub fn has_doctype(&self, name: &str) -> bool {
        self.definition(name).is_some()
    }

    /// Whether the DocType is protected.
    #[must_use]
    pub fn is_protected(&self, name: &str) -> bool {
        self.protected.contains(name) || crate::draft::is_protected_doctype(name)
    }

    /// How many rows the DocType's table holds (0 when unknown).
    #[must_use]
    pub fn rows(&self, doctype: &str) -> u64 {
        self.rows.get(doctype).copied().unwrap_or(0)
    }
}
