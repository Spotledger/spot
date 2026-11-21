//! Grouping consecutive rows into one parent plus its child rows (brief item 3).
//!
//! The common import layout: a parent's values sit on its first row and child rows follow
//! with the parent's columns empty; a **group key** column (or consecutive identical keys)
//! says when the next parent starts. [`Grouper`] folds a stream of rows into [`Group`]s one
//! at a time, holding only the current group, so memory stays bounded by the largest parent
//! plus a fixed buffer.
//!
//! # The rules
//!
//! - The first row starts a group, always. With a key column, every later row starts one
//!   when its raw group-key text differs from the open group's key (a missing key cell reads
//!   as empty text, so Excel's padding never splits a group). Without a key column, a row
//!   that sets any parent column starts a new parent and a row that sets none extends the
//!   open parent.
//! - A parent row inside a group is the new parent's first row; later rows that set parent
//!   values again overwrite them (the last row of a repeating parent block wins).
//! - Child cells accrue into the open child row per table; a parent value anywhere in the
//!   row, or a child value repeating a header that row already has, opens the next row.
//! - A child-shaped row with no parent standing (the file's first rows, right after a key
//!   change) closes its group as [`RowFault::Orphan`] (`import.orphan_row`): the file does
//!   not say which parent the child rows belong to, and inventing one would corrupt data.
//! - Cells convert when the row is fed, so a bad cell fails its row alone ([`Group::Fault`],
//!   [`RowFault::Cells`], with column name and raw text for the report) and the group state
//!   stands as it was: one bad row never poisons a parent.
//!
//! # Ordering
//!
//! A feed call returns at most one thing: the parent (or orphan fault) that closed, or the
//! refused row — always in file order, since a row is converted and grouped as it arrives.

use std::collections::BTreeMap;

use crate::convert::{self, ConversionError};
use crate::mapping::{MappingPlan, Target};
use crate::source::{Cell, Row};

/// One converted, mapped cell, carrying the plan target it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct CellValue {
    /// The 1-based source row the cell came from (for mapping report entries back).
    pub row: u64,
    /// The column's header.
    pub header: String,
    /// The column's 0-based source index.
    pub column: usize,
    /// The plan target this column was bound to (a clone, so cells own their target).
    pub target: Target,
    /// The converted value; `None` when unset (missing column, or empty with
    /// `empty_to_null`).
    pub value: Option<spot_app_api::prelude::Value>,
    /// The cell's raw text, for the error report.
    pub text: String,
}

impl CellValue {
    /// Whether the cell set a value.
    pub fn is_set(&self) -> bool {
        self.value.is_some()
    }
}

/// A row that could not join a group.
#[derive(Clone, Debug, PartialEq)]
pub enum RowFault {
    /// A child-shaped row with no parent standing (`import.orphan_row`).
    Orphan(Row),
    /// One or more cells failed conversion; each with its column, the cell's raw text and
    /// the refusal (so the report can name exactly what the user typed).
    Cells(Row, Vec<FailedCell>),
}

/// One failed cell of a refused row.
#[derive(Clone, Debug, PartialEq)]
pub struct FailedCell {
    /// The column's 0-based source index (the raw text is read back from the row by it).
    pub index: usize,
    /// The column's header.
    pub header: String,
    /// The failed cell's raw text, as the file holds it.
    pub text: String,
    /// The conversion's refusal (code and message).
    pub error: ConversionError,
}

/// A finished parent, or the faults of one row.
#[derive(Clone, Debug, PartialEq)]
pub enum Group {
    /// A complete parent: the rows it was built from, its field values and its child rows.
    Done {
        /// The parent's rows in file order.
        rows: Vec<Row>,
        /// Values per parent field, in the order the file first set them.
        fields: Vec<(String, CellValue)>,
        /// Child rows per table field, in the order the file had them.
        children: Vec<(String, Vec<Vec<CellValue>>)>,
    },
    /// The row is refused; the stream turns it into report entries.
    Fault(RowFault),
}

impl Group {
    /// The group's first source row number (1-based), for the report.
    pub fn first_row(&self) -> Option<u64> {
        match self {
            Group::Done { rows, .. } => rows.first().map(|r| r.number),
            Group::Fault(RowFault::Orphan(row)) | Group::Fault(RowFault::Cells(row, _)) => {
                Some(row.number)
            }
        }
    }
}

