//! The per-DocType chain configuration: which edge types make each DocType's chain, and each
//! item DocType's bill-of-materials edge and per-unit quantity field.
//!
//! The configuration is metadata text (TOML), read as data. The crate's logic never names an
//! ERP DocType: [`ChainConfig::defaults`] carries the crate's own fixtures for tests, and an app
//! passes its own text to [`ChainConfig::parse`]. A `[[chain]]` entry maps a DocType to the
//! canonical edge-type names of its chain; a `[[bom]]` entry maps an item DocType to the edge
//! from a component record to the record that uses it, plus the field on the component record
//! that carries the per-unit quantity.
//!
//! ```toml
//! [[chain]]
//! doctype = "po"
//! edges = ["grn.po", "invoice.grn", "payment.invoice"]
//!
//! [[bom]]
//! doctype = "item"
//! edge = "item.parent_item"
//! qty_field = "qty"
//! ```

use std::collections::BTreeMap;

use spot_storage_contract::Name;

use crate::error::LineageError;

/// The compiled-in default configuration, kept as data for tests.
pub const DEFAULT_CHAINS: &str = include_str!("defaults.toml");

/// One DocType's chain: the edge types followed together at each level.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainSpec {
    /// The canonical edge-type names, in the order the configuration lists them.
    pub edges: Vec<String>,
}

/// One item DocType's bill of materials: the component-to-user edge and the per-unit quantity
/// field on the component record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BomSpec {
    /// The canonical edge-type name from a component record to the record that uses it.
    pub edge: String,
    /// The field on the component record carrying the per-unit quantity.
    pub qty_field: Name,
}

/// The chains and bills of materials of a deployment, by DocType.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChainConfig {
    chains: BTreeMap<Name, ChainSpec>,
    boms: BTreeMap<Name, BomSpec>,
}

impl ChainConfig {
    /// An empty configuration: every lookup is `lineage.unknown_chain` or `lineage.unknown_bom`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The crate's built-in configuration, for tests.
    ///
    /// # Errors
    ///
    /// [`LineageError::Config`] when the compiled-in text does not parse; a test asserts that it
    /// always does.
    pub fn defaults() -> Result<Self, LineageError> {
        Self::parse(DEFAULT_CHAINS)
    }

    /// Parses chain configuration text.
    ///
    /// # Errors
    ///
    /// [`LineageError::Config`] naming the first offending path: unparsable TOML, an entry that
    /// is not a table, a missing or mistyped field, an empty chain, or a name that is not a valid
    /// storage identifier.
    pub fn parse(text: &str) -> Result<Self, LineageError> {
        let root: toml::Table =
            toml::from_str(text).map_err(|error| config_error("", error.message().to_owned()))?;
        let mut config = Self::new();
        if let Some(value) = root.get("chain") {
            let entries = value
                .as_array()
                .ok_or_else(|| config_error("chain", "expected an array of tables"))?;
            for (index, entry) in entries.iter().enumerate() {
                let path = format!("chain[{index}]");
                let table = as_table(entry, &path)?;
                let doctype = text_field(table, "doctype", &path)?;
                let edges = text_array(table, "edges", &path)?;
                if edges.is_empty() {
                    return Err(config_error(
                        format!("{path}.edges"),
                        "a chain needs at least one edge type",
                    ));
                }
                let name = doctype_name(&doctype, &format!("{path}.doctype"))?;
                config.chains.insert(name, ChainSpec { edges });
            }
        }
        if let Some(value) = root.get("bom") {
            let entries = value
                .as_array()
                .ok_or_else(|| config_error("bom", "expected an array of tables"))?;
            for (index, entry) in entries.iter().enumerate() {
                let path = format!("bom[{index}]");
                let table = as_table(entry, &path)?;
                let doctype = text_field(table, "doctype", &path)?;
                let edge = text_field(table, "edge", &path)?;
                let qty_field = text_field(table, "qty_field", &path)?;
                let name = doctype_name(&doctype, &format!("{path}.doctype"))?;
                let qty_field = doctype_name(&qty_field, &format!("{path}.qty_field"))?;
                config.boms.insert(name, BomSpec { edge, qty_field });
            }
        }
        Ok(config)
    }

    /// The chain of `doctype`, when the configuration has one.
    #[must_use]
    pub fn chain(&self, doctype: &Name) -> Option<&ChainSpec> {
        self.chains.get(doctype)
    }

    /// The bill of materials of the item `doctype`, when the configuration has one.
    #[must_use]
    pub fn bom(&self, doctype: &Name) -> Option<&BomSpec> {
        self.boms.get(doctype)
    }

    /// The DocTypes with a chain, in name order.
    pub fn chained_doctypes(&self) -> impl Iterator<Item = &Name> {
        self.chains.keys()
    }

    /// The item DocTypes with a bill of materials, in name order.
    pub fn bom_doctypes(&self) -> impl Iterator<Item = &Name> {
        self.boms.keys()
    }
}

fn config_error(path: impl Into<String>, message: impl Into<String>) -> LineageError {
    LineageError::Config {
        path: path.into(),
        message: message.into(),
    }
}

fn as_table<'a>(value: &'a toml::Value, path: &str) -> Result<&'a toml::Table, LineageError> {
    value
        .as_table()
        .ok_or_else(|| config_error(path.to_owned(), "expected a table"))
}

fn text_field(table: &toml::Table, key: &str, path: &str) -> Result<String, LineageError> {
    table
        .get(key)
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| config_error(format!("{path}.{key}"), "expected a string"))
}

fn text_array(table: &toml::Table, key: &str, path: &str) -> Result<Vec<String>, LineageError> {
    let array = table
        .get(key)
        .and_then(toml::Value::as_array)
        .ok_or_else(|| config_error(format!("{path}.{key}"), "expected an array of strings"))?;
    array
        .iter()
        .map(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                config_error(format!("{path}.{key}"), "every entry must be a string")
            })
        })
        .collect()
}

fn doctype_name(text: &str, path: &str) -> Result<Name, LineageError> {
    Name::new(text).map_err(|_| config_error(path.to_owned(), format!("`{text}` is not a name")))
}
