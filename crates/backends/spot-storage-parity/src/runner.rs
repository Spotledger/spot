//! The runner and the report.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use spot_storage_contract::StorageBackend;

use crate::cases::{Case, Divergence, Scope, load_cases, load_register};
use crate::catalog::{
    geo_catalog, long_names_catalog, parity_catalog, range_catalog, search_catalog,
};
use crate::driver::{BackendFactory, CaseNeeds};
use crate::exec::{Observed, error_kind, execute};
use crate::rules::{Rule, RuleStatus, load_rules};
use crate::values::{Ids, canon, expected_form, parse_value};

/// What one backend did for every case.
#[derive(Debug, Clone)]
pub struct BackendRun {
    /// The backend's name.
    pub backend: String,
    /// Case id to outcome.
    pub outcomes: BTreeMap<String, Observed>,
    /// Cases the harness answered itself, without asking the backend (the closed extension
    /// allow-list, SEM-EXT-02). They say nothing about whether the backend is available.
    pub by_harness: BTreeSet<String>,
}

impl BackendRun {
    fn skipped(&self) -> bool {
        !self.outcomes.is_empty()
            && self
                .outcomes
                .iter()
                .all(|(id, o)| *o == Observed::Skipped || self.by_harness.contains(id))
    }
}

/// A case that a backend did not pass.
#[derive(Debug, Clone)]
pub struct Difference {
    /// The rule it proves.
    pub rule: String,
    /// The case id.
    pub case: String,
    /// What the case checks.
    pub title: String,
    /// What was expected.
    pub expected: String,
    /// What each backend did.
    pub observed: BTreeMap<String, String>,
    /// The backends that did not pass.
    pub failed_on: Vec<String>,
}

/// Pass and fail counts of one rule on one backend.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    /// Cases passed.
    pub passed: usize,
    /// Cases failed.
    pub failed: usize,
    /// Cases not run (the backend is not available).
    pub skipped: usize,
}

/// The result for one rule.
#[derive(Debug, Clone)]
pub struct RuleResult {
    /// Decided or open.
    pub status: RuleStatus,
    /// Counts per backend.
    pub per_backend: BTreeMap<String, Tally>,
}

/// The outcome of a run over all backends.
#[derive(Debug, Clone, Default)]
pub struct ParityReport {
    /// Backends that ran, in order.
    pub backends: Vec<String>,
    /// Backends that were not available.
    pub skipped_backends: Vec<String>,
    /// Number of cases.
    pub cases: usize,
    /// Per-rule results.
    pub rules: BTreeMap<String, RuleResult>,
    /// Open rules that have cases (their answers are recorded, not decided).
    pub open_rules_with_cases: Vec<String>,
    /// Failed cases whose rule is not in the divergence register: these fail the run.
    pub unregistered: Vec<Difference>,
    /// Failed cases whose rule is in the register: reported, not failing.
    pub registered: Vec<(Difference, Divergence)>,
    /// Register entries whose rule no longer has a failing case: these fail the run.
    pub stale_register: Vec<Divergence>,
    /// Problems in the inputs (a case naming no rule, a register entry naming no rule).
    pub problems: Vec<String>,
}

impl ParityReport {
    /// Whether the run is green: no unlisted difference, no stale register entry, no problem.
    pub fn is_green(&self) -> bool {
        self.unregistered.is_empty() && self.stale_register.is_empty() && self.problems.is_empty()
    }

    /// Panics with the report unless it is green.
    pub fn assert_green(&self) {
        assert!(self.is_green(), "storage parity failed:\n{self}");
    }
}