/// The group under construction.
#[derive(Debug, Default)]
struct CurrentGroup {
    /// The group key's raw text from the row that opened the group.
    key: String,
    rows: Vec<Row>,
    /// Set parent values by field name; `field_order` keeps first-set order.
    fields: BTreeMap<String, CellValue>,
    field_order: Vec<String>,
    /// Child rows per table field, in file order.
    children: Vec<(String, Vec<Vec<CellValue>>)>,
    /// The table whose child row is open, and the headers that row holds: new values extend
    /// it; a repeated header (or a parent value) opens the next row.
    open_child: Option<(String, Vec<String>)>,
}

impl CurrentGroup {
    /// Whether any parent value was set (a group that set none is not a parent).
    fn is_parent(&self) -> bool {
        !self.fields.is_empty()
    }

    /// The finished group: [`Group::Done`] when the group set parent values, otherwise
    /// [`RowFault::Orphan`] when it accrued only child rows (parentless children must reach
    /// the report, not vanish), otherwise an empty `Done` that the caller drops.
    fn done(self) -> Group {
        if self.is_parent() {
            return self.into_parent();
        }
        if let Some(first) = self.rows.first()
            && !self.children.is_empty()
        {
            return Group::Fault(RowFault::Orphan(first.clone()));
        }
        Group::Done {
            rows: Vec::new(),
            fields: Vec::new(),
            children: Vec::new(),
        }
    }

    /// The finished [`Group::Done`] (only called for a group that set parent values).
    fn into_parent(self) -> Group {
        let fields = self
            .field_order
            .iter()
            .filter_map(|name| self.fields.get(name).map(|v| (name.clone(), v.clone())))
            .collect();
        Group::Done {
            rows: self.rows,
            fields,
            children: self.children,
        }
    }
}

/// Folds a stream of rows into groups according to a plan.
///
/// The streaming contract, kept simple: [`Grouper::feed`] returns **at most one** [`Group`]
/// per row — the refused row, or the group (parent or orphan fault) the row closed — always
/// in file order, since every row is converted and grouped as it arrives.
#[derive(Debug)]
pub struct Grouper {
    plan: MappingPlan,
    /// The group being built (`None` before the first row).
    current: Option<CurrentGroup>,
}

impl Grouper {
    /// A grouper over `plan`.
    pub fn new(plan: MappingPlan) -> Self {
        Self {
            plan,
            current: None,
        }
    }

    /// The plan in force.
    pub fn plan(&self) -> &MappingPlan {
        &self.plan
    }

    /// Feeds one row; `Some` when the row is **refused** or closed a group.
    pub fn feed(&mut self, row: Row) -> Option<Group> {
        let converted = self.convert_row(&row);
        if self.starts_group(&row) {
            match converted {
                Ok(cells) => {
                    let closed = self.close_group();
                    self.open_group(&row);
                    self.absorb(cells);
                    match self.current.as_mut() {
                        Some(opened) => opened,
                        // Unreachable: `open_group` above.
                        None => return closed,
                    }
                    .rows
                    .push(row);
                    return closed;
                }
                Err(failures) => {
                    // The row cannot start a parent; the open group stays open (a refused
                    // row between two parent rows never closes the first parent: its child
                    // cells belong to no one).
                    return Some(Group::Fault(RowFault::Cells(row, failures)));
                }
            }
        } else {
            match converted {
                Ok(cells) => {
                    let open = self.current.as_mut()?;
                    absorb_cells(open, cells);
                    open.rows.push(row);
                }
                Err(failures) => return Some(Group::Fault(RowFault::Cells(row, failures))),
            }
        }
        None
    }

    /// The last group, when the source's rows are over. A group that set no parent value
    /// and holds no child rows is neither document nor fault: there is nothing to import
    /// and nothing the user did wrong.
    pub fn finish(self) -> Option<Group> {
        let current = self.current?;
        let done = current.done();
        is_importable_group(&done).then_some(done)
    }

    /// Closes the open group, if any, as a finished parent or an orphan fault.
    fn close_group(&mut self) -> Option<Group> {
        let current = self.current.take()?;
        let done = current.done();
        is_importable_group(&done).then_some(done)
    }

    /// Opens a fresh group keyed by `row`'s key text.
    fn open_group(&mut self, row: &Row) {
        self.current = Some(CurrentGroup {
            key: self.key_text(row),
            ..CurrentGroup::default()
        });
    }

