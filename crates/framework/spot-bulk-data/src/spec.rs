//! What an export is asked for: [`ExportSpec`], the field set it is planned against, and the
//! parts a stream yields.

use spot_storage_contract::{Expr, Nulls, OrderDir};

use crate::format::Format;

/// The alias the export's own query uses for the DocType (filters name it).
pub const EXPORT_ALIAS: &str = "d";
/// The alias a child part's query uses for the child entity.
pub const CHILD_ALIAS: &str = "c";
/// The system column holding a child row's parent id.
pub const PARENT_COLUMN: &str = "parent";
/// The system column holding a child row's position.
pub const IDX_COLUMN: &str = "idx";

/// One sort key of an export. The record id is always the final tie-break, added by the plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderKey {
    /// The field's storage name.
    pub field: String,
    /// Ascending or descending.
    pub dir: OrderDir,
    /// Where nulls sort.
    pub nulls: Nulls,
}

impl OrderKey {
    /// An ascending key with nulls last.
    #[must_use]
    pub fn asc(field: impl Into<String>) -> OrderKey {
        OrderKey {
            field: field.into(),
            dir: OrderDir::Asc,
            nulls: Nulls::Last,
        }
    }

    /// A descending key with nulls last.
    #[must_use]
    pub fn desc(field: impl Into<String>) -> OrderKey {
        OrderKey {
            field: field.into(),
            dir: OrderDir::Desc,
            nulls: Nulls::Last,
        }
    }
}

/// A request to export one DocType.
#[derive(Clone, Debug)]
pub struct ExportSpec {
    /// The DocType's storage name.
    pub doctype: String,
    /// The parent columns to export (field names). Empty means every field of the field set.
    pub columns: Vec<String>,
    /// The row filter, naming fields through [`EXPORT_ALIAS`]; `None` is every row the caller
    /// may read.
    pub filter: Option<Expr>,
    /// The sort keys. Empty defaults to `(modified)` when the field set has a `modified` field,
    /// otherwise to the record id alone.
    pub order: Vec<OrderKey>,
    /// CSV or NDJSON.
    pub format: Format,
    /// A policy cap on the number of rows; reaching it fails with `bulk.limit`.
    pub max_rows: Option<u64>,
    /// Export each child table of the field set as its own part, keyed by parent id.
    pub include_children: bool,
    /// Defuse spreadsheet formula injection in CSV text cells (see [`crate::format`]).
    pub harden_csv: bool,
}

impl ExportSpec {
    /// A CSV export of every column the caller may export.
    #[must_use]
    pub fn csv(doctype: impl Into<String>) -> ExportSpec {
        ExportSpec::new(doctype, Format::Csv)
    }

    /// An NDJSON export of every column the caller may export.
    #[must_use]
    pub fn ndjson(doctype: impl Into<String>) -> ExportSpec {
        ExportSpec::new(doctype, Format::Ndjson)
    }

    /// An export of `doctype` in `format`.
    #[must_use]
    pub fn new(doctype: impl Into<String>, format: Format) -> ExportSpec {
        ExportSpec {
            doctype: doctype.into(),
            columns: Vec::new(),
            filter: None,
            order: Vec::new(),
            format,
            max_rows: None,
            include_children: false,
            harden_csv: true,
        }
    }

    /// Requests exactly `columns` (in this order).
    #[must_use]
    pub fn with_columns(mut self, columns: &[&str]) -> ExportSpec {
        self.columns = columns.iter().map(|c| (*c).to_owned()).collect();
        self
    }

    /// Sets the row filter.
    #[must_use]
    pub fn with_filter(mut self, filter: Expr) -> ExportSpec {
        self.filter = Some(filter);
        self
    }

    /// Sets the sort keys.
    #[must_use]
    pub fn with_order(mut self, order: Vec<OrderKey>) -> ExportSpec {
        self.order = order;
        self
    }

    /// Sets the row cap.
    #[must_use]
    pub fn with_max_rows(mut self, max_rows: u64) -> ExportSpec {
        self.max_rows = Some(max_rows);
        self
    }

    /// Includes child tables as separate parts.
    #[must_use]
    pub fn with_children(mut self) -> ExportSpec {
        self.include_children = true;
        self
    }

    /// Turns the CSV formula hardening off.
    #[must_use]
    pub fn without_hardening(mut self) -> ExportSpec {
        self.harden_csv = false;
        self
    }
}

/// A child table of a DocType: the table field, the child DocType and the child's columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildTable {
    /// The parent's table field name.
    pub table: String,
    /// The child DocType's storage name.
    pub doctype: String,
    /// The child columns (field names).
    pub columns: Vec<String>,
}

impl ChildTable {
    /// A child table with its columns.
    #[must_use]
    pub fn new(table: impl Into<String>, doctype: impl Into<String>, columns: &[&str]) -> Self {
        ChildTable {
            table: table.into(),
            doctype: doctype.into(),
            columns: columns.iter().map(|c| (*c).to_owned()).collect(),
        }
    }
}

/// The fields of a DocType, as the caller's metadata knows them.
///
/// The plan is built against this instead of a catalog: the export crate never holds metadata,
/// and the query is validated against the catalog by the backend when it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldSet {
    /// The DocType's storage name.
    pub doctype: String,
    /// The text field holding the creating user's name (the owner rules compare it).
    pub owner_field: String,
    /// The parent fields.
    pub fields: Vec<String>,
    /// The child tables.
    pub children: Vec<ChildTable>,
}

impl FieldSet {
    /// A field set with no child tables.
    #[must_use]
    pub fn new(
        doctype: impl Into<String>,
        owner_field: impl Into<String>,
        fields: &[&str],
    ) -> Self {
        FieldSet {
            doctype: doctype.into(),
            owner_field: owner_field.into(),
            fields: fields.iter().map(|f| (*f).to_owned()).collect(),
            children: Vec::new(),
        }
    }

    /// Adds a child table.
    #[must_use]
    pub fn with_child(mut self, child: ChildTable) -> Self {
        self.children.push(child);
        self
    }

    /// Whether the field set declares a field named `name`.
    #[must_use]
    pub fn has_field(&self, name: &str) -> bool {
        self.fields.iter().any(|f| f == name)
    }
}

/// One part of an export: the parent rows, or one child table's rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    /// The parent DocType's rows.
    Parent,
    /// One child table's rows, keyed by parent id.
    Child(String),
}

impl Part {
    /// The stable name used in the cursor and audit records.
    #[must_use]
    pub fn as_str(&self) -> String {
        match self {
            Part::Parent => "parent".to_owned(),
            Part::Child(table) => format!("child:{table}"),
        }
    }
}
