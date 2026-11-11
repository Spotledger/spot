//! Diffing the live schema against the catalogue and ordering the safe changes.
//!
//! Every change falls into one class: **additive** (online, no data at risk), **needs backfill**
//! (a required field added to a table that holds rows) or **refused** (destructive or lossy; the
//! expand and contract path of the migration lifecycle, WP-MIG, handles those). A plan with any
//! refusal is never applied, not even in part.
//!
//! # Extensions and exclusion constraints (ADR-087)
//!
//! A schema migration only **requires** an extension; it never creates one. [`required_extensions`]
//! derives the list from the catalogue (a geo field needs `postgis`; an exclusion with scope
//! columns, or the scalar GiST operator class, needs `btree_gist`; a trigram class or typo
//! tolerance needs `pg_trgm`). When [`PlanOptions::extensions`] carries what the database has,
//! the plan refuses a missing extension with [`codes::EXTENSION_UNAVAILABLE`] and an old one with
//! [`codes::EXTENSION_VERSION`], both naming the audited `extension.install` operation as the
//! remedy; [`crate::apply`] performs the same check against the database before it runs a single
//! step, so a tenant is never left half-migrated. `CREATE EXTENSION` is never planned here:
//! [`Run::Extension`] is the unit the install operation (WP-STO-12.T2) plans, and this module's
//! planner never emits it.
//!
//! An exclusion constraint is added **after** every column and index, as one `ALTER TABLE .. ADD
//! CONSTRAINT .. EXCLUDE USING gist`. PostgreSQL has no `NOT VALID` or `CONCURRENTLY` form for
//! it (ADR-087 section 5.3): the statement checks every existing row and builds its index inside
//! the `ALTER`, holding a lock that blocks writes for the duration. The decision, tested on real
//! PostgreSQL:
//!
//! - on a table with no rows (including a table created by the same plan) the step is
//!   [`Risk::Additive`] and runs in a short transaction;
//! - on a table that holds rows it is [`Risk::NeedsValidation`] and runs as [`Run::Outside`], so
//!   it gets the long `index_statement_timeout` (not the 60 s default, which would abort a legal
//!   build) and the `lock_timeout` retry path. The statement is atomic: a failure (existing rows
//!   that overlap, a timeout) leaves no constraint and no index. The overlap error is the
//!   server's primary message, which names the constraint and never row values; repairing the
//!   data first is the operator's job, and a big table gets a maintenance window.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};
use spot_storage_contract::{
    Catalog, EntityDef, EntityKind, ExclusionDef, FieldDef, FieldType, IndexMethod, LinkCheck,
    LinkTarget, Name, OpClass,
};

use crate::ddl::{self, Col, IndexShape};
use crate::error::{Refusal, codes};
use crate::extra::{self, ExtraTable};
use crate::live::{LiveExtension, LiveSchema};
use crate::names::{
    PROGRESS_TABLE, exclusion_name, fk_name, gin_index_name, gist_index_name, length_check_name,
    parent_fk_name, parent_index_name, quote,
};

/// How risky a step is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Risk {
    /// Safe and online.
    Additive,
    /// Part of filling a new required field in a table that holds rows.
    NeedsBackfill,
    /// Validates every existing row and builds an index inside one statement that blocks writes:
    /// an exclusion constraint added to a table that holds rows (ADR-087 section 5.3).
    NeedsValidation,
}

/// What to fill a new required column with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackfillSpec {
    /// The table.
    pub table: String,
    /// The column.
    pub column: String,
    /// The column type, as `format_type` spells it.
    pub ty: String,
    /// The value, as text; cast to `ty` by the database.
    pub value: String,
}

/// How a step runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Run {
    /// These statements in one short transaction.
    Tx(Vec<String>),
    /// One statement outside a transaction (`CREATE INDEX CONCURRENTLY`).
    Outside(String),
    /// Batched, resumable fill of the NULLs of a column; each batch commits on its own.
    Backfill(BackfillSpec),
    /// A last sweep, then `SET NOT NULL`, in one transaction.
    SetNotNull(BackfillSpec),
    /// `prepare` of the backend: installs the change feed capture on new tables and records the
    /// catalogue version. Idempotent.
    Finalise,
    /// A requirement on the database's extension `name`, at least `min_version`: the unit the
    /// audited `extension.install` operation plans (ADR-087 section 4.3). Applying it **checks**
    /// `pg_available_extensions` and `pg_extension` and refuses when they do not satisfy it; it
    /// never runs `CREATE EXTENSION` here (WP-STO-12.T2 owns the install). The schema planner
    /// never emits it.
    Extension {
        /// The extension, from the closed list in [`crate::live::MANAGED_EXTENSIONS`].
        name: String,
        /// The lowest acceptable version, dotted numbers.
        min_version: String,
    },
}

/// One ordered step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    /// A stable identity, for example `add_column:party.note`.
    pub key: String,
    /// The class.
    pub risk: Risk,
    /// What it does, in one sentence.
    pub sentence: String,
    /// How it runs.
    pub run: Run,
}

impl Step {
    fn canonical(&self) -> String {
        let run = match &self.run {
            Run::Tx(sql) => format!("tx:{}", sql.join(";")),
            Run::Outside(sql) => format!("outside:{sql}"),
            Run::Backfill(b) => format!("backfill:{}.{}:{}:{}", b.table, b.column, b.ty, b.value),
            Run::SetNotNull(b) => {
                format!("set_not_null:{}.{}:{}:{}", b.table, b.column, b.ty, b.value)
            }
            Run::Finalise => "finalise".to_owned(),
            Run::Extension { name, min_version } => format!("extension:{name}>={min_version}"),
        };
        format!("{}|{:?}|{run}", self.key, self.risk)
    }
}

/// Inputs of planning beyond the two schemas.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanOptions {
    /// Values to fill a new required field with, by `(entity, field)`, as text. Without one,
    /// the type's zero value is used; a link has none and is refused.
    pub defaults: BTreeMap<(String, String), String>,
    /// What the database has, by extension name (see [`crate::live::LiveExtension`]). With it
    /// the plan refuses a missing or too old extension the catalogue needs; without it
    /// (`None`) the plan does not judge extensions and `apply` still checks them against the
    /// database before it changes anything. [`crate::dry_run`] fills it in.
    pub extensions: Option<BTreeMap<String, LiveExtension>>,
    /// Lowest acceptable extension versions that replace the defaults of
    /// [`default_min_version`], by extension name. A configurable default, not a measured value.
    pub extension_min_versions: BTreeMap<String, String>,
    /// Non-document tables an owning crate contributes (WP-STO-12.T3), in dependency order.
    /// Each one the database does not have yet becomes one step after all document steps; see
    /// the [`crate::ExtraTable`] rules. Empty (the default) changes nothing about a plan.
    pub extra_tables: Vec<ExtraTable>,
}

