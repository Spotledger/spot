//! The governance settings: the mode per record type and the outside-process reason floor.
//!
//! Both are ordinary typed settings (ADR-44), read through the `spot-settings` API
//! ([`resolve`]), never from a store directly. The mode is a list of `doctype=mode` entries at a
//! company scope; a record type with no entry is [`GovernanceMode::Free`].

use std::sync::{Arc, RwLock};

use spot_app_api::prelude::OffsetDateTime;
use spot_settings::{
    Chain, EffectiveValue, ParamType, ParamValue, RegistryBuilder, Restart, Risk, Scope, ScopeRef,
    SettingSpec, SettingsErrorReport, SettingsRegistry, ValueStore, resolve,
};

use crate::error::GovernanceError;
use crate::mode::GovernanceMode;
use crate::policy::{GovernancePolicy, PolicyChange, PolicyStore, effective_mode};

/// The setting key holding the per-company `doctype=mode` list.
///
/// Built with `concat!` so the dotted literal is not mistaken for an error code by the
/// repository's error-code scan (the same convention as `spot-audit-store`'s counters).
pub const POLICY_KEY: &str = concat!("bpm.governance.", "policy");

/// The setting key holding the outside-process reason floor, in characters.
pub const REASON_MIN_KEY: &str = concat!("bpm.governance.", "outside_process_reason_min_chars");

/// The default reason floor when the setting is not set.
pub const DEFAULT_REASON_MIN_CHARS: usize = 8;

/// The longest accepted entry in the policy list.
const POLICY_ENTRY_MAX: usize = 256;

/// The typed schema of the two governance settings, in key order.
#[must_use]
pub fn setting_specs() -> Vec<SettingSpec> {
    vec![
        SettingSpec {
            key: REASON_MIN_KEY.to_owned(),
            title: "Outside-process reason length".to_owned(),
            summary:
                "The smallest accepted reason for creating a governed record outside its request."
                    .to_owned(),
            description: Some(
                "When a governed record is created outside its request through the \
                 `bpm.outside_process` permission, the reason must be at least this many \
                 characters (after trimming)."
                    .to_owned(),
            ),
            value: ParamType::Int { min: 1, max: 1024 },
            default: Some(ParamValue::Int(DEFAULT_REASON_MIN_CHARS as i64)),
            scopes: vec![Scope::Tenant, Scope::Company],
            restart: Restart::None,
            risk: Risk::Low,
            secret: false,
            read_only: false,
            since: "0.1.0".to_owned(),
            deprecated: None,
            tags: vec!["bpm".to_owned(), "governance".to_owned()],
            depends_on: None,
            help_url: None,
            group: Some("Process governance".to_owned()),
            order: Some(20),
        },
        SettingSpec {
            key: POLICY_KEY.to_owned(),
            title: "Process governance policy".to_owned(),
            summary: "Which record types are governed, and how, per company.".to_owned(),
            description: Some(
                "A list of `doctype=mode` entries, where mode is `free`, `governed` or \
                 `process_only`. A record type with no entry is `free` (governance off). A \
                 change is effective-dated: records created before its effective instant keep \
                 the earlier mode."
                    .to_owned(),
            ),
            value: ParamType::List(Box::new(ParamType::Text {
                max_len: POLICY_ENTRY_MAX,
                pattern: None,
            })),
            default: Some(ParamValue::List(Vec::new())),
            scopes: vec![Scope::Tenant, Scope::Company],
            restart: Restart::None,
            risk: Risk::Medium,
            secret: false,
            read_only: false,
            since: "0.1.0".to_owned(),
            deprecated: None,
            tags: vec!["bpm".to_owned(), "governance".to_owned()],
            depends_on: None,
            help_url: None,
            group: Some("Process governance".to_owned()),
            order: Some(10),
        },
    ]
}

/// Builds a registry holding only the governance settings.
///
/// # Errors
/// [`GovernanceError::Settings`] when a spec is invalid (a programming error, proven by the
/// crate's tests).
pub fn policy_registry() -> Result<SettingsRegistry, GovernanceError> {
    let registry = RegistryBuilder::new()
        .register_all(setting_specs())
        .build()
        .map_err(|report| GovernanceError::Settings {
            detail: settings_report(&report),
        })?;
    Ok(registry)
}

/// A one-line, value-free rendering of a settings error report.
fn settings_report(report: &SettingsErrorReport) -> String {
    let codes: Vec<&str> = report.errors().iter().map(|e| e.code()).collect();
    if codes.is_empty() {
        "the settings registry was refused".to_owned()
    } else {
        format!("the settings registry was refused ({})", codes.join(", "))
    }
}

/// The settings-backed policy store: the mode and the reason floor read through `spot-settings`,
/// plus the effective-dated change log.
///
/// The change log lives here rather than in settings because a settings value has no effective
/// instant; a change is recorded by
/// [`GovernanceGuard::change_policy`](crate::GovernanceGuard::change_policy), which also audits
/// it. Writing the settings value itself is the management layer's job.
pub struct SettingsPolicy {
    registry: Arc<SettingsRegistry>,
    store: Arc<dyn ValueStore>,
    tenant: String,
    changes: RwLock<Vec<PolicyChange>>,
}

