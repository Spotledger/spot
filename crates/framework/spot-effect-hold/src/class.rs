//! Effect classes and the per-class hold policy table.
//!
//! Every external side effect announces its [`EffectClass`]. The [`PolicyTable`] says, per class,
//! whether it is held during a migration, what happens on release and on discard, and how long a
//! hold may last before a release needs explicit confirmation. Unknown classes are **held** by
//! default: the failure mode of a missing registration must be "we held too much", never "we
//! let an external call out during a rolled-back migration".

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// One class of external side effect.
///
/// `JobQueue` and `Other` carry a name so apps and platform crates can register their own; the
/// name is part of the class key and the never-hold allow-list.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum EffectClass {
    /// An outbound email.
    Mail,
    /// A bank or payment-provider API call.
    BankApi,
    /// An outbound webhook delivery.
    Webhook,
    /// A third-party connector request.
    Connector,
    /// An SMS.
    Sms,
    /// A mobile push notification.
    Push,
    /// A job queue: held jobs are left unclaimed, not failed.
    JobQueue(String),
    /// An edge/branch sync message.
    EdgeSync,
    /// A fiscal transmission of an issued document. Never held (ADR-43).
    FiscalTransmission,
    /// A registered class the platform does not name.
    Other(String),
}

impl EffectClass {
    /// The class' stable key, used in the policy table and the never-hold allow-list.
    ///
    /// `mail`, `bank_api`, `webhook`, `connector`, `sms`, `push`, `job:<name>`, `edge_sync`,
    /// `fiscal_transmission` and `other:<name>`.
    #[must_use]
    pub fn key(&self) -> String {
        match self {
            EffectClass::Mail => "mail".to_owned(),
            EffectClass::BankApi => "bank_api".to_owned(),
            EffectClass::Webhook => "webhook".to_owned(),
            EffectClass::Connector => "connector".to_owned(),
            EffectClass::Sms => "sms".to_owned(),
            EffectClass::Push => "push".to_owned(),
            EffectClass::JobQueue(name) => format!("job:{name}"),
            EffectClass::EdgeSync => "edge_sync".to_owned(),
            EffectClass::FiscalTransmission => "fiscal_transmission".to_owned(),
            EffectClass::Other(name) => format!("other:{name}"),
        }
    }

    /// Whether this class is a fiscal transmission of an issued document.
    #[must_use]
    pub fn is_fiscal(&self) -> bool {
        matches!(self, EffectClass::FiscalTransmission)
    }
}

/// What release does with a held effect.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OnRelease {
    /// Re-dispatch it through the dispatcher, with its original idempotency key.
    Dispatch,
}

/// What discard does with a held effect.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OnDiscard {
    /// Drop it silently: the source document was rolled back with the database.
    Drop,
    /// Drop it and raise a task for a person to review (a bank payment instruction, say).
    DropAndNotify,
}

/// The policy for one class.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassPolicy {
    /// Whether the class is held while a migration is unverified. Default `true`; ignored when
    /// `never_hold` is set.
    pub hold: bool,
    /// Never held, whatever the migration state. Only fiscal transmissions may set this without
    /// a lead-approved allow-list entry.
    pub never_hold: bool,
    /// What release does.
    pub on_release: OnRelease,
    /// What discard does.
    pub on_discard: OnDiscard,
    /// A hold older than this raises an alert and, for classes where staleness is harmful,
    /// refuses an automatic release (the effect is listed in `needs_confirmation`).
    pub max_hold: Option<Duration>,
}

impl Default for ClassPolicy {
    fn default() -> Self {
        Self {
            hold: true,
            never_hold: false,
            on_release: OnRelease::Dispatch,
            on_discard: OnDiscard::Drop,
            max_hold: None,
        }
    }
}

impl ClassPolicy {
    /// The policy of a class that is never held (fiscal transmissions, and allow-listed classes).
    #[must_use]
    pub fn never_hold() -> Self {
        Self {
            hold: false,
            never_hold: true,
            on_release: OnRelease::Dispatch,
            on_discard: OnDiscard::Drop,
            max_hold: None,
        }
    }

    /// The same policy with a maximum hold age.
    #[must_use]
    pub fn with_max_hold(mut self, max_hold: Duration) -> Self {
        self.max_hold = Some(max_hold);
        self
    }

    /// The same policy that raises a task for a person on discard.
    #[must_use]
    pub fn notify_on_discard(mut self) -> Self {
        self.on_discard = OnDiscard::DropAndNotify;
        self
    }
}

/// The configuration one operator supplies for a site's policy table.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PolicyConfig {
    /// Per-class overrides, keyed by [`EffectClass::key`].
    #[serde(default)]
    pub overrides: BTreeMap<String, ClassPolicyConfig>,
    /// Class keys a lead has approved as `never_hold` besides fiscal transmissions.
    #[serde(default)]
    pub never_hold_allow: Vec<String>,
}

