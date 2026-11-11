//! The incident corpus: the hand-editable record of every synthetic support case and the format
//! the release gate reads.
//!
//! A corpus is a directory of `*.toml` files, each a list of `[[incident]]` rows. Each row names
//! the incident, its tier, the fault to inject, the observable symptoms, the resolution that is
//! expected, the actions it must never take, and the check that proves the symptom cleared.
//!
//! The loader validates structure; [`Corpus::validate`] also validates every expected remedy
//! against the live [`spot_remedies::Registry`], so a corpus row cannot name a remedy or a
//! parameter the platform does not have.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use spot_agent_tiers::Tier;
use spot_remedies::{Registry, RemedyId};

use crate::error::{EvalError, Result};
use crate::fault::FaultSpec;

/// The incident's part of the platform, used for coverage and reporting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// A background job failed or is stuck.
    JobFailure,
    /// A BPM case is stuck.
    StuckCase,
    /// A sync item is held in quarantine.
    QuarantinedSync,
    /// A cache namespace holds poisoned or stale entries.
    CachePoisoning,
    /// A permission cache is stale after a role change.
    PermissionMisconfiguration,
    /// An app upgrade left generated state stale.
    FailedAppUpgrade,
    /// Edge sync has drifted from its source.
    Drift,
    /// A configuration change left a stale config cache.
    ConfigError,
    /// The incident must be escalated, never fixed by the support agent.
    Escalation,
    /// A prompt-injection incident: the fault is ordinary, the data carries instructions.
    PromptInjection,
}

impl Category {
    /// The stable snake_case text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::JobFailure => "job_failure",
            Self::StuckCase => "stuck_case",
            Self::QuarantinedSync => "quarantined_sync",
            Self::CachePoisoning => "cache_poisoning",
            Self::PermissionMisconfiguration => "permission_misconfiguration",
            Self::FailedAppUpgrade => "failed_app_upgrade",
            Self::Drift => "drift",
            Self::ConfigError => "config_error",
            Self::Escalation => "escalation",
            Self::PromptInjection => "prompt_injection",
        }
    }
}

/// Where untrusted text arrived from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectionSurface {
    /// A document's body text.
    DocumentText,
    /// A comment on a record.
    Comment,
    /// An attachment's file name.
    AttachmentName,
    /// A tool result the agent received.
    ToolResult,
}

impl InjectionSurface {
    /// The stable snake_case text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DocumentText => "document_text",
            Self::Comment => "comment",
            Self::AttachmentName => "attachment_name",
            Self::ToolResult => "tool_result",
        }
    }
}

/// One piece of untrusted text with the surface it arrived on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Injection {
    /// The surface: document text, comment, attachment name or tool result.
    pub surface: InjectionSurface,
    /// The text. It is data, never an instruction.
    pub payload: String,
}

/// The observable signals and the plain-language symptom.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Symptoms {
    /// The evidence signatures the diagnosis exposes (for example `job.failed`).
    pub signals: Vec<String>,
    /// One sentence an operator could read.
    pub summary: String,
}

/// What the incident expects the agent to do.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExpectedResolution {
    /// Apply this catalogued remedy with these parameters.
    Remedy {
        /// The remedy's stable id.
        remedy: String,
        /// The exact parameters the remedy must receive.
        #[serde(default)]
        params: serde_json::Value,
    },
    /// Escalate to a human, changing nothing.
    Escalate,
}

impl ExpectedResolution {
    /// The expected remedy id, when there is one.
    #[must_use]
    pub fn remedy(&self) -> Option<&str> {
        match self {
            Self::Remedy { remedy, .. } => Some(remedy),
            Self::Escalate => None,
        }
    }

    /// The expected parameters, as a JSON object.
    #[must_use]
    pub fn params(&self) -> serde_json::Value {
        match self {
            Self::Remedy { params, .. } => {
                if params.is_null() {
                    serde_json::Value::Object(serde_json::Map::new())
                } else {
                    params.clone()
                }
            }
            Self::Escalate => serde_json::Value::Object(serde_json::Map::new()),
        }
    }

    /// Whether the incident must be escalated.
    #[must_use]
    pub fn is_escalation(&self) -> bool {
        matches!(self, Self::Escalate)
    }
}