impl fmt::Display for ParityReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "parity: {} cases, {} rules, backends: {}{}",
            self.cases,
            self.rules.len(),
            self.backends.join(", "),
            if self.skipped_backends.is_empty() {
                String::new()
            } else {
                format!(" (not run: {})", self.skipped_backends.join(", "))
            }
        )?;
        for (id, r) in &self.rules {
            let counts: Vec<String> = r
                .per_backend
                .iter()
                .map(|(b, t)| format!("{b} {}/{}", t.passed, t.passed + t.failed))
                .collect();
            let marker = if r.per_backend.values().any(|t| t.failed > 0) {
                "FAIL"
            } else {
                "ok  "
            };
            writeln!(f, "  {marker} {id} [{:?}] {}", r.status, counts.join("  "))?;
        }
        if !self.open_rules_with_cases.is_empty() {
            writeln!(
                f,
                "open rules with cases (answers recorded, not decided): {}",
                self.open_rules_with_cases.join(", ")
            )?;
        }
        let diff = |f: &mut fmt::Formatter<'_>, d: &Difference| -> fmt::Result {
            writeln!(f, "  {} / {}: {}", d.rule, d.case, d.title)?;
            writeln!(f, "      expected: {}", d.expected)?;
            for (b, o) in &d.observed {
                let mark = if d.failed_on.contains(b) {
                    "DIFFERS"
                } else {
                    "same   "
                };
                writeln!(f, "      {b:<9} {mark} {o}")?;
            }
            Ok(())
        };
        if !self.unregistered.is_empty() {
            writeln!(
                f,
                "DIFFERENCES not in the register ({}):",
                self.unregistered.len()
            )?;
            for d in &self.unregistered {
                diff(f, d)?;
            }
        }
        if !self.registered.is_empty() {
            writeln!(f, "known divergences ({}):", self.registered.len())?;
            for (d, entry) in &self.registered {
                diff(f, d)?;
                writeln!(
                    f,
                    "      accepted: {} (owner {})",
                    entry.reason, entry.owner
                )?;
            }
        }
        for s in &self.stale_register {
            writeln!(
                f,
                "STALE register entry: {}{} no longer diverges ({}); remove it",
                s.rule,
                s.case.as_ref().map_or(String::new(), |c| format!(" / {c}")),
                s.reason
            )?;
        }
        for p in &self.problems {
            writeln!(f, "PROBLEM: {p}")?;
        }
        Ok(())
    }
}

/// Which value kinds and text modes the cases cover.
#[derive(Debug, Clone, Default)]
pub struct Coverage {
    /// Value variants written in a case: `Null`, `Bool`, `Int`, `Decimal`, `Text`, `Date`,
    /// `Time`, `DateTime`, `Id`, `Json`, `Bytes`.
    pub variants: BTreeSet<&'static str>,
    /// Text modes used in a query: `Equals`, `Prefix`, `Contains`, `Suffix`.
    pub modes: BTreeSet<&'static str>,
}

/// Runs the cases on backends and builds the report.
pub struct ParityRunner {
    /// The cases.
    pub cases: Vec<Case>,
    /// The accepted divergences.
    pub register: Vec<Divergence>,
    /// The rules.
    pub rules: BTreeMap<String, Rule>,
}

impl ParityRunner {
    /// Builds a runner from parts.
    pub fn new(cases: Vec<Case>, register: Vec<Divergence>, rules: BTreeMap<String, Rule>) -> Self {
        Self {
            cases,
            register,
            rules,
        }
    }

