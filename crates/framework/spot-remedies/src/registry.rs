//! The registry: the catalogue of remedies, its tier gating, the mandatory dry-run and the undo
//! log.
//!
//! A remedy runs only through the registry, and the registry runs it only when
//! [`spot_agent_tiers::Authoriser`] allows the agent's action under the hard floor. Apply takes
//! the [`Plan`] that dry-run produced and refuses anything else; every apply stores an
//! [`UndoRecord`] and undo refuses when the window has closed or a later change conflicts.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value as Json;
use spot_agent_tiers::{ActionCatalogue, AgentIdentity, Authoriser, Decision, Policy};

use crate::audit::AuditKind;
use crate::ctx::{Gate, RemedyCtx, RemedySettings};
use crate::error::RemedyError;
use crate::model::{Applied, Plan, RemedyId, RemedyMeta, UndoRecord, UndoToken, Undone};
use crate::remedy::Remedy;
use crate::seeds;

/// The undo records a registry has stored, keyed by token.
#[derive(Default)]
struct UndoLog {
    records: BTreeMap<String, UndoRecord>,
    /// The hashes of plans this registry's dry-run issued and no apply has used yet. The hash
    /// is an integrity check, not a secret, so `apply` accepts only a plan the registry itself
    /// issued: a caller cannot seal a plan of its own and skip the dry-run.
    issued: std::collections::BTreeSet<String>,
}

/// The catalogue of remedies and everything needed to run one safely.
pub struct Registry {
    remedies: BTreeMap<RemedyId, Box<dyn Remedy>>,
    order: Vec<RemedyId>,
    authoriser: Authoriser,
    settings: RemedySettings,
    undo: Mutex<UndoLog>,
    seq: AtomicU64,
}

impl Registry {
    /// Builds a registry from a set of remedies, an authoriser policy and the settings.
    ///
    /// # Errors
    ///
    /// [`RemedyError::InvalidId`] for a malformed id; [`RemedyError::Duplicate`] for two
    /// remedies with one id; [`RemedyError::MetaInvalid`] when a remedy's metadata breaks a rule
    /// (an empty title, a required parameter that is not a property, or an irreversible undo
    /// above T3); [`RemedyError::CatalogueInvalid`] when the action catalogue does not load.
    pub fn new(
        remedies: Vec<Box<dyn Remedy>>,
        policy: Policy,
        settings: RemedySettings,
    ) -> Result<Self, RemedyError> {
        let mut map: BTreeMap<RemedyId, Box<dyn Remedy>> = BTreeMap::new();
        let mut order = Vec::new();
        for remedy in remedies {
            let id = remedy.id();
            if !RemedyId::is_valid(id.as_str()) {
                return Err(RemedyError::InvalidId {
                    id: id.as_str().to_owned(),
                });
            }
            let meta = remedy.meta();
            validate_meta(&id, &meta)?;
            if map.contains_key(&id) {
                return Err(RemedyError::Duplicate {
                    remedy: id.as_str().to_owned(),
                });
            }
            order.push(id.clone());
            map.insert(id, remedy);
        }
        order.sort();
        let catalogue = action_catalogue(&order, &map, settings.domain)?;
        Ok(Self {
            remedies: map,
            order,
            authoriser: Authoriser::with_policy(catalogue, policy),
            settings,
            undo: Mutex::new(UndoLog::default()),
            seq: AtomicU64::new(1),
        })
    }

    /// The registry with every seed remedy, under the default policy.
    ///
    /// # Errors
    ///
    /// A [`RemedyError`] when a seed breaks a registry rule.
    pub fn with_seeds(settings: RemedySettings) -> Result<Self, RemedyError> {
        Self::new(seeds::all(), Policy::default(), settings)
    }

    /// The registry with every seed remedy and default settings.
    ///
    /// # Errors
    ///
    /// A [`RemedyError`] when a seed breaks a registry rule.
    pub fn seeds() -> Result<Self, RemedyError> {
        Self::with_seeds(RemedySettings::default())
    }