/// The lowest version of a managed extension a catalogue accepts unless
/// [`PlanOptions::extension_min_versions`] says otherwise. These are conservative floors (the
/// features the constructs use have existed for years), not measured values.
pub fn default_min_version(extension: &str) -> &'static str {
    match extension {
        "postgis" => "3.0",
        "btree_gist" => "1.5",
        "pg_trgm" => "1.4",
        _ => "1.0",
    }
}

/// An extension the catalogue needs, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionNeed {
    /// The extension.
    pub name: String,
    /// The lowest acceptable version.
    pub min_version: String,
    /// The first entity that needs it.
    pub entity: String,
    /// What needs it, in a few words, for the refusal text.
    pub because: String,
}

/// The extensions `wanted` needs, sorted by name (ADR-087 sections 4.3, 5.7 and 7.8). A range
/// field alone needs none: range types and `&&` are core PostgreSQL. A single-column exclusion
/// needs none either.
pub fn required_extensions(wanted: &Catalog, options: &PlanOptions) -> Vec<ExtensionNeed> {
    let mut needs: BTreeMap<String, ExtensionNeed> = BTreeMap::new();
    let mut need = |extension: &str, entity: &EntityDef, because: String| {
        needs
            .entry(extension.to_owned())
            .or_insert_with(|| ExtensionNeed {
                name: extension.to_owned(),
                min_version: options
                    .extension_min_versions
                    .get(extension)
                    .cloned()
                    .unwrap_or_else(|| default_min_version(extension).to_owned()),
                entity: entity.name.as_str().to_owned(),
                because,
            });
    };
    for entity in wanted.entities() {
        for field in &entity.fields {
            if matches!(field.ty, FieldType::Geo { .. }) {
                need("postgis", entity, format!("geo field `{}`", field.name));
            }
        }
        for index in &entity.indexes {
            match index.opclass {
                Some(OpClass::GistTrgm | OpClass::GinTrgm) => {
                    need("pg_trgm", entity, format!("trigram index `{}`", index.name))
                }
                Some(OpClass::GistScalar) => need(
                    "btree_gist",
                    entity,
                    format!("scalar GiST index `{}`", index.name),
                ),
                None => {}
            }
        }
        for x in &entity.exclusions {
            if !x.scope.is_empty() {
                need(
                    "btree_gist",
                    entity,
                    format!("exclusion `{}` with scope columns", x.name),
                );
            }
        }
        if entity.search.as_ref().is_some_and(|s| s.typo_tolerance) {
            need("pg_trgm", entity, "typo tolerant search".to_owned());
        }
    }
    needs.into_values().collect()
}

/// Whether `have` satisfies `need`; `None` when it does, else the refusal. `live` is what the
/// database reports for the extension (`None` is the same as "not offered").
pub fn check_extension(need: &ExtensionNeed, live: Option<&LiveExtension>) -> Option<Refusal> {
    let remedy = "run the audited `extension.install` operation (Admin Center database manager \
                  or the CLI), then plan again";
    let (code, reason) = match live.and_then(|l| l.installed_version.as_deref()) {
        Some(v) if version_at_least(v, &need.min_version) => return None,
        Some(v) => (
            codes::EXTENSION_VERSION,
            format!(
                "extension `{}` is installed at version {v} but the catalogue needs at least {} \
                 ({}); {remedy}",
                need.name, need.min_version, need.because
            ),
        ),
        None => match live.and_then(|l| l.available_version.as_deref()) {
            Some(a) if !version_at_least(a, &need.min_version) => (
                codes::EXTENSION_VERSION,
                format!(
                    "extension `{}` is not installed and the server offers only version {a}, \
                     but the catalogue needs at least {} ({}); install a newer package on the \
                     server, then {remedy}",
                    need.name, need.min_version, need.because
                ),
            ),
            Some(_) => (
                codes::EXTENSION_UNAVAILABLE,
                format!(
                    "extension `{}` is not installed in this database, and the catalogue needs \
                     it ({}); a migration never creates an extension: {remedy}",
                    need.name, need.because
                ),
            ),
            None => (
                codes::EXTENSION_UNAVAILABLE,
                format!(
                    "extension `{}` is not available on this server, and the catalogue needs it \
                     ({}); have it installed on the server, then {remedy}",
                    need.name, need.because
                ),
            ),
        },
    };
    Some(Refusal {
        entity: need.entity.clone(),
        field: None,
        code,
        reason,
    })
}

/// Whether the dotted version `have` is at least `want`. Each segment compares by its leading
/// digits (`4dev` is 4); a missing or non-numeric segment is 0.
pub fn version_at_least(have: &str, want: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> {
        v.split('.')
            .map(|seg| {
                let digits: String = seg.chars().take_while(char::is_ascii_digit).collect();
                digits.parse().unwrap_or(0)
            })
            .collect()
    };
    let (mut h, mut w) = (parts(have), parts(want));
    let len = h.len().max(w.len());
    h.resize(len, 0);
    w.resize(len, 0);
    h >= w
}

/// One column of an exclusion constraint: `column WITH operator`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExclusionElement {
    /// The column.
    pub column: String,
    /// The operator: `=` for a scope column, `&&` for the range.
    pub operator: &'static str,
}

/// An exclusion constraint as a plan object (ADR-087 section 5.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExclusionConstraint {
    /// The table.
    pub table: String,
    /// The constraint's physical name (`spot_<table>_<name>_excl`).
    pub name: String,
    /// The columns and operators, scope first, the range last.
    pub elements: Vec<ExclusionElement>,
    /// A `WHERE (..)` predicate. Only this crate builds one (the contract has none yet), so it
    /// is never text from an app.
    pub(crate) predicate: Option<String>,
}

impl ExclusionConstraint {
    /// The constraint `def` declares on `table`.
    pub fn from_def(table: &Name, def: &ExclusionDef) -> Self {
        let mut elements: Vec<ExclusionElement> = def
            .scope
            .iter()
            .map(|s| ExclusionElement {
                column: s.as_str().to_owned(),
                operator: "=",
            })
            .collect();
        elements.push(ExclusionElement {
            column: def.field.as_str().to_owned(),
            operator: "&&",
        });
        Self {
            table: table.as_str().to_owned(),
            name: exclusion_name(table, &def.name),
            elements,
            predicate: None,
        }
    }

    /// The `WHERE` predicate, if any.
    pub fn predicate(&self) -> Option<&str> {
        self.predicate.as_deref()
    }

    /// The `ALTER TABLE .. ADD CONSTRAINT` statement.
    pub fn sql(&self) -> String {
        ddl::add_exclusion(self)
    }

    /// `pg_get_constraintdef` text, reduced to what must match: no quotes, no blanks, lower case.
    fn comparable(&self) -> String {
        let elements: Vec<String> = self
            .elements
            .iter()
            .map(|e| format!("{} WITH {}", e.column, e.operator))
            .collect();
        let predicate = self
            .predicate()
            .map_or_else(String::new, |p| format!(" WHERE ({p})"));
        comparable_definition(&format!(
            "EXCLUDE USING gist ({}){predicate}",
            elements.join(", ")
        ))
    }
}

