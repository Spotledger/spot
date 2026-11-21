//! A serialisable snapshot of a promotion, so it survives between runs (WP-ALM-04.T6).
//!
//! [`Promotion`] itself carries no serde and its setters stay `pub(crate)`. A durable store keeps
//! a [`Snapshot`] instead: plain data with `deny_unknown_fields`, a schema [`SNAPSHOT_VERSION`]
//! and the store revision. [`Promotion::snapshot`] makes one; [`Promotion::restore`] is the only
//! way back and it re-checks everything it can:
//!
//! - an unknown schema version is refused before the rest is read
//!   ([`PromotionError::SnapshotVersion`]);
//! - the id, digests, states and roles must parse, the journal must end in the recorded state,
//!   the plan must still hash to the digest approval covered and must name the promotion's
//!   bundle, and the pre-image bytes must open to the digest recorded for them
//!   ([`PromotionError::SnapshotInvalid`]).
//!
//! What a snapshot does not hold: the typed changes of the bundle's apply plan (`ApplyPlan` has
//! no persisted form). It keeps the plan's hash and size instead, and the service re-derives the
//! plan against the target and refuses one whose hash differs before a rehearsal or a
//! verification needs it. No snapshot holds a secret: it names codes, item keys and digests, and
//! the pre-image is the target's own customisation bundle.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use spot_custom_bundle::{Bundle, Digest, Limits, SectionRegistry};
use spot_environments::{EnvRef, Role};
use spot_migration_approval::PlanDigest;
use spot_migration_audit::RunId;
use spot_migration_plan::{ReleasePlan, Risk};
use spot_rehearsal::RehearsalReport;
use spot_tenancy::TenantId;

use crate::error::PromotionError;
use crate::plan::{IssueSource, PlanIssue, PromotionPlan};
use crate::ports::{ApplyEvidence, FailureEvidence, RehearsalEvidence};
use crate::state::{Promotion, PromotionState, Transition};

/// The schema version of [`Snapshot`] this build writes and reads.
pub const SNAPSHOT_VERSION: u32 = 1;

/// An environment address.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvDoc {
    /// The environment set's name.
    pub set: String,
    /// The role's stable name (`dev`, `test`, `prod`).
    pub role: String,
    /// The tenant.
    pub tenant: TenantId,
}

/// One conflict or refusal folded into the plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueDoc {
    /// `merge` or `upgrade`.
    pub source: String,
    /// The stable conflict code.
    pub code: String,
    /// The item the conflict is filed under.
    pub item: String,
    /// One plain-language sentence.
    pub message: String,
    /// How risky the item is.
    pub risk: Risk,
    /// Whether the item stops apply.
    pub blocking: bool,
}

/// The persisted part of a [`PromotionPlan`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanDoc {
    /// The release plan a reviewer reads.
    pub release: ReleasePlan,
    /// The digest approval covers.
    pub digest: PlanDigest,
    /// The bundle digest, lower-case hex.
    pub bundle_digest: String,
    /// The bundle apply plan's deterministic hash.
    pub apply_hash: String,
    /// How many items the apply plan writes.
    pub apply_changes: u64,
    /// The conflicts folded into the plan.
    pub issues: Vec<IssueDoc>,
    /// The planner's compatibility warnings.
    pub warnings: Vec<String>,
}

/// The captured pre-image of the target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreImageDoc {
    /// The bundle's canonical digest, lower-case hex.
    pub digest: String,
    /// The packed archive, lower-case hex.
    pub bytes_hex: String,
}

/// One journaled transition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransitionDoc {
    /// The state left, or `None` for the first record.
    pub from: Option<String>,
    /// The state entered.
    pub to: String,
    /// Unix microseconds.
    pub at: i64,
    /// The identity that caused it.
    pub actor: String,
    /// One sentence for a human.
    pub note: String,
}

/// A promotion as a durable store keeps it. See the module docs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    /// The schema version ([`SNAPSHOT_VERSION`]).
    pub version: u32,
    /// The store revision the promotion was read at.
    pub revision: u64,
    /// The promotion id.
    pub id: String,
    /// The bundle digest, lower-case hex.
    pub bundle_digest: String,
    /// The environment the bundle comes from.
    pub from_env: EnvDoc,
    /// The environment it is promoted to.
    pub to_env: EnvDoc,
    /// Who requested it.
    pub requester: String,
    /// The state's stable name.
    pub state: String,
    /// The plan, once planned.
    pub plan: Option<PlanDoc>,
    /// The rehearsal report, once a rehearsal ran.
    pub rehearsal: Option<RehearsalReport>,
    /// The pre-image, once captured.
    pub pre_image: Option<PreImageDoc>,
    /// The apply evidence.
    pub apply: Option<ApplyEvidence>,
    /// The failure evidence.
    pub failure: Option<FailureEvidence>,
    /// Every transition, oldest first.
    pub journal: Vec<TransitionDoc>,
    /// How many execution attempts there have been.
    pub executions: u32,
}

