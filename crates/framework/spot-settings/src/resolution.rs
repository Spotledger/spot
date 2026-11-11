//! The scope chain and effective-value resolution.

use crate::builder::SettingsRegistry;
use crate::error::{SettingsError, SettingsErrorKind, SettingsErrorReport};
use crate::model::{Considered, EffectiveValue, Origin, Resolved, ScopeRef, StoredValue};
use crate::spec::SettingSpec;
use crate::store::ValueStore;

/// An ordered list of scopes that apply to one asker, broad to narrow.
///
/// A chain need not include every level; it must be strictly broad-to-narrow, which
/// [`Chain::new`] checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chain {
    scopes: Vec<ScopeRef>,
}

impl Chain {
    /// A chain from `scopes`, which must be strictly broad-to-narrow.
    ///
    /// # Errors
    ///
    /// Returns `settings.scopes_order` when a scope is not narrower than the one before it.
    pub fn new(scopes: Vec<ScopeRef>) -> Result<Self, SettingsErrorReport> {
        let mut report = SettingsErrorReport::new();
        for pair in scopes.windows(2) {
            if pair[0].scope() >= pair[1].scope() {
                report.push(SettingsError::new(
                    SettingsErrorKind::ScopesOrder,
                    format!(
                        "chain scopes are not broad-to-narrow: {} is not narrower than {}",
                        pair[1], pair[0]
                    ),
                ));
            }
        }
        report.into_result()?;
        Ok(Self { scopes })
    }

    /// A chain for a context, skipping the levels that do not apply.
    #[must_use]
    pub fn for_context(
        fleet: bool,
        group: Option<&str>,
        server: Option<&str>,
        tenant: Option<&str>,
        company: Option<&str>,
        user: Option<&str>,
    ) -> Self {
        let mut scopes = Vec::new();
        if fleet {
            scopes.push(ScopeRef::Fleet);
        }
        if let Some(group) = group {
            scopes.push(ScopeRef::Group(group.to_owned()));
        }
        if let Some(server) = server {
            scopes.push(ScopeRef::Server(server.to_owned()));
        }
        if let Some(tenant) = tenant {
            scopes.push(ScopeRef::Tenant(tenant.to_owned()));
        }
        if let Some(company) = company {
            scopes.push(ScopeRef::Company(company.to_owned()));
        }
        if let Some(user) = user {
            scopes.push(ScopeRef::User(user.to_owned()));
        }
        Self { scopes }
    }

    /// The scopes, broad to narrow.
    #[must_use]
    pub fn scopes(&self) -> &[ScopeRef] {
        &self.scopes
    }

    /// The narrowest scope in the chain, if any.
    #[must_use]
    pub fn narrowest(&self) -> Option<&ScopeRef> {
        self.scopes.last()
    }

    /// Whether the chain holds no scopes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.scopes.is_empty()
    }
}

impl From<Vec<ScopeRef>> for Chain {
    /// Builds a chain without re-checking order (used with chains already known ordered).
    fn from(scopes: Vec<ScopeRef>) -> Self {
        Self { scopes }
    }
}

/// Converts a stored value into its effective view: secrets become [`EffectiveValue::Redacted`].
pub(crate) fn effective(spec: &SettingSpec, stored: &StoredValue) -> EffectiveValue {
    match stored {
        StoredValue::Secret(_) => EffectiveValue::Redacted,
        StoredValue::Value(value) => {
            if spec.secret {
                EffectiveValue::Redacted
            } else {
                EffectiveValue::Value(value.clone())
            }
        }
    }
}

/// The default as an effective value: a secret has no default and reads as redacted.
pub(crate) fn default_effective(spec: &SettingSpec) -> EffectiveValue {
    match (&spec.secret, &spec.default) {
        (true, _) => EffectiveValue::Redacted,
        (false, Some(value)) => EffectiveValue::Value(value.clone()),
        (false, None) => EffectiveValue::Redacted,
    }
}

/// Resolves a setting's effective value for `chain`.
///
/// The narrowest set value wins. Every scope in the chain is listed in [`Resolved::chain`]
/// broad to narrow, with what it held, so the console can explain an override. A secret's
/// text is never returned; its effective value is [`EffectiveValue::Redacted`].
///
/// # Errors
///
/// Returns `settings.unknown_setting` when `key` is not registered.
pub fn resolve(
    key: &str,
    chain: &Chain,
    store: &dyn ValueStore,
    registry: &SettingsRegistry,
) -> Result<Resolved, SettingsErrorReport> {
    let Some(spec) = registry.get(key) else {
        return Err(SettingsError::new(
            SettingsErrorKind::UnknownSetting,
            format!("setting '{key}' is not registered"),
        )
        .into());
    };

    let mut considered = Vec::with_capacity(chain.scopes().len());
    for scope_ref in chain.scopes() {
        let stored = if spec.scopes.contains(&scope_ref.scope()) {
            store.get(scope_ref, key)
        } else {
            None
        };
        considered.push(Considered {
            scope: scope_ref.clone(),
            stored: stored.as_ref().map(|value| effective(spec, value)),
        });
    }

    // The narrowest set scope wins: the last `Some` in broad-to-narrow order.
    let mut winner: Option<(EffectiveValue, ScopeRef)> = None;
    for entry in &considered {
        if let Some(value) = &entry.stored {
            winner = Some((value.clone(), entry.scope.clone()));
        }
    }

    let (value, origin) = match winner {
        Some((value, scope)) => (value, Origin::Set(scope)),
        None => (default_effective(spec), Origin::Default),
    };

    Ok(Resolved {
        value,
        origin,
        chain: considered,
    })
}
