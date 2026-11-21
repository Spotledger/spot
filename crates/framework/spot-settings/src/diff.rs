//! Differs-from-default and the effective-value hash.

use crate::builder::SettingsRegistry;
use crate::hash::CanonicalHasher;
use crate::model::{Difference, EffectiveHash, EffectiveValue, Origin, ScopeRef};
use crate::resolution::{Chain, resolve};
use crate::spec::SettingSpec;
use crate::store::ValueStore;

/// Whether a resolved setting differs from its default.
///
/// For a non-secret setting the comparison is on the **effective value**, not on where it came
/// from: setting a value equal to the default is not a difference. A secret has no default, so
/// a secret that is set at all is a difference.
#[must_use]
pub(crate) fn is_different(spec: &SettingSpec, value: &EffectiveValue, origin: &Origin) -> bool {
    if spec.secret {
        return !matches!(origin, Origin::Default);
    }
    match &spec.default {
        Some(default) => *value != EffectiveValue::Value(default.clone()),
        None => !matches!(origin, Origin::Default),
    }
}

/// Every setting whose effective value differs from its default.
///
/// `scope_filter` restricts the result to settings whose effective value is set exactly at
/// that scope; `None` returns differences at any scope. Results are in key order.
#[must_use]
pub fn differs(
    chain: &Chain,
    store: &dyn ValueStore,
    registry: &SettingsRegistry,
    scope_filter: Option<&ScopeRef>,
) -> Vec<Difference> {
    let mut differences = Vec::new();
    for spec in registry.specs() {
        let Ok(resolved) = resolve(&spec.key, chain, store, registry) else {
            continue;
        };
        if !is_different(spec, &resolved.value, &resolved.origin) {
            continue;
        }
        if let Some(filter) = scope_filter {
            let at_filter = matches!(&resolved.origin, Origin::Set(scope) if scope == filter);
            if !at_filter {
                continue;
            }
        }
        differences.push(Difference {
            key: spec.key.clone(),
            value: resolved.value,
            origin: resolved.origin,
            default: spec.default.clone(),
        });
    }
    differences.sort_by(|left, right| left.key.cmp(&right.key));
    differences
}

/// The hash of every explicitly set effective value, sorted by key.
///
/// A secret contributes the constant marker `redacted`; its text is never hashed. This is the
/// basis for the desired-state drift check (ADM-08).
#[must_use]
pub fn effective_hash(
    chain: &Chain,
    store: &dyn ValueStore,
    registry: &SettingsRegistry,
) -> EffectiveHash {
    let mut entries: Vec<(String, EffectiveValue)> = Vec::new();
    for spec in registry.specs() {
        let Ok(resolved) = resolve(&spec.key, chain, store, registry) else {
            continue;
        };
        if matches!(resolved.origin, Origin::Default) {
            continue;
        }
        entries.push((spec.key.clone(), resolved.value));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    let mut hasher = CanonicalHasher::new("spot-settings/effective/v1");
    for (key, value) in entries {
        hasher.text(&key);
        match value {
            EffectiveValue::Value(value) => {
                hasher.text("value");
                hasher.field(serde_json::to_string(&value).unwrap_or_default().as_bytes());
            }
            EffectiveValue::Redacted => hasher.text("redacted"),
        }
        hasher.end_record();
    }
    EffectiveHash(hasher.finish())
}
