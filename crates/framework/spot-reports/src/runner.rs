//! The runner: a [`CompiledReport`], a principal and arguments to the contract's
//! `ReportResult`.
//!
//! Every run goes through the query IR with the caller's permission filter ANDed on (ADR-45,
//! ADR-070 section 12). The steps are fixed:
//!
//! 1. bind and validate arguments (an unknown parameter or a value that does not fit its type
//!    is `report.bad_argument`, naming the parameter, never its value);
//! 2. refuse a filter, sort, group or aggregate on a field the caller cannot read unmasked and report
//!    (`spot-field-guard::check_query_fields_for` with `Channel::Report`, the no-inference rule);
//! 3. AND the read filter for the base DocType and, in each join's `on`, for the joined
//!    DocType (a joined row the caller may not read yields NULLs, never the row);
//! 4. reduce the columns with the field guard for [`Channel::Report`]: a hidden column is
//!    removed with a [`Message`] saying how many, a masked one is masked;
//! 5. execute through a [`QueryExecutor`] with the row cap `limit + 1` to set `truncated`;
//! 6. evaluate the computed columns and encode every value for its column type.
//!
//! A principal with no read right on the base DocType gets an empty result of the same shape,
//! never an error that reveals existence.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value as Json;
use spot_field_guard::{Channel, FieldGuard, QueryField, QueryUse};
use spot_http::api::service::{Message, MessageLevel, ReportColumn, ReportResult};
use spot_perm_query::{PermissionFilter, ReadFilter, ReadTarget};
use spot_permissions::{MaskFormat, PermissionEngine, Principal};
use spot_storage_contract::{Expr, Name, Query, RecordId, Rows, StorageError, Value};

use crate::compiled::{
    CREATED_BY, ColumnOrigin, CompiledFilter, CompiledParameter, CompiledReport, Rhs,
};
use crate::definition::ParamType;
use crate::encode;
use crate::error::{ReportError, codes};

/// A boxed `Send` future, the same idiom the service traits use.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Runs one compiled query. The fake backend implements it in tests; the PostgreSQL backend
/// implements it in the binary. A report never sees SQL.
pub trait QueryExecutor: Send + Sync {
    /// Runs `query` and returns every row.
    fn execute<'a>(&'a self, query: &'a Query) -> BoxFuture<'a, Result<Rows, StorageError>>;
}

/// Where a run's timing line goes. The handle is injected, so no global state.
pub trait Telemetry: Send + Sync {
    /// One report ran: its name, the rows it returned and how long it took, in milliseconds.
    /// Argument values are never passed.
    fn report_run(&self, report: &str, rows: usize, elapsed_ms: u64);
}

/// A telemetry handle that discards everything.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoTelemetry;

impl Telemetry for NoTelemetry {
    fn report_run(&self, _report: &str, _rows: usize, _elapsed_ms: u64) {}
}

/// The message code for columns the caller may not report on that were removed.
pub const COLUMNS_REMOVED: &str = "report.columns_removed";

/// What a run produced.
#[derive(Clone, Debug, PartialEq)]
pub struct RunOutput {
    /// The contract's `ReportResult`.
    pub result: ReportResult,
    /// Non-fatal messages (for example, how many columns were removed).
    pub messages: Vec<Message>,
}

/// The raw arguments of a run: the request's `filters` object.
#[derive(Clone, Debug, Default)]
pub struct ReportArgs {
    values: BTreeMap<String, Json>,
}

impl ReportArgs {
    /// No arguments.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// From the request's `filters`. It must be a JSON object; each value is checked against
    /// its parameter's type when the report binds.
    ///
    /// # Errors
    ///
    /// [`codes::BAD_ARGUMENT`] when `filters` is not an object.
    pub fn from_json(value: &Json) -> Result<Self, ReportError> {
        match value {
            Json::Null => Ok(Self::empty()),
            Json::Object(map) => Ok(ReportArgs {
                values: map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            }),
            _ => Err(ReportError::new(
                codes::BAD_ARGUMENT,
                "`filters` must be an object keyed by parameter name",
            )),
        }
    }

    /// From a map.
    #[must_use]
    pub fn from_map(values: BTreeMap<String, Json>) -> Self {
        ReportArgs { values }
    }
}

