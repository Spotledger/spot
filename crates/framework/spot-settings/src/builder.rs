//! The settings registry and the builder that validates it.

use std::collections::{BTreeMap, BTreeSet};

use spot_mgmt_ops::value_check::check_param_value;

use crate::error::{SettingsError, SettingsErrorKind, SettingsErrorReport};
use crate::spec::SettingSpec;

/// The compiled, validated registry of every setting.
///
/// Specs are kept in key order, so iteration, search and screens are deterministic without
/// a sort step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsRegistry {
    specs: BTreeMap<String, SettingSpec>,
    by_area: BTreeMap<String, Vec<String>>,
}

impl SettingsRegistry {
    /// Gets a spec by key.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&SettingSpec> {
        self.specs.get(key)
    }

    /// Whether `key` is registered.
    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.specs.contains_key(key)
    }

    /// Every spec, in key order.
    pub fn specs(&self) -> impl Iterator<Item = &SettingSpec> {
        self.specs.values()
    }

    /// Every spec in an area (the key's first segment).
    pub fn specs_in_area(&self, area: &str) -> Vec<&SettingSpec> {
        self.by_area
            .get(area)
            .map(|keys| keys.iter().filter_map(|key| self.specs.get(key)).collect())
            .unwrap_or_default()
    }

    /// The number of registered settings.
    #[must_use]
    pub fn len(&self) -> usize {
        self.specs.len()
    }

    /// Whether the registry holds no settings.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    /// Every key, in order.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.specs.keys()
    }
}

/// Builds a [`SettingsRegistry`] from specs, validating everything at once.
///
/// [`RegistryBuilder::build`] reports **every** problem it finds rather than the first:
/// duplicate keys, a default that fails its own type, a missing default for a non-secret or a
/// present one for a secret, an empty or misordered scope list, an unknown or cyclic
/// `depends_on`, and a deprecation that points nowhere.
#[derive(Clone, Debug, Default)]
pub struct RegistryBuilder {
    specs: Vec<SettingSpec>,
}

impl RegistryBuilder {
    /// A builder with no specs.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one spec.
    #[must_use]
    pub fn push_spec(mut self, spec: SettingSpec) -> Self {
        self.specs.push(spec);
        self
    }

    /// Adds one spec (alias of [`RegistryBuilder::push_spec`], for building from a path).
    #[must_use]
    pub fn add_spec(self, spec: SettingSpec) -> Self {
        self.push_spec(spec)
    }

    /// Registers every spec from a platform crate or app.
    ///
    /// This is the acceptance line: a spec in code appears in the registry, screens and search
    /// with no further work.
    #[must_use]
    pub fn register_all(mut self, specs: impl IntoIterator<Item = SettingSpec>) -> Self {
        self.specs.extend(specs);
        self
    }

    /// Builds the registry.
    ///
    /// # Errors
    ///
    /// Returns every problem found, each carrying a stable `settings.*` code.
    pub fn build(self) -> Result<SettingsRegistry, SettingsErrorReport> {
        let mut report = SettingsErrorReport::new();

        // Duplicate keys.
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for spec in &self.specs {
            if !seen.insert(spec.key.as_str()) {
                report.push(SettingsError::new(
                    SettingsErrorKind::DuplicateKey,
                    format!("duplicate setting key '{}'", spec.key),
                ));
            }
        }

        let mut registry = SettingsRegistry {
            specs: BTreeMap::new(),
            by_area: BTreeMap::new(),
        };

        for spec in &self.specs {
            if let Err(message) = spec.key_error() {
                report.push(SettingsError::new(
                    SettingsErrorKind::KeyFormat,
                    format!("setting '{}': {message}", spec.key),
                ));
            }

            validate_scopes(spec, &mut report);
            validate_default(spec, &mut report);

            let area = spec.area().to_owned();
            registry
                .by_area
                .entry(area)
                .or_default()
                .push(spec.key.clone());
            registry.specs.insert(spec.key.clone(), spec.clone());
        }

        validate_depends_on(&self.specs, &mut report);
        validate_deprecated(&self.specs, &mut report);

        report.into_result()?;
        Ok(registry)
    }
}