impl Snapshot {
    /// The snapshot as a JSON value (what a `jsonb` column stores).
    ///
    /// # Errors
    /// [`PromotionError::SnapshotInvalid`] when serialisation fails.
    pub fn to_value(&self) -> Result<Value, PromotionError> {
        serde_json::to_value(self).map_err(|_| invalid("the snapshot could not be serialised"))
    }

    /// The snapshot as JSON text.
    ///
    /// # Errors
    /// [`PromotionError::SnapshotInvalid`] when serialisation fails.
    pub fn to_json(&self) -> Result<String, PromotionError> {
        serde_json::to_string(self).map_err(|_| invalid("the snapshot could not be serialised"))
    }

    /// Reads a snapshot from a JSON value. The version is checked first, so a snapshot of a
    /// newer schema is told apart from a corrupt one.
    ///
    /// # Errors
    /// [`PromotionError::SnapshotVersion`] for another schema version and
    /// [`PromotionError::SnapshotInvalid`] for anything that does not read as a snapshot.
    pub fn from_value(value: Value) -> Result<Self, PromotionError> {
        let found = value
            .get("version")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid("the snapshot names no schema version"))?;
        if found != u64::from(SNAPSHOT_VERSION) {
            return Err(PromotionError::SnapshotVersion {
                found: u32::try_from(found).unwrap_or(u32::MAX),
                supported: SNAPSHOT_VERSION,
            });
        }
        serde_json::from_value(value).map_err(|_| invalid("the snapshot is not a valid snapshot"))
    }

    /// Reads a snapshot from JSON text (see [`from_value`](Self::from_value)).
    ///
    /// # Errors
    /// As [`from_value`](Self::from_value).
    pub fn from_json(text: &str) -> Result<Self, PromotionError> {
        let value: Value =
            serde_json::from_str(text).map_err(|_| invalid("the snapshot is not valid JSON"))?;
        Self::from_value(value)
    }
}

fn invalid(message: &str) -> PromotionError {
    PromotionError::SnapshotInvalid {
        message: message.to_owned(),
    }
}

fn env_doc(env: &EnvRef) -> EnvDoc {
    EnvDoc {
        set: env.set.clone(),
        role: env.role.name().to_owned(),
        tenant: env.tenant,
    }
}

fn env_ref(doc: EnvDoc) -> Result<EnvRef, PromotionError> {
    let role = Role::ALL
        .into_iter()
        .find(|role| role.name() == doc.role)
        .ok_or_else(|| invalid("an environment names an unknown role"))?;
    Ok(EnvRef {
        set: doc.set,
        role,
        tenant: doc.tenant,
    })
}

fn digest_of(text: &str) -> Result<Digest, PromotionError> {
    text.parse::<Digest>()
        .map_err(|_| invalid("a digest is not 64 hex characters"))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len() / 2)
        .map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok())
        .collect()
}

fn state_of(name: &str) -> Result<PromotionState, PromotionError> {
    PromotionState::from_name(name).ok_or_else(|| invalid("a state is not one this build knows"))
}

fn plan_doc(plan: &PromotionPlan) -> PlanDoc {
    PlanDoc {
        release: plan.release.clone(),
        digest: plan.digest.clone(),
        bundle_digest: plan.bundle_digest.to_hex(),
        apply_hash: plan.apply_hash.clone(),
        apply_changes: u64::try_from(plan.apply_len).unwrap_or(u64::MAX),
        issues: plan
            .issues
            .iter()
            .map(|issue| IssueDoc {
                source: issue.source.as_str().to_owned(),
                code: issue.code.clone(),
                item: issue.item.clone(),
                message: issue.message.clone(),
                risk: issue.risk,
                blocking: issue.blocking,
            })
            .collect(),
        warnings: plan.warnings.clone(),
    }
}

