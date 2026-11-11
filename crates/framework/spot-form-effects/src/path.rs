//! A field path in the form the form-event endpoint and the UI effects use:
//! `field` or `table[idx].field`, with `idx` counting from 1.

use std::fmt;

/// A parsed field path.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TargetPath {
    /// The child table, when the path names a row.
    pub table: Option<String>,
    /// The row's 1-based position, when the path names a row.
    pub index: Option<usize>,
    /// The field (a top-level field, or a column of the row's child DocType).
    pub field: String,
}

impl TargetPath {
    /// A top-level field path.
    #[must_use]
    pub fn field(name: &str) -> TargetPath {
        TargetPath {
            table: None,
            index: None,
            field: name.to_owned(),
        }
    }

    /// A row column path.
    #[must_use]
    pub fn row(table: &str, index: usize, field: &str) -> TargetPath {
        TargetPath {
            table: Some(table.to_owned()),
            index: Some(index),
            field: field.to_owned(),
        }
    }

    /// Parses the wire form.
    ///
    /// # Errors
    ///
    /// [`PathError`] when the text is empty, has more than one `[`, a non-numeric or zero index,
    /// or an empty table or field.
    pub fn parse(text: &str) -> Result<TargetPath, PathError> {
        if text.is_empty() {
            return Err(PathError::Empty);
        }
        let invalid = || PathError::Malformed(text.to_owned());
        match text.split_once('[') {
            None => {
                if !is_name(text) {
                    return Err(invalid());
                }
                Ok(TargetPath::field(text))
            }
            Some((table, rest)) => {
                if !is_name(table) {
                    return Err(invalid());
                }
                let (index, field) = rest.split_once("].").ok_or_else(invalid)?;
                if !is_name(field) {
                    return Err(invalid());
                }
                let index: usize = index.parse().map_err(|_| invalid())?;
                if index == 0 {
                    return Err(invalid());
                }
                Ok(TargetPath::row(table, index, field))
            }
        }
    }

    /// The path as the wire writes it (the 1-based index when there is one).
    #[must_use]
    pub fn wire(&self) -> String {
        match (&self.table, self.index) {
            (Some(table), Some(index)) => format!("{table}[{index}].{}", self.field),
            _ => self.field.clone(),
        }
    }

    /// Whether the path names a row of `table`.
    #[must_use]
    pub fn is_in_table(&self, table: &str) -> bool {
        self.table.as_deref() == Some(table)
    }
}

impl fmt::Display for TargetPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.wire())
    }
}

/// Why a path is not well formed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathError {
    /// The path is empty.
    Empty,
    /// The path is not `field` or `table[idx].field` with `idx` from 1.
    Malformed(String),
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathError::Empty => f.write_str("a field path is empty"),
            PathError::Malformed(text) => write!(
                f,
                "`{text}` is not a field path: use `field` or `table[idx].field` with idx from 1"
            ),
        }
    }
}

/// A field or table name: non-empty, no bracket, dot or whitespace. Whether it exists is the
/// metadata's question, asked after this syntax check.
fn is_name(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| !matches!(c, '[' | ']' | '.') && !c.is_whitespace())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn a_top_level_field_parses() {
        assert_eq!(TargetPath::parse("total"), Ok(TargetPath::field("total")));
        assert_eq!(TargetPath::field("total").wire(), "total");
    }

    #[test]
    fn a_row_path_keeps_its_one_based_index() {
        let path = TargetPath::parse("items[2].qty").unwrap();
        assert_eq!(path.table.as_deref(), Some("items"));
        assert_eq!(path.index, Some(2));
        assert_eq!(path.field, "qty");
        assert_eq!(path.wire(), "items[2].qty");
        assert!(path.is_in_table("items"));
    }

    #[test]
    fn malformed_paths_are_refused() {
        for text in [
            "",
            "items[0].qty",
            "items[x].qty",
            "items[1]qty",
            "a.b",
            "a[1].b.c",
        ] {
            assert!(TargetPath::parse(text).is_err(), "{text}");
        }
    }
}