fn comparable_definition(text: &str) -> String {
    text.chars()
        .filter(|c| *c != '"' && !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The requirement step for `name` at `min_version`, for the install operation to plan.
pub fn extension_step(name: &str, min_version: &str) -> Step {
    Step {
        key: format!("extension:{name}"),
        risk: Risk::Additive,
        sentence: format!("Require the extension `{name}` at version {min_version} or later."),
        run: Run::Extension {
            name: name.to_owned(),
            min_version: min_version.to_owned(),
        },
    }
}

/// The SHA-256 over the canonical text of `steps`: what [`MigrationPlan::plan_hash`] holds.
/// The hash is stable between runs and changes with any step (an extension requirement or an
/// exclusion included), so a review binds to exactly what it saw.
pub fn plan_hash_of(steps: &[Step]) -> String {
    let mut hasher = Sha256::new();
    for s in steps {
        hasher.update(s.canonical().as_bytes());
        hasher.update(b"\n");
    }
    hasher
        .finalize()
        .iter()
        .fold(String::new(), |mut acc, byte| {
            acc.push_str(&format!("{byte:02x}"));
            acc
        })
}

/// An ordered list of steps and the changes refused.
#[derive(Clone, Debug)]
pub struct MigrationPlan {
    /// The steps, in execution order.
    pub steps: Vec<Step>,
    /// The refused changes. Non-empty means `apply` does nothing.
    pub refused: Vec<Refusal>,
    /// SHA-256 over the canonical step text: a later review binds to it.
    pub plan_hash: String,
    pub(crate) wanted: Catalog,
    pub(crate) options: PlanOptions,
}

impl MigrationPlan {
    /// Whether there is nothing to do and nothing refused.
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty() && self.refused.is_empty()
    }
}

/// Plans the change from `live` to `wanted` with default options.
pub fn plan(live: &LiveSchema, wanted: &Catalog) -> MigrationPlan {
    plan_with(live, wanted, &PlanOptions::default())
}

/// Plans the change from `live` to `wanted`.
pub fn plan_with(live: &LiveSchema, wanted: &Catalog, options: &PlanOptions) -> MigrationPlan {
    let mut b = Builder {
        live,
        options,
        phases: Default::default(),
        refused: Vec::new(),
        new_tables: BTreeSet::new(),
    };
    b.run(wanted);
    let mut steps: Vec<Step> = b.phases.into_values().flatten().collect();
    if !steps.is_empty() {
        steps.push(Step {
            key: "finalise".into(),
            risk: Risk::Additive,
            sentence: "Install the change feed capture on new tables and record the catalogue \
                       version."
                .into(),
            run: Run::Finalise,
        });
    }
    let plan_hash = plan_hash_of(&steps);
    MigrationPlan {
        steps,
        refused: b.refused,
        plan_hash,
        wanted: wanted.clone(),
        options: options.clone(),
    }
}

/// Execution order of the groups of steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Phase {
    CreateTable,
    AddColumn,
    Alter,
    Backfill,
    SetNotNull,
    ForeignKey,
    Validate,
    Index,
    Exclusion,
    /// The extra tables of the hook: after every document step.
    Extra,
}

struct Builder<'a> {
    live: &'a LiveSchema,
    options: &'a PlanOptions,
    phases: BTreeMap<Phase, Vec<Step>>,
    refused: Vec<Refusal>,
    new_tables: BTreeSet<String>,
}