    /// The settings.
    #[must_use]
    pub fn settings(&self) -> &RemedySettings {
        &self.settings
    }

    /// The remedy ids, in order.
    #[must_use]
    pub fn ids(&self) -> &[RemedyId] {
        &self.order
    }

    /// How many remedies are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// One remedy by id.
    #[must_use]
    pub fn get(&self, id: &RemedyId) -> Option<&dyn Remedy> {
        self.remedies.get(id).map(AsRef::as_ref)
    }

    /// One remedy's metadata by id.
    #[must_use]
    pub fn meta(&self, id: &RemedyId) -> Option<RemedyMeta> {
        self.get(id).map(Remedy::meta)
    }

    /// The undo record for a token, if it is still open.
    #[must_use]
    pub fn undo_record(&self, token: &UndoToken) -> Option<UndoRecord> {
        self.lock().records.get(token.as_str()).cloned()
    }

    /// Authorises a remedy for an agent under a gate, recording the decision.
    ///
    /// # Errors
    ///
    /// [`RemedyError::Unknown`] when no remedy has the id; [`RemedyError::Refused`] when the
    /// tier authoriser refuses the action, carrying the authoriser's own code and sentence.
    pub fn authorise(
        &self,
        agent: &AgentIdentity,
        id: &RemedyId,
        gate: &Gate,
        ctx: &mut RemedyCtx<'_>,
    ) -> Result<(), RemedyError> {
        if self.get(id).is_none() {
            return Err(RemedyError::Unknown {
                remedy: id.as_str().to_owned(),
            });
        }
        let decision = self
            .authoriser
            .decide(agent, id.as_str(), &gate.tier_context());
        match decision {
            Decision::Allow { .. } => {
                ctx.record(AuditKind::Authorised, id, "authorised");
                Ok(())
            }
            Decision::Refuse { reason } => {
                let code = reason.code().to_owned();
                let detail = reason.sentence();
                ctx.record(AuditKind::Refused, id, &detail);
                Err(RemedyError::Refused {
                    remedy: id.as_str().to_owned(),
                    code,
                    detail,
                })
            }
        }
    }

    /// Plans a remedy: authorise, validate the parameters, check the precondition and dry-run.
    ///
    /// # Errors
    ///
    /// [`RemedyError`] for an unknown or refused remedy, invalid parameters, a failed
    /// precondition, or a dry-run that cannot plan.
    pub fn dry_run(
        &self,
        agent: &AgentIdentity,
        id: &RemedyId,
        params: &Json,
        gate: &Gate,
        ctx: &mut RemedyCtx<'_>,
    ) -> Result<Plan, RemedyError> {
        self.authorise(agent, id, gate, ctx)?;
        let remedy = self.get(id).ok_or_else(|| RemedyError::Unknown {
            remedy: id.as_str().to_owned(),
        })?;
        validate_params(id, &remedy.meta(), params)?;
        let check = remedy.precondition(ctx, params);
        if !check.is_ready() {
            return Err(RemedyError::PreconditionFailed {
                remedy: id.as_str().to_owned(),
                detail: check.summary().to_owned(),
            });
        }
        let plan = remedy.dry_run(ctx, params)?;
        if !plan.verify() {
            return Err(RemedyError::PlanStale {
                remedy: id.as_str().to_owned(),
                detail: "the dry-run produced a plan whose hash does not bind it".to_owned(),
            });
        }
        self.lock().issued.insert(plan.hash.as_str().to_owned());
        ctx.record(AuditKind::Planned, id, &plan.summary());
        Ok(plan)
    }