/// The bound values of a run, by filter binding name.
#[derive(Clone, Debug, Default)]
struct Bound {
    values: BTreeMap<String, Value>,
}

impl Bound {
    fn resolve(&self, binding: &str) -> Option<Value> {
        self.values.get(binding).cloned()
    }
}

/// How a column is shown to the caller.
#[derive(Clone, Debug)]
enum Vis {
    /// The real value.
    Value,
    /// The masked form.
    Masked(MaskFormat),
    /// Removed.
    Hidden,
}

/// What the field guard says of a column for [`Channel::Report`].
#[derive(Clone, Copy, Debug)]
enum Guarded {
    Value,
    /// Masked, but the guard hands back no format.
    Masked,
    Hidden,
}

/// The stricter of the permission tool's classification and the guard's. `Hidden` beats
/// masked beats the real value. A guard "masked" over a column the permission tool shows in
/// full cannot be masked (no format), so it is hidden: never widened to `read`.
fn strictest(by_perm: Vis, by_guard: Guarded) -> Vis {
    match (by_perm, by_guard) {
        (Vis::Hidden, _) | (_, Guarded::Hidden) => Vis::Hidden,
        (Vis::Masked(f), _) => Vis::Masked(f),
        (Vis::Value, Guarded::Masked) => Vis::Hidden,
        (Vis::Value, Guarded::Value) => Vis::Value,
    }
}

/// The report runner: the decisions, the field guard, the executor and the telemetry handle.
pub struct ReportRunner {
    engine: Arc<PermissionEngine>,
    guard: FieldGuard,
    executor: Arc<dyn QueryExecutor>,
    telemetry: Arc<dyn Telemetry>,
}

impl ReportRunner {
    /// A runner over its collaborators.
    #[must_use]
    pub fn new(
        engine: Arc<PermissionEngine>,
        guard: FieldGuard,
        executor: Arc<dyn QueryExecutor>,
        telemetry: Arc<dyn Telemetry>,
    ) -> Self {
        ReportRunner {
            engine,
            guard,
            executor,
            telemetry,
        }
    }

