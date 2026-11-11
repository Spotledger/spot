//! Validation, change sets, summarisation and application.

use spot_mgmt_ops::value_check::check_param_value;
use spot_mgmt_ops::{ParamValue, Risk};

use crate::builder::SettingsRegistry;
use crate::error::{SettingsError, SettingsErrorKind, SettingsErrorReport};
use crate::hash::CanonicalHasher;
use crate::model::{EffectiveValue, Restart, ScopeRef, StoredValue};
use crate::resolution::effective;
use crate::store::{SettingWrite, ValueStore};

/// One change to a setting: set a value (plain or secret) at a scope, or remove it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// The setting key.
    pub key: String,
    /// The scope to write at.
    pub scope: ScopeRef,
    /// The new value, or `None` to remove any value at this scope.
    pub value: Option<StoredValue>,
}

impl Change {
    /// A change that sets a plain value.
    #[must_use]
    pub fn set(scope: ScopeRef, key: impl Into<String>, value: ParamValue) -> Self {
        Self {
            scope,
            key: key.into(),
            value: Some(StoredValue::Value(value)),
        }
    }

    /// A change that sets a secret.
    #[must_use]
    pub fn set_secret(scope: ScopeRef, key: impl Into<String>, secret: impl Into<String>) -> Self {
        Self {
            scope,
            key: key.into(),
            value: Some(StoredValue::Secret(crate::model::SecretValue::new(secret))),
        }
    }

    /// A change that removes any value at the scope.
    #[must_use]
    pub fn remove(scope: ScopeRef, key: impl Into<String>) -> Self {
        Self {
            scope,
            key: key.into(),
            value: None,
        }
    }
}

/// A batch of changes, validated, summarised and applied as a whole.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeSet {
    changes: Vec<Change>,
}

impl ChangeSet {
    /// An empty change set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a change.
    #[must_use]
    pub fn push_change(mut self, change: Change) -> Self {
        self.changes.push(change);
        self
    }

    /// Adds every change.
    #[must_use]
    pub fn extend(mut self, changes: impl IntoIterator<Item = Change>) -> Self {
        self.changes.extend(changes);
        self
    }

    /// The changes, in insertion order.
    #[must_use]
    pub fn changes(&self) -> &[Change] {
        &self.changes
    }

    /// Whether the set holds no changes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Validates every change and reports **all** problems, each tagged with its key.
    ///
    /// # Errors
    ///
    /// Returns `settings.unknown_setting`, `settings.read_only`, `settings.scope_not_allowed`,
    /// `settings.validation_failed` or `settings.change_set_invalid`.
    pub fn validate(&self, registry: &SettingsRegistry) -> Result<(), SettingsErrorReport> {
        validate_changes(&self.changes, registry)
    }

    /// A stable, input-order-independent hash of the change set.
    ///
    /// Changes are sorted by key then scope before hashing, so a shuffled batch hashes the
    /// same. A secret's text is never hashed: a secret change contributes the constant marker
    /// `secret`.
    #[must_use]
    pub fn hash(&self) -> String {
        let mut ordered: Vec<&Change> = self.changes.iter().collect();
        ordered.sort_by(|left, right| {
            left.key
                .cmp(&right.key)
                .then_with(|| left.scope.canonical().cmp(&right.scope.canonical()))
        });

        let mut hasher = CanonicalHasher::new("spot-settings/changeset/v1");
        for change in ordered {
            hasher.text(&change.key);
            hasher.text(&change.scope.canonical());
            match &change.value {
                Some(StoredValue::Value(value)) => {
                    hasher.text("value");
                    hasher.field(canonical_value(value).as_bytes());
                }
                Some(StoredValue::Secret(_)) => {
                    hasher.text("secret");
                }
                None => {
                    hasher.text("remove");
                }
            }
            hasher.end_record();
        }
        hasher.finish()
    }
}

/// Validates a batch of changes, reporting every problem with its key.
///
/// # Errors
///
/// As [`ChangeSet::validate`].
pub fn validate_changes(
    changes: &[Change],
    registry: &SettingsRegistry,
) -> Result<(), SettingsErrorReport> {
    let mut report = SettingsErrorReport::new();

    for change in changes {
        let Some(spec) = registry.get(&change.key) else {
            report.push(SettingsError::new(
                SettingsErrorKind::UnknownSetting,
                format!("change names unregistered setting '{}'", change.key),
            ));
            continue;
        };

        if spec.read_only {
            report.push(SettingsError::new(
                SettingsErrorKind::ReadOnly,
                format!("setting '{}' is read-only", change.key),
            ));
        }

        if !spec.scopes.contains(&change.scope.scope()) {
            report.push(SettingsError::new(
                SettingsErrorKind::ScopeNotAllowed,
                format!(
                    "setting '{}' cannot be set at scope {}",
                    change.key, change.scope
                ),
            ));
        }

        if let Some(value) = &change.value {
            let checked = match value {
                StoredValue::Value(value) => value.clone(),
                StoredValue::Secret(secret) => ParamValue::Text(secret.expose().to_owned()),
            };
            for problem in check_param_value(&change.key, &spec.value, &checked) {
                report.push(SettingsError::new(
                    SettingsErrorKind::ValidationFailed,
                    problem.to_string(),
                ));
            }
        }
    }

    let mut seen: Vec<(String, String)> = Vec::with_capacity(changes.len());
    for change in changes {
        let key = (change.key.clone(), change.scope.canonical());
        if seen.contains(&key) {
            report.push(SettingsError::new(
                SettingsErrorKind::ChangeSetInvalid,
                format!(
                    "change set names '{}' at {} more than once",
                    change.key, change.scope
                ),
            ));
        } else {
            seen.push(key);
        }
    }

    report.into_result()
}

