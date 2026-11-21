//! The policy of one site: the mode of each record type, the entry processes and the scopes a
//! record may name.
//!
//! The settings of `[services.process_governance]` seed the effective-dated, scope-narrowing
//! store of `spot-bpm-entry` ([`ScopedPolicyStore`]), so the governed guard and the
//! `process_only` guard read one source and cannot disagree about a record type's mode. A
//! record that names no company (or one with no entry of its own) falls back to the entry set
//! on the site itself.

use std::collections::BTreeSet;
use std::sync::Arc;

use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_entry::{
    EntryProcesses, MemoryEntrySettings, ScopeResolver, ScopedPolicyStore, scope_chain,
};
use spot_bpm_governance::{
    GovernanceError, GovernanceMode, GovernancePolicy, PolicyChange, PolicyStore,
};

use super::settings::Settings;

/// The mode source of one site: the settings' entries, with the site as the root scope.
pub struct SitePolicy {
    inner: ScopedPolicyStore,
    site: String,
}

impl SitePolicy {
    /// The policy `settings` describe for `site`.
    #[must_use]
    pub fn from_settings(settings: &Settings, site: &str) -> Self {
        let inner = ScopedPolicyStore::new(settings.reason_min_chars);
        for entry in &settings.governed {
            if let Some(mode) = GovernanceMode::parse(&entry.mode) {
                let scope = entry.scope.as_deref().unwrap_or(site);
                inner.seed(scope, &entry.doctype, mode);
            }
        }
        Self {
            inner,
            site: site.to_owned(),
        }
    }

    /// The policy as the shared trait object the guards take.
    #[must_use]
    pub fn shared(self) -> Arc<dyn PolicyStore> {
        Arc::new(self)
    }
}

impl PolicyStore for SitePolicy {
    fn mode_at(
        &self,
        company: &str,
        doctype: &str,
        at: OffsetDateTime,
    ) -> Result<GovernanceMode, GovernanceError> {
        let own = self.inner.mode_at(company, doctype, at)?;
        if own != GovernanceMode::Free || company == self.site {
            return Ok(own);
        }
        // No explicit entry on the company's own path: the site-wide entry applies.
        self.inner.mode_at(&self.site, doctype, at)
    }

    fn min_reason_chars(&self, company: &str) -> Result<usize, GovernanceError> {
        self.inner.min_reason_chars(company)
    }

    fn current(&self, company: &str, doctype: &str) -> Result<GovernancePolicy, GovernanceError> {
        self.inner.current(company, doctype)
    }

    fn changes(&self, company: &str, doctype: &str) -> Result<Vec<PolicyChange>, GovernanceError> {
        self.inner.changes(company, doctype)
    }

    fn record_change(&self, change: PolicyChange) -> Result<(), GovernanceError> {
        self.inner.record_change(change)
    }
}

/// The entry processes the settings name; a record type with none gets the generated process.
#[must_use]
pub fn entry_processes(settings: &Settings, site: &str) -> Arc<EntryProcesses> {
    let source = MemoryEntrySettings::new();
    for entry in &settings.entry {
        let scope = entry.scope.as_deref().unwrap_or(site);
        source.set_entry(&entry.doctype, scope, entry.process.trim());
    }
    Arc::new(EntryProcesses::new(source.shared()))
}

/// Resolves the scope a create is judged in: the site itself, or a company the settings list
/// (or a unit below a listed one). A claim anywhere else resolves to nothing, so a caller cannot
/// pick the scope whose mode suits them by naming it in a field.
pub struct SiteScopes {
    site: String,
    companies: BTreeSet<String>,
}

impl SiteScopes {
    /// The resolver of `site` over the settings' company list.
    #[must_use]
    pub fn new(site: &str, companies: &[String]) -> Self {
        Self {
            site: site.to_owned(),
            companies: companies.iter().cloned().collect(),
        }
    }
}

impl ScopeResolver for SiteScopes {
    fn resolve(&self, _actor: &str, claimed: Option<&str>) -> Option<String> {
        let Some(claim) = claimed.map(str::trim).filter(|text| !text.is_empty()) else {
            return Some(self.site.clone());
        };
        if claim == self.site {
            return Some(self.site.clone());
        }
        scope_chain(claim)
            .iter()
            .any(|unit| self.companies.contains(*unit))
            .then(|| claim.to_owned())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::services::process_governance::settings::{EntryProcessEntry, GovernedEntry};

    fn settings() -> Settings {
        let mut s = Settings::default();
        s.governed.push(GovernedEntry {
            doctype: "invoice".to_owned(),
            scope: None,
            mode: "governed".to_owned(),
        });
        s.governed.push(GovernedEntry {
            doctype: "item".to_owned(),
            scope: Some("acme".to_owned()),
            mode: "process_only".to_owned(),
        });
        s.entry.push(EntryProcessEntry {
            doctype: "item".to_owned(),
            scope: Some("acme".to_owned()),
            process: "new_item".to_owned(),
        });
        s.companies = vec!["acme".to_owned()];
        s
    }

    #[test]
    fn a_site_wide_entry_covers_companies_and_a_scoped_one_only_its_units() {
        let policy = SitePolicy::from_settings(&settings(), "main");
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(1);
        assert_eq!(
            policy.mode_at("main", "invoice", now).unwrap(),
            GovernanceMode::Governed
        );
        assert_eq!(
            policy.mode_at("acme/north", "invoice", now).unwrap(),
            GovernanceMode::Governed
        );
        assert_eq!(
            policy.mode_at("acme/north", "item", now).unwrap(),
            GovernanceMode::ProcessOnly
        );
        assert_eq!(
            policy.mode_at("main", "item", now).unwrap(),
            GovernanceMode::Free
        );
        assert_eq!(
            policy.mode_at("main", "other", now).unwrap(),
            GovernanceMode::Free
        );
    }

    #[test]
    fn a_claim_outside_the_listed_scopes_resolves_to_nothing() {
        let scopes = SiteScopes::new("main", &["acme".to_owned()]);
        assert_eq!(scopes.resolve("user:1", None).as_deref(), Some("main"));
        assert_eq!(scopes.resolve("user:1", Some(" ")).as_deref(), Some("main"));
        assert_eq!(
            scopes.resolve("user:1", Some("acme/north")).as_deref(),
            Some("acme/north")
        );
        assert_eq!(scopes.resolve("user:1", Some("other")), None);
    }

    #[test]
    fn entry_processes_use_the_setting_then_the_generated_default() {
        let processes = entry_processes(&settings(), "main");
        let configured = processes.entry_process("item", "acme/north").unwrap();
        assert_eq!(configured.id, "new_item");
        assert!(!configured.generated);
        let generated = processes.entry_process("invoice", "main").unwrap();
        assert!(generated.generated);
    }
}