    /// The crate's own `cases/` directory.
    pub fn cases_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("cases")
    }

    /// The rules document.
    pub fn rules_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../docs/storage-semantics.md")
    }

    /// Loads the repository's cases, register and rules.
    pub fn from_repo() -> Result<Self, String> {
        Ok(Self::new(
            load_cases(&Self::cases_dir())?,
            load_register(&Self::cases_dir())?,
            load_rules(&Self::rules_path())?,
        ))
    }

    /// Runs every case on a fresh store from `factory`.
    ///
    /// A case is skipped on a backend outside its [`Scope`] (ADR-087 option B: the fake does not
    /// model geo filters, ranges, exclusions or search), and a `prepare` case judges what
    /// `prepare` answers rather than running a command.
    pub async fn run_backend<F: BackendFactory>(&self, factory: &F) -> BackendRun {
        let mut outcomes = BTreeMap::new();
        let mut by_harness = BTreeSet::new();
        for case in &self.cases {
            let scope = match case.scope() {
                Ok(s) => s,
                Err(m) => {
                    outcomes.insert(case.id.clone(), Observed::BrokenCase(m));
                    continue;
                }
            };
            if !scope.covers(factory.name()) {
                outcomes.insert(case.id.clone(), Observed::Skipped);
                continue;
            }
            // ADR-087 section 4.3, SEM-EXT-02: an extension outside the closed allow-list never
            // reaches a database, so the harness answers the audited `extension.install` refusal
            // without asking the backend. The check is the same on every backend.
            if crate::cases::off_list_extension(case).is_some() {
                by_harness.insert(case.id.clone());
                outcomes.insert(
                    case.id.clone(),
                    Observed::Error(crate::cases::EXTENSION_NOT_ALLOWED.to_owned()),
                );
                continue;
            }
            let locale = case.collation.as_deref().unwrap_or("en");
            let built = match case.catalog.as_deref() {
                None | Some("main") => parity_catalog(locale),
                Some("geo") => geo_catalog(locale),
                Some("range") => range_catalog(locale),
                Some("search") => search_catalog(locale),
                Some("long") => long_names_catalog(locale),
                Some(other) => {
                    outcomes.insert(
                        case.id.clone(),
                        Observed::BrokenCase(format!("unknown catalog `{other}`")),
                    );
                    continue;
                }
            };
            let catalog = match built {
                Ok(c) => Arc::new(c),
                Err(e) => {
                    outcomes.insert(
                        case.id.clone(),
                        Observed::BrokenCase(format!("collation: {e}")),
                    );
                    continue;
                }
            };
            let needs = CaseNeeds {
                extensions: &case.extensions,
            };
            let fresh = factory.fresh(catalog, &needs).await;
            let observed = match fresh {
                Ok(Some(driver)) => match case.op {
                    crate::cases::Op::Prepare => Observed::Ok,
                    _ => execute(case, &driver).await,
                },
                Ok(None) => Observed::Skipped,
                // A `prepare` case asserts what `prepare` answers for a catalog, so a refusal
                // from the backend is its verdict, not a broken harness; every other case needs
                // a store and a failure here cannot be judged.
                Err(e) if case.op == crate::cases::Op::Prepare => Observed::Error(error_kind(&e)),
                Err(e) => Observed::BrokenCase(format!("backend not usable: {e}")),
            };
            outcomes.insert(case.id.clone(), observed);
        }
        BackendRun {
            backend: factory.name().to_owned(),
            outcomes,
            by_harness,
        }
    }

    /// Compares the runs with the expectations. `reference` names the run that
    /// `same_as_reference` cases compare against.
    pub fn report(&self, runs: &[BackendRun], reference: &str) -> ParityReport {
        let mut report = ParityReport {
            cases: self.cases.len(),
            ..ParityReport::default()
        };
        for r in runs {
            if r.skipped() {
                report.skipped_backends.push(r.backend.clone());
            } else {
                report.backends.push(r.backend.clone());
            }
        }
        for (id, rule) in &self.rules {
            let per_backend = report
                .backends
                .iter()
                .map(|b| (b.clone(), Tally::default()))
                .collect();
            report.rules.insert(
                id.clone(),
                RuleResult {
                    status: rule.status,
                    per_backend,
                },
            );
        }
        let reference_run = runs.iter().find(|r| r.backend == reference);
        let mut open_with_cases = BTreeSet::new();
        let mut differences: Vec<Difference> = Vec::new();
        for case in &self.cases {
            let Some(rule) = self.rules.get(&case.rule) else {
                report.problems.push(format!(
                    "case `{}` names the unknown rule {}",
                    case.id, case.rule
                ));
                continue;
            };
            if rule.status == RuleStatus::Open {
                open_with_cases.insert(case.rule.clone());
            }
            let expected_text = expected_text(case);
            let reference_obs = reference_run.and_then(|r| r.outcomes.get(&case.id));
            let scope = case.scope().unwrap_or(Scope::Both);
            let mut failed_on = Vec::new();
            let mut observed = BTreeMap::new();
            for run in runs.iter().filter(|r| !r.skipped()) {
                let Some(obs) = run.outcomes.get(&case.id) else {
                    report.problems.push(format!(
                        "{} has no outcome for case `{}`",
                        run.backend, case.id
                    ));
                    continue;
                };
                // A case scoped to other backends is not judged here, and is never counted as a
                // pass: it is a skip (ADR-087 option B).
                if !scope.covers(&run.backend) {
                    if let Some(t) = report
                        .rules
                        .get_mut(&case.rule)
                        .and_then(|r| r.per_backend.get_mut(&run.backend))
                    {
                        t.skipped += 1;
                    }
                    continue;
                }
                observed.insert(run.backend.clone(), obs.to_string());
                let passed = verdict(case, obs, reference_obs, run.backend == reference);
                if let Some(t) = report
                    .rules
                    .get_mut(&case.rule)
                    .and_then(|r| r.per_backend.get_mut(&run.backend))
                {
                    if passed {
                        t.passed += 1;
                    } else {
                        t.failed += 1;
                    }
                }
                if !passed {
                    failed_on.push(run.backend.clone());
                }
            }
            if !failed_on.is_empty() {
                differences.push(Difference {
                    rule: case.rule.clone(),
                    case: case.id.clone(),
                    title: case.title.clone(),
                    expected: expected_text,
                    observed,
                    failed_on,
                });
            }
        }
        report.open_rules_with_cases = open_with_cases.into_iter().collect();
        for entry in &self.register {
            if !self.rules.contains_key(&entry.rule) {
                report.problems.push(format!(
                    "the register names the unknown rule {}",
                    entry.rule
                ));
            }
        }
        let covers = |e: &Divergence, d: &Difference| {
            e.rule == d.rule && e.case.as_ref().is_none_or(|c| *c == d.case)
        };
        for d in differences {
            match self.register.iter().find(|e| covers(e, &d)) {
                Some(entry) => report.registered.push((d, entry.clone())),
                None => report.unregistered.push(d),
            }
        }
        // With a backend missing, a difference between backends cannot be seen, so a register
        // entry cannot be judged stale.
        if report.skipped_backends.is_empty() {
            report.stale_register = self
                .register
                .iter()
                .filter(|e| {
                    self.rules.contains_key(&e.rule)
                        && !report.registered.iter().any(|(d, entry)| {
                            covers(e, d) && entry.rule == e.rule && entry.case == e.case
                        })
                })
                .cloned()
                .collect();
        }
        report
    }

    /// Problems that make the case set incomplete: a decided rule without a case, a case that
    /// names no rule, a value variant or text mode no case uses.
    pub fn completeness_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let used: BTreeSet<&str> = self.cases.iter().map(|c| c.rule.as_str()).collect();
        for (id, rule) in &self.rules {
            if rule.status == RuleStatus::Decided && !used.contains(id.as_str()) {
                problems.push(format!("decided rule {id} has no case"));
            }
        }
        for c in &self.cases {
            if !self.rules.contains_key(&c.rule) {
                problems.push(format!("case `{}` names the unknown rule {}", c.id, c.rule));
            }
        }
        let cov = self.coverage();
        for v in [
            "Null", "Bool", "Int", "Decimal", "Text", "Date", "Time", "DateTime", "Id", "Json",
            "Bytes",
        ] {
            if !cov.variants.contains(v) {
                problems.push(format!("no case writes a {v} value"));
            }
        }
        for m in ["Equals", "Prefix", "Contains", "Suffix"] {
            if !cov.modes.contains(m) {
                problems.push(format!("no case uses the text mode {m}"));
            }
        }
        problems.extend(self.capability_problems());
        problems
    }

    /// The option-B checks of ADR-087 sections 4.5, 5.5 and 7.9: a construct the fake does not
    /// advertise is proven by a case scoped to the fake that expects `Unsupported` — recorded as
    /// a capability, never as an entry in `cases/divergences.toml` — and a fake-only case says
    /// nothing but that.
    pub fn capability_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let caps = spot_storage_fake::FakeBackend::new().capabilities();
        let not_served = [
            ("geo filters", "SEM-GEO-03", caps.geo_filters),
            (
                "range values, overlap and exclusion",
                "SEM-RNG-08",
                caps.range_exclusion,
            ),
            ("search", "SEM-SCH-04", caps.search),
        ];
        for (what, rule, advertised) in not_served {
            if advertised {
                continue;
            }
            let proven = self.cases.iter().any(|c| {
                c.rule == rule
                    && c.expect.error.as_deref() == Some("unsupported")
                    && c.scope().is_ok_and(|s| s == Scope::Fake)
            });
            if !proven {
                problems.push(format!(
                    "the fake does not serve {what}, so {rule} needs a fake-only case expecting \
                     `unsupported`"
                ));
            }
        }
        for c in &self.cases {
            if c.scope == Some(Scope::Fake.as_str().to_owned())
                && c.expect.error.as_deref() != Some("unsupported")
                && !c.expect.same_as_reference
            {
                problems.push(format!(
                    "case `{}` is fake-only but does not expect `unsupported`: a case scoped to \
                     the fake asserts the capability answer (ADR-087 option B)",
                    c.id
                ));
            }
        }
        problems
    }

    /// The value variants and text modes the cases use.
    pub fn coverage(&self) -> Coverage {
        let mut cov = Coverage::default();
        let mut ids = Ids::new();
        for c in &self.cases {
            let lines = c.setup.iter().chain(c.write.iter());
            for line in lines {
                for w in crate::dsl::words(line).unwrap_or_default().iter().skip(1) {
                    if let Some((_, v)) = w.split_once('=')
                        && let Ok(value) = parse_value(v, &mut ids)
                    {
                        cov.variants.insert(variant_name(&value));
                    }
                }
            }
            let q = &c.query;
            let texts = q
                .select
                .iter()
                .chain(q.filter.iter())
                .chain(q.group.iter())
                .chain(q.having.iter())
                .chain(q.order.iter());
            for t in texts {
                for (op, mode) in [
                    ("(equals", "Equals"),
                    ("(prefix", "Prefix"),
                    ("(contains", "Contains"),
                    ("(suffix", "Suffix"),
                ] {
                    if t.contains(op) {
                        cov.modes.insert(mode);
                    }
                }
            }
        }
        cov
    }
}