    /// Whether `row` opens a new group.
    fn starts_group(&self, row: &Row) -> bool {
        match &self.current {
            None => true,
            Some(open) => {
                if let Some(index) = self.plan.group_key() {
                    return row.cells.get(index).map(Cell::as_str).unwrap_or("") != open.key;
                }
                // No key column: a row that sets any parent column starts a new parent.
                row.cells.iter().enumerate().any(|(i, cell)| {
                    matches!(self.plan.target(i), Some(Target::Field(_)))
                        && !cell.as_str().trim().is_empty()
                })
            }
        }
    }

    /// The row's raw group-key text (empty without a key column).
    fn key_text(&self, row: &Row) -> String {
        match self.plan.group_key() {
            Some(index) => row
                .cells
                .get(index)
                .map(Cell::as_str)
                .unwrap_or("")
                .to_owned(),
            None => String::new(),
        }
    }

    /// Converts every mapped cell of `row`; every failure comes back together, so one row
    /// reports all its bad columns at once.
    fn convert_row(&self, row: &Row) -> Result<Vec<CellValue>, Vec<FailedCell>> {
        let mut cells = Vec::new();
        let mut failures = Vec::new();
        for (index, cell) in row.cells.iter().enumerate() {
            let Some(target) = self.plan.target(index) else {
                continue;
            };
            if matches!(target, Target::GroupKey) {
                continue;
            }
            let Some(spec) = self.plan.specs.get(&qualified_name(target)) else {
                continue;
            };
            let header = self.plan.headers()[index].clone();
            match cell {
                Cell::Missing => cells.push(CellValue {
                    row: row.number,
                    header,
                    column: index,
                    target: target.clone(),
                    value: None,
                    text: String::new(),
                }),
                Cell::Text(text) => {
                    match convert::convert(spec, text, &self.plan.options, &self.plan.formats) {
                        Ok(value) => cells.push(CellValue {
                            row: row.number,
                            header,
                            column: index,
                            target: target.clone(),
                            value,
                            text: text.clone(),
                        }),
                        Err(error) => failures.push(FailedCell {
                            index,
                            header,
                            text: text.clone(),
                            error,
                        }),
                    }
                }
            }
        }
        if failures.is_empty() {
            Ok(cells)
        } else {
            Err(failures)
        }
    }

    /// The row's cells go into the just-opened group.
    fn absorb(&mut self, cells: Vec<CellValue>) {
        let current = match self.current.as_mut() {
            Some(current) => current,
            // Unreachable: `feed` opened the group just before calling this.
            None => return,
        };
        absorb_cells(current, cells);
    }
}

/// A qualified spec name for a target: the field, or `table.field` for a child cell.
fn qualified_name(target: &Target) -> String {
    match target {
        Target::Field(name) => name.clone(),
        Target::Child(table, field) => format!("{table}.{field}"),
        Target::GroupKey => String::new(),
    }
}

/// A closed group worth delivering: a parent with rows, or an orphan fault. An empty shell
/// (no rows, no values — a key-only row) is dropped.
fn is_importable_group(group: &Group) -> bool {
    match group {
        Group::Done { rows, fields, .. } => !rows.is_empty() && !fields.is_empty(),
        Group::Fault(_) => true,
    }
}