    /// Runs `report` for `principal` with `args`, honouring at most the report's maximum rows
    /// and never more than the requested `limit`.
    pub fn run<'a>(
        &'a self,
        report: &'a CompiledReport,
        principal: &'a Principal,
        args: &'a ReportArgs,
        limit: Option<u64>,
    ) -> BoxFuture<'a, Result<RunOutput, ReportError>> {
        Box::pin(async move { self.run_inner(report, principal, args, limit).await })
    }

    async fn run_inner(
        &self,
        report: &CompiledReport,
        principal: &Principal,
        args: &ReportArgs,
        limit: Option<u64>,
    ) -> Result<RunOutput, ReportError> {
        let started = Instant::now();
        let bound = bind(report, args)?;
        let effective = effective_limit(report, limit);

        // The report's own query, with the bound filter.
        let mut query = report.build_query(&|binding| bound.resolve(binding))?;

        // The base read filter.
        let base_alias = report.base_alias();
        let base_filter = self.read_filter(principal, report.base.as_str(), &base_alias)?;
        let no_base = matches!(base_filter, ReadFilter::None);
        if let ReadFilter::Filter(expr) = base_filter {
            query.filter = Some(match query.filter.take() {
                Some(existing) => Expr::And(vec![existing, expr]),
                None => expr,
            });
        }

        // Each join's read filter goes into its `on`, so an unreadable joined row yields nulls.
        for (i, join) in report.joins.iter().enumerate() {
            let filter = self.read_filter(principal, join.doctype.as_str(), &join.alias)?;
            match filter {
                ReadFilter::All => {}
                ReadFilter::None => {
                    if let Some(j) = query.joins.get_mut(i) {
                        j.on = Expr::Value(Value::Bool(false));
                    }
                }
                ReadFilter::Filter(expr) => {
                    if let Some(j) = query.joins.get_mut(i) {
                        j.on = Expr::And(vec![j.on.clone(), expr]);
                    }
                }
            }
        }

        // Step 2: the no-inference rule. A caller with no read right on the base is not refused:
        // it gets an empty result of the same shape, so existence is never revealed.
        if !no_base {
            self.check_query_fields(report, principal, &bound)?;
        }

        // Step 4: reduce the columns for the caller. `restrict_columns` is the query-column
        // tool (WP-PLT-04): it judges a field for the least revealing row shape the caller can
        // reach, so an owner-only reader keeps the columns it can report on its own rows. The
        // field guard's `columns` judges without a document and would hide them all.
        let (vis, removed) = self.classify_columns(report, principal, &query);
        let mut messages = Vec::new();
        if removed > 0 {
            messages.push(Message {
                level: MessageLevel::Warning,
                code: COLUMNS_REMOVED.to_owned(),
                text: format!(
                    "{removed} column(s) you may not report on were removed from the result"
                ),
                field: None,
            });
        }
        let kept: BTreeSet<String> = vis
            .iter()
            .filter(|(_, v)| !matches!(v, Vis::Hidden))
            .map(|(name, _)| name.clone())
            .collect();
        let columns: Vec<ReportColumn> = report
            .columns
            .iter()
            .filter(|c| kept.contains(&c.name))
            .map(|c| ReportColumn {
                name: c.name.clone(),
                label: c.label.clone(),
                field_type: c.field_type.clone(),
            })
            .collect();

        // An empty result keeps the shape: no base rows, or no readable columns.
        if no_base || kept.is_empty() {
            self.telemetry.report_run(&report.name, 0, millis(started));
            return Ok(RunOutput {
                result: ReportResult {
                    columns,
                    rows: Vec::new(),
                    truncated: false,
                },
                messages,
            });
        }

        query
            .select
            .retain(|s| keep_select(s.alias.as_str(), &kept));
        query.limit = Some(effective.saturating_add(1));

        let mut rows = self
            .executor
            .execute(&query)
            .await
            .map_err(map_storage_error)?;

        let truncated = rows.rows.len() > effective as usize;
        if truncated {
            rows.rows.truncate(effective as usize);
        }

        // Mask the masked columns before anything reads them.
        for (name, v) in &vis {
            if let Vis::Masked(format) = v
                && let Some(index) = rows.columns.iter().position(|c| c.as_str() == name)
            {
                for row in &mut rows.rows {
                    if let Some(value) = row.get_mut(index) {
                        *value = encode::masked_value(value, format);
                    }
                }
            }
        }

        let out_rows = self.project(report, &vis, &rows)?;
        self.telemetry
            .report_run(&report.name, out_rows.len(), millis(started));
        Ok(RunOutput {
            result: ReportResult {
                columns,
                rows: out_rows,
                truncated,
            },
            messages,
        })
    }

    fn read_filter(
        &self,
        principal: &Principal,
        doctype: &str,
        alias: &Name,
    ) -> Result<ReadFilter, ReportError> {
        let owner_field = Name::new(CREATED_BY)
            .map_err(|e| ReportError::new(codes::INVALID_DEFINITION, e.to_string()))?;
        let target = ReadTarget {
            alias: alias.clone(),
            owner_field,
        };
        PermissionFilter::for_read(&self.engine, principal, doctype, &target).map_err(map_perm)
    }

    fn check_query_fields(
        &self,
        report: &CompiledReport,
        principal: &Principal,
        bound: &Bound,
    ) -> Result<(), ReportError> {
        let mut by_entity: BTreeMap<String, Vec<QueryField>> = BTreeMap::new();
        if let Some(filter) = &report.filter {
            collect_filter_fields(filter, bound, &mut by_entity);
        }
        for path in &report.group_by {
            if is_system_field(path.field.as_str()) {
                continue;
            }
            by_entity
                .entry(path.entity.clone())
                .or_default()
                .push(QueryField::new(path.field.as_str(), QueryUse::Group));
        }
        for order in &report.order {
            if order.agg.is_none() && !is_system_field(order.path.field.as_str()) {
                by_entity
                    .entry(order.path.entity.clone())
                    .or_default()
                    .push(QueryField::new(order.path.field.as_str(), QueryUse::Sort));
            }
        }
        for column in &report.columns {
            if let ColumnOrigin::Aggregate { path, .. } | ColumnOrigin::Avg { path } =
                &column.origin
            {
                if is_system_field(path.field.as_str()) {
                    continue;
                }
                by_entity
                    .entry(path.entity.clone())
                    .or_default()
                    .push(QueryField::new(path.field.as_str(), QueryUse::Aggregate));
            }
        }
        for (entity, fields) in by_entity {
            self.guard
                .check_query_fields_for(Channel::Report, principal, &entity, &fields)
                .map_err(|e| denied(e.paths()))?;
        }
        Ok(())
    }

    fn classify_columns(
        &self,
        report: &CompiledReport,
        principal: &Principal,
        query: &Query,
    ) -> (Vec<(String, Vis)>, usize) {
        let restricted = PermissionFilter::restrict_columns(
            &self.engine,
            principal,
            report.base.as_str(),
            query,
        );
        let kept: BTreeSet<String> = restricted
            .query
            .select
            .iter()
            .map(|s| s.alias.as_str().to_owned())
            .collect();
        let masks: BTreeMap<String, MaskFormat> = restricted
            .masks
            .columns
            .iter()
            .map(|m| (m.column.as_str().to_owned(), m.rule))
            .collect();

        let mut vis: BTreeMap<String, Vis> = BTreeMap::new();
        for column in &report.columns {
            let v = match &column.origin {
                ColumnOrigin::Field(path) | ColumnOrigin::Aggregate { path, .. } => strictest(
                    classify_alias(&column.name, &kept, &masks),
                    self.report_channel_vis(principal, path),
                ),
                ColumnOrigin::Avg { path } => {
                    let sum = format!("{}__sum", column.name);
                    let count = format!("{}__count", column.name);
                    let by_perm = if !kept.contains(&sum) || !kept.contains(&count) {
                        Vis::Hidden
                    } else if let Some(format) = masks.get(&sum).or_else(|| masks.get(&count)) {
                        Vis::Masked(*format)
                    } else {
                        Vis::Value
                    };
                    strictest(by_perm, self.report_channel_vis(principal, path))
                }
                ColumnOrigin::Computed(index) => {
                    let computed = &report.computed[*index];
                    let mut v = Vis::Value;
                    for input in &computed.inputs {
                        let input_vis = vis.get(input).cloned().unwrap_or(Vis::Value);
                        if !matches!(input_vis, Vis::Value) {
                            v = Vis::Hidden;
                            break;
                        }
                    }
                    v
                }
            };
            vis.insert(column.name.clone(), v);
        }
        let removed = report
            .columns
            .iter()
            .filter(|c| matches!(vis.get(&c.name), Some(Vis::Hidden)))
            .count();
        let ordered: Vec<(String, Vis)> = report
            .columns
            .iter()
            .map(|c| {
                (
                    c.name.clone(),
                    vis.get(&c.name).cloned().unwrap_or(Vis::Hidden),
                )
            })
            .collect();
        (ordered, removed)
    }

    /// What the field guard shows of one column for [`Channel::Report`], which needs the
    /// `report` right (not `read`). Fail closed: a guard error, an unknown DocType or a
    /// refusal hides the column. System fields carry no protection and are not asked.
    fn report_channel_vis(
        &self,
        principal: &Principal,
        path: &crate::compiled::FieldPath,
    ) -> Guarded {
        if is_system_field(path.field.as_str()) {
            return Guarded::Value;
        }
        let requested = [path.field.as_str().to_owned()];
        match self
            .guard
            .columns(Channel::Report, principal, &path.entity, &requested)
        {
            Ok(cols) if !cols.refused.is_empty() => Guarded::Hidden,
            Ok(cols) if !cols.allowed.is_empty() => Guarded::Value,
            Ok(cols) if !cols.masked.is_empty() => Guarded::Masked,
            _ => Guarded::Hidden,
        }
    }

    fn project(
        &self,
        report: &CompiledReport,
        vis: &[(String, Vis)],
        rows: &Rows,
    ) -> Result<Vec<Vec<Json>>, ReportError> {
        let index_of = |name: &str| rows.columns.iter().position(|c| c.as_str() == name);
        let mut out = Vec::with_capacity(rows.rows.len());
        for row in &rows.rows {
            let mut projected = Vec::with_capacity(report.columns.len());
            let mut values: BTreeMap<String, Value> = BTreeMap::new();
            for (name, _) in vis {
                if let Some(index) = index_of(name)
                    && let Some(value) = row.get(index)
                {
                    values.insert(name.clone(), value.clone());
                }
            }
            for column in &report.columns {
                let Some((_, v)) = vis.iter().find(|(n, _)| n == &column.name) else {
                    continue;
                };
                if matches!(v, Vis::Hidden) {
                    continue;
                }
                let json = match &column.origin {
                    ColumnOrigin::Field(_) | ColumnOrigin::Aggregate { .. } => {
                        let value = index_of(&column.name)
                            .and_then(|i| row.get(i))
                            .cloned()
                            .unwrap_or(Value::Null);
                        encode::encode_value(&value, &column.field_type)
                    }
                    ColumnOrigin::Avg { .. } => {
                        let sum = index_of(&format!("{}__sum", column.name))
                            .and_then(|i| row.get(i))
                            .cloned()
                            .unwrap_or(Value::Null);
                        let count = index_of(&format!("{}__count", column.name))
                            .and_then(|i| row.get(i))
                            .cloned()
                            .unwrap_or(Value::Null);
                        Json::Array(vec![
                            encode::encode_value(&sum, &encode::FieldType::Decimal { scale: 0 }),
                            encode::encode_value(&count, &encode::FieldType::Int),
                        ])
                    }
                    ColumnOrigin::Computed(index) => {
                        let computed = &report.computed[*index];
                        let json = self.eval_computed(computed, &values)?;
                        encode::encode_value(&json, &computed.field_type)
                    }
                };
                projected.push(json);
            }
            out.push(projected);
        }
        Ok(out)
    }

    fn eval_computed(
        &self,
        computed: &crate::compiled::CompiledComputed,
        values: &BTreeMap<String, Value>,
    ) -> Result<Value, ReportError> {
        let now = spot_expr::DateTime::from_micros(0).ok_or_else(|| {
            ReportError::new(
                codes::EXPRESSION,
                "no clock is available for a computed column",
            )
        })?;
        let mut ctx = spot_expr::MapContext::new(now);
        for (name, value) in values {
            if let Ok(lang) = spot_expr::mapping::from_storage(value) {
                ctx = ctx.with(&format!("doc.{name}"), lang);
            }
        }
        let result = computed
            .expr
            .eval(&ctx)
            .map_err(|e| ReportError::at(codes::EXPRESSION, &computed.name, e.to_string()))?;
        spot_expr::mapping::to_storage(&result).map_err(|_| {
            ReportError::at(
                codes::EXPRESSION,
                &computed.name,
                "the computed value has no storage form",
            )
        })
    }
}

