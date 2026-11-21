//! The support-case corpus: the hand-editable record of known symptoms and the remedy that
//! resolves them, plus a deterministic matcher that seeds the support agent.
//!
//! A corpus is a directory of `*.toml` files, each a list of `[[case]]` rows. A case names a
//! symptom, the evidence signatures that identify it, the remedy that resolves it, a parameter
//! template and the outcome. [`Corpus::suggest`] matches observed signatures against the cases
//! by exact signature intersection — no LLM, no scoring heuristic beyond the count — and returns
//! the remedies in a deterministic order.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Deserialize;
use serde_json::Value as Json;

use crate::error::RemedyError;
use crate::model::RemedyId;
use crate::registry::Registry;

/// How a case turned out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Outcome {
    /// The symptom was resolved.
    Resolved,
    /// The symptom was mitigated, not resolved.
    Mitigated,
    /// The case was escalated.
    Escalated,
}

impl Outcome {
    /// The stable snake_case text form, which is also the TOML form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Resolved => "resolved",
            Outcome::Mitigated => "mitigated",
            Outcome::Escalated => "escalated",
        }
    }

    /// Parses the stable text form.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "resolved" => Some(Outcome::Resolved),
            "mitigated" => Some(Outcome::Mitigated),
            "escalated" => Some(Outcome::Escalated),
            _ => None,
        }
    }
}

/// One support case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CorpusCase {
    /// The case's stable id.
    pub id: String,
    /// The symptom in words.
    pub symptom: String,
    /// The evidence signatures that identify it.
    pub evidence: Vec<String>,
    /// The remedy that resolves it.
    pub remedy: RemedyId,
    /// The parameter template for the remedy.
    pub params: Json,
    /// How it turned out.
    pub outcome: Outcome,
}

/// One suggested remedy and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    /// The suggested remedy.
    pub remedy: RemedyId,
    /// How many evidence signatures matched across its cases.
    pub score: u32,
    /// The case ids that matched.
    pub cases: Vec<String>,
}