/// Inserts converted cells into the open group: parent fields overwrite (later rows win and
/// close the open child row), child cells accrue into the open child row.
fn absorb_cells(current: &mut CurrentGroup, cells: Vec<CellValue>) {
    for cell in cells {
        if !cell.is_set() {
            continue;
        }
        match cell.target {
            Target::Field(ref name) => {
                if !current.fields.contains_key(name) {
                    current.field_order.push(name.clone());
                }
                current.fields.insert(name.clone(), cell);
                current.open_child = None;
            }
            Target::Child(ref table, _) => {
                let table = table.clone();
                let extends = current.open_child.as_ref().is_some_and(|(t, headers)| {
                    t.as_str() == table && !headers.contains(&cell.header)
                });
                if !extends {
                    match current
                        .children
                        .iter_mut()
                        .find(|(t, _)| t.as_str() == table)
                    {
                        Some((_, rows)) => rows.push(Vec::new()),
                        None => current.children.push((table.clone(), vec![Vec::new()])),
                    }
                    current.open_child = Some((table.clone(), Vec::new()));
                }
                let rows = match current
                    .children
                    .iter_mut()
                    .find(|(t, _)| t.as_str() == table)
                {
                    Some((_, rows)) => rows,
                    // Unreachable: the push above (or an earlier row) created it.
                    None => return,
                };
                let header = cell.header.clone();
                match rows.last_mut() {
                    Some(last) => last,
                    // Unreachable: a row was pushed above or by the open case.
                    None => return,
                }
                .push(cell);
                if let Some((t, headers)) = current.open_child.as_mut()
                    && t.as_str() == table
                {
                    headers.push(header);
                }
            }
            Target::GroupKey => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::Override;
    use crate::convert::{ConversionOptions, Formats};
    use crate::testmeta;

    /// A plan for `doctype` over `headers`, auto-matched, with optional overrides.
    fn plan(doctype: &str, headers: &[&str], overrides: &[Override]) -> MappingPlan {
        let meta = testmeta::meta();
        let headers: Vec<String> = headers.iter().map(|h| (*h).to_owned()).collect();
        let (d, dt, t) = Formats::default_lists();
        let formats = Formats::build(&d, &dt, &t).unwrap();
        MappingPlan::build(
            &meta,
            doctype,
            &headers,
            overrides,
            ConversionOptions::default(),
            formats,
        )
        .unwrap()
    }

    fn row(number: u64, cells: &[&str]) -> Row {
        Row {
            number,
            cells: cells.iter().map(|c| Cell::Text((*c).to_owned())).collect(),
        }
    }

    /// Collects every group a row stream produces, in order.
    fn all_groups(grouper: Grouper, rows: Vec<Row>) -> Vec<Group> {
        let mut grouper = grouper;
        let mut out = Vec::new();
        for row in rows {
            if let Some(group) = grouper.feed(row) {
                out.push(group);
            }
        }
        if let Some(group) = grouper.finish() {
            out.push(group);
        }
        out
    }

    /// Consecutive rows with parent columns (no key column) become ONE parent; child rows
    /// accrue into the open child row and a repeated parent value closes it.
    #[test]
    fn parent_plus_child_rows_fold_into_one_group() {
        // number | item | qty | kind: `number` only on the parent's first row.
        let g = Grouper::new(plan(
            "sales_invoice",
            &["number", "item", "qty", "kind"],
            &[],
        ));
        let groups = all_groups(
            g,
            vec![
                row(1, &["SI-1", "bolt", "2", "Goods"]),
                row(2, &["", "nut", "1.5", "Service"]),
                row(3, &["SI-2", "bolt", "3", "Goods"]),
            ],
        );
        assert_eq!(groups.len(), 2, "{groups:?}");
        let Some(Group::Done {
            rows,
            fields,
            children,
        }) = groups.first()
        else {
            panic!("first group must be a parent: {groups:?}")
        };
        assert_eq!(rows.iter().map(|r| r.number).collect::<Vec<_>>(), [1, 2]);
        // The parent's `number` from row 1.
        assert_eq!(fields[0].0, "number");
        assert_eq!(fields[0].1.text, "SI-1");
        // One child table with two child rows.
        assert_eq!(children.len(), 1);
        let (table, child_rows) = &children[0];
        assert_eq!(table, "items");
        assert_eq!(child_rows.len(), 2);
        assert_eq!(child_rows[1][0].text, "nut");
        // Group two is the second parent.
        let Some(Group::Done { rows, .. }) = groups.get(1) else {
            panic!("second group must be a parent: {groups:?}")
        };
        assert_eq!(rows.iter().map(|r| r.number).collect::<Vec<_>>(), [3]);
    }

    /// A group key column (override `group_key`) splits parents; padding in later rows
    /// keeps the group open.
    #[test]
    fn group_key_splits_and_blank_padding_keeps_the_group() {
        let g = Grouper::new(plan(
            "sales_invoice",
            &["ref", "number", "item"],
            &[Override::new("ref", "group_key")],
        ));
        let groups = all_groups(
            g,
            vec![
                row(1, &["A", "SI-1", "bolt"]),
                row(2, &["A", "", "nut"]),
                row(3, &["B", "SI-2", ""]),
            ],
        );
        let Some(Group::Done { rows, .. }) = groups.first() else {
            panic!("first parent must close on the key change: {groups:?}")
        };
        assert_eq!(rows.iter().map(|r| r.number).collect::<Vec<_>>(), [1, 2]);
        let Some(Group::Done { rows, .. }) = groups.get(1) else {
            panic!("second parent must close at finish: {groups:?}")
        };
        assert_eq!(rows.iter().map(|r| r.number).collect::<Vec<_>>(), [3]);
    }

    /// A child row with no parent standing (or after a key change) is `import.orphan_row`
    /// and reaches the report instead of vanishing.
    #[test]
    fn child_row_without_parent_is_an_orphan() {
        let g = Grouper::new(plan("sales_invoice", &["number", "item", "qty"], &[]));
        let groups = all_groups(g, vec![row(1, &["", "bolt", "2"])]);
        let Some(Group::Fault(RowFault::Orphan(orphan))) = groups.first() else {
            panic!("a parentless child row must be an orphan fault: {groups:?}")
        };
        assert_eq!(orphan.number, 1);
        assert_eq!(groups.len(), 1);
    }

    /// A bad cell fails its row alone: the group state stands, the row comes back as
    /// [`RowFault::Cells`], and the group closes cleanly afterwards.
    #[test]
    fn a_bad_cell_refuses_its_row_only() {
        let g = Grouper::new(plan("sales_invoice", &["number", "item", "qty"], &[]));
        let groups = all_groups(
            g,
            vec![
                row(1, &["SI-1", "bolt", "2"]),
                row(2, &["", "nut", "not a number"]),
                row(3, &["", "screw", "3"]),
            ],
        );
        // Row 2 refused with its failed cell; rows 1+3 build the parent.
        let Some(Group::Fault(RowFault::Cells(refused, failed))) = groups.first() else {
            panic!("the bad row must be refused: {groups:?}")
        };
        assert_eq!(refused.number, 2);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].header, "qty");
        assert_eq!(failed[0].text, "not a number");
        assert_eq!(
            failed[0].error.code, "import.bad_value",
            "the raw text and the code feed the report"
        );
        let Some(Group::Done { rows, children, .. }) = groups.get(1) else {
            panic!("the parent must still close at finish: {groups:?}")
        };
        assert_eq!(rows.iter().map(|r| r.number).collect::<Vec<_>>(), [1, 3]);
        let (_, child_rows) = &children[0];
        assert_eq!(child_rows.len(), 2, "good child rows on both sides survive");
    }

    /// Child values repeating the open child row's header open the next child row; a
    /// parent value mid-table also closes it.
    #[test]
    fn repeated_child_header_opens_the_next_child_row() {
        // Two `qty`-style headers cannot repeat in one file, so use two `item` columns:
        // item | qty header pairs. Simpler trick: `kind` then `item` then `item` again is
        // a duplicate-column case; instead rely on the documented rule directly: the same
        // header twice in one child row is the signal the NEXT row began.
        let g = Grouper::new(plan(
            "sales_invoice",
            &["number", "item", "qty", "kind"],
            &[],
        ));
        // Row 1 opens: number + item(bolt). Row 2 repeats `item` -> second child row.
        let groups = all_groups(
            g,
            vec![
                row(1, &["SI-1", "bolt", "2", "Goods"]),
                row(2, &["", "nut", "1", "Service"]),
                row(3, &["", "bolt", "4", "Goods"]),
            ],
        );
        assert_eq!(groups.len(), 1);
        let Some(Group::Done { children, .. }) = groups.into_iter().next() else {
            panic!("one parent at finish")
        };
        let (_, child_rows) = &children[0];
        // `item` repeats per row, so every child value row stays its own child row.
        assert_eq!(
            child_rows.len(),
            3,
            "one child row per file row: {child_rows:?}"
        );
    }

    /// A row that sets nothing at all (all cells empty) never STARTS a parent: it extends
    /// the open one, and rows around it still group per the rule (a row that sets a parent
    /// column starts a new parent).
    #[test]
    fn an_all_empty_row_neither_opens_nor_splits() {
        let g = Grouper::new(plan("customer", &["name", "rating"], &[]));
        let groups = all_groups(
            g,
            vec![
                row(1, &["Acme", "A"]),
                row(2, &["", ""]),
                row(3, &["Beta", "B"]),
            ],
        );
        // The empty row extended parent 1 (it set nothing, so it did not open a group);
        // row 3 sets a parent column, so it opens parent 2. Two parents, no phantom
        // parent from the empty row itself.
        assert_eq!(groups.len(), 2, "{groups:?}");
        let Some(Group::Done { rows, fields, .. }) = groups.first() else {
            panic!("parent 1: {groups:?}")
        };
        assert_eq!(rows.iter().map(|r| r.number).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(fields[0].1.text, "Acme");
        let Some(Group::Done { rows, .. }) = groups.get(1) else {
            panic!("parent 2: {groups:?}")
        };
        assert_eq!(rows.iter().map(|r| r.number).collect::<Vec<_>>(), [3]);
    }
}