fn effective_limit(report: &CompiledReport, limit: Option<u64>) -> u32 {
    let requested = limit.unwrap_or(u64::from(report.default_limit));
    let capped = requested.min(u64::from(report.max_rows));
    u32::try_from(capped.max(1)).unwrap_or(report.max_rows)
}

fn classify_alias(
    name: &str,
    kept: &BTreeSet<String>,
    masks: &BTreeMap<String, MaskFormat>,
) -> Vis {
    if !kept.contains(name) {
        Vis::Hidden
    } else if let Some(format) = masks.get(name) {
        Vis::Masked(*format)
    } else {
        Vis::Value
    }
}

fn keep_select(alias: &str, kept: &BTreeSet<String>) -> bool {
    if kept.contains(alias) {
        return true;
    }
    for suffix in ["__sum", "__count"] {
        if let Some(base) = alias.strip_suffix(suffix)
            && kept.contains(base)
        {
            return true;
        }
    }
    false
}

fn collect_filter_fields(
    filter: &CompiledFilter,
    bound: &Bound,
    out: &mut BTreeMap<String, Vec<QueryField>>,
) {
    match filter {
        CompiledFilter::All(children) | CompiledFilter::Any(children) => {
            for c in children {
                collect_filter_fields(c, bound, out);
            }
        }
        CompiledFilter::Not(inner) => collect_filter_fields(inner, bound, out),
        CompiledFilter::Cmp {
            entity, field, rhs, ..
        } => {
            if is_system_field(field.as_str()) {
                return;
            }
            if let Rhs::Param(binding) = rhs
                && bound.resolve(binding).is_none()
            {
                return;
            }
            out.entry(entity.clone())
                .or_default()
                .push(QueryField::new(field.as_str(), QueryUse::Filter));
        }
    }
}