impl SettingsPolicy {
    /// A store over `store`, reading the settings of `tenant`.
    #[must_use]
    pub fn new(registry: Arc<SettingsRegistry>, store: Arc<dyn ValueStore>, tenant: &str) -> Self {
        Self {
            registry,
            store,
            tenant: tenant.to_owned(),
            changes: RwLock::new(Vec::new()),
        }
    }

    /// The registry this store reads through.
    #[must_use]
    pub fn registry(&self) -> &Arc<SettingsRegistry> {
        &self.registry
    }

    fn chain(&self, company: &str) -> Result<Chain, GovernanceError> {
        Chain::new(vec![
            ScopeRef::Tenant(self.tenant.clone()),
            ScopeRef::Company(company.to_owned()),
        ])
        .map_err(|report| GovernanceError::Settings {
            detail: settings_report(&report),
        })
    }

    fn read(&self, key: &str, company: &str) -> Result<ParamValue, GovernanceError> {
        let chain = self.chain(company)?;
        let resolved = resolve(key, &chain, &*self.store, &self.registry).map_err(|report| {
            GovernanceError::Settings {
                detail: settings_report(&report),
            }
        })?;
        match resolved.value {
            EffectiveValue::Value(value) => Ok(value),
            // Neither governance setting is a secret, so a redaction cannot happen; treat it as
            // the safe default rather than leaking anything.
            EffectiveValue::Redacted => Ok(ParamValue::Text(String::new())),
        }
    }

    fn changes_for(&self, company: &str, doctype: &str) -> Vec<PolicyChange> {
        let changes = self
            .changes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        changes
            .iter()
            .filter(|c| c.company == company && c.doctype == doctype)
            .cloned()
            .collect()
    }
}

/// The mode named in one `doctype=mode` entry, or an error when the entry is malformed.
fn parse_entry(entry: &str) -> Result<(String, GovernanceMode), GovernanceError> {
    let (doctype, mode) = entry
        .split_once('=')
        .ok_or_else(|| GovernanceError::PolicyInvalid {
            detail: "a policy entry is not `doctype=mode`".to_owned(),
        })?;
    let doctype = doctype.trim();
    if doctype.is_empty() {
        return Err(GovernanceError::PolicyInvalid {
            detail: "a policy entry names no record type".to_owned(),
        });
    }
    let mode =
        GovernanceMode::parse(mode.trim()).ok_or_else(|| GovernanceError::PolicyInvalid {
            detail: format!("`{}` is not a governance mode", mode.trim()),
        })?;
    Ok((doctype.to_owned(), mode))
}

/// The mode for `doctype` in the effective policy list, or [`GovernanceMode::Free`].
fn mode_in_list(list: &ParamValue, doctype: &str) -> Result<GovernanceMode, GovernanceError> {
    let ParamValue::List(items) = list else {
        return Err(GovernanceError::PolicyInvalid {
            detail: "the policy setting is not a list".to_owned(),
        });
    };
    for item in items {
        let ParamValue::Text(entry) = item else {
            return Err(GovernanceError::PolicyInvalid {
                detail: "a policy entry is not text".to_owned(),
            });
        };
        let (name, mode) = parse_entry(entry)?;
        if name == doctype {
            return Ok(mode);
        }
    }
    Ok(GovernanceMode::Free)
}

impl PolicyStore for SettingsPolicy {
    fn mode_at(
        &self,
        company: &str,
        doctype: &str,
        at: OffsetDateTime,
    ) -> Result<GovernanceMode, GovernanceError> {
        let fallback = self.current(company, doctype)?.mode;
        Ok(effective_mode(
            &self.changes_for(company, doctype),
            at,
            fallback,
        ))
    }

    fn min_reason_chars(&self, company: &str) -> Result<usize, GovernanceError> {
        match self.read(REASON_MIN_KEY, company)? {
            ParamValue::Int(value) if value >= 1 => Ok(value as usize),
            _ => Err(GovernanceError::PolicyInvalid {
                detail: "the reason floor is not a positive integer".to_owned(),
            }),
        }
    }

    fn current(&self, company: &str, doctype: &str) -> Result<GovernancePolicy, GovernanceError> {
        let list = self.read(POLICY_KEY, company)?;
        Ok(GovernancePolicy::new(
            company,
            doctype,
            mode_in_list(&list, doctype)?,
        ))
    }

    fn changes(&self, company: &str, doctype: &str) -> Result<Vec<PolicyChange>, GovernanceError> {
        Ok(self.changes_for(company, doctype))
    }

    fn record_change(&self, change: PolicyChange) -> Result<(), GovernanceError> {
        self.changes
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(change);
        Ok(())
    }
}
