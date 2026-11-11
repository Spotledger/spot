//! Pinned process versions: publishing a new version never touches a running Case.
//!
//! A Case is pinned when it starts: [`CaseRecord::run`](spot_bpm_engine::CaseRecord) carries the
//! process name, the version number and the compiled content hash. The engine re-checks that pin
//! on every step (`Engine::process_for`), so a Case on version 1 keeps running version 1 after
//! version 2 is published for the same process.
//!
//! This module makes that guarantee visible to a host:
//!
//! - [`pinned`] resolves the definition the registry holds and proves its hash still matches the
//!   Case's pin;
//! - [`publish`] publishes a version through the engine and reports which running Cases remain
//!   on older pins. It never writes a Case; the list is evidence that publishing is not a
//!   migration.

use serde::{Deserialize, Serialize};
use spot_app_api::prelude::SiteId;
use spot_bpm_documents::{ProcessAuthor, PublishGuard, validate_process_version};
use spot_bpm_engine::{CaseRecord, CaseStore, Engine, EngineError, ProcessDefinition, TimerPort};
use spot_bpm_model::{NodeKind, ProcessVersion};

use crate::error::{RefusedNode, Result, VersioningError};

/// The stable identity of one published process version.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct VersionRef {
    /// The process name.
    pub name: String,
    /// The version number.
    pub version: u32,
    /// The compiled content hash as 64 lowercase hex characters.
    pub hash: String,
}

impl VersionRef {
    /// The pin a Case record carries, without consulting the registry.
    #[must_use]
    pub fn of(record: &CaseRecord) -> Self {
        Self {
            name: record.run.dag_key.clone(),
            version: record.run.dag_version,
            hash: record.run.content_hash.clone(),
        }
    }
}

impl core::fmt::Display for VersionRef {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} v{} ({})", self.name, self.version, self.hash)
    }
}

/// One running Case and the process version it pins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedCase {
    /// The Case.
    pub case: spot_bpm_engine::CaseId,
    /// The version it pins.
    pub pin: VersionRef,
}

/// What publishing a process version did to the running Cases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishReport {
    /// The version that was published.
    pub published: VersionRef,
    /// Every running Case passed in and the pin it kept.
    pub running: Vec<PinnedCase>,
    /// Running Cases that pin a different version or hash from the one just published. They
    /// stay there: this list is informational, so a host can see the population a migration
    /// would have to cover.
    pub on_other_versions: Vec<spot_bpm_engine::CaseId>,
}

/// Resolves the definition a Case pins and proves its content hash still matches.
///
/// # Errors
/// [`VersioningError::ProcessMismatch`] when the registry does not hold the pinned version or
/// holds a definition whose compiled hash differs from the Case's pin.
pub fn pinned<S: CaseStore + 'static, T: TimerPort + 'static>(
    engine: &Engine<S, T>,
    record: &CaseRecord,
) -> Result<VersionRef> {
    let key = record.run.dag_key.clone();
    let version = record.run.dag_version;
    let definition =
        engine
            .processes()
            .get(&key, version)
            .ok_or_else(|| VersioningError::ProcessMismatch {
                key: key.clone(),
                version,
            })?;
    let hash = definition.compiled.dag.hash().to_string();
    if hash != record.run.content_hash {
        return Err(VersioningError::ProcessMismatch { key, version });
    }
    Ok(VersionRef {
        name: key,
        version,
        hash,
    })
}

/// The inputs of the publish guard: who may act as which service principal, for which tenant and
/// on behalf of which author.
#[derive(Clone, Copy)]
pub struct PublishCheck<'a> {
    /// The guard (registry and permission lookup) the compiled version is run through.
    pub guard: &'a PublishGuard,
    /// The tenant the version is published for.
    pub tenant: &'a SiteId,
    /// Who authored the version.
    pub author: &'a ProcessAuthor,
}

