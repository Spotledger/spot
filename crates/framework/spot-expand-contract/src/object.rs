//! The object a schema change acts on: a table, one column of a table, or one index of a table.
//!
//! Every schema-shaping [`Change`](spot_migration_plan::Change) carries a `target` such as
//! `party` (a table) or `party.note` (a column). [`Object`] parses that text once, so the
//! classification, the usage manifest and the gate all compare the same value instead of
//! re-splitting strings.
//!
//! # Indexes
//!
//! An index is its own kind of object. Its change target is `table.index_name`, which has the
//! same shape as `table.column`, so the kind cannot be read from the text alone: the manifest
//! spells an index `index:table.name` and [`Object::parse`] reads that prefix. The kind is the
//! `index` flag of [`Object`] (a flag rather than an enum, because every other part of the
//! value is shared). A table or column entry **never** covers an index and an index entry covers
//! only itself: a manifest that names a table (as `from_catalog` does for every table) must not
//! make the drop of an index of that table look "still used" for ever.
//!
//! Names are the storage names of the change model: lowercase letters, digits and underscores.
//! The rule is deliberately a little wider (an upper-case letter is accepted) so a lab or a
//! custom DocType label never turns into a spurious refusal; the storage registry is the
//! authority on what a table or column is really called. Names are folded to lower case (the
//! database folds unquoted identifiers), so `Party.Note` and `party.note` are one object and a
//! differently-cased manifest entry can never let a contract ship early.

use std::fmt;

use crate::error::Error;

/// Longest name this crate accepts, in bytes (the PostgreSQL identifier limit).
pub const MAX_NAME_LEN: usize = 63;

/// Prefix of an index in the manifest text: `index:table.name`.
pub const INDEX_PREFIX: &str = "index:";

/// A table, one column of a table, or one index of a table.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Object {
    /// The table's storage name.
    pub table: String,
    /// The column's storage name, or `None` for the whole table. For an index
    /// ([`Object::is_index`]) this is the index's storage name.
    pub column: Option<String>,
    /// Whether this is an index of the table rather than a column. When set, `column` is
    /// always `Some` (the index name).
    pub index: bool,
}

impl Object {
    /// The whole table `table`.
    pub fn table(table: &str) -> Result<Self, Error> {
        check_name(table)?;
        Ok(Self {
            table: table.to_ascii_lowercase(),
            column: None,
            index: false,
        })
    }

    /// The column `table.column`.
    pub fn column(table: &str, column: &str) -> Result<Self, Error> {
        check_name(table)?;
        check_name(column)?;
        Ok(Self {
            table: table.to_ascii_lowercase(),
            column: Some(column.to_ascii_lowercase()),
            index: false,
        })
    }

    /// The index `name` of `table`.
    pub fn index(table: &str, name: &str) -> Result<Self, Error> {
        check_name(table)?;
        check_name(name)?;
        Ok(Self {
            table: table.to_ascii_lowercase(),
            column: Some(name.to_ascii_lowercase()),
            index: true,
        })
    }

    /// Parses `table`, `table.column` or `index:table.name`.
    pub fn parse(text: &str) -> Result<Self, Error> {
        if let Some(rest) = text.strip_prefix(INDEX_PREFIX) {
            return match rest.split_once('.') {
                Some((table, name)) if !name.contains('.') => Self::index(table, name),
                _ => Err(Error::BadObject {
                    name: text.to_owned(),
                    reason: "an index is written `index:table.name`".to_owned(),
                }),
            };
        }
        match text.split_once('.') {
            None => Self::table(text),
            Some((table, column)) => {
                if column.contains('.') {
                    return Err(Error::BadObject {
                        name: text.to_owned(),
                        reason: "a name is one table and at most one column".to_owned(),
                    });
                }
                Self::column(table, column)
            }
        }
    }

    /// Whether this object is the whole table rather than one column.
    #[must_use]
    pub fn is_table(&self) -> bool {
        self.column.is_none()
    }

    /// Whether this object is an index.
    #[must_use]
    pub fn is_index(&self) -> bool {
        self.index
    }

    /// The text form a manifest uses: [`Display`](fmt::Display), with `index:` before an index.
    #[must_use]
    pub fn manifest_text(&self) -> String {
        if self.index {
            format!("{INDEX_PREFIX}{self}")
        } else {
            self.to_string()
        }
    }

    /// Whether this object (a usage entry) means the binary touches `other`.
    ///
    /// A table entry covers every column of the table, because a binary that names the table
    /// but not the column may still read or write it: the manifest is an over-approximation.
    /// An index is not a column: a table or column entry never covers an index, and an index
    /// entry covers only the same index.
    #[must_use]
    pub fn covers(&self, other: &Self) -> bool {
        if self.index != other.index {
            return false;
        }
        if self.table != other.table {
            return false;
        }
        match (&self.column, &other.column) {
            (_, None) | (None, _) => true,
            (Some(left), Some(right)) => left == right,
        }
    }
}

impl fmt::Display for Object {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.column {
            Some(column) => write!(f, "{}.{column}", self.table),
            None => f.write_str(&self.table),
        }
    }
}

/// A storage name: 1 to [`MAX_NAME_LEN`] bytes of letters, digits and underscores, not starting
/// with a digit.
fn check_name(name: &str) -> Result<(), Error> {
    let mut chars = name.chars();
    let valid = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if valid {
        Ok(())
    } else {
        Err(Error::BadObject {
            name: name.to_owned(),
            reason: format!(
                "a storage name is 1 to {MAX_NAME_LEN} letters, digits or underscores and does \
                 not start with a digit"
            ),
        })
    }
}