fn variant_name(v: &spot_storage_contract::Value) -> &'static str {
    use spot_storage_contract::Value;
    match v {
        Value::Null => "Null",
        Value::Bool(_) => "Bool",
        Value::Int(_) => "Int",
        Value::Decimal(_) => "Decimal",
        Value::Text(_) => "Text",
        Value::Date(_) => "Date",
        Value::Time(_) => "Time",
        Value::DateTime(_) => "DateTime",
        Value::Id(_) => "Id",
        Value::Json(_) => "Json",
        Value::Bytes(_) => "Bytes",
        Value::GeoPoint(_) => "GeoPoint",
        Value::Range(_) => "Range",
    }
}

fn expected_text(case: &Case) -> String {
    let e = &case.expect;
    if let Some(rows) = &e.rows {
        return Observed::Rows(expected_rows(rows)).to_string();
    }
    if let Some(k) = &e.error {
        return format!("error {k}");
    }
    if e.ok {
        return "ok".into();
    }
    "the same as the reference backend".into()
}

fn expected_rows(rows: &[Vec<String>]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| {
            r.iter()
                .map(|cell| {
                    if cell.starts_with("id:") {
                        cell.clone()
                    } else {
                        match parse_value(cell, &mut Ids::new()) {
                            Ok(v) => canon(&expected_form(v), &Ids::new()),
                            Err(m) => format!("<bad expected value: {m}>"),
                        }
                    }
                })
                .collect()
        })
        .collect()
}

fn sorted(mut rows: Vec<Vec<String>>) -> Vec<Vec<String>> {
    rows.sort();
    rows
}

/// Whether `obs` is what `case` expects.
fn verdict(case: &Case, obs: &Observed, reference: Option<&Observed>, is_reference: bool) -> bool {
    let e = &case.expect;
    if let Some(rows) = &e.rows {
        let want = expected_rows(rows);
        return match obs {
            Observed::Rows(got) if e.unordered => sorted(got.clone()) == sorted(want),
            Observed::Rows(got) => *got == want,
            _ => false,
        };
    }
    if let Some(k) = &e.error {
        return *obs == Observed::Error(k.clone());
    }
    if e.ok {
        return *obs == Observed::Ok;
    }
    // same_as_reference
    match obs {
        Observed::BrokenCase(_) | Observed::Inconsistent(_) | Observed::Skipped => false,
        _ if is_reference => true,
        _ => match reference {
            Some(Observed::Skipped) | None => true,
            Some(r) if e.unordered => match (obs, r) {
                (Observed::Rows(a), Observed::Rows(b)) => sorted(a.clone()) == sorted(b.clone()),
                (a, b) => a == b,
            },
            Some(r) => obs == r,
        },
    }
}
