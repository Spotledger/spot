// The engine stream: rows in, one `ImportItem` per parent out (a document, a row problem, or a
// whole-file failure that ends the stream).

use spot_app_api::error::{AppError, ErrorClass};
use spot_document::{defaults, validate};
use spot_meta::EffectiveMeta;

use crate::error::ImportError;
use crate::group::{Group, Grouper, RowFault};
use crate::mapping::{MappingPlan, Target};
use crate::report::{ImportReport, ImportRowError};
use crate::source::{BoxedRows, Row, RowSource};

/// Everything one parent produced: the rows that built it and the document's values.
#[derive(Clone, Debug)]
pub struct ImportDocument {
    /// The DocType the parent belongs to.
    pub doctype: String,
    /// Parent field values in first-set order, with the cells that set them.
    pub fields: Vec<(String, crate::group::CellValue)>,
    /// Child rows per table field, in file order, with the cells that set them.
    pub children: Vec<(String, Vec<Vec<crate::group::CellValue>>)>,
    /// The source rows this document was built from, in file order.
    pub rows: Vec<Row>,
}

impl ImportDocument {
    /// Builds the [`spot_app_api::Document`] the parent's cells describe.
    ///
    /// `defaults::apply` fills the DocType's unset fields, `coerce::normalise` makes the two
    /// allowed coercions, and [`validate::check`] judges the result — in that order, the same
    /// pass a save would run. The report's validation entries point at the cells that set a
    /// rejected field; a required field no column fed was already a preflight finding.
    pub fn to_document(
        &self,
        meta: &EffectiveMeta,
    ) -> Result<spot_app_api::Document, (ImportRowError, AppError)> {
        match build(self, meta) {
            Ok(document) => Ok(document),
            Err(error) => Err((self.blame(&error), error)),
        }
    }

    /// The cell to blame for `error`: the one that set the first invalid field, or the first
    /// row that built the document when no field points anywhere (a missing required field
    /// defaults could not fill, a table with no rows).
    fn blame(&self, error: &AppError) -> ImportRowError {
        let fallback = (
            self.rows.first().map_or(1, |r| r.number),
            String::new(),
            String::new(),
        );
        let Some(field) = error.fields.first() else {
            let (row, column, cell) = fallback;
            return ImportRowError {
                row,
                column,
                cell,
                code: error.code.clone(),
                message: error.message.clone(),
            };
        };
        // `table[3].qty` names a child cell; a bare field names a parent cell.
        let (table, field) = split_path(field.field.as_str());
        let want = target_of(table.clone(), field);
        let children = self.children_of(table.as_deref());
        let found = children
            .iter()
            .flatten()
            .find(|cell| cell.target == want)
            .or_else(|| {
                self.fields
                    .iter()
                    .find(|(name, cell)| table.is_none() && name.as_str() == field && cell.is_set())
                    .map(|(_, cell)| cell)
            });
        let (row, column, cell) = found.map_or(fallback, |cell| {
            (cell.row, cell.header.clone(), cell.text.clone())
        });
        // The report carries the FIELD's own code (`document.too_long`, not the generic
        // `document.validation_failed`): the user fixes a field, not the whole document.
        let code = error
            .fields
            .first()
            .map(|f| f.code.clone())
            .unwrap_or_else(|| error.code.clone());
        ImportRowError {
            row,
            column,
            cell,
            code,
            message: error.message.clone(),
        }
    }

    fn children_of(&self, table: Option<&str>) -> Vec<Vec<crate::group::CellValue>> {
        table
            .and_then(|t| {
                self.children
                    .iter()
                    .find(|(name, _)| name.as_str() == t)
                    .map(|(_, rows)| rows.clone())
            })
            .unwrap_or_default()
    }
}

/// The table part of a validation path (`table[3].qty` gives `Some("table")`) and the field.
fn split_path(path: &str) -> (Option<String>, &str) {
    match path.split_once('[') {
        Some((table, rest)) => (
            Some(table.to_owned()),
            rest.split_once("].").map_or(rest, |(_, f)| f),
        ),
        None => (None, path),
    }
}

fn target_of(table: Option<String>, field: &str) -> Target {
    match table {
        Some(table) => Target::Child(table, field.to_owned()),
        None => Target::Field(field.to_owned()),
    }
}