/// Publishes a process version through the publish guard and reports the pins the running Cases
/// kept.
///
/// The version is compiled once (without registering it) and the guard
/// ([`validate_process_version`]) runs over the compiled DAG. Only when it finds no violation is
/// the compiled definition registered, so a refused version is never partially published. A guard
/// that cannot answer is a violation, never a pass.
///
/// Publishing is additive: the function never writes a Case record and never re-points one at
/// the new version. A host that wants running Cases on the new version calls
/// [`crate::migration::plan_migration`] for each of them.
///
/// # Errors
/// [`VersioningError::Engine`] when the model does not compile or the registry refuses the
/// version; [`VersioningError::PublishRefused`] with every violation (node and rule only) when
/// the guard refuses it, in which case nothing is registered.
pub fn publish_guarded<S: CaseStore + 'static, T: TimerPort + 'static>(
    engine: &Engine<S, T>,
    version: &ProcessVersion,
    running: &[CaseRecord],
    check: &PublishCheck<'_>,
) -> Result<PublishReport> {
    let definition =
        ProcessDefinition::compile(version).map_err(|problems| EngineError::ProcessInvalid {
            detail: problems
                .iter()
                .map(|problem| problem.message.clone())
                .collect::<Vec<_>>()
                .join("; "),
        })?;
    let violations = validate_process_version(
        check.guard,
        check.tenant,
        check.author,
        &definition.compiled.dag,
    );
    if !violations.is_empty() {
        return Err(VersioningError::PublishRefused {
            violations: violations
                .iter()
                .map(|violation| RefusedNode {
                    node: violation.node.as_str().to_owned(),
                    rule: violation.code.to_owned(),
                })
                .collect(),
        });
    }
    let registered = engine.processes().publish(definition);
    Ok(report(&registered, running))
}

/// Publishes a process version without the publish guard and reports the pins the running Cases
/// kept.
///
/// A Document node with a `principal` is **not checked** by this entry point, so a version that
/// carries one is refused with [`VersioningError::GuardRequired`]: the unguarded entry point
/// cannot be used to bypass the guard. Use [`publish_guarded`].
///
/// # Errors
/// [`VersioningError::GuardRequired`] when a Document node names a `principal`;
/// [`VersioningError::Engine`] when the model does not compile or the registry refuses the
/// version.
#[deprecated(note = "use publish_guarded")]
pub fn publish<S: CaseStore + 'static, T: TimerPort + 'static>(
    engine: &Engine<S, T>,
    version: &ProcessVersion,
    running: &[CaseRecord],
) -> Result<PublishReport> {
    let guarded_node = version.nodes.iter().find(
        |node| matches!(&node.kind, NodeKind::Document(config) if config.principal.is_some()),
    );
    if let Some(node) = guarded_node {
        return Err(VersioningError::GuardRequired {
            node: node.id.as_str().to_owned(),
        });
    }
    let definition = engine.publish(version)?;
    Ok(report(&definition, running))
}

fn report(definition: &ProcessDefinition, running: &[CaseRecord]) -> PublishReport {
    let published = VersionRef {
        name: definition.name().to_owned(),
        version: definition.number(),
        hash: definition.compiled.dag.hash().to_string(),
    };
    let mut pins = Vec::with_capacity(running.len());
    let mut on_other_versions = Vec::new();
    for record in running {
        let pin = VersionRef::of(record);
        if pin != published {
            on_other_versions.push(record.run.id.clone());
        }
        pins.push(PinnedCase {
            case: record.run.id.clone(),
            pin,
        });
    }
    PublishReport {
        published,
        running: pins,
        on_other_versions,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    use spot_bpm_engine::CaseId;
    use spot_bpm_model::from_text;

    use super::*;

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(value) => return value,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    #[test]
    fn a_version_ref_renders() {
        let pin = VersionRef {
            name: "order".to_owned(),
            version: 3,
            hash: "ab".to_owned(),
        };
        assert_eq!(pin.to_string(), "order v3 (ab)");
    }

    #[test]
    fn a_case_pin_round_trips_through_json() {
        let text = r#"
name = "pin_test"
app = "versioning_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"
terminal = true
"#;
        let version = from_text(text).unwrap();
        let harness = spot_bpm_engine::Harness::default();
        let case = CaseId::new("case_pin").unwrap();
        let record = block_on(harness.start(&version, case, "pin-key")).unwrap();
        let pin = VersionRef::of(&record);
        let json = serde_json::to_string(&pin).unwrap();
        assert_eq!(serde_json::from_str::<VersionRef>(&json).unwrap(), pin);
    }
}
