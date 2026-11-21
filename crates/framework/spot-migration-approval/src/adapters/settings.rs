//! [`SettingsPolicySource`]: the approval policy kept in `spot-settings` under
//! [`SETTING_KEY`](crate::SETTING_KEY).

use std::sync::Arc;

use spot_settings::{ParamValue, ScopeRef, StoredValue, ValueStore};

use crate::error::ApprovalError;
use crate::policy::{ApprovalPolicy, SETTING_KEY};

/// The strictest valid policy (see [`ApprovalPolicy::strictest`]). Used whenever the stored
/// policy is absent, unreadable or invalid.
#[must_use]
pub fn strictest_policy() -> ApprovalPolicy {
    ApprovalPolicy::strictest()
}

/// Where a loaded policy came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PolicySource {
    /// A stored value that parsed and validated.
    Stored,
    /// Nothing is stored: the strictest policy (a site that wants looser rules sets one).
    Unset,
    /// The stored value could not be read or failed validation: the strictest policy.
    Strictest {
        /// Why, in a sentence (never the stored text).
        reason: String,
    },
}

/// Reads and writes the approval policy through a `spot-settings` [`ValueStore`].
pub struct SettingsPolicySource {
    store: Arc<dyn ValueStore>,
    scope: ScopeRef,
}

impl SettingsPolicySource {
    /// A source over `store`, reading and writing at `scope` (usually the tenant or fleet).
    #[must_use]
    pub fn new(store: Arc<dyn ValueStore>, scope: ScopeRef) -> Self {
        Self { store, scope }
    }

    /// The effective policy and where it came from. Never fails and never loosens: an absent
    /// value, one that is unreadable (a secret, not text) and one that is invalid (does not
    /// parse, fails [`ApprovalPolicy::validate`]) each give [`strictest_policy`].
    #[must_use]
    pub fn load(&self) -> (ApprovalPolicy, PolicySource) {
        let strictest = |reason: &str| {
            (
                strictest_policy(),
                PolicySource::Strictest {
                    reason: reason.to_owned(),
                },
            )
        };
        match self.store.get(&self.scope, SETTING_KEY) {
            None => (strictest_policy(), PolicySource::Unset),
            Some(StoredValue::Value(ParamValue::Text(text))) => {
                match ApprovalPolicy::from_setting(&text) {
                    Ok(policy) => (policy, PolicySource::Stored),
                    Err(_) => strictest("the stored policy is not valid"),
                }
            }
            Some(_) => strictest("the stored policy is not a text value"),
        }
    }

    /// The effective policy (see [`Self::load`]).
    #[must_use]
    pub fn policy(&self) -> ApprovalPolicy {
        self.load().0
    }

    /// Validates and stores `policy`.
    ///
    /// # Errors
    /// [`ApprovalError::BadPolicy`] when it does not validate (nothing is written).
    pub fn store(&self, policy: &ApprovalPolicy) -> Result<(), ApprovalError> {
        policy.validate()?;
        let text = policy.to_setting();
        if text.is_empty() {
            return Err(ApprovalError::BadPolicy {
                message: "the policy could not be encoded".to_owned(),
            });
        }
        self.store.put(
            &self.scope,
            SETTING_KEY,
            StoredValue::Value(ParamValue::Text(text)),
        );
        Ok(())
    }
}
