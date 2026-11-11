//! The report registry: reports registered per app, listed for a principal and looked up by
//! name.
//!
//! Listing is about *existence*, never data: a report whose `roles` do not admit the caller is
//! neither listed nor found. The data a caller may see in an admitted report is still decided
//! per run by the permission filter.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spot_permissions::Principal;

use crate::compile::Compiler;
use crate::compiled::CompiledReport;
use crate::definition::ReportDef;
use crate::error::{ReportError, codes};

/// One registered report: the app that shipped it, its definition and its compiled form.
#[derive(Clone, Debug)]
pub struct RegisteredReport {
    /// The app that declares it.
    pub app: String,
    /// The definition as loaded.
    pub def: Arc<ReportDef>,
    /// The compiled form at the metadata version it was registered at.
    pub compiled: Arc<CompiledReport>,
}

/// The reports of every installed app, keyed by name.
#[derive(Clone, Debug, Default)]
pub struct ReportRegistry {
    reports: BTreeMap<String, RegisteredReport>,
}

impl ReportRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of registered reports.
    #[must_use]
    pub fn len(&self) -> usize {
        self.reports.len()
    }

    /// Whether no report is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.reports.is_empty()
    }

    /// Registers a report of `app`, compiling it. A name already registered by another app is
    /// refused with both app names.
    ///
    /// # Errors
    ///
    /// [`codes::DUPLICATE`] when the name is taken, or every [`ReportError`] the compiler
    /// reports.
    pub fn register(
        &mut self,
        app: &str,
        def: Arc<ReportDef>,
        compiler: &Compiler<'_>,
    ) -> Result<(), Vec<ReportError>> {
        if let Some(existing) = self.reports.get(&def.name) {
            return Err(vec![ReportError::at(
                codes::DUPLICATE,
                &def.name,
                format!(
                    "the report `{}` is already declared by `{}`, so `{app}` cannot declare it",
                    def.name, existing.app
                ),
            )]);
        }
        let compiled = Arc::new(compiler.compile(&def)?);
        self.reports.insert(
            def.name.clone(),
            RegisteredReport {
                app: app.to_owned(),
                def,
                compiled,
            },
        );
        Ok(())
    }

    /// The report called `name`, whatever its roles.
    #[must_use]
    pub fn lookup(&self, name: &str) -> Option<&RegisteredReport> {
        self.reports.get(name)
    }

    /// The report called `name` that admits `principal`: a name that exists but whose roles do
    /// not admit the caller answers exactly as a name that does not exist.
    #[must_use]
    pub fn lookup_for(&self, name: &str, principal: &Principal) -> Option<&RegisteredReport> {
        let report = self.reports.get(name)?;
        report
            .compiled
            .admits(principal.is_system, &roles_of(principal))
            .then_some(report)
    }

    /// Every report whose roles admit `principal`, ordered by label.
    #[must_use]
    pub fn list_for(&self, principal: &Principal) -> Vec<&RegisteredReport> {
        let roles = roles_of(principal);
        let mut out: Vec<&RegisteredReport> = self
            .reports
            .values()
            .filter(|r| r.compiled.admits(principal.is_system, &roles))
            .collect();
        out.sort_by(|a, b| {
            a.compiled
                .label
                .cmp(&b.compiled.label)
                .then_with(|| a.compiled.name.cmp(&b.compiled.name))
        });
        out
    }
}

/// The role names a principal holds across all its assignments.
#[must_use]
pub fn roles_of(principal: &Principal) -> BTreeSet<String> {
    principal
        .assignments
        .iter()
        .flat_map(|a| a.roles.iter().map(|r| r.0.clone()))
        .collect()
}