    /// Applies a plan that dry-run produced, storing its undo record.
    ///
    /// # Errors
    ///
    /// [`RemedyError::PlanStale`] when the plan's hash does not bind it to this remedy and its
    /// parameters; [`RemedyError::PreconditionFailed`] when the precondition is no longer met;
    /// [`RemedyError`] for an unknown or refused remedy, invalid parameters, or a failed
    /// operation.
    pub fn apply(
        &self,
        agent: &AgentIdentity,
        id: &RemedyId,
        plan: &Plan,
        gate: &Gate,
        ctx: &mut RemedyCtx<'_>,
    ) -> Result<Applied, RemedyError> {
        self.authorise(agent, id, gate, ctx)?;
        let remedy = self.get(id).ok_or_else(|| RemedyError::Unknown {
            remedy: id.as_str().to_owned(),
        })?;
        if plan.remedy != *id {
            return Err(RemedyError::PlanStale {
                remedy: id.as_str().to_owned(),
                detail: format!("the plan belongs to `{}`", plan.remedy),
            });
        }
        if !plan.verify() {
            return Err(RemedyError::PlanStale {
                remedy: id.as_str().to_owned(),
                detail: "the plan's hash does not bind its remedy, parameters and steps".to_owned(),
            });
        }
        // One dry-run authorises one apply: a plan the registry never issued, or one already
        // applied, is refused.
        if !self.lock().issued.contains(plan.hash.as_str()) {
            return Err(RemedyError::PlanStale {
                remedy: id.as_str().to_owned(),
                detail:
                    "the plan was not issued by a dry-run of this registry, or was already applied"
                        .to_owned(),
            });
        }
        let meta = remedy.meta();
        validate_params(id, &meta, &plan.params)?;
        let check = remedy.precondition(ctx, &plan.params);
        if !check.is_ready() {
            return Err(RemedyError::PreconditionFailed {
                remedy: id.as_str().to_owned(),
                detail: check.summary().to_owned(),
            });
        }
        self.lock().issued.remove(plan.hash.as_str());
        let mut applied = remedy.apply(ctx, plan)?;
        let now = ctx.now();
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let token = UndoToken::derive(&plan.hash, now, seq);
        applied.undo_token = token.clone();
        let record = UndoRecord {
            token: token.clone(),
            remedy: id.clone(),
            plan_hash: plan.hash.clone(),
            applied_at: now,
            expires_at: now.saturating_add(self.settings.undo_window_secs),
            changes: applied.changes.clone(),
            undo: meta.undo.clone(),
        };
        self.lock()
            .records
            .insert(token.as_str().to_owned(), record);
        ctx.record(AuditKind::Applied, id, &format!("applied {}", plan.hash));
        Ok(applied)
    }

    /// Undoes an apply, refusing an unknown or expired token, a conflict, or an irreversible
    /// remedy.
    ///
    /// # Errors
    ///
    /// [`RemedyError::UndoUnknown`], [`RemedyError::UndoExpired`], [`RemedyError::UndoConflict`],
    /// [`RemedyError::Irreversible`], or a failure from the remedy's own undo.
    pub fn undo(
        &self,
        agent: &AgentIdentity,
        id: &RemedyId,
        token: &UndoToken,
        gate: &Gate,
        ctx: &mut RemedyCtx<'_>,
    ) -> Result<Undone, RemedyError> {
        let outcome = self.undo_inner(agent, id, token, gate, ctx);
        match &outcome {
            Ok(_) => {}
            Err(error) => {
                ctx.record(AuditKind::UndoRefused, id, &error.sentence());
            }
        }
        outcome
    }