impl PolicyConfig {
    /// Parses a policy configuration from JSON (the form `spot-settings` stores).
    ///
    /// # Errors
    /// [`Error::InvalidInput`] when the text is not a policy configuration.
    pub fn from_json(text: &str) -> Result<Self> {
        serde_json::from_str(text).map_err(|error| Error::InvalidInput {
            what: "policy config".to_owned(),
            reason: error.to_string(),
        })
    }
}

/// One class's override, as configuration carries it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ClassPolicyConfig {
    /// `hold`; defaults to `true`.
    #[serde(default)]
    pub hold: Option<bool>,
    /// `never_hold`; defaults to `false`.
    #[serde(default)]
    pub never_hold: bool,
    /// Raise a task for a person on discard.
    #[serde(default)]
    pub notify_on_discard: bool,
    /// `max_hold` in whole seconds.
    #[serde(default)]
    pub max_hold_seconds: Option<u64>,
}

impl ClassPolicyConfig {
    fn into_policy(self, class_key: &str) -> Result<ClassPolicy> {
        let mut policy = ClassPolicy {
            hold: self.hold.unwrap_or(true),
            never_hold: self.never_hold,
            ..ClassPolicy::default()
        };
        if self.notify_on_discard {
            policy.on_discard = OnDiscard::DropAndNotify;
        }
        if let Some(seconds) = self.max_hold_seconds {
            if seconds == 0 {
                return Err(Error::InvalidInput {
                    what: format!("max_hold_seconds for {class_key}"),
                    reason: "must be greater than zero".to_owned(),
                });
            }
            policy.max_hold = Some(Duration::from_secs(seconds));
        }
        Ok(policy)
    }
}

/// The registered policy per class, with the never-hold allow-list.
#[derive(Clone, Debug)]
pub struct PolicyTable {
    policies: BTreeMap<String, ClassPolicy>,
    never_hold_allow: BTreeSet<String>,
}

impl Default for PolicyTable {
    fn default() -> Self {
        Self::new()
    }
}

impl PolicyTable {
    /// A table with the platform defaults: fiscal transmissions are never held; every other
    /// class is held, dispatched on release and dropped on discard.
    #[must_use]
    pub fn new() -> Self {
        let mut policies = BTreeMap::new();
        policies.insert(
            EffectClass::FiscalTransmission.key(),
            ClassPolicy::never_hold(),
        );
        Self {
            policies,
            never_hold_allow: BTreeSet::new(),
        }
    }

    /// Builds a table from configuration.
    ///
    /// Fiscal transmissions can never be configured holdable
    /// ([`Error::FiscalCannotBeHeld`]), and a non-fiscal class may only be `never_hold` when its
    /// key is in `never_hold_allow` ([`Error::NeverHoldNotAllowed`]).
    ///
    /// # Errors
    /// As above, or when a `max_hold_seconds` is zero.
    pub fn load(config: PolicyConfig) -> Result<Self> {
        let mut table = Self::new();
        table.never_hold_allow = config.never_hold_allow.iter().cloned().collect();
        for (key, override_config) in config.overrides {
            let policy = override_config.into_policy(&key)?;
            table.register_key(&key, policy)?;
        }
        Ok(table)
    }

    /// Registers a class' policy.
    ///
    /// # Errors
    /// [`Error::FiscalCannotBeHeld`] when fiscal transmissions are made holdable, and
    /// [`Error::NeverHoldNotAllowed`] when another class asks to be `never_hold` without an
    /// allow-list entry.
    pub fn register(&mut self, class: &EffectClass, policy: ClassPolicy) -> Result<()> {
        self.register_key(&class.key(), policy)
    }

    fn register_key(&mut self, class_key: &str, policy: ClassPolicy) -> Result<()> {
        let fiscal = class_key == EffectClass::FiscalTransmission.key();
        if fiscal && !policy.never_hold {
            return Err(Error::FiscalCannotBeHeld {
                class: class_key.to_owned(),
            });
        }
        if policy.never_hold && !fiscal && !self.never_hold_allow.contains(class_key) {
            return Err(Error::NeverHoldNotAllowed {
                class: class_key.to_owned(),
            });
        }
        self.policies.insert(class_key.to_owned(), policy);
        Ok(())
    }

    /// The policy for a class: the registered one, or the held-by-default policy for a class
    /// nobody registered.
    #[must_use]
    pub fn policy_for(&self, class: &EffectClass) -> ClassPolicy {
        self.policies.get(&class.key()).cloned().unwrap_or_default()
    }

    /// Whether the class is never held.
    #[must_use]
    pub fn is_never_hold(&self, class: &EffectClass) -> bool {
        self.policy_for(class).never_hold
    }

    /// The lead-approved never-hold allow-list.
    #[must_use]
    pub fn never_hold_allow(&self) -> &BTreeSet<String> {
        &self.never_hold_allow
    }
}
