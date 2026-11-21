//! The [`Catalogue`]: every registered [`OperationSpec`], built through
//! [`CatalogueBuilder`], with lookup, sorted iteration and console search.

use std::collections::{BTreeMap, BTreeSet};

use regex::Regex;

use crate::error::{CatalogueError, InvocationError};
use crate::model::{
    OpId, OperationSpec, ParamSpec, ParamType, ParamValue, is_boolish, is_param_name, kebab,
};
use crate::validated::Validated;
use crate::value_check::check_value;

/// Builds a [`Catalogue`] one [`OperationSpec`] at a time, remembering which module
/// registered each (a duplicate id names both sides), and validates the whole set on
/// [`CatalogueBuilder::build`].
///
/// Modules implement [`crate::OperationModule`] and call [`CatalogueBuilder::add`] in
/// their `register`; the host collects the modules it loads and builds one catalogue.
#[derive(Debug, Default)]
pub struct CatalogueBuilder {
    /// The specs, in registration order, each tagged with its registering module's name.
    entries: Vec<(String, OperationSpec)>,
}

impl CatalogueBuilder {
    /// Starts an empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one operation spec, remembering `module` as its source.
    ///
    /// Validation happens at `build`, not here: a module's `register` stays a stream of
    /// plain adds, and every problem across every module is reported together. Chains
    /// (`b.add(...).add(...)`) work, but a plain statement sequence is the normal shape.
    pub fn add(&mut self, module: &str, spec: OperationSpec) -> &mut Self {
        self.entries.push((module.to_owned(), spec));
        self
    }

    /// Validates every added operation and builds the catalogue; every problem found is
    /// returned together, each naming its operation.
    ///
    /// # Errors
    ///
    /// Every [`CatalogueError`] the added specs have: duplicate ids (naming both
    /// modules), duplicate parameter names, invalid defaults, secret parameters with
    /// defaults, invalid regexes, unusable `Int` ranges, empty enums and flag collisions.
    pub fn build(&self) -> Result<Catalogue, Vec<CatalogueError>> {
        let mut problems: Vec<CatalogueError> = Vec::new();
        let mut by_id: BTreeMap<&str, usize> = BTreeMap::new();
        for (position, (module, spec)) in self.entries.iter().enumerate() {
            if let Some(&first_position) = by_id.get(spec.id.as_str()) {
                let (first_module, _) = &self.entries[first_position];
                problems.push(CatalogueError::DuplicateId {
                    module_a: first_module.clone(),
                    module_b: module.clone(),
                    id: spec.id.as_str().to_owned(),
                });
                continue;
            }
            by_id.insert(spec.id.as_str(), position);
            problems.extend(check_spec(spec));
        }
        if problems.is_empty() {
            let mut specs = BTreeMap::new();
            for (id, &position) in &by_id {
                let (module, spec) = &self.entries[position];
                specs.insert((*id).to_owned(), (module.clone(), spec.clone()));
            }
            return Ok(Catalogue { by_id: specs });
        }
        Err(problems)
    }
}

/// Checks one spec's internals: parameter names, duplicates, defaults, secrets, ranges,
/// enums, regexes and flag collisions. Returns the problems in declaration order.
fn check_spec(spec: &OperationSpec) -> Vec<CatalogueError> {
    let op = spec.id.as_str().to_owned();
    let mut problems: Vec<CatalogueError> = Vec::new();

    let mut seen: BTreeSet<&str> = BTreeSet::new();
    // Every flag spelling claimed so far: `flag -> parameter name`, with each boolean
    // claiming both `--name` and `--no-name`.
    let mut claimed: BTreeMap<String, String> = BTreeMap::new();

    for param in &spec.params {
        if !is_param_name(&param.name) {
            problems.push(CatalogueError::ParamName {
                op: op.clone(),
                param: param.name.clone(),
            });
            continue;
        }
        if !seen.insert(&param.name) {
            problems.push(CatalogueError::DuplicateParam {
                op: op.clone(),
                param: param.name.clone(),
            });
            continue;
        }
        // A secret is always supplied at call time (or prompted for); it never has a
        // default that would sit in the spec.
        if param.secret && param.default.is_some() {
            problems.push(CatalogueError::SecretWithDefault {
                op: op.clone(),
                param: param.name.clone(),
            });
        }
        if let Some(default) = &param.default {
            check_default(&op, param, default, &mut problems);
        }
        match &param.ty {
            ParamType::Int { min, max } if min > max => {
                problems.push(CatalogueError::IntRangeEmpty {
                    op: op.clone(),
                    param: param.name.clone(),
                    min: *min,
                    max: *max,
                });
            }
            ParamType::Enum(choices) if choices.is_empty() => {
                problems.push(CatalogueError::EnumEmpty {
                    op: op.clone(),
                    param: param.name.clone(),
                });
            }
            ParamType::Text { max_len, pattern } => {
                if let Some(pattern) = pattern
                    && let Err(message) = Regex::new(pattern)
                {
                    problems.push(CatalogueError::PatternInvalid {
                        op: op.clone(),
                        param: param.name.clone(),
                        message: message.to_string(),
                    });
                }
                if *max_len == 0 && param.required {
                    problems.push(CatalogueError::TextShape {
                        op: op.clone(),
                        param: param.name.clone(),
                        detail:
                            "max_len is 0 and the parameter is required: no value can ever pass"
                                .to_owned(),
                    });
                }
            }
            _ => {}
        }

        // Flag bookkeeping: this parameter claims `--<flag>`, and a booleanish parameter
        // also claims `--no-<flag>` — the false spelling of its flag.
        let flag = kebab(&param.name);
        let mut claims = vec![flag.clone()];
        if is_boolish(&param.ty) {
            claims.push(format!("no-{flag}"));
        }
        for claim in &claims {
            if let Some(other) = claimed.get(claim) {
                // A true duplicate parameter was already reported; only cross-parameter
                // collisions reach here (same name twice is `seen`-guarded above).
                if *other != param.name {
                    problems.push(CatalogueError::FlagCollision {
                        op: op.clone(),
                        flag: claim.clone(),
                        param: param.name.clone(),
                        other: other.clone(),
                    });
                }
            } else {
                claimed.insert(claim.clone(), param.name.clone());
            }
        }
    }
    problems
}