/// The loaded corpus.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Corpus {
    cases: Vec<CorpusCase>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusFile {
    #[serde(default)]
    case: Vec<RawCase>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCase {
    id: String,
    symptom: String,
    #[serde(default)]
    evidence: Vec<String>,
    remedy: String,
    #[serde(default)]
    params: Json,
    outcome: String,
}

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
    /// [`RemedyError::CorpusInvalid`] when the TOML does not parse or a case breaks a rule (an
    /// empty id or symptom, no evidence, a malformed remedy id, or an unknown outcome).
    pub fn from_toml(text: &str) -> Result<Self, RemedyError> {
        let file: CorpusFile =
            toml::from_str(text).map_err(|error| RemedyError::CorpusInvalid {
                at: "corpus".to_owned(),
                detail: error.to_string(),
            })?;
        let mut cases = Vec::with_capacity(file.case.len());
        for raw in file.case {
            cases.push(convert(raw)?);
        }
        Ok(Self { cases })
    }

    /// Parses one corpus file.
    ///
    /// # Errors
    ///
    /// [`RemedyError::CorpusInvalid`] when the file cannot be read or parsed.
    pub fn from_file(path: &Path) -> Result<Self, RemedyError> {
        let text = std::fs::read_to_string(path).map_err(|error| RemedyError::CorpusInvalid {
            at: path.display().to_string(),
            detail: error.to_string(),
        })?;
        Self::from_toml(&text).map_err(|error| match error {
            RemedyError::CorpusInvalid { detail, .. } => RemedyError::CorpusInvalid {
                at: path.display().to_string(),
                detail,
            },
            other => other,
        })
    }

    /// Parses every `*.toml` in a directory, in file-name order.
    ///
    /// # Errors
    ///
    /// [`RemedyError::CorpusInvalid`] when the directory cannot be read or a file cannot be
    /// parsed.
    pub fn from_dir(dir: &Path) -> Result<Self, RemedyError> {
        let entries = std::fs::read_dir(dir).map_err(|error| RemedyError::CorpusInvalid {
            at: dir.display().to_string(),
            detail: error.to_string(),
        })?;
        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| RemedyError::CorpusInvalid {
                at: dir.display().to_string(),
                detail: error.to_string(),
            })?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("toml") {
                paths.push(path);
            }
        }
        paths.sort();
        let mut cases = Vec::new();
        for path in paths {
            let mut corpus = Self::from_file(&path)?;
            cases.append(&mut corpus.cases);
        }
        Ok(Self { cases })
    }

    /// The cases, in load order.
    #[must_use]
    pub fn cases(&self) -> &[CorpusCase] {
        &self.cases
    }

    /// How many cases are loaded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cases.len()
    }

    /// Whether the corpus is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cases.is_empty()
    }

    /// Validates every case against a registry: the remedy must exist and the parameter template
    /// must satisfy its schema.
    ///
    /// # Errors
    ///
    /// [`RemedyError::CorpusInvalid`] naming the offending case, or
    /// [`RemedyError::ParamsInvalid`] / [`RemedyError::Unknown`] from the registry check.
    pub fn validate(&self, registry: &Registry) -> Result<(), RemedyError> {
        for case in &self.cases {
            let Some(meta) = registry.meta(&case.remedy) else {
                return Err(RemedyError::CorpusInvalid {
                    at: case.id.clone(),
                    detail: format!("the remedy `{}` is not registered", case.remedy),
                });
            };
            meta.params
                .validate(&case.params)
                .map_err(|violation| RemedyError::CorpusInvalid {
                    at: case.id.clone(),
                    detail: format!(
                        "the parameter template is invalid at `{}`: {}",
                        violation.path, violation.detail
                    ),
                })?;
        }
        Ok(())
    }

    /// Suggests remedies for a set of observed signatures, by exact signature intersection.
    ///
    /// A case scores one per evidence signature present in `symptoms`. Scores are summed per
    /// remedy; the result is sorted by score descending, then by remedy id ascending, so the
    /// order is deterministic.
    #[must_use]
    pub fn suggest(&self, symptoms: &[String]) -> Vec<Suggestion> {
        let observed: BTreeSet<&str> = symptoms.iter().map(String::as_str).collect();
        let mut by_remedy: BTreeMap<RemedyId, Suggestion> = BTreeMap::new();
        for case in &self.cases {
            let matched = case
                .evidence
                .iter()
                .filter(|signature| observed.contains(signature.as_str()))
                .count();
            if matched == 0 {
                continue;
            }
            let entry = by_remedy
                .entry(case.remedy.clone())
                .or_insert_with(|| Suggestion {
                    remedy: case.remedy.clone(),
                    score: 0,
                    cases: Vec::new(),
                });
            entry.score = entry.score.saturating_add(matched as u32);
            entry.cases.push(case.id.clone());
        }
        let mut suggestions: Vec<Suggestion> = by_remedy.into_values().collect();
        suggestions.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.remedy.cmp(&b.remedy)));
        suggestions
    }
}

/// Converts a raw TOML row, validating it.
fn convert(raw: RawCase) -> Result<CorpusCase, RemedyError> {
    if raw.id.trim().is_empty() {
        return Err(RemedyError::CorpusInvalid {
            at: "corpus".to_owned(),
            detail: "a case has an empty id".to_owned(),
        });
    }
    if raw.symptom.trim().is_empty() {
        return Err(RemedyError::CorpusInvalid {
            at: raw.id.clone(),
            detail: "the symptom is empty".to_owned(),
        });
    }
    if raw.evidence.is_empty() {
        return Err(RemedyError::CorpusInvalid {
            at: raw.id.clone(),
            detail: "the evidence is empty".to_owned(),
        });
    }
    let remedy = RemedyId::parse(&raw.remedy).map_err(|_| RemedyError::CorpusInvalid {
        at: raw.id.clone(),
        detail: format!("the remedy `{}` is not a valid remedy id", raw.remedy),
    })?;
    let outcome = Outcome::parse(&raw.outcome).ok_or_else(|| RemedyError::CorpusInvalid {
        at: raw.id.clone(),
        detail: format!(
            "the outcome `{}` is not resolved, mitigated or escalated",
            raw.outcome
        ),
    })?;
    Ok(CorpusCase {
        id: raw.id,
        symptom: raw.symptom,
        evidence: raw.evidence,
        remedy,
        params: if raw.params.is_null() {
            Json::Object(serde_json::Map::new())
        } else {
            raw.params
        },
        outcome,
    })
}