fn plan_of(doc: PlanDoc) -> Result<PromotionPlan, PromotionError> {
    let issues = doc
        .issues
        .into_iter()
        .map(|issue| {
            let source = match issue.source.as_str() {
                "merge" => IssueSource::Merge,
                "upgrade" => IssueSource::Upgrade,
                _ => return Err(invalid("a plan item names an unknown source")),
            };
            Ok(PlanIssue {
                source,
                code: issue.code,
                item: issue.item,
                message: issue.message,
                risk: issue.risk,
                blocking: issue.blocking,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let plan = PromotionPlan {
        release: doc.release,
        digest: doc.digest,
        apply: None,
        apply_hash: doc.apply_hash,
        apply_len: usize::try_from(doc.apply_changes).unwrap_or(usize::MAX),
        bundle_digest: digest_of(&doc.bundle_digest)?,
        issues,
        warnings: doc.warnings,
    };
    // A plan edited after approval no longer hashes to its digest.
    plan.check_unchanged()
        .map_err(|_| invalid("the stored plan does not match the digest approval covered"))?;
    Ok(plan)
}

impl Promotion {
    /// The promotion as a [`Snapshot`], at the revision it was read at.
    ///
    /// # Errors
    /// [`PromotionError::SnapshotInvalid`] when the pre-image cannot be packed.
    pub fn snapshot(&self) -> Result<Snapshot, PromotionError> {
        let pre_image = self
            .pre_image
            .as_ref()
            .map(|bundle| {
                let bytes = bundle
                    .repack()
                    .map_err(|_| invalid("the pre-image could not be packed"))?;
                Ok::<_, PromotionError>(PreImageDoc {
                    digest: bundle.digest().to_hex(),
                    bytes_hex: to_hex(&bytes),
                })
            })
            .transpose()?;
        Ok(Snapshot {
            version: SNAPSHOT_VERSION,
            revision: self.revision,
            id: self.id.to_string(),
            bundle_digest: self.bundle_digest.to_hex(),
            from_env: env_doc(&self.from_env),
            to_env: env_doc(&self.to_env),
            requester: self.requester.clone(),
            state: self.state.as_str().to_owned(),
            plan: self.plan.as_ref().map(plan_doc),
            rehearsal: self.rehearsal.as_ref().map(|r| r.report.clone()),
            pre_image,
            apply: self.apply.clone(),
            failure: self.failure.clone(),
            journal: self
                .journal
                .iter()
                .map(|t| TransitionDoc {
                    from: t.from.map(|state| state.as_str().to_owned()),
                    to: t.to.as_str().to_owned(),
                    at: t.at,
                    actor: t.actor.clone(),
                    note: t.note.clone(),
                })
                .collect(),
            executions: self.executions,
        })
    }

    /// Rebuilds a promotion from a [`Snapshot`]: the restore constructor. `limits` and
    /// `registry` open the pre-image archive (use the ones the applier captured it with).
    ///
    /// The restored plan has no typed apply plan; see the module docs.
    ///
    /// # Errors
    /// [`PromotionError::SnapshotVersion`] and [`PromotionError::SnapshotInvalid`].
    pub fn restore(
        snapshot: Snapshot,
        limits: &Limits,
        registry: &SectionRegistry,
    ) -> Result<Promotion, PromotionError> {
        if snapshot.version != SNAPSHOT_VERSION {
            return Err(PromotionError::SnapshotVersion {
                found: snapshot.version,
                supported: SNAPSHOT_VERSION,
            });
        }
        let id =
            RunId::new(snapshot.id).map_err(|_| invalid("the promotion id is not a valid id"))?;
        let bundle_digest = digest_of(&snapshot.bundle_digest)?;
        let state = state_of(&snapshot.state)?;
        let journal = snapshot
            .journal
            .into_iter()
            .map(|t| -> Result<Transition, PromotionError> {
                Ok(Transition {
                    from: t.from.as_deref().map(state_of).transpose()?,
                    to: state_of(&t.to)?,
                    at: t.at,
                    actor: t.actor,
                    note: t.note,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if journal.last().map(|t| t.to) != Some(state) {
            return Err(invalid("the journal does not end in the recorded state"));
        }
        let plan = snapshot.plan.map(plan_of).transpose()?;
        if let Some(plan) = &plan
            && plan.bundle_digest != bundle_digest
        {
            return Err(invalid("the plan is for another bundle"));
        }
        if plan.is_none() && state != PromotionState::Requested {
            return Err(invalid("a promotion past its request has a plan"));
        }
        let pre_image = snapshot
            .pre_image
            .map(|doc| {
                let expected = digest_of(&doc.digest)?;
                let bytes = from_hex(&doc.bytes_hex)
                    .ok_or_else(|| invalid("the pre-image is not hexadecimal"))?;
                let bundle = Bundle::open(&bytes, limits, registry, true)
                    .map_err(|_| invalid("the pre-image does not open as a bundle"))?;
                if bundle.digest() != expected {
                    return Err(invalid("the pre-image does not match its digest"));
                }
                Ok(bundle)
            })
            .transpose()?;
        Ok(Promotion {
            id,
            bundle_digest,
            from_env: env_ref(snapshot.from_env)?,
            to_env: env_ref(snapshot.to_env)?,
            requester: snapshot.requester,
            state,
            plan,
            rehearsal: snapshot
                .rehearsal
                .map(|report| RehearsalEvidence { report }),
            pre_image,
            apply: snapshot.apply,
            failure: snapshot.failure,
            journal,
            executions: snapshot.executions,
            revision: snapshot.revision,
        })
    }
}