    fn undo_inner(
        &self,
        agent: &AgentIdentity,
        id: &RemedyId,
        token: &UndoToken,
        gate: &Gate,
        ctx: &mut RemedyCtx<'_>,
    ) -> Result<Undone, RemedyError> {
        self.authorise(agent, id, gate, ctx)?;
        let record = self
            .lock()
            .records
            .get(token.as_str())
            .cloned()
            .ok_or_else(|| RemedyError::UndoUnknown {
                token: token.as_str().to_owned(),
            })?;
        if record.remedy != *id {
            return Err(RemedyError::UndoUnknown {
                token: token.as_str().to_owned(),
            });
        }
        if !record.undo.is_reversible() {
            let reason = match &record.undo {
                crate::model::UndoKind::Irreversible { reason } => reason.clone(),
                crate::model::UndoKind::Reversible { .. } => String::new(),
            };
            return Err(RemedyError::Irreversible {
                remedy: id.as_str().to_owned(),
                reason,
            });
        }
        let now = ctx.now();
        if record.is_expired(now) {
            return Err(RemedyError::UndoExpired {
                token: token.as_str().to_owned(),
                expired_at: record.expires_at,
                now,
            });
        }
        for change in &record.changes {
            match ctx.fingerprint(&change.target) {
                Some(actual) if actual == change.after => {}
                Some(actual) => {
                    return Err(RemedyError::UndoConflict {
                        token: token.as_str().to_owned(),
                        target: change.target.clone(),
                        expected: change.after.clone(),
                        actual,
                    });
                }
                None => {
                    return Err(RemedyError::UndoConflict {
                        token: token.as_str().to_owned(),
                        target: change.target.clone(),
                        expected: change.after.clone(),
                        actual: "<missing>".to_owned(),
                    });
                }
            }
        }
        let remedy = self.get(id).ok_or_else(|| RemedyError::Unknown {
            remedy: id.as_str().to_owned(),
        })?;
        let applied = Applied {
            remedy: id.clone(),
            plan_hash: record.plan_hash.clone(),
            changes: record.changes.clone(),
            undo_token: token.clone(),
        };
        let undone = remedy.undo(ctx, &applied)?;
        self.lock().records.remove(token.as_str());
        ctx.record(AuditKind::Undone, id, &format!("undid {}", token));
        Ok(undone)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, UndoLog> {
        self.undo
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Validates a remedy's metadata against the registry's rules.
fn validate_meta(id: &RemedyId, meta: &RemedyMeta) -> Result<(), RemedyError> {
    if meta.title.trim().is_empty() {
        return Err(RemedyError::MetaInvalid {
            remedy: id.as_str().to_owned(),
            detail: "the title is empty".to_owned(),
        });
    }
    if !meta.undo.is_reversible() && meta.tier != spot_agent_tiers::Tier::T3 {
        return Err(RemedyError::MetaInvalid {
            remedy: id.as_str().to_owned(),
            detail: "an irreversible remedy must be T3".to_owned(),
        });
    }
    for name in &meta.params.required {
        if !meta.params.properties.contains_key(name) {
            return Err(RemedyError::MetaInvalid {
                remedy: id.as_str().to_owned(),
                detail: format!("the required parameter `{name}` is not a declared property"),
            });
        }
    }
    Ok(())
}

/// Validates parameters against a remedy's schema.
fn validate_params(id: &RemedyId, meta: &RemedyMeta, params: &Json) -> Result<(), RemedyError> {
    meta.params
        .validate(params)
        .map_err(|violation| RemedyError::ParamsInvalid {
            remedy: id.as_str().to_owned(),
            path: violation.path,
            detail: violation.detail,
        })
}

/// Builds the tier authoriser's action catalogue from the remedies: one action per remedy, named
/// by its id, at the remedy's tier and effect, in the settings' domain.
fn action_catalogue(
    order: &[RemedyId],
    remedies: &BTreeMap<RemedyId, Box<dyn Remedy>>,
    domain: spot_agent_tiers::Domain,
) -> Result<ActionCatalogue, RemedyError> {
    let mut text = String::new();
    for id in order {
        let Some(remedy) = remedies.get(id) else {
            continue;
        };
        let meta = remedy.meta();
        text.push_str("[[actions]]\n");
        text.push_str(&format!("name = {:?}\n", id.as_str()));
        text.push_str(&format!("tier = {:?}\n", meta.tier.as_str()));
        text.push_str(&format!("domain = {:?}\n", domain.as_str()));
        text.push_str(&format!("effect = {:?}\n", meta.effect().as_str()));
    }
    ActionCatalogue::from_toml(&text).map_err(|error| RemedyError::CatalogueInvalid {
        detail: error.to_string(),
    })
}