/// The highest risk, the restarts implied, the settings affected through `depends_on`, and
/// plain sentences describing a change set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeSummary {
    /// The highest risk among the changed settings (`Risk::Read` for an empty set).
    pub highest_risk: Risk,
    /// The distinct restarts implied, in key order.
    pub restarts: Vec<Restart>,
    /// Whether the change set requires any restart.
    pub restart_needed: bool,
    /// Settings that will be affected through their `depends_on`, in key order.
    pub affected_via_depends_on: Vec<String>,
    /// One plain sentence per change, in key order.
    pub sentences: Vec<String>,
    /// The sentences joined into one paragraph.
    pub summary: String,
}

/// Summarises a change set for approval.
///
/// A secret's value never appears: its sentence names the key and says "secret".
#[must_use]
pub fn summarise(change_set: &ChangeSet, registry: &SettingsRegistry) -> ChangeSummary {
    let mut ordered: Vec<&Change> = change_set.changes().iter().collect();
    ordered.sort_by(|left, right| {
        left.key
            .cmp(&right.key)
            .then_with(|| left.scope.canonical().cmp(&right.scope.canonical()))
    });

    let mut highest_risk = Risk::Read;
    let mut restarts: Vec<Restart> = Vec::new();
    let mut affected: Vec<String> = Vec::new();
    let mut sentences: Vec<String> = Vec::new();

    for change in ordered {
        let spec = registry.get(&change.key);
        if let Some(spec) = spec {
            if spec.risk > highest_risk {
                highest_risk = spec.risk;
            }
            if spec.restart.needed() && !restarts.contains(&spec.restart) {
                restarts.push(spec.restart.clone());
            }
            for other in registry.specs() {
                if other
                    .depends_on
                    .as_ref()
                    .is_some_and(|dep| dep.key == change.key)
                    && !affected.contains(&other.key)
                {
                    affected.push(other.key.clone());
                }
            }
            sentences.push(sentence(change));
        } else {
            sentences.push(format!("Change '{}' (unknown setting).", change.key));
        }
    }

    affected.sort();
    let summary = if sentences.is_empty() {
        "No changes.".to_owned()
    } else {
        sentences.join(" ")
    };
    let restart_needed = !restarts.is_empty();

    ChangeSummary {
        highest_risk,
        restarts,
        restart_needed,
        affected_via_depends_on: affected,
        sentences,
        summary,
    }
}

/// One plain sentence for a change; a secret's value is never included.
fn sentence(change: &Change) -> String {
    match &change.value {
        Some(StoredValue::Value(value)) => {
            format!("Set '{}' to {:?} at {}.", change.key, value, change.scope)
        }
        Some(StoredValue::Secret(_)) => format!(
            "Set secret '{}' at {}; the value is not shown.",
            change.key, change.scope
        ),
        None => format!("Remove any value of '{}' at {}.", change.key, change.scope),
    }
}

/// The before and after of one applied change; secrets appear only as redactions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedChange {
    /// The setting key.
    pub key: String,
    /// The scope written.
    pub scope: ScopeRef,
    /// The value before, `None` when unset.
    pub before: Option<EffectiveValue>,
    /// The value after, `None` when removed.
    pub after: Option<EffectiveValue>,
}

/// Applies a change set through the store's batch write.
///
/// The store applies the batch atomically where it can (the in-memory store does). The
/// returned records carry effective (never secret) before and after values.
///
/// # Errors
///
/// Returns the store's error, or `settings.unknown_setting` for a change whose spec is gone.
pub fn apply_changes(
    change_set: &ChangeSet,
    registry: &SettingsRegistry,
    store: &dyn ValueStore,
) -> Result<Vec<AppliedChange>, SettingsErrorReport> {
    let writes: Vec<SettingWrite> = change_set
        .changes()
        .iter()
        .map(|change| SettingWrite {
            scope: change.scope.clone(),
            key: change.key.clone(),
            value: change.value.clone(),
        })
        .collect();

    let outcomes = store.apply(&writes).map_err(SettingsErrorReport::from)?;

    let mut applied = Vec::with_capacity(outcomes.len());
    for outcome in outcomes {
        let spec = registry.get(&outcome.key);
        applied.push(AppliedChange {
            key: outcome.key.clone(),
            scope: outcome.scope.clone(),
            before: spec
                .and_then(|spec| outcome.before.as_ref().map(|value| effective(spec, value))),
            after: spec.and_then(|spec| outcome.after.as_ref().map(|value| effective(spec, value))),
        });
    }
    Ok(applied)
}

/// A deterministic canonical text for a value, for hashing.
fn canonical_value(value: &ParamValue) -> String {
    serde_json::to_string(value).unwrap_or_default()
}