impl Builder<'_> {
    fn push(&mut self, phase: Phase, key: String, risk: Risk, sentence: String, run: Run) {
        self.phases.entry(phase).or_default().push(Step {
            key,
            risk,
            sentence,
            run,
        });
    }

    fn refuse(&mut self, entity: &str, field: Option<&str>, reason: String) {
        self.refused.push(Refusal {
            entity: entity.to_owned(),
            field: field.map(str::to_owned),
            code: codes::REFUSED,
            reason,
        });
    }

    fn run(&mut self, wanted: &Catalog) {
        if let Some(have) = &self.live.collation
            && have != wanted.collation.as_str()
        {
            self.refuse(
                "",
                None,
                format!(
                    "the collation cannot change (database `{have}`, catalogue `{}`): existing \
                     text would no longer be in order",
                    wanted.collation.as_str()
                ),
            );
        }
        if let Some(inventory) = &self.options.extensions {
            for need in required_extensions(wanted, self.options) {
                if let Some(refusal) = check_extension(&need, inventory.get(&need.name)) {
                    self.refused.push(refusal);
                }
            }
        }
        let wanted_names: BTreeSet<&str> = wanted.entities().map(|e| e.name.as_str()).collect();
        for table in self.live.tables.keys() {
            if !table.starts_with("spot_") && !wanted_names.contains(table.as_str()) {
                self.refuse(
                    table,
                    None,
                    format!(
                        "table `{table}` is in the database but not in the catalogue; dropping \
                         or renaming a table is not an online change"
                    ),
                );
            }
        }
        for entity in wanted.entities() {
            self.entity(entity);
        }
        for entity in wanted.entities() {
            self.links(entity);
            self.indexes(entity);
        }
        for entity in wanted.entities() {
            self.exclusions(entity);
        }
        self.extras(&wanted_names);
    }

    /// The extra tables of the hook: validated as a whole, then one step for each whose marker
    /// (a table, or an index) the database does not have.
    fn extras(&mut self, catalogue_tables: &BTreeSet<&str>) {
        let extras = &self.options.extra_tables;
        if extras.is_empty() {
            return;
        }
        let refusals = extra::refusals(extras, catalogue_tables);
        if !refusals.is_empty() {
            self.refused.extend(refusals);
            return;
        }
        for extra in extras {
            let present = self.live.tables.contains_key(&extra.table)
                || self.live.indexes.contains_key(&extra.table);
            if !present {
                self.push(
                    Phase::Extra,
                    format!("{}{}", extra::STEP_PREFIX, extra.table),
                    Risk::Additive,
                    format!("Create the platform object `{}`.", extra.table),
                    Run::Tx(extra.create.clone()),
                );
            }
        }
    }

    // --- tables and columns --------------------------------------------------------------

    fn entity(&mut self, entity: &EntityDef) {
        let table = entity.name.as_str();
        let Some(live_table) = self.live.tables.get(table) else {
            self.new_tables.insert(table.to_owned());
            self.push(
                Phase::CreateTable,
                format!("create_table:{table}"),
                Risk::Additive,
                format!("Create table `{table}`."),
                Run::Tx(vec![ddl::create_table(entity)]),
            );
            return;
        };
        let wanted_cols = ddl::columns(entity);
        for col in &wanted_cols {
            match live_table.columns.get(&col.name) {
                None => self.missing_column(entity, col, live_table.has_rows),
                Some(live_col) => self.existing_column(entity, col, live_col),
            }
        }
        for name in live_table.columns.keys() {
            if !wanted_cols.iter().any(|c| &c.name == name) {
                self.refuse(
                    table,
                    Some(name),
                    format!(
                        "column `{table}.{name}` is in the database but not in the catalogue; \
                         dropping or renaming a column is not an online change"
                    ),
                );
            }
        }
        for field in &entity.fields {
            if matches!(field.ty, FieldType::Text { .. })
                && live_table.columns.contains_key(field.name.as_str())
            {
                self.length_limit(entity, field);
            }
        }
    }

    fn missing_column(&mut self, entity: &EntityDef, col: &Col, has_rows: bool) {
        let table = entity.name.as_str();
        let Some(field) = &col.field else {
            self.refuse(
                table,
                Some(&col.name),
                format!(
                    "table `{table}` lacks the system column `{}`; it was not created by this \
                     backend or has a different kind",
                    col.name
                ),
            );
            return;
        };
        let check = ddl::max_chars(field).map(|max| {
            format!(
                "ALTER TABLE {} ADD {}",
                quote(table),
                ddl::length_check(&entity.name, &field.name, max)
            )
        });
        let key = format!("{table}.{}", col.name);
        let with_check = |mut sql: Vec<String>| {
            sql.extend(check.clone());
            sql
        };
        if !col.not_null || !has_rows {
            self.push(
                Phase::AddColumn,
                format!("add_column:{key}"),
                Risk::Additive,
                format!("Add column `{key}`."),
                Run::Tx(with_check(vec![format!(
                    "ALTER TABLE {} ADD COLUMN {}",
                    quote(table),
                    ddl::column_definition(col, false)
                )])),
            );
            return;
        }
        // Required, and the table holds records.
        let Some(spec) = self.backfill_spec(entity, col, field) else {
            return;
        };
        self.push(
            Phase::AddColumn,
            format!("add_column:{key}"),
            Risk::NeedsBackfill,
            format!("Add column `{key}` as optional; it becomes required after the backfill."),
            Run::Tx(with_check(vec![
                format!(
                    "ALTER TABLE {} ADD COLUMN {}",
                    quote(table),
                    ddl::column_definition(col, true)
                ),
                mark_expanding(&key),
            ])),
        );
        self.backfill_steps(&key, spec);
    }

    fn backfill_spec(
        &mut self,
        entity: &EntityDef,
        col: &Col,
        field: &FieldDef,
    ) -> Option<BackfillSpec> {
        let table = entity.name.as_str();
        let value = self
            .options
            .defaults
            .get(&(table.to_owned(), col.name.clone()))
            .cloned()
            .or_else(|| zero_value(&field.ty).map(str::to_owned));
        let Some(value) = value else {
            self.refused.push(Refusal {
                entity: table.to_owned(),
                field: Some(col.name.clone()),
                code: codes::NO_BACKFILL_DEFAULT,
                reason: format!(
                    "`{table}.{}` is new and required, `{table}` holds records and the field has \
                     no default to fill them with",
                    col.name
                ),
            });
            return None;
        };
        Some(BackfillSpec {
            table: table.to_owned(),
            column: col.name.clone(),
            ty: col.ty.clone(),
            value,
        })
    }

    fn backfill_steps(&mut self, key: &str, spec: BackfillSpec) {
        self.push(
            Phase::Backfill,
            format!("backfill:{key}"),
            Risk::NeedsBackfill,
            format!("Fill the empty values of `{key}` in batches."),
            Run::Backfill(spec.clone()),
        );
        self.push(
            Phase::SetNotNull,
            format!("set_not_null:{key}"),
            Risk::NeedsBackfill,
            format!("Make `{key}` required."),
            Run::SetNotNull(spec),
        );
    }

    fn existing_column(&mut self, entity: &EntityDef, col: &Col, live: &crate::live::LiveColumn) {
        let table = entity.name.as_str();
        let key = format!("{table}.{}", col.name);
        if ddl::normalise_type(&live.ty) != ddl::normalise_type(&col.ty) {
            match (parse_numeric(&live.ty), parse_numeric(&col.ty)) {
                (Some((p1, s1)), Some((p2, s2))) if s2 >= s1 && p2 - s2 >= p1 - s1 => {
                    self.push(
                        Phase::Alter,
                        format!("widen_numeric:{key}"),
                        Risk::Additive,
                        format!("Widen `{key}` from {} to {}.", live.ty, col.ty),
                        Run::Tx(vec![format!(
                            "ALTER TABLE {} ALTER COLUMN {} TYPE {}",
                            quote(table),
                            quote(&col.name),
                            col.ty
                        )]),
                    );
                }
                (Some(_), Some(_)) => {
                    self.refuse(
                        table,
                        Some(&col.name),
                        format!(
                            "`{key}` would narrow from {} to {}; narrowing can lose digits",
                            live.ty, col.ty
                        ),
                    );
                    return;
                }
                _ => {
                    self.refuse(
                        table,
                        Some(&col.name),
                        format!(
                            "`{key}` would change type from {} to {}; a change of type family \
                             can lose values",
                            live.ty, col.ty
                        ),
                    );
                    return;
                }
            }
        }
        match (live.not_null, col.not_null) {
            (true, false) => self.push(
                Phase::Alter,
                format!("drop_not_null:{key}"),
                Risk::Additive,
                format!("Make `{key}` optional."),
                Run::Tx(vec![format!(
                    "ALTER TABLE {} ALTER COLUMN {} DROP NOT NULL",
                    quote(table),
                    quote(&col.name)
                )]),
            ),
            (false, true) if self.live.expanding.contains(&key) => {
                // An earlier run added it optional on its way to required: finish the job.
                if let Some(field) = &col.field
                    && let Some(spec) = self.backfill_spec(entity, col, field)
                {
                    self.backfill_steps(&key, spec);
                }
            }
            (false, true) => self.refuse(
                table,
                Some(&col.name),
                format!(
                    "`{key}` is optional in the database but required in the catalogue; making \
                     an existing column required can fail on existing records"
                ),
            ),
            _ => {}
        }
    }

    fn length_limit(&mut self, entity: &EntityDef, field: &FieldDef) {
        let table = entity.name.as_str();
        let key = format!("{table}.{}", field.name);
        let name = length_check_name(&entity.name, &field.name);
        let want = ddl::max_chars(field);
        let live = self.live.constraints.get(&(table.to_owned(), name.clone()));
        let have = live.and_then(|c| parse_limit(&c.definition));
        let drop = format!(
            "ALTER TABLE {} DROP CONSTRAINT {}",
            quote(table),
            quote(&name)
        );
        match (have, want) {
            (None, None) => {}
            (None, Some(_)) => self.refuse(
                table,
                Some(field.name.as_str()),
                format!("`{key}` gains a length limit; a new limit can fail on existing text"),
            ),
            (Some(_), None) => self.push(
                Phase::Alter,
                format!("drop_limit:{key}"),
                Risk::Additive,
                format!("Remove the length limit of `{key}`."),
                Run::Tx(vec![drop]),
            ),
            (Some(l), Some(w)) if w < l => self.refuse(
                table,
                Some(field.name.as_str()),
                format!("`{key}` would lower its length limit from {l} to {w}"),
            ),
            (Some(l), Some(w)) if w > l => {
                self.push(
                    Phase::Alter,
                    format!("widen_limit:{key}"),
                    Risk::Additive,
                    format!("Raise the length limit of `{key}` from {l} to {w}."),
                    Run::Tx(vec![format!(
                        "ALTER TABLE {} DROP CONSTRAINT {}, ADD {} NOT VALID",
                        quote(table),
                        quote(&name),
                        ddl::length_check(&entity.name, &field.name, w)
                    )]),
                );
                self.validate(table, &name, &key);
            }
            (Some(_), Some(_)) => {
                if live.is_some_and(|c| !c.validated) {
                    self.validate(table, &name, &key);
                }
            }
        }
    }

    fn validate(&mut self, table: &str, constraint: &str, key: &str) {
        self.push(
            Phase::Validate,
            format!("validate:{table}.{constraint}"),
            Risk::Additive,
            format!("Check the existing records against the constraint of `{key}`."),
            Run::Tx(vec![format!(
                "ALTER TABLE {} VALIDATE CONSTRAINT {}",
                quote(table),
                quote(constraint)
            )]),
        );
    }

    // --- links ---------------------------------------------------------------------------

    fn links(&mut self, entity: &EntityDef) {
        let table = &entity.name;
        let mut wanted: Vec<(String, Name, String)> = Vec::new();
        if let EntityKind::Child { parent } = &entity.kind {
            wanted.push((
                spot_storage_contract::schema::system::PARENT.to_owned(),
                parent.clone(),
                parent_fk_name(table),
            ));
        }
        for field in &entity.fields {
            let FieldType::Link { target, check } = &field.ty else {
                continue;
            };
            let fk = fk_name(table, field.name.as_str());
            match (target, check) {
                (LinkTarget::Entity(target), LinkCheck::Database) => {
                    wanted.push((field.name.as_str().to_owned(), target.clone(), fk));
                }
                _ => {
                    if self
                        .live
                        .constraints
                        .contains_key(&(table.as_str().to_owned(), fk))
                    {
                        self.refuse(
                            table.as_str(),
                            Some(field.name.as_str()),
                            format!(
                                "link `{table}.{}` is no longer checked by the database; \
                                 removing a check is not an online change",
                                field.name
                            ),
                        );
                    }
                }
            }
        }
        for (column, target, fk) in wanted {
            let key = format!("{table}.{fk}");
            match self
                .live
                .constraints
                .get(&(table.as_str().to_owned(), fk.clone()))
            {
                Some(c) if c.validated => {}
                Some(_) => self.validate(table.as_str(), &fk, &key),
                None => {
                    let populated = self
                        .live
                        .tables
                        .get(table.as_str())
                        .is_some_and(|t| t.has_rows);
                    if populated {
                        self.push(
                            Phase::ForeignKey,
                            format!("add_fk:{key}"),
                            Risk::Additive,
                            format!("Add the foreign key `{fk}` without checking old records."),
                            Run::Tx(vec![ddl::add_foreign_key(
                                table, &column, &target, &fk, true,
                            )]),
                        );
                        self.validate(table.as_str(), &fk, &key);
                    } else {
                        self.push(
                            Phase::ForeignKey,
                            format!("add_fk:{key}"),
                            Risk::Additive,
                            format!("Add the foreign key `{fk}`."),
                            Run::Tx(vec![ddl::add_foreign_key(
                                table, &column, &target, &fk, false,
                            )]),
                        );
                    }
                }
            }
        }
    }

    // --- indexes -------------------------------------------------------------------------

    fn indexes(&mut self, entity: &EntityDef) {
        let table = &entity.name;
        let mut wanted: Vec<WantedIndex> = entity
            .indexes
            .iter()
            .map(|i| WantedIndex {
                name: match i.method {
                    IndexMethod::BTree => i.name.as_str().to_owned(),
                    IndexMethod::Gist => gist_index_name(table, &i.name),
                    IndexMethod::Gin => gin_index_name(table, &i.name),
                },
                columns: i.fields.iter().map(|f| f.as_str().to_owned()).collect(),
                include: i.include.iter().map(|f| f.as_str().to_owned()).collect(),
                unique: i.unique,
                method: i.method,
                opclass: i.opclass,
            })
            .collect();
        if matches!(entity.kind, EntityKind::Child { .. }) {
            let parent_idx: Vec<String> = vec![
                spot_storage_contract::schema::system::PARENT.to_owned(),
                spot_storage_contract::schema::system::IDX.to_owned(),
            ];
            if !wanted.iter().any(|w| w.columns == parent_idx) {
                wanted.push(WantedIndex {
                    name: parent_index_name(table),
                    columns: parent_idx,
                    include: Vec::new(),
                    unique: false,
                    method: IndexMethod::BTree,
                    opclass: None,
                });
            }
        }
        let is_new = self.new_tables.contains(table.as_str());
        for w in wanted {
            let name = w.name.as_str();
            let key = format!("{table}.{name}");
            let fields: Vec<&str> = w.columns.iter().map(String::as_str).collect();
            let include: Vec<&str> = w.include.iter().map(String::as_str).collect();
            let shape = IndexShape {
                unique: w.unique,
                method: w.method,
                opclass: w.opclass,
                include: &include,
            };
            let create =
                |concurrently: bool| ddl::create_index(name, table, &fields, shape, concurrently);
            let run = |sql: String| {
                if is_new {
                    Run::Tx(vec![sql])
                } else {
                    Run::Outside(sql)
                }
            };
            match self.live.indexes.get(name) {
                None => self.push(
                    Phase::Index,
                    format!("create_index:{key}"),
                    Risk::Additive,
                    format!("Create index `{name}` on `{table}`."),
                    run(create(!is_new)),
                ),
                Some(live) if !live.valid => {
                    self.push(
                        Phase::Index,
                        format!("drop_invalid_index:{key}"),
                        Risk::Additive,
                        format!("Drop the unusable leftover of a failed build of `{name}`."),
                        Run::Outside(format!("DROP INDEX CONCURRENTLY {}", quote(name))),
                    );
                    self.push(
                        Phase::Index,
                        format!("create_index:{key}"),
                        Risk::Additive,
                        format!("Create index `{name}` on `{table}`."),
                        Run::Outside(create(true)),
                    );
                }
                Some(live) => {
                    // `indkey` lists the key columns and then the `INCLUDE` columns.
                    let all: Vec<&String> = w.columns.iter().chain(&w.include).collect();
                    let have: Vec<&String> = live.columns.iter().collect();
                    if live.table != table.as_str() || live.unique != w.unique || have != all {
                        self.refuse(
                            table.as_str(),
                            None,
                            format!(
                                "index `{name}` exists with a different definition than the \
                                 catalogue's; changing an index is not an online change"
                            ),
                        );
                    }
                }
            }
        }
    }

    // --- exclusion constraints -------------------------------------------------------------

    fn exclusions(&mut self, entity: &EntityDef) {
        let live = self.live;
        let table = &entity.name;
        let populated = live.tables.get(table.as_str()).is_some_and(|t| t.has_rows);
        let mut wanted_names = BTreeSet::new();
        for def in &entity.exclusions {
            let x = ExclusionConstraint::from_def(table, def);
            wanted_names.insert(x.name.clone());
            let key = format!("{table}.{}", def.name);
            match live
                .constraints
                .get(&(table.as_str().to_owned(), x.name.clone()))
            {
                Some(c) if c.kind == 'x' => {
                    if comparable_definition(&c.definition) != x.comparable() {
                        self.refuse(
                            table.as_str(),
                            None,
                            format!(
                                "exclusion `{}` exists with a different definition than the \
                                 catalogue's; changing an exclusion is not an online change",
                                x.name
                            ),
                        );
                    }
                }
                Some(_) => self.refuse(
                    table.as_str(),
                    None,
                    format!(
                        "the name `{}` of exclusion `{key}` is taken by another constraint",
                        x.name
                    ),
                ),
                None if populated => self.push(
                    Phase::Exclusion,
                    format!("add_exclusion:{key}"),
                    Risk::NeedsValidation,
                    format!(
                        "Add the overlap exclusion `{}` to `{table}`; it checks every existing \
                         record and builds its index while writes to the table wait.",
                        x.name
                    ),
                    Run::Outside(x.sql()),
                ),
                None => self.push(
                    Phase::Exclusion,
                    format!("add_exclusion:{key}"),
                    Risk::Additive,
                    format!("Add the overlap exclusion `{}` to `{table}`.", x.name),
                    Run::Tx(vec![x.sql()]),
                ),
            }
        }
        for ((t, name), c) in &live.constraints {
            if t == table.as_str() && c.kind == 'x' && !wanted_names.contains(name) {
                self.refuse(
                    table.as_str(),
                    None,
                    format!(
                        "exclusion `{name}` is in the database but not in the catalogue; \
                         dropping an exclusion is not an online change"
                    ),
                );
            }
        }
    }
}

