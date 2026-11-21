//! Seam integration: a composition contributes its controllers, step overrides and seam
//! providers to `spot-seams`' [`RegistryBuilder`], so an extension's overrides go through the
//! same install-time conflict detection and produce the same `explain` output as any other app.
//!
//! Composition never invents a second conflict checker: it hands the contributions to the one
//! registry, and two extensions that both `Wrap` the same step are refused there with
//! `override.duplicate_wrap`, exactly as they would be for a native app.

use std::sync::Arc;

use spot_app_api::seams::SeamProvider;
use spot_app_api::{Controller, StepOverride};
use spot_seams::{Conflict, Registry, RegistryBuilder};

/// A seam an app needs at a major version: the composition's view of `RegistryBuilder::require_seam`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeamNeed {
    /// The seam name.
    pub name: String,
    /// The major version required.
    pub major: u32,
}

impl SeamNeed {
    /// A requirement on `name` at major `major`.
    #[must_use]
    pub fn new(name: &str, major: u32) -> Self {
        Self {
            name: name.to_owned(),
            major,
        }
    }
}

/// A seam an app provides at a major version.
#[derive(Clone)]
pub struct SeamProviderDecl {
    /// The seam name.
    pub name: String,
    /// The major version provided.
    pub major: u32,
    /// The provider.
    pub provider: Arc<dyn SeamProvider>,
}

impl SeamProviderDecl {
    /// A provider of `name` at major `major`.
    #[must_use]
    pub fn new(name: &str, major: u32, provider: Arc<dyn SeamProvider>) -> Self {
        Self {
            name: name.to_owned(),
            major,
            provider,
        }
    }
}

/// Everything one app contributes to the step and seam registry.
pub struct StepContribution {
    /// The app.
    pub app: String,
    /// The app's controller: its pipeline steps and whitelisted methods.
    pub controller: Arc<dyn Controller>,
    /// The overrides it declares (ADR-08).
    pub overrides: Vec<StepOverride>,
    /// The seams it provides.
    pub provides: Vec<SeamProviderDecl>,
    /// The seams it needs.
    pub requires: Vec<SeamNeed>,
}

impl StepContribution {
    /// A contribution of `app`'s controller, with no overrides or seams.
    #[must_use]
    pub fn new(app: &str, controller: Arc<dyn Controller>) -> Self {
        Self {
            app: app.to_owned(),
            controller,
            overrides: Vec::new(),
            provides: Vec::new(),
            requires: Vec::new(),
        }
    }

    /// Adds an override.
    #[must_use]
    pub fn override_step(mut self, over: StepOverride) -> Self {
        self.overrides.push(over);
        self
    }

    /// Adds a provided seam.
    #[must_use]
    pub fn provide(mut self, provider: SeamProviderDecl) -> Self {
        self.provides.push(provider);
        self
    }

    /// Adds a required seam.
    #[must_use]
    pub fn require(mut self, need: SeamNeed) -> Self {
        self.requires.push(need);
        self
    }
}

/// The effect classes a platform host knows: the one list in `spot-seams`.
pub use spot_seams::PLATFORM_EFFECT_CLASSES;

/// Builds the step and seam registry from every app's contribution.
///
/// Registration order never changes the result: [`RegistryBuilder`] sorts everything, so the
/// plans, the `explain` output and the conflicts are the same whatever order the contributions
/// arrive in.
///
/// # Errors
///
/// Every [`Conflict`] found, sorted and deduplicated — a duplicate step, an override of an
/// unknown step, a cycle between overrides, two extensions that both wrap or replace one step, a
/// missing seam, an effect class outside [`PLATFORM_EFFECT_CLASSES`] (`effects.unknown_class`),
/// and the rest of `spot-seams`' install-time checks.
pub fn build_registry(contributions: &[StepContribution]) -> Result<Registry, Vec<Conflict>> {
    let classes = spot_seams::platform_effect_classes();
    let mut builder = RegistryBuilder::for_platform();
    builder.known_effect_classes(&classes);
    for contribution in contributions {
        builder.add_controller(&contribution.app, Arc::clone(&contribution.controller));
        for over in &contribution.overrides {
            builder.add_override(&contribution.app, over.clone());
        }
        for provided in &contribution.provides {
            builder.add_seam_provider(
                &contribution.app,
                &provided.name,
                provided.major,
                Arc::clone(&provided.provider),
            );
        }
        for need in &contribution.requires {
            builder.require_seam(&contribution.app, &need.name, need.major);
        }
    }
    builder.build()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use spot_app_api::{AppResult, BoxFuture, HookContext, Phase, StepName, Target};
    use std::time::Duration;

    struct OneStep {
        doctype: String,
        step: StepName,
    }

    impl Controller for OneStep {
        fn doctype(&self) -> &str {
            &self.doctype
        }

        fn steps(&self) -> Vec<spot_app_api::StepDecl> {
            vec![spot_app_api::StepDecl {
                name: self.step.clone(),
                point: spot_app_api::HookPoint::of(Phase::Validate, Target::Save),
                priority: 0,
                budget: Duration::from_millis(10),
            }]
        }

        fn run_step<'a>(
            &'a self,
            _step: &'a StepName,
            _ctx: &'a mut dyn HookContext,
        ) -> BoxFuture<'a, AppResult<()>> {
            Box::pin(async move { Ok(()) })
        }
    }

    fn controller(app: &str, step: &str) -> Arc<dyn Controller> {
        Arc::new(OneStep {
            doctype: "Generic Doc".to_owned(),
            step: StepName::new(&format!("{app}:{step}")).unwrap(),
        })
    }

    #[test]
    fn two_extensions_wrapping_one_step_conflict() {
        let host = StepContribution::new("host", controller("host", "adjust"));
        let first = StepContribution::new("ext_a", controller("ext_a", "wrap")).override_step(
            StepOverride {
                target: StepName::new("host:adjust").unwrap(),
                kind: spot_app_api::OverrideKind::Wrap,
                by: Some(StepName::new("ext_a:wrap").unwrap()),
                reason: "first".to_owned(),
            },
        );
        let second = StepContribution::new("ext_b", controller("ext_b", "wrap")).override_step(
            StepOverride {
                target: StepName::new("host:adjust").unwrap(),
                kind: spot_app_api::OverrideKind::Wrap,
                by: Some(StepName::new("ext_b:wrap").unwrap()),
                reason: "second".to_owned(),
            },
        );
        let conflicts = match build_registry(&[host, first, second]) {
            Ok(_) => panic!("two wrappers of one step must conflict"),
            Err(conflicts) => conflicts,
        };
        assert!(
            conflicts
                .iter()
                .any(|c| c.code == "override.duplicate_wrap")
        );
    }
}