/// Checks the scope list is non-empty and in broad-to-narrow order.
fn validate_scopes(spec: &SettingSpec, report: &mut SettingsErrorReport) {
    if spec.scopes.is_empty() {
        report.push(SettingsError::new(
            SettingsErrorKind::ScopesEmpty,
            format!("setting '{}' has no scopes", spec.key),
        ));
        return;
    }
    for pair in spec.scopes.windows(2) {
        if pair[0] >= pair[1] {
            report.push(SettingsError::new(
                SettingsErrorKind::ScopesOrder,
                format!(
                    "setting '{}' scopes are not broad-to-narrow: {} is not narrower than {}",
                    spec.key, pair[1], pair[0]
                ),
            ));
            return;
        }
    }
}

/// Checks the default rules and validates a present default through the shared value check.
fn validate_default(spec: &SettingSpec, report: &mut SettingsErrorReport) {
    match (&spec.secret, &spec.default) {
        (true, Some(_)) => {
            report.push(SettingsError::new(
                SettingsErrorKind::SecretWithDefault,
                format!("secret setting '{}' must not have a default", spec.key),
            ));
        }
        (false, None) => {
            report.push(SettingsError::new(
                SettingsErrorKind::DefaultMissing,
                format!("setting '{}' needs a default", spec.key),
            ));
        }
        (false, Some(value)) => {
            for problem in check_param_value(&spec.key, &spec.value, value) {
                report.push(SettingsError::new(
                    SettingsErrorKind::DefaultInvalid,
                    problem.to_string(),
                ));
            }
        }
        (true, None) => {}
    }
}

/// Checks every `depends_on` names a registered key and the graph has no cycle.
fn validate_depends_on(specs: &[SettingSpec], report: &mut SettingsErrorReport) {
    let known: BTreeSet<&str> = specs.iter().map(|spec| spec.key.as_str()).collect();
    for spec in specs {
        if let Some(depends_on) = &spec.depends_on
            && !known.contains(depends_on.key.as_str())
        {
            report.push(SettingsError::new(
                SettingsErrorKind::DependsOnUnknown,
                format!(
                    "setting '{}' depends_on unknown key '{}'",
                    spec.key, depends_on.key
                ),
            ));
        }
    }

    // Depth-first walk of the depends_on graph; a node on the current stack closes a cycle.
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        New,
        Open,
        Done,
    }
    let edges: BTreeMap<&str, Option<&str>> = specs
        .iter()
        .map(|spec| {
            (
                spec.key.as_str(),
                spec.depends_on.as_ref().map(|dep| dep.key.as_str()),
            )
        })
        .collect();
    let mut marks: BTreeMap<&str, Mark> = edges.keys().map(|key| (*key, Mark::New)).collect();
    let mut cycle_reported: BTreeSet<&str> = BTreeSet::new();

    for start in edges.keys() {
        if marks.get(start) != Some(&Mark::New) {
            continue;
        }
        let mut stack: Vec<&str> = vec![start];
        while let Some(current) = stack.last().copied() {
            let Some(next) = edges.get(current).copied().flatten() else {
                marks.insert(current, Mark::Done);
                stack.pop();
                continue;
            };
            match marks.get(next).copied().unwrap_or(Mark::New) {
                Mark::New => {
                    marks.insert(next, Mark::Open);
                    stack.push(next);
                }
                Mark::Open => {
                    if cycle_reported.insert(next) {
                        report.push(SettingsError::new(
                            SettingsErrorKind::DependsOnCycle,
                            format!("setting '{}' depends_on cycle through '{next}'", start),
                        ));
                    }
                    marks.insert(current, Mark::Done);
                    stack.pop();
                }
                Mark::Done => {
                    marks.insert(current, Mark::Done);
                    stack.pop();
                }
            }
        }
    }
}

/// Checks every deprecation points at a registered key.
fn validate_deprecated(specs: &[SettingSpec], report: &mut SettingsErrorReport) {
    let known: BTreeSet<&str> = specs.iter().map(|spec| spec.key.as_str()).collect();
    for spec in specs {
        if let Some(deprecated) = &spec.deprecated
            && !known.contains(deprecated.use_instead.as_str())
        {
            report.push(SettingsError::new(
                SettingsErrorKind::DeprecatedUnknown,
                format!(
                    "setting '{}' deprecation points to unknown key '{}'",
                    spec.key, deprecated.use_instead
                ),
            ));
        }
    }
}