/// One index the catalogue wants, with its physical name.
struct WantedIndex {
    name: String,
    columns: Vec<String>,
    include: Vec<String>,
    unique: bool,
    method: IndexMethod,
    opclass: Option<OpClass>,
}

/// The statement recording that a column is on its way to required, for resuming.
fn mark_expanding(key: &str) -> String {
    format!(
        "INSERT INTO {} (key, state) VALUES ('{}', 'expanding') \
         ON CONFLICT (key) DO UPDATE SET state = 'expanding'",
        quote(PROGRESS_TABLE),
        key.replace('\'', "''")
    )
}

/// The value a required column of this type starts with, when there is a neutral one.
fn zero_value(ty: &FieldType) -> Option<&'static str> {
    match ty {
        FieldType::Bool => Some("false"),
        FieldType::Int | FieldType::Decimal { .. } => Some("0"),
        FieldType::Text { .. } => Some(""),
        FieldType::Date => Some("1970-01-01"),
        FieldType::Time => Some("00:00:00"),
        FieldType::DateTime => Some("1970-01-01T00:00:00Z"),
        FieldType::Json => Some("{}"),
        FieldType::Bytes => Some("\\x"),
        // ADR-087: there is no neutral geo point or range.
        FieldType::Link { .. } | FieldType::Geo { .. } | FieldType::Range { .. } => None,
    }
}