/// Whether a field is a storage system column, which carries no business data and is never
/// subject to the no-inference rule (the same set `spot-perm-query` skips).
fn is_system_field(field: &str) -> bool {
    matches!(field, "id" | "version" | "parent" | "parent_field" | "idx")
}

fn bind(report: &CompiledReport, args: &ReportArgs) -> Result<Bound, ReportError> {
    for key in args.values.keys() {
        if !report.parameters.iter().any(|p| &p.name == key) {
            return Err(ReportError::at(
                codes::BAD_ARGUMENT,
                key,
                format!("`{key}` is not a parameter of this report"),
            ));
        }
    }
    let mut values = BTreeMap::new();
    for p in &report.parameters {
        let value = match args.values.get(&p.name) {
            Some(json) => Some(parse_arg(p, json)?),
            None => p.default.clone(),
        };
        match value {
            Some(v) => {
                values.insert(p.binding.clone(), v);
            }
            None if p.required => {
                return Err(ReportError::at(
                    codes::BAD_ARGUMENT,
                    &p.name,
                    format!("the required parameter `{}` was not supplied", p.name),
                ));
            }
            None => {}
        }
    }
    Ok(Bound { values })
}

fn parse_arg(p: &CompiledParameter, json: &Json) -> Result<Value, ReportError> {
    let bad = || {
        ReportError::at(
            codes::BAD_ARGUMENT,
            &p.name,
            format!("`{}` does not fit a `{}` parameter", p.name, p.ty.as_str()),
        )
    };
    match p.ty {
        ParamType::Text => json
            .as_str()
            .map(|s| Value::Text(s.to_owned()))
            .ok_or_else(bad),
        ParamType::Int => match json {
            Json::Number(n) => n.as_i64().map(Value::Int).ok_or_else(bad),
            _ => Err(bad()),
        },
        ParamType::Decimal => match json {
            Json::String(s) => s
                .parse::<rust_decimal::Decimal>()
                .map(Value::Decimal)
                .map_err(|_| bad()),
            Json::Number(n) => n
                .as_i64()
                .map(|i| Value::Decimal(rust_decimal::Decimal::from(i)))
                .ok_or_else(bad),
            _ => Err(bad()),
        },
        ParamType::Date => match json {
            Json::String(s) => {
                time::Date::parse(s, time::macros::format_description!("[year]-[month]-[day]"))
                    .map(Value::Date)
                    .map_err(|_| bad())
            }
            _ => Err(bad()),
        },
        ParamType::Link => match json {
            Json::String(s) => parse_record_id(s).map(Value::Id).ok_or_else(bad),
            _ => Err(bad()),
        },
        ParamType::Select => match json {
            Json::String(s) => match &p.options {
                crate::compiled::ParamOptions::Values(values) if values.contains(s) => {
                    Ok(Value::Text(s.clone()))
                }
                _ => Err(bad()),
            },
            _ => Err(bad()),
        },
    }
}

fn parse_record_id(s: &str) -> Option<RecordId> {
    let uuid = uuid::Uuid::parse_str(s).ok()?;
    if uuid.to_string() != s || uuid.get_version_num() != 7 {
        return None;
    }
    RecordId::from_bytes(*uuid.as_bytes()).ok()
}

fn denied(paths: &[String]) -> ReportError {
    let list = paths.join(", ");
    ReportError::at(
        codes::FIELD_DENIED,
        paths.first().cloned().unwrap_or_default(),
        format!("The caller may not use these fields here: {list}"),
    )
}

fn map_perm(e: spot_perm_query::PermQueryError) -> ReportError {
    match e {
        spot_perm_query::PermQueryError::TooManyShares { .. }
        | spot_perm_query::PermQueryError::FilterTooLarge { .. }
        | spot_perm_query::PermQueryError::QueryTooLarge => {
            ReportError::new(codes::TOO_LARGE, e.to_string())
        }
        spot_perm_query::PermQueryError::FieldNotFilterable { path } => denied(&[path]),
    }
}

fn map_storage_error(e: StorageError) -> ReportError {
    ReportError::new(codes::INVALID_DEFINITION, e.to_string())
}

fn millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}