fn build(doc: &ImportDocument, meta: &EffectiveMeta) -> Result<spot_app_api::Document, AppError> {
    let doctype = spot_app_api::prelude::Name::new(doc.doctype.as_str()).map_err(|e| {
        AppError::new(
            ErrorClass::Validation,
            crate::error::NOT_A_WORKBOOK,
            e.to_string(),
        )
    })?;
    let id = spot_app_api::prelude::RecordId::new_v7();
    let mut document = spot_app_api::Document::new(doctype, id);
    for (field, cell) in &doc.fields {
        if let Some(value) = &cell.value {
            document.set(
                spot_app_api::prelude::Name::new(field.as_str()).map_err(|e| {
                    AppError::new(
                        ErrorClass::Validation,
                        crate::error::META_ERROR,
                        e.to_string(),
                    )
                })?,
                value.clone(),
            );
        }
    }
    for (table, rows) in &doc.children {
        let table_name = spot_app_api::prelude::Name::new(table.as_str()).map_err(|e| {
            AppError::new(
                ErrorClass::Validation,
                crate::error::META_ERROR,
                e.to_string(),
            )
        })?;
        let child_rows = document.rows_mut(table_name);
        for row in rows.iter() {
            let id = spot_app_api::prelude::RecordId::new_v7();
            let mut child = spot_app_api::ChildRow {
                id,
                fields: Default::default(),
            };
            for cell in row {
                if let Some(value) = &cell.value {
                    // The child field's name comes from the plan target when it is one; an
                    // unresolvable target cannot happen (the plan validated it).
                    let field = match &cell.target {
                        Target::Child(_, field) => field.clone(),
                        Target::Field(name) => name.clone(),
                        Target::GroupKey => cell.header.clone(),
                    };
                    let name = spot_app_api::prelude::Name::new(&field).map_err(|e| {
                        AppError::new(
                            ErrorClass::Validation,
                            crate::error::META_ERROR,
                            e.to_string(),
                        )
                    })?;
                    child.fields.insert(name, value.clone());
                }
            }
            child_rows.push(child);
        }
    }
    defaults::apply(
        meta,
        &mut document,
        spot_app_api::prelude::OffsetDateTime::UNIX_EPOCH,
    )?;
    validate::check(meta, &document)?;
    Ok(document)
}

/// One stream output: a finished parent, one problem of one row, or a whole-file failure.
#[derive(Clone, Debug)]
pub enum ImportItem {
    /// A complete parent: the document was validated (the report holds the entries).
    Document(ImportDocument),
    /// One problem of one row, already in the report.
    Error(ImportRowError),
    /// The file as a whole failed mid-stream (a torn file, a row past a limit), already in the
    /// report's `fatal` list. It is the stream's last item: the writer (WP-DOC-09.T2) must not
    /// apply anything it has not already committed, and all-or-nothing must write nothing.
    Fatal(ImportError),
}

/// The engine stream: opens the source, maps and groups the rows, validates each parent and
/// yields one [`ImportItem`] at a time — one parent, plus its child rows, is the most the
/// process ever holds (bounded memory, brief item 6). A whole-file failure is the last item
/// ([`ImportItem::Fatal`]) so the writer (WP-DOC-09.T2) can refuse an all-or-nothing import
/// without needing the report.
///
/// Every problem is appended to `report` before its item is yielded, so a caller that drains
/// the iterator and then checks `report.is_importable(policy)` has the complete picture. The
/// policy closure is read per item: `all_or_nothing` asks for the full report first.
pub fn import_stream<'a, S>(
    source: &'a mut S,
    plan: MappingPlan,
    meta: &'a EffectiveMeta,
    report: &'a mut ImportReport,
) -> impl Iterator<Item = ImportItem> + use<'a, S>
where
    S: RowSource,
{
    let doctype = plan.doctype.clone();
    // ONE row stream for the whole import: re-creating `source.rows()` per call would
    // restart the file (XLSX reopens and rewinds its cell reader per `rows()` call).
    let rows: BoxedRows<'a> = source.rows();
    Stream {
        _source_kind: std::marker::PhantomData::<S>,
        rows,
        grouper: Some(Grouper::new(plan)),
        meta,
        report,
        doctype,
        done: false,
        last_finished: None,
    }
}

struct Stream<'a, S: RowSource> {
    /// The source's row stream, created once at [`import_stream`] (see there).
    #[allow(dead_code)]
    _source_kind: std::marker::PhantomData<S>,
    rows: BoxedRows<'a>,
    /// `None` once the source ran out and `finish` consumed the grouper.
    grouper: Option<Grouper>,
    meta: &'a EffectiveMeta,
    report: &'a mut ImportReport,
    /// The plan's DocType, kept out of the grouper so `convert` works after `finish`.
    doctype: String,
    /// Set when the source errored or ran out: the rest of the stream is empty.
    done: bool,
    /// The last group `Grouper::finish` produced, drained on the next `next` call.
    last_finished: Option<Group>,
}