/// `(precision, scale)` of `numeric(p,s)`.
fn parse_numeric(ty: &str) -> Option<(u32, u32)> {
    let inner = ty.strip_prefix("numeric(")?.strip_suffix(')')?;
    let (p, s) = inner.split_once(',')?;
    Some((p.trim().parse().ok()?, s.trim().parse().ok()?))
}

/// The `n` of `CHECK ((char_length(x) <= n))`.
fn parse_limit(definition: &str) -> Option<u32> {
    let rest = definition.split("<= ").nth(1)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::live::{LiveColumn, LiveTable};
    use spot_storage_contract::{Collation, EntityDef, FieldDef, IndexDef, RangeOf};

    fn n(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    fn fd(name: &str, ty: FieldType, nullable: bool) -> FieldDef {
        FieldDef {
            name: n(name),
            ty,
            nullable,
        }
    }

    fn occupancy(scope: &[&str], extra_indexes: Vec<IndexDef>) -> Catalog {
        let entity = EntityDef {
            name: n("occupancy"),
            kind: EntityKind::Document,
            fields: vec![
                fd("unit", FieldType::Text { max_chars: None }, false),
                fd(
                    "during",
                    FieldType::Range {
                        of: RangeOf::DateTime,
                    },
                    true,
                ),
                fd("spot", FieldType::Geo { srid: 4326 }, true),
            ],
            children: vec![],
            indexes: extra_indexes,
            exclusions: vec![ExclusionDef {
                name: n("unit_during"),
                field: n("during"),
                scope: scope.iter().map(|s| n(s)).collect(),
                message: "That unit is already taken for these dates.".into(),
            }],
            search: None,
        };
        Catalog::new(1, Collation::new("en").unwrap(), vec![entity]).unwrap()
    }

    fn inventory(items: &[(&str, Option<&str>, Option<&str>)]) -> PlanOptions {
        PlanOptions {
            extensions: Some(
                items
                    .iter()
                    .map(|(name, available, installed)| {
                        (
                            (*name).to_owned(),
                            LiveExtension {
                                name: (*name).to_owned(),
                                available_version: available.map(str::to_owned),
                                installed_version: installed.map(str::to_owned),
                            },
                        )
                    })
                    .collect(),
            ),
            ..PlanOptions::default()
        }
    }

    /// A live schema holding `occupancy` with its columns, optionally with rows.
    fn live_occupancy(has_rows: bool) -> LiveSchema {
        let col = |name: &str, ty: &str, not_null: bool| {
            (
                name.to_owned(),
                LiveColumn {
                    name: name.to_owned(),
                    ty: ty.to_owned(),
                    not_null,
                    collation: None,
                },
            )
        };
        let columns = [
            col("id", "uuid", true),
            col("version", "bigint", true),
            col("unit", "text", true),
            col("during", "tstzrange", false),
            col("spot", "geometry(Point,4326)", false),
        ]
        .into_iter()
        .collect();
        let mut live = LiveSchema::default();
        live.tables
            .insert("occupancy".to_owned(), LiveTable { columns, has_rows });
        live
    }

    fn step<'a>(plan: &'a MigrationPlan, key: &str) -> &'a Step {
        plan.steps
            .iter()
            .find(|s| s.key == key)
            .unwrap_or_else(|| panic!("no step {key} in {:?}", plan.steps))
    }

    #[test]
    fn numeric_and_limit_parsing() {
        assert_eq!(parse_numeric("numeric(18,2)"), Some((18, 2)));
        assert_eq!(parse_numeric("bigint"), None);
        assert_eq!(parse_limit("CHECK ((char_length(name) <= 140))"), Some(140));
        assert_eq!(
            parse_limit("CHECK ((char_length(name) <= 140)) NOT VALID"),
            Some(140)
        );
        assert_eq!(parse_limit("CHECK (x > 1)"), None);
    }

    #[test]
    fn versions_compare_by_dotted_numbers() {
        assert!(version_at_least("1.8", "1.5"));
        assert!(version_at_least("1.5", "1.5"));
        assert!(version_at_least("1.10", "1.9"));
        assert!(version_at_least("3.5.2", "3.5"));
        assert!(!version_at_least("1.4", "1.5"));
        assert!(!version_at_least("3.0", "3.0.1"));
        assert!(version_at_least("3.4dev", "3.4"));
    }

    #[test]
    fn a_new_table_gets_its_exclusion_as_an_additive_step_after_the_indexes() {
        let wanted = occupancy(&["unit"], vec![]);
        let p = plan(&LiveSchema::default(), &wanted);
        assert!(p.refused.is_empty());
        let keys: Vec<&str> = p.steps.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "create_table:occupancy",
                "add_exclusion:occupancy.unit_during",
                "finalise"
            ]
        );
        let x = step(&p, "add_exclusion:occupancy.unit_during");
        assert_eq!(x.risk, Risk::Additive);
        assert_eq!(
            x.run,
            Run::Tx(vec![
                "ALTER TABLE \"occupancy\" ADD CONSTRAINT \"spot_occupancy_unit_during_excl\" \
                 EXCLUDE USING gist (\"unit\" WITH =, \"during\" WITH &&)"
                    .into()
            ])
        );
        // The column types are the real ones now (ADR-087 section 4.3, 5).
        let Run::Tx(create) = &step(&p, "create_table:occupancy").run else {
            panic!("create table runs in a transaction");
        };
        assert!(create[0].contains("\"during\" tstzrange"), "{}", create[0]);
        assert!(
            create[0].contains("\"spot\" public.geometry(Point,4326)"),
            "{}",
            create[0]
        );
    }

    #[test]
    fn a_single_column_exclusion_has_no_scope_element() {
        let p = plan(&LiveSchema::default(), &occupancy(&[], vec![]));
        let Run::Tx(sql) = &step(&p, "add_exclusion:occupancy.unit_during").run else {
            panic!("a new table's exclusion runs in a transaction");
        };
        assert!(
            sql[0].ends_with("EXCLUDE USING gist (\"during\" WITH &&)"),
            "{}",
            sql[0]
        );
    }

    #[test]
    fn an_exclusion_on_a_populated_table_needs_validation_and_runs_outside() {
        let wanted = occupancy(&["unit"], vec![]);
        let empty = plan(&live_occupancy(false), &wanted);
        assert_eq!(
            step(&empty, "add_exclusion:occupancy.unit_during").risk,
            Risk::Additive
        );
        let full = plan(&live_occupancy(true), &wanted);
        let x = step(&full, "add_exclusion:occupancy.unit_during");
        assert_eq!(x.risk, Risk::NeedsValidation);
        assert!(matches!(x.run, Run::Outside(_)), "{:?}", x.run);
        assert_ne!(empty.plan_hash, full.plan_hash);
    }

    #[test]
    fn an_exclusion_that_is_already_there_is_not_planned_again() {
        let wanted = occupancy(&["unit"], vec![]);
        let mut live = live_occupancy(true);
        live.constraints.insert(
            ("occupancy".into(), "spot_occupancy_unit_during_excl".into()),
            crate::live::LiveConstraint {
                name: "spot_occupancy_unit_during_excl".into(),
                table: "occupancy".into(),
                kind: 'x',
                validated: true,
                definition: "EXCLUDE USING gist (unit WITH =, during WITH &&)".into(),
            },
        );
        let p = plan(&live, &wanted);
        assert!(p.is_empty(), "{:?} {:?}", p.steps, p.refused);
        // A different definition, or one the catalogue dropped, is refused.
        let other = occupancy(&[], vec![]);
        let p = plan(&live, &other);
        assert_eq!(p.refused.len(), 1, "{:?}", p.refused);
        assert!(p.refused[0].reason.contains("different definition"));
        let mut dropped = occupancy(&["unit"], vec![]);
        let entity = dropped.entities().next().unwrap().clone();
        dropped = Catalog::new(
            2,
            Collation::new("en").unwrap(),
            vec![EntityDef {
                exclusions: vec![],
                ..entity
            }],
        )
        .unwrap();
        let p = plan(&live, &dropped);
        assert!(
            p.refused
                .iter()
                .any(|r| r.reason.contains("dropping an exclusion")),
            "{:?}",
            p.refused
        );
    }

    #[test]
    fn the_plan_hash_is_stable_and_pinned_and_follows_exclusion_and_extension_steps() {
        let wanted = occupancy(&["unit"], vec![]);
        let a = plan(&LiveSchema::default(), &wanted);
        let b = plan(&LiveSchema::default(), &wanted);
        assert_eq!(
            a.plan_hash, b.plan_hash,
            "the hash does not change between runs"
        );
        assert_eq!(
            a.plan_hash, GOLDEN_HASH,
            "the golden hash moved: review the step text"
        );
        let single = plan(&LiveSchema::default(), &occupancy(&[], vec![]));
        assert_ne!(
            a.plan_hash, single.plan_hash,
            "a changed exclusion changes the hash"
        );

        let mut steps = a.steps.clone();
        let base = plan_hash_of(&steps);
        assert_eq!(base, a.plan_hash);
        steps.insert(0, extension_step("btree_gist", "1.5"));
        let with = plan_hash_of(&steps);
        assert_ne!(base, with, "an extension step changes the hash");
        steps[0] = extension_step("btree_gist", "1.6");
        assert_ne!(with, plan_hash_of(&steps), "so does its minimum version");
        steps[0] = extension_step("pg_trgm", "1.5");
        assert_ne!(with, plan_hash_of(&steps), "and its name");
    }

    const GOLDEN_HASH: &str = "e634d8cc358126aabdbc6880f63d65c35956581584d1209446c43670c8f65a8c";

    #[test]
    fn gist_and_gin_indexes_are_named_with_the_prefix_and_use_their_method() {
        let spatial = IndexDef {
            name: n("spot_where"),
            fields: vec![n("spot")],
            unique: false,
            method: IndexMethod::Gist,
            opclass: None,
            include: vec![],
        };
        let trgm = IndexDef {
            name: n("unit_trgm"),
            fields: vec![n("unit")],
            unique: false,
            method: IndexMethod::Gin,
            opclass: Some(OpClass::GinTrgm),
            include: vec![],
        };
        let wanted = occupancy(&[], vec![spatial, trgm]);
        // On an existing table the builds are concurrent.
        let p = plan(&live_occupancy(false), &wanted);
        let Run::Outside(gist) =
            &step(&p, "create_index:occupancy.spot_occupancy_spot_where_gix").run
        else {
            panic!("an index on an existing table builds outside a transaction");
        };
        assert_eq!(
            gist,
            "CREATE INDEX CONCURRENTLY \"spot_occupancy_spot_where_gix\" ON \"occupancy\" \
             USING gist (\"spot\")"
        );
        let Run::Outside(gin) =
            &step(&p, "create_index:occupancy.spot_occupancy_unit_trgm_ginx").run
        else {
            panic!("an index on an existing table builds outside a transaction");
        };
        assert_eq!(
            gin,
            "CREATE INDEX CONCURRENTLY \"spot_occupancy_unit_trgm_ginx\" ON \"occupancy\" \
             USING gin (\"unit\" gin_trgm_ops)"
        );
        // Indexes come before the exclusion.
        let keys: Vec<&str> = p.steps.iter().map(|s| s.key.as_str()).collect();
        let last_index = keys
            .iter()
            .rposition(|k| k.starts_with("create_index:"))
            .unwrap();
        let excl = keys
            .iter()
            .position(|k| k.starts_with("add_exclusion:"))
            .unwrap();
        assert!(last_index < excl, "{keys:?}");
    }

    #[test]
    fn a_live_geometry_column_spelled_either_way_plans_as_unchanged() {
        let wanted = occupancy(&[], vec![]);
        let mut live = live_occupancy(false);
        for spelling in [
            "geometry(Point,4326)",
            "public.geometry(Point,4326)",
            "public.geometry(Point, 4326)",
        ] {
            live.tables
                .get_mut("occupancy")
                .unwrap()
                .columns
                .get_mut("spot")
                .unwrap()
                .ty = spelling.to_owned();
            let p = plan(&live, &wanted);
            assert!(
                p.refused.is_empty()
                    && p.steps
                        .iter()
                        .all(|s| s.key.starts_with("add_exclusion:") || s.key == "finalise"),
                "{spelling}: {:?} {:?}",
                p.steps,
                p.refused
            );
        }
        assert_eq!(
            ddl::normalise_type("public.geometry(Point,4326)"),
            "geometry(Point,4326)"
        );
        assert_eq!(ddl::normalise_type("numeric(18,2)"), "numeric(18,2)");
        assert_eq!(
            ddl::normalise_type("time without time zone"),
            "time without time zone"
        );
        // Range types need no normalisation: format_type spells them as the column types do.
        for ty in ["int8range", "numrange", "daterange", "tstzrange"] {
            assert_eq!(ddl::normalise_type(ty), ty);
        }
    }

    #[test]
    fn each_range_and_geo_field_maps_to_its_column_type() {
        assert_eq!(
            ddl::column_type(&FieldType::Geo { srid: 4326 }),
            "public.geometry(Point,4326)"
        );
        assert_eq!(
            ddl::column_type(&FieldType::Range { of: RangeOf::Int }),
            "int8range"
        );
        assert_eq!(
            ddl::column_type(&FieldType::Range {
                of: RangeOf::Decimal
            }),
            "numrange"
        );
        assert_eq!(
            ddl::column_type(&FieldType::Range { of: RangeOf::Date }),
            "daterange"
        );
        assert_eq!(
            ddl::column_type(&FieldType::Range {
                of: RangeOf::DateTime
            }),
            "tstzrange"
        );
    }

    #[test]
    fn the_catalogue_names_the_extensions_it_needs() {
        let opts = PlanOptions::default();
        let needs = required_extensions(&occupancy(&["unit"], vec![]), &opts);
        let names: Vec<&str> = needs.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, ["btree_gist", "postgis"]);
        // A range with a single-column exclusion needs only what the geo field needs.
        let names: Vec<String> = required_extensions(&occupancy(&[], vec![]), &opts)
            .into_iter()
            .map(|x| x.name)
            .collect();
        assert_eq!(names, ["postgis"]);
        let mut over = PlanOptions::default();
        over.extension_min_versions
            .insert("postgis".into(), "3.9".into());
        let needs = required_extensions(&occupancy(&[], vec![]), &over);
        assert_eq!(needs[0].min_version, "3.9");
        assert_eq!(default_min_version("btree_gist"), "1.5");
    }

    #[test]
    fn a_missing_or_old_extension_refuses_the_plan_naming_extension_install() {
        let wanted = occupancy(&["unit"], vec![]);
        // Nothing installed, nothing offered: unavailable.
        let p = plan_with(&LiveSchema::default(), &wanted, &inventory(&[]));
        let codes_seen: Vec<&str> = p.refused.iter().map(|r| r.code).collect();
        assert_eq!(
            codes_seen,
            [codes::EXTENSION_UNAVAILABLE, codes::EXTENSION_UNAVAILABLE]
        );
        assert!(
            p.refused
                .iter()
                .all(|r| r.reason.contains("extension.install"))
        );
        assert!(p.refused[0].reason.contains("`btree_gist`"));
        // Offered but not created in this database: still unavailable, and still not created.
        let p = plan_with(
            &LiveSchema::default(),
            &wanted,
            &inventory(&[
                ("btree_gist", Some("1.8"), None),
                ("postgis", Some("3.5"), None),
            ]),
        );
        assert_eq!(p.refused.len(), 2);
        assert!(p.refused[0].reason.contains("never creates an extension"));
        assert!(
            p.steps
                .iter()
                .all(|s| !matches!(s.run, Run::Extension { .. }))
        );
        // Installed but too old: the version code.
        let p = plan_with(
            &LiveSchema::default(),
            &wanted,
            &inventory(&[
                ("btree_gist", Some("1.8"), Some("1.4")),
                ("postgis", Some("3.5"), Some("3.5.0")),
            ]),
        );
        assert_eq!(p.refused.len(), 1);
        assert_eq!(p.refused[0].code, codes::EXTENSION_VERSION);
        // Not installed and the server offers only an old one.
        let p = plan_with(
            &LiveSchema::default(),
            &wanted,
            &inventory(&[
                ("btree_gist", Some("1.2"), None),
                ("postgis", Some("3.5"), Some("3.5.0")),
            ]),
        );
        assert_eq!(p.refused[0].code, codes::EXTENSION_VERSION);
        // Both present: nothing refused.
        let p = plan_with(
            &LiveSchema::default(),
            &wanted,
            &inventory(&[
                ("btree_gist", Some("1.8"), Some("1.8")),
                ("postgis", Some("3.5"), Some("3.5.0")),
            ]),
        );
        assert!(p.refused.is_empty(), "{:?}", p.refused);
        // Without an inventory the plan does not judge; `apply` checks the database.
        assert!(plan(&LiveSchema::default(), &wanted).refused.is_empty());
    }
}