/// The check that proves the incident is over.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuccessCheck {
    /// The state target, for example `job:job-1041` or `cache:permissions`.
    pub target: String,
    /// The state the target must reach (`queued`, `healthy`, `synced`, `0`, ...).
    pub expect: String,
    /// The tenant-wide invariants that must still hold after the fix.
    #[serde(default)]
    pub invariants: Vec<String>,
}

/// One incident in the corpus.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Incident {
    /// The stable incident id (`job-failed-payment-sync`).
    pub id: String,
    /// The human-facing title.
    pub title: String,
    /// The platform area the incident belongs to.
    pub category: Category,
    /// The tier the expected resolution needs.
    pub tier: Tier,
    /// The observable symptoms.
    pub symptoms: Symptoms,
    /// The fault to inject.
    pub fault: FaultSpec,
    /// What the agent should do.
    pub expected_resolution: ExpectedResolution,
    /// Actions this incident must never take, whatever the data says.
    #[serde(default)]
    pub forbidden_actions: Vec<String>,
    /// The success check.
    pub success_check: SuccessCheck,
    /// Untrusted text carried by the incident's data (prompt-injection suite).
    #[serde(default)]
    pub injections: Vec<Injection>,
}

/// The loaded incident corpus.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Corpus {
    incidents: Vec<Incident>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusFile {
    #[serde(default)]
    incident: Vec<Incident>,
}

/// The invariants [`crate::tenant::Tenant`] knows how to check.
pub const KNOWN_INVARIANTS: &[&str] = &[
    "cache_clean",
    "edges_in_sync",
    "finance_unmodified",
    "no_failed_jobs",
    "no_held_quarantine",
    "no_stale_indexes",
    "no_stuck_cases",
    "tenant_scope",
    "audit_intact",
];

impl Corpus {
    /// An empty corpus.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses one corpus file's text.
    ///
    /// # Errors
    ///
    /// [`EvalError::CorpusInvalid`] when the TOML does not parse or an incident breaks a rule.
    pub fn from_toml(text: &str, at: &str) -> Result<Self> {
        let file: CorpusFile = toml::from_str(text).map_err(|error| EvalError::CorpusInvalid {
            at: at.to_owned(),
            detail: error.to_string(),
        })?;
        let corpus = Self {
            incidents: file.incident,
        };
        corpus.check_shape(at)?;
        Ok(corpus)
    }

    /// Parses one corpus file.
    ///
    /// # Errors
    ///
    /// [`EvalError::CorpusIo`] when it cannot be read, or [`EvalError::CorpusInvalid`] when it
    /// does not parse or breaks a rule.
    pub fn from_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|error| EvalError::CorpusIo {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?;
        Self::from_toml(&text, &path.display().to_string())
    }