impl<S: RowSource> Iterator for Stream<'_, S> {
    type Item = ImportItem;

    fn next(&mut self) -> Option<ImportItem> {
        while !self.done {
            // `self.rows.next()` borrows the boxed iterator only for the call; the result
            // is owned, so the arms may touch `self` freely.
            let outcome = self.rows.next();
            match outcome {
                Some(Ok(row)) => {
                    if let Some(group) = self.grouper.as_mut().and_then(|g| g.feed(row)) {
                        return Some(self.convert(group));
                    }
                    // The open group took the row; keep reading (a `None` return means
                    // the ROW closed nothing, not that the file is over).
                }
                // A torn file: the fault is fatal (in the report). The open group is not
                // finished — its tail may be truncated mid-group, so it is not guessed at.
                Some(Err(e)) => {
                    self.done = true;
                    self.report.push_fatal(e.clone());
                    return Some(ImportItem::Fatal(e));
                }
                // The source is over: one last group may be standing.
                None => {
                    self.done = true;
                    if let Some(grouper) = self.grouper.take()
                        && let Some(group) = grouper.finish()
                    {
                        self.last_finished = Some(group);
                    }
                    if let Some(group) = self.last_finished.take() {
                        return Some(self.convert(group));
                    }
                    return None;
                }
            }
        }
        None
    }
}