/// Checks whether `default` would satisfy `param`'s own type. The catalogue reuses the
/// invocation check and re-emits each problem as an authoring error named
/// [`CatalogueError::DefaultInvalid`].
fn check_default(
    op: &str,
    param: &ParamSpec,
    default: &ParamValue,
    problems: &mut Vec<CatalogueError>,
) {
    let mut local: Vec<InvocationError> = Vec::new();
    check_value(op, &param.name, &param.ty, default, &mut local);
    for error in local {
        problems.push(CatalogueError::DefaultInvalid {
            op: op.to_owned(),
            param: param.name.clone(),
            detail: default_detail(&error),
        });
    }
}

/// The words a default's rejection turns into, keeping the type-check vocabulary.
fn default_detail(error: &InvocationError) -> String {
    match error {
        InvocationError::WrongType { .. } => {
            "the default's shape does not fit the parameter's type".to_owned()
        }
        InvocationError::OutOfRange {
            value, min, max, ..
        } => {
            format!("the default {value} is out of range {min}..{max}")
        }
        InvocationError::PatternMismatch { .. } => {
            "the default does not match the parameter's pattern".to_owned()
        }
        InvocationError::NotInEnum { .. } => {
            "the default is not one of the parameter's choices".to_owned()
        }
        InvocationError::TooLong { max_len, .. } => {
            format!("the default is longer than {max_len} characters")
        }
        other => format!("the default was refused: {other}"),
    }
}

/// The built catalogue: every operation the modules registered, validated on construction.
///
/// Deterministic: ids are kept sorted, parameters stay in declaration order, and search
/// is a case-insensitive substring match over id, title and summary.
#[derive(Clone, Debug)]
pub struct Catalogue {
    /// The operation specs that passed validation, keyed by id, with their modules.
    by_id: BTreeMap<String, (String, OperationSpec)>,
}

impl Catalogue {
    /// Starts a [`CatalogueBuilder`] for a fresh empty catalogue.
    #[must_use]
    pub fn builder() -> CatalogueBuilder {
        CatalogueBuilder::new()
    }

    /// Builds a catalogue from the given modules, in the order listed.
    ///
    /// The modules are `&dyn` behind the scenes: a host that loads one implementation
    /// passes a homogeneous slice; heterogeneous module sets register through a
    /// [`CatalogueBuilder`] directly.
    ///
    /// # Errors
    ///
    /// The same errors [`CatalogueBuilder::build`] reports.
    pub fn from_modules<M: crate::OperationModule + ?Sized>(
        modules: &[Box<M>],
    ) -> Result<Self, Vec<CatalogueError>> {
        let mut builder = CatalogueBuilder::new();
        for module in modules {
            module.as_ref().register(&mut builder);
        }
        builder.build()
    }

    /// The spec for `id`, when the catalogue knows the operation.
    #[must_use]
    pub fn get(&self, id: &OpId) -> Option<&OperationSpec> {
        self.by_id.get(id.as_str()).map(|(_, spec)| spec)
    }

    /// The module that registered `id`, when the catalogue knows the operation.
    #[must_use]
    pub fn module_of(&self, id: &OpId) -> Option<&str> {
        self.by_id
            .get(id.as_str())
            .map(|(module, _)| module.as_str())
    }

    /// Every operation, in sorted id order.
    pub fn iter(&self) -> impl Iterator<Item = &OperationSpec> {
        self.by_id.values().map(|(_, spec)| spec)
    }

    /// How many operations the catalogue holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether the catalogue holds no operation.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Every operation whose id, title or summary contains `text` case-insensitively, in
    /// sorted id order. Empty text matches nothing (the console passes a real query).
    #[must_use]
    pub fn search(&self, text: &str) -> Vec<&OperationSpec> {
        let needle = text.to_lowercase();
        if needle.is_empty() {
            return Vec::new();
        }
        self.by_id
            .values()
            .filter(|(_, spec)| {
                spec.id.as_str().to_lowercase().contains(&needle)
                    || spec.title.to_lowercase().contains(&needle)
                    || spec.summary.to_lowercase().contains(&needle)
            })
            .map(|(_, spec)| spec)
            .collect()
    }

    /// Validates an invocation against this catalogue, filling defaults into
    /// [`Validated`]; every problem found is reported together.
    ///
    /// # Errors
    ///
    /// One [`InvocationError`] per problem: unknown operation, unknown parameter, missing
    /// required parameter, wrong type, out of range, pattern mismatch, not in enum, too
    /// long.
    pub fn validate(
        &self,
        invocation: &crate::Invocation,
    ) -> Result<Validated, Vec<InvocationError>> {
        crate::validate::validate(self, invocation)
    }
}