    /// Parses every `*.toml` in a directory, in file-name order.
    ///
    /// # Errors
    ///
    /// [`EvalError::CorpusIo`] when the directory cannot be read or a file cannot be read.
    pub fn from_dir(dir: &Path) -> Result<Self> {
        let entries = std::fs::read_dir(dir).map_err(|error| EvalError::CorpusIo {
            path: dir.to_path_buf(),
            detail: error.to_string(),
        })?;
        let mut paths: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| EvalError::CorpusIo {
                path: dir.to_path_buf(),
                detail: error.to_string(),
            })?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("toml") {
                paths.push(path);
            }
        }
        paths.sort();
        let mut corpus = Self::new();
        for path in paths {
            let mut one = Self::from_file(&path)?;
            corpus.incidents.append(&mut one.incidents);
        }
        corpus.check_shape(&dir.display().to_string())?;
        Ok(corpus)
    }

    /// The incidents, in load order.
    #[must_use]
    pub fn incidents(&self) -> &[Incident] {
        &self.incidents
    }

    /// How many incidents are loaded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.incidents.len()
    }

    /// Whether the corpus is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.incidents.is_empty()
    }

    /// One incident by id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Incident> {
        self.incidents.iter().find(|incident| incident.id == id)
    }

    /// The two corpora joined, in order.
    #[must_use]
    pub fn merged(mut self, other: Self) -> Self {
        self.incidents.extend(other.incidents);
        self
    }

    /// How many incidents expect escalation.
    #[must_use]
    pub fn escalation_count(&self) -> usize {
        self.incidents
            .iter()
            .filter(|incident| incident.expected_resolution.is_escalation())
            .count()
    }

    /// The categories present, in stable order.
    #[must_use]
    pub fn categories(&self) -> BTreeSet<Category> {
        self.incidents
            .iter()
            .map(|incident| incident.category)
            .collect()
    }

    /// Validates every expected remedy and parameter against the registry.
    ///
    /// # Errors
    ///
    /// [`EvalError::ResolutionInvalid`] naming the incident and what is wrong.
    pub fn validate(&self, registry: &Registry) -> Result<()> {
        for incident in &self.incidents {
            let ExpectedResolution::Remedy { remedy, params } = &incident.expected_resolution
            else {
                // A T2 escalation is valid: the hard floor, not the tier, forces it.
                continue;
            };
            let id = RemedyId::parse(remedy).map_err(|_| EvalError::ResolutionInvalid {
                at: incident.id.clone(),
                detail: format!("`{remedy}` is not a valid remedy id"),
            })?;
            let Some(meta) = registry.meta(&id) else {
                return Err(EvalError::ResolutionInvalid {
                    at: incident.id.clone(),
                    detail: format!("the remedy `{remedy}` is not registered"),
                });
            };
            let params = if params.is_null() {
                serde_json::Value::Object(serde_json::Map::new())
            } else {
                params.clone()
            };
            meta.params
                .validate(&params)
                .map_err(|violation| EvalError::ResolutionInvalid {
                    at: incident.id.clone(),
                    detail: format!(
                        "the expected parameters are invalid at `{}`: {}",
                        violation.path, violation.detail
                    ),
                })?;
            if meta.tier > incident.tier {
                return Err(EvalError::ResolutionInvalid {
                    at: incident.id.clone(),
                    detail: format!(
                        "the remedy needs {} but the incident declares {}",
                        meta.tier.as_str(),
                        incident.tier.as_str()
                    ),
                });
            }
        }
        Ok(())
    }

    /// Checks the rules that do not need the registry: unique ids, non-empty fields, known
    /// invariants and a success target that matches the fault.
    pub(crate) fn check_shape(&self, at: &str) -> Result<()> {
        let mut ids = BTreeSet::new();
        for incident in &self.incidents {
            if incident.id.trim().is_empty() {
                return Err(EvalError::CorpusInvalid {
                    at: at.to_owned(),
                    detail: "an incident has an empty id".to_owned(),
                });
            }
            if !ids.insert(incident.id.as_str()) {
                return Err(EvalError::CorpusInvalid {
                    at: incident.id.clone(),
                    detail: "the incident id is used twice".to_owned(),
                });
            }
            if incident.title.trim().is_empty() {
                return Err(EvalError::CorpusInvalid {
                    at: incident.id.clone(),
                    detail: "the title is empty".to_owned(),
                });
            }
            if incident.symptoms.signals.is_empty() {
                return Err(EvalError::CorpusInvalid {
                    at: incident.id.clone(),
                    detail: "the symptoms carry no signals".to_owned(),
                });
            }
            if incident.symptoms.summary.trim().is_empty() {
                return Err(EvalError::CorpusInvalid {
                    at: incident.id.clone(),
                    detail: "the symptom summary is empty".to_owned(),
                });
            }
            if incident.success_check.target.trim().is_empty()
                || incident.success_check.expect.trim().is_empty()
            {
                return Err(EvalError::CorpusInvalid {
                    at: incident.id.clone(),
                    detail: "the success check has an empty target or expectation".to_owned(),
                });
            }
            for invariant in &incident.success_check.invariants {
                if !KNOWN_INVARIANTS.contains(&invariant.as_str()) {
                    return Err(EvalError::CorpusInvalid {
                        at: incident.id.clone(),
                        detail: format!("the invariant `{invariant}` is not implemented"),
                    });
                }
            }
            for action in &incident.forbidden_actions {
                if action.trim().is_empty() {
                    return Err(EvalError::CorpusInvalid {
                        at: incident.id.clone(),
                        detail: "a forbidden action is empty".to_owned(),
                    });
                }
            }
            for injection in &incident.injections {
                if injection.payload.trim().is_empty() {
                    return Err(EvalError::CorpusInvalid {
                        at: incident.id.clone(),
                        detail: "an injection payload is empty".to_owned(),
                    });
                }
            }
            if !incident
                .fault
                .matches_target(&incident.success_check.target)
            {
                return Err(EvalError::CorpusInvalid {
                    at: incident.id.clone(),
                    detail: format!(
                        "the success target `{}` does not name a record the fault injected",
                        incident.success_check.target
                    ),
                });
            }
        }
        Ok(())
    }
}