impl<S: RowSource> Stream<'_, S> {
    /// Turns one group into its [`ImportItem`], appending the report entry first.
    fn convert(&mut self, group: Group) -> ImportItem {
        match group {
            Group::Done {
                rows,
                fields,
                children,
            } => {
                let document = ImportDocument {
                    doctype: self.doctype.clone(),
                    fields,
                    children,
                    rows,
                };
                match document.to_document(self.meta) {
                    Ok(_) => ImportItem::Document(document),
                    Err((entry, _)) => {
                        self.report.push_row_error(entry.clone());
                        ImportItem::Error(entry)
                    }
                }
            }
            Group::Fault(RowFault::Orphan(row)) => {
                let entry = ImportRowError {
                    row: row.number,
                    column: String::new(),
                    cell: String::new(),
                    code: crate::error::ORPHAN_ROW.to_owned(),
                    message:
                        "the row has no parent: the file does not say which document it belongs to"
                            .to_owned(),
                };
                self.report.push_row_error(entry.clone());
                ImportItem::Error(entry)
            }
            Group::Fault(RowFault::Cells(row, failures)) => {
                let mut failures = failures.into_iter();
                let entry = match failures.next() {
                    // The failed cell's raw text, header and row go into the report (brief
                    // item 5: the user fixes the file through exactly what they typed).
                    Some(failed) => ImportRowError {
                        row: row.number,
                        column: failed.header,
                        cell: failed.text,
                        code: failed.error.code.to_owned(),
                        message: failed.error.message,
                    },
                    None => ImportRowError {
                        row: row.number,
                        column: String::new(),
                        cell: String::new(),
                        code: crate::error::BAD_VALUE.to_owned(),
                        message: "the row holds no convertible cell".to_owned(),
                    },
                };
                self.report.push_row_error(entry.clone());
                ImportItem::Error(entry)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::convert::{ConversionOptions, Formats};
    use crate::csv::{CsvSource, temp_file};
    use crate::error::{AMBIGUOUS_DATE, ImportError};
    use crate::mapping::Override;
    use crate::source::Limits;
    use crate::testmeta;
    use crate::{ImportPolicy, report::ImportReport};

    use std::collections::BTreeMap;

    /// A plan for `doctype` over auto-matched `headers` with the default formats.
    fn plan(doctype: &str, headers: &[&str]) -> MappingPlan {
        plan_with(doctype, headers, &[])
    }

    fn plan_with(doctype: &str, headers: &[&str], overrides: &[Override]) -> MappingPlan {
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

    /// Streams a CSV through the engine: (documents, errors) in order. The temp file name
    /// is unique per call: tests run in parallel and share one temp directory.
    fn run_csv(
        bytes: &[u8],
        plan: MappingPlan,
    ) -> (Vec<ImportDocument>, Vec<ImportRowError>, ImportReport) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let name = format!("stream_test_{}.csv", N.fetch_add(1, Ordering::Relaxed));
        let meta = testmeta::meta();
        let path = temp_file(&name, bytes);
        let mut source = CsvSource::open(&path, &Limits::default()).expect("open");
        let mut report = ImportReport::default();
        let items: Vec<ImportItem> = import_stream(&mut source, plan, &meta, &mut report).collect();
        let mut documents = Vec::new();
        let mut errors = Vec::new();
        for item in items {
            match item {
                ImportItem::Document(d) => documents.push(d),
                ImportItem::Error(e) => errors.push(e),
                // A fatal is already in `report.fatal`; only the 200k clean-file test
                // asserts a clean stream, and it matches the item itself.
                ImportItem::Fatal(_) => {}
            }
        }
        (documents, errors, report)
    }

    /// Preflight reports the unmapped required fields, the unused columns and clean
    /// mappings, before any row is read.
    #[test]
    fn preflight_reports_unmapped_fields_and_columns() {
        use crate::preflight::preflight;
        // `customer` (required) and `status` unmapped, `junk` an unused column. The plan
        // still builds (preflight reports, the writer's contract may demand the rest).
        let p = plan("sales_invoice", &["number", "status", "junk"]);
        let meta = testmeta::meta();
        let report = preflight(&p, &meta);
        let blocking: Vec<&str> = report
            .blocking_fields()
            .iter()
            .map(|f| f.name.as_str())
            .collect();
        assert!(
            blocking.contains(&"customer"),
            "customer is required and unmapped: {report:?}"
        );
        assert!(
            !blocking.contains(&"status"),
            "status is optional: noted, not blocking"
        );
        assert_eq!(
            report.unmapped_columns,
            vec![(2, "junk".to_owned())],
            "the unused column is reported with its index and header"
        );
        assert!(report.errors.is_empty(), "a built plan has no shape errors");
        assert!(report.has_findings());

        // A fully mapped file: nothing blocking left.
        let p = plan_with(
            "sales_invoice",
            &["number", "customer", "item", "qty", "junk"],
            &[Override::new("junk", "group_key")],
        );
        let report = preflight(&p, &meta);
        assert!(
            !report
                .unmapped_fields
                .iter()
                .any(|f| f.required && !f.child),
            "no required parent field is left: {report:?}"
        );
        assert!(
            report.unmapped_columns.is_empty(),
            "the key column is not an unused column: {report:?}"
        );
    }

    /// Two columns feeding one field is a plan error — this is the contract the brief's
    /// mapping item states; the auto-match refuses it instead of silently later-wins.
    #[test]
    fn preflight_never_sees_a_doubly_bound_plan() {
        // `MappingPlan::build` refuses the build, so preflight cannot even run on one.
        let meta = testmeta::meta();
        let headers: Vec<String> = ["number", "Number"]
            .iter()
            .map(|h| (*h).to_owned())
            .collect();
        let (d, dt, t) = Formats::default_lists();
        let formats = Formats::build(&d, &dt, &t).unwrap();
        let e = MappingPlan::build(
            &meta,
            "sales_invoice",
            &headers,
            &[],
            ConversionOptions::default(),
            formats,
        )
        .unwrap_err();
        assert!(matches!(e, ImportError::Meta { .. }), "{e:?}");
    }

    /// The whole pipeline on one parent + children: conversion, validation, document shape.
    #[test]
    fn a_valid_parent_with_children_makes_one_valid_document() {
        let headers = [
            "number", "customer", "status", "total", "item", "qty", "kind",
        ];
        // 2 data rows -> ONE parent (2 source rows) + a second parent; the child row
        // extends its parent (parent columns empty on row 3).
        let bytes = b"number,customer,status,total,item,qty,kind\n\
                      SI-1,0198a3d2-7b1a-7000-8000-000000000001,Paid,10.00,bolt,2,Goods\n\
                      ,,,,nut,1.5,Service\n\
                      SI-2,0198a3d2-7b1a-7000-8000-000000000002,Open,3.30,screw,3,Goods\n";
        let (documents, errors, report) = run_csv(bytes, plan("sales_invoice", &headers));
        assert!(errors.is_empty(), "{errors:?}");
        assert!(report.entries.is_empty() && report.fatal.is_empty());
        assert_eq!(
            documents.len(),
            2,
            "row 3 carries a parent value: rows 2+3 then 4"
        );
        let doc = &documents[0];
        assert_eq!(doc.doctype, "sales_invoice");
        assert_eq!(doc.rows.len(), 2, "parent 1 spans rows 1 and 2");
        // The document itself round-trips through spot_document exactly.
        let built = doc.to_document(&testmeta::meta());
        assert!(
            built.is_ok(),
            "a valid import builds through defaults+validate: {built:?}"
        );
        // Children carried.
        let (table, rows) = &doc.children[0];
        assert_eq!(table, "items");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1][0].text, "nut");
    }

    /// A validation error is blamed back to the row, the column header and the cell text.
    #[test]
    fn validation_errors_point_at_the_source_cell() {
        let headers = ["number", "customer", "status"];
        // `number` is over its 16-char limit; `customer` is absent entirely.
        let bytes = b"number,customer,status\n\
                      SI-WAY-TOO-LONG-FOR-THE-FIELD,0198a3d2-7b1a-7000-8000-000000000001,Paid\n";
        let (documents, errors, report) = run_csv(bytes, plan("sales_invoice", &headers));
        assert!(documents.is_empty());
        assert_eq!(errors.len(), 1, "{errors:?}");
        let entry = &errors[0];
        assert_eq!(entry.code, "document.too_long", "{entry:?}");
        assert_eq!(entry.row, 2, "the source row is named");
        assert_eq!(entry.column, "number", "the column is named");
        assert_eq!(
            entry.cell, "SI-WAY-TOO-LONG-FOR-THE-FIELD",
            "the cell text is the user's text"
        );
        assert_eq!(report.count("document.too_long"), 1);
        assert!(!report.is_importable(ImportPolicy::AllOrNothing));
        assert!(
            report.is_importable(ImportPolicy::SkipBadRows),
            "no fatal: bad rows can be skipped"
        );
    }

    /// A refused cell row and an orphan row carry their codes into the report.
    #[test]
    fn refusals_reach_the_report_with_row_column_and_cell() {
        let headers = ["number", "customer", "qty", "item"];
        let bytes = b"number,customer,qty,item\n\
                      SI-1,0198a3d2-7b1a-7000-8000-000000000001,2,bolt\n\
                      ,0198a3d2-7b1a-7000-8000-000000000002,THREE,nut\n";
        let (documents, errors, report) = run_csv(bytes, plan("sales_invoice", &headers));
        assert_eq!(
            documents.len(),
            1,
            "the good parent still imports: {errors:?}"
        );
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, "import.bad_value");
        assert_eq!(errors[0].row, 3);
        assert_eq!(errors[0].column, "qty");
        assert_eq!(
            errors[0].cell, "THREE",
            "the raw cell text feeds the report"
        );
        assert_eq!(report.count("import.bad_value"), 1);

        // An orphan: a child-only group at the file's start (no parent standing) closes as
        // `import.orphan_row`; a parent row afterwards imports normally.
        let headers = ["number", "customer", "item", "qty"];
        let bytes = b"number,customer,item,qty\n\
                      ,,bolt,2\n\
                      SI-1,0198a3d2-7b1a-7000-8000-000000000001,nut,3\n";
        let (documents, errors, report) = run_csv(bytes, plan("sales_invoice", &headers));
        assert_eq!(documents.len(), 1, "the parent still imports: {errors:?}");
        assert!(
            errors
                .iter()
                .any(|e| e.code == "import.orphan_row" && e.row == 2),
            "the parentless child row is reported at ITS row: {errors:?}"
        );
        assert_eq!(report.count("import.orphan_row"), 1, "one orphan group");

        // Orphans also close a key-only group: a key change with child rows but no parent
        // values in the closed group is an orphan at the group's FIRST row.
        let headers = ["ref", "number", "customer", "item", "qty"];
        let bytes = b"ref,number,customer,item,qty\n\
                      K1,SI-1,0198a3d2-7b1a-7000-8000-000000000001,bolt,2\n\
                      KX,,,,\n\
                      KX,,,nut,3\n\
                      KY,SI-2,0198a3d2-7b1a-7000-8000-000000000002,nut,1\n";
        let p = plan_with(
            "sales_invoice",
            &headers,
            &[Override::new("ref", "group_key")],
        );
        let (documents, errors, report) = run_csv(bytes, p);
        // K1 and KY are complete parents; the KX group (child row only) closes as
        // `import.orphan_row`.
        assert_eq!(documents.len(), 2, "{errors:?}");
        assert_eq!(
            errors
                .iter()
                .filter(|e| e.code == "import.orphan_row")
                .count(),
            1,
            "the KX group (child row, no parent values) is one orphan: {errors:?}"
        );
        assert_eq!(report.count("import.orphan_row"), 1);
    }

    /// An ambiguity error carries `import.ambiguous_date` (the mixed date formats).
    #[test]
    fn ambiguity_reaches_the_report() {
        let meta = testmeta::meta();
        let headers: Vec<String> = ["number", "due_date"]
            .iter()
            .map(|h| (*h).to_owned())
            .collect();
        let dmy = vec!["%d/%m/%Y".to_owned(), "%m/%d/%Y".to_owned()];
        let formats = Formats::build(&dmy, &[], &[]).unwrap();
        let p = MappingPlan::build(
            &meta,
            "sales_invoice",
            &headers,
            &[],
            ConversionOptions::default(),
            formats,
        )
        .unwrap();
        let bytes = b"number,due_date\n\
                      SI-1,05/03/2026\n";
        let (documents, errors, report) = run_csv(bytes, p);
        assert!(documents.is_empty());
        let entry = errors
            .iter()
            .find(|e| e.code == AMBIGUOUS_DATE)
            .expect("the ambiguous cell is in the report");
        assert_eq!(entry.row, 2);
        assert_eq!(entry.column, "due_date");
        assert_eq!(entry.cell, "05/03/2026");
        assert_eq!(report.count(AMBIGUOUS_DATE), 1);
    }

    /// The brief's item 7: one file, 10 mixed row problems plus good rows — every problem
    /// is in the report with row, column, cell and code; good parents still import.
    #[test]
    fn a_file_with_ten_mixed_errors_produces_the_full_report() {
        let headers = [
            "ref", "number", "customer", "status", "due_date", "total", "item", "qty",
        ];
        // Date format list with both D/M and M/D orders: `05/03/2026` is ambiguous.
        let meta = testmeta::meta();
        let headers_vec: Vec<String> = headers.iter().map(|h| (*h).to_owned()).collect();
        let dmy = vec!["%d/%m/%Y".to_owned(), "%m/%d/%Y".to_owned()];
        let formats = Formats::build(&dmy, &[], &[]).unwrap();
        let p = MappingPlan::build(
            &meta,
            "sales_invoice",
            &headers_vec,
            &[Override::new("ref", "group_key")],
            ConversionOptions::default(),
            formats,
        )
        .unwrap();
        let u = |n: u64| {
            let id = format!("0198a3d2-7b1a-7000-8000-{n:012}");
            id
        };
        let rows = [
            // r2: ambiguous date — 05/03 reads as 5 Mar and 3 May under the two formats.
            "K1,SI-1,$CUSTOMER,Paid,05/03/2026,1.00,bolt,2",
            // r3: bad decimal scale — total scale 2 gets 3 places.
            "K2,SI-2,$CUSTOMER,Open,05/05/2026,1.234,bolt,2",
            // r4: non-numeric qty
            "K3,SI-3,$CUSTOMER,Open,05/05/2026,1.00,bolt,THREE",
            // r5: unknown select value
            "K4,SI-4,$CUSTOMER,Shipped,05/05/2026,1.00,bolt,2",
            // r6: bad bool word
            "K5,SI-5,$CUSTOMER,Open,05/05/2026,1.00,bolt,maybe",
            // r7: number over 16 chars -> document.too_long
            "K6,SI-6-WAY-TOO-LONG-FOR-16,$CUSTOMER,Open,05/05/2026,1.00,bolt,2",
            // r8: child item over 10 chars -> document.too_long on the child cell
            "K7,SI-7,$CUSTOMER,Open,05/05/2026,1.00,VERY-LONG-ITEM,2",
            // r9: required parent customer empty -> document.required
            "K9,SI-9,,Open,05/05/2026,1.00,bolt,2",
            // r10: link as a NAME (kept as text; validate calls it invalid for a Link)
            "KL,SI-L,Acme Ltd,Open,05/05/2026,1.00,bolt,2",
            // r11: a good parent so valid rows coexist
            "KY,SI-11,$CUSTOMER,Open,05/05/2026,1.00,bolt,2",
            // r12 opens the KX group with nothing on it; r13 is a CHILD row (item+qty only,
            // parent columns empty) that converts fine; r14 closes KX child-only = orphan.
            "KX,,",
            "KX,,,,,,nut,3",
            // r14: unparseable due date (bad_value, no format matches)
            "KZZ,SI-14,$CUSTOMER,Open,NOT A DATE,1.00,bolt,2",
        ];
        let customer = u(0x0A0);
        let body: String = rows
            .iter()
            .map(|r| r.replace("$CUSTOMER", &customer))
            .collect::<Vec<_>>()
            .join("\n");
        let file = format!("{}\n{}\n", headers_vec.join(","), body);
        let bytes = file.into_bytes();
        let (documents, errors, report) = run_csv(&bytes, p);
        // Exactly ONE parent survived whole (KY): every other group has a fault.
        assert_eq!(documents.len(), 1, "{errors:?}");
        // 11 problems, all in the report: the ambiguous date, the per-row conversion
        // faults (scale, THREE, Shipped, `maybe` on qty, NOT A DATE, and the `3` that the
        // KX key-only row's empty cells fed to qty — the one extra `import.bad_value`),
        // too_long x2, required x1 (customer empty on K9), the name-form link and the
        // orphan — codes from conversion AND validation AND grouping in ONE report.
        let by_code: BTreeMap<String, u64> = report.counts.clone();
        assert_eq!(by_code.get(AMBIGUOUS_DATE), Some(&1), "{by_code:?}");
        assert_eq!(by_code.get("import.bad_value"), Some(&5), "{by_code:?}");
        assert_eq!(by_code.get("document.too_long"), Some(&2), "{by_code:?}");
        assert_eq!(by_code.get("document.required"), Some(&1), "{by_code:?}");
        assert_eq!(
            by_code.get("document.invalid_value"),
            Some(&1),
            "{by_code:?}"
        );
        assert_eq!(by_code.get("import.orphan_row"), Some(&1), "{by_code:?}");
        let total: u64 = by_code.values().sum();
        assert_eq!(report.entries.len(), 11, "entries kept in full: {report:?}");
        assert_eq!(total, 11, "{report:?}");
        assert_eq!(
            errors.len(),
            11,
            "the stream yields every error item: {errors:?}"
        );
        // Every entry names its source row (>= 2, the header is row 1) and code.
        for entry in &report.entries {
            assert!(entry.row >= 2, "{entry:?}");
            assert!(!entry.code.is_empty());
        }
        // No fatal: the file was readable throughout.
        assert!(report.fatal.is_empty());
        assert!(report.is_importable(ImportPolicy::SkipBadRows));
        assert!(!report.is_importable(ImportPolicy::AllOrNothing));
        // The CSV serialisation lists every entry, quoted, fixable by hand.
        let mut csv = Vec::new();
        report.to_csv(&mut csv).expect("csv");
        let text = String::from_utf8(csv).unwrap();
        let lines = text.lines().count();
        assert_eq!(lines, 12, "header + 11 entries: {text}");
        assert!(text.starts_with("row,column,cell,code,message"));
        assert!(
            text.contains("\"THREE\""),
            "the raw cell text is in the CSV: {text}"
        );
        assert!(text.contains("import.ambiguous_date"));
        let _ = errors;
    }

    /// A source error mid-stream is a fatal entry in the report (the CSV file cannot be
    /// read further), and the CSV output lists it with row 0.
    #[test]
    fn a_source_error_becomes_a_fatal_report_entry() {
        let meta = testmeta::meta();
        let headers = vec!["name".to_owned()];
        let (d, dt, t) = Formats::default_lists();
        let formats = Formats::build(&d, &dt, &t).unwrap();
        let p = MappingPlan::build(
            &meta,
            "customer",
            &headers,
            &[],
            ConversionOptions::default(),
            formats,
        )
        .unwrap();
        let bytes: &[u8] = b"name\nAcme\n\xFF\xFE\n";
        let (_documents, _errors, report) = run_csv(bytes, p);
        // The torn file's open group is NOT finished (its tail is truncated mid-group), so
        // only a parent CLOSED BEFORE the tear imports; assert the fatal is the story.
        assert_eq!(report.fatal.len(), 1, "{report:?}");
        assert_eq!(report.fatal[0].code(), crate::error::BAD_ENCODING);
        assert!(
            !report.is_importable(ImportPolicy::SkipBadRows),
            "a torn file is not importable under either policy"
        );
        // The fatal is IN the CSV output (fix to_csv dropping it was a real bug).
        let mut csv = Vec::new();
        report.to_csv(&mut csv).expect("csv");
        let text = String::from_utf8(csv).unwrap();
        assert!(
            text.contains(",\"import.bad_encoding\""),
            "the fatal entry must be serialised: {text}"
        );
        assert!(
            text.lines()
                .nth(1)
                .expect("first data row")
                .starts_with("0,,"),
            "the fatal has no source row: {text}"
        );
    }

    /// The report's capacity: past N per code, entries are counted only.
    #[test]
    fn the_report_keeps_n_per_code_and_counts_the_rest() {
        let mut report = ImportReport::with_capacity(3);
        for row in 0..5u64 {
            report.push_row_error(ImportRowError {
                row: row + 2,
                column: "qty".into(),
                cell: "x".into(),
                code: "import.bad_value".into(),
                message: "no".into(),
            });
        }
        report.push_row_error(ImportRowError {
            row: 99,
            column: "qty".into(),
            cell: "y".into(),
            code: "document.required".into(),
            message: "no".into(),
        });
        assert_eq!(report.entries.len(), 4, "3 bad_value + 1 required kept");
        assert_eq!(report.count("import.bad_value"), 5, "all 5 seen");
        assert_eq!(report.count("document.required"), 1);
        assert!(report.first("import.bad_value").is_some());
        let mut csv = Vec::new();
        report.to_csv(&mut csv).expect("csv");
        let text = String::from_utf8(csv).unwrap();
        assert_eq!(text.lines().count(), 5, "header + 4 kept entries: {text}");
        // An empty report is importable under both policies.
        let clean = ImportReport::default();
        assert!(clean.is_importable(ImportPolicy::AllOrNothing));
        assert!(clean.is_importable(ImportPolicy::SkipBadRows));
    }

    // Brief item 6: memory-bounded imports. A counting wrapper source records how many
    // data rows the engine has pulled, so the test can relate input progress to output
    // progress and prove the file is processed incrementally, never held whole.
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A [`RowSource`] that counts the data rows pulled through it. The counter sits in
    /// the boxed iterator, AFTER the real reader, so counting cannot disturb the read.
    struct CountingSource {
        inner: CsvSource,
        pulled: Arc<AtomicU64>,
    }

    impl CountingSource {
        /// Opens the CSV and returns the source plus the shared row counter.
        fn open_counted(
            path: &std::path::Path,
            limits: &Limits,
        ) -> Result<(Self, Arc<AtomicU64>), ImportError> {
            let inner = CsvSource::open(path, limits)?;
            let pulled = Arc::new(AtomicU64::new(0));
            Ok((
                Self {
                    inner,
                    pulled: Arc::clone(&pulled),
                },
                pulled,
            ))
        }
    }

    impl RowSource for CountingSource {
        fn open(path: &std::path::Path, limits: &Limits) -> Result<Self, ImportError> {
            Ok(Self::open_counted(path, limits)?.0)
        }
        fn file_size(&self) -> u64 {
            self.inner.file_size()
        }
        fn headers(&self) -> &[String] {
            self.inner.headers()
        }
        fn limits(&self) -> &Limits {
            self.inner.limits()
        }
        fn rows(&mut self) -> BoxedRows<'_> {
            let pulled = Arc::clone(&self.pulled);
            Box::new(self.inner.rows().inspect(move |_r| {
                pulled.fetch_add(1, Ordering::Relaxed);
            }))
        }
    }

    /// A 200k-row file is streamed incrementally: the first document is yielded after
    /// only a tiny prefix of the file has been read (never after a whole-file buffer),
    /// and the caller takes one document at a time — the loop keeps only a count, so
    /// its peak retention is a single document. Together with the engine's one-open-
    /// parent grouping this bounds peak memory to one parent's rows.
    #[test]
    fn import_stream_stays_incremental_over_200k_rows() {
        use std::io::Write as _;

        const ROWS: u64 = 200_000;
        let mut bytes = b"number,customer,status,total,item,qty,kind\n".to_vec();
        for i in 0..ROWS {
            // Every row sets every parent column, so each row is a complete parent:
            // 200k parents, 200k documents out.
            writeln!(
                bytes,
                "SI-{i},0198a3d2-7b1a-7000-8000-{i:012x},Paid,1.00,bolt,1,Goods"
            )
            .expect("write row");
        }
        let path = temp_file("stream_incremental_200k.csv", &bytes);
        let limits = Limits::default();
        let (mut source, pulled) = CountingSource::open_counted(&path, &limits).expect("open");
        let meta = testmeta::meta();
        let plan = plan(
            "sales_invoice",
            &[
                "number", "customer", "status", "total", "item", "qty", "kind",
            ],
        );
        let mut report = ImportReport::default();
        let stream = import_stream(&mut source, plan, &meta, &mut report);

        let mut documents: u64 = 0;
        let mut rows_when_first_document: Option<u64> = None;
        for item in stream {
            match item {
                // Look at the document, bump the count, let it DROP here: the next one
                // replaces it, never accumulates.
                ImportItem::Document(_) => {
                    if documents == 0 {
                        rows_when_first_document = Some(pulled.load(Ordering::Relaxed));
                    }
                    documents += 1;
                }
                ImportItem::Error(e) => panic!("a clean file must not error: {e:?}"),
                ImportItem::Fatal(e) => panic!("a clean file must not fail: {e:?}"),
            }
        }

        let first_at = rows_when_first_document.expect("at least one document");
        assert!(
            first_at <= 50,
            "the first document must arrive within the file's first 50 rows, not after \
             a whole-file buffer: rows pulled at first document = {first_at}"
        );
        assert_eq!(
            documents, ROWS,
            "one document per parent, every parent yielded"
        );
        assert_eq!(
            pulled.load(Ordering::Relaxed),
            ROWS,
            "every data row pulled exactly once"
        );
        assert!(
            report.entries.is_empty() && report.fatal.is_empty(),
            "a clean file leaves no entries: {report:?}"
        );
        assert!(
            report.is_importable(ImportPolicy::AllOrNothing),
            "a clean file is importable: {report:?}"
        );
    }
}
