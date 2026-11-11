//! The behaviour changelog: the DocTypes and seams a release changes, and the check that the
//! L5 differential engine (WP-TST-05) finds no divergence the changelog does not name.
//!
//! A release changes behaviour, not only schema. The changelog names each such change by key —
//! a DocType or a seam — with a one-sentence summary, the version it appeared in, its kind
//! (rounding, tax, valuation, posting, validation, other) and its sensitivity. The L5
//! differential engine reports divergences against this release; [`BehaviourChangelog::diff_against`]
//! returns the ones no entry matches, which is how "L5 flags undeclared behaviour changes" is
//! made true.
//!
//! The L5 report's shape is defined by WP-TST-05, which is not briefed yet, so this crate depends
//! on the [`DivergenceSource`] trait and adapts when that shape lands.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// What a behaviour change is keyed to: a DocType or a seam (`<app>:<seam>`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum BehaviourKey {
    /// A DocType's behaviour changed, named by its label.
    DocType(String),
    /// A seam's behaviour changed, named as the seam registry names it (`<app>:<seam>`).
    Seam(String),
}

impl BehaviourKey {
    /// Parses the documented text form: `doctype:<name>` or `seam:<name>`.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let Some((tag, name)) = text.split_once(':') else {
            return Err(Error::BadKey {
                key: text.to_owned(),
                reason: "a behaviour key is `doctype:<name>` or `seam:<name>`".to_owned(),
            });
        };
        if name.is_empty() {
            return Err(Error::BadKey {
                key: text.to_owned(),
                reason: "the key names nothing after the `:`".to_owned(),
            });
        }
        match tag {
            "doctype" => Ok(Self::DocType(name.to_owned())),
            "seam" => Ok(Self::Seam(name.to_owned())),
            other => Err(Error::BadKey {
                key: text.to_owned(),
                reason: format!("`{other}` is not a behaviour key kind (doctype, seam)"),
            }),
        }
    }

    /// The documented text form. This is the stable key: the changelog and the L5 report are
    /// matched on it, and it is what the file stores.
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Self::DocType(name) => format!("doctype:{name}"),
            Self::Seam(name) => format!("seam:{name}"),
        }
    }
}

impl fmt::Display for BehaviourKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text())
    }
}

impl TryFrom<String> for BehaviourKey {
    type Error = Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<BehaviourKey> for String {
    fn from(value: BehaviourKey) -> Self {
        value.text()
    }
}

/// What kind of behaviour a change touches; the first four are the replay-sensitive kinds of
/// ADR-22 (WS12 replay policy).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BehaviourKind {
    /// How money or quantities round.
    Rounding,
    /// How tax is computed or applied.
    Tax,
    /// How inventory or assets are valued.
    Valuation,
    /// What is posted to the ledger.
    Posting,
    /// What a validation accepts or rejects.
    Validation,
    /// Anything else.
    Other,
}

/// How much care a behaviour change needs. `High` is the replay trigger of ADR-22 (tax,
/// valuation, GL posting); a release containing a high-sensitivity entry must replay per site
/// policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    /// A cosmetic or local change.
    Low,
    /// A change a reviewer should read.
    Medium,
    /// A change that can move money; replay is required.
    High,
}

impl Sensitivity {
    /// Whether this sensitivity forces a behavioural replay under the `auto` replay policy.
    #[must_use]
    pub fn is_high(self) -> bool {
        self == Self::High
    }
}

/// One declared behaviour change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BehaviourEntry {
    /// What changed: a DocType or a seam.
    pub key: BehaviourKey,
    /// What changed, in one sentence, for a human reader.
    pub summary: String,
    /// The version that introduced the change.
    pub since_version: String,
    /// The kind of behaviour.
    pub kind: BehaviourKind,
    /// How much care the change needs.
    pub sensitivity: Sensitivity,
}

/// The L5 differential engine's report shape, reduced to what the changelog consumes.
///
/// WP-TST-05 owns the real shape. This crate depends on the trait so the report can change
/// without touching the changelog; the L5 crate implements it and adapts its divergences.
pub trait DivergenceSource {
    /// The divergences the L5 engine found for this release.
    fn divergences(&self) -> Vec<Divergence>;
}

/// One behaviour divergence the L5 engine found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Divergence {
    /// What diverged: a DocType or a seam.
    pub key: BehaviourKey,
    /// What the engine saw, in one sentence.
    pub summary: String,
}

impl DivergenceSource for Vec<Divergence> {
    fn divergences(&self) -> Vec<Divergence> {
        self.clone()
    }
}

impl DivergenceSource for [Divergence] {
    fn divergences(&self) -> Vec<Divergence> {
        self.to_vec()
    }
}

impl DivergenceSource for () {
    fn divergences(&self) -> Vec<Divergence> {
        Vec::new()
    }
}

/// A release's declared behaviour changes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BehaviourChangelog {
    /// The declared entries.
    pub entries: Vec<BehaviourEntry>,
}

/// The on-disk file: `[[entry]]` rows.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default, rename = "entry")]
    entries: Vec<BehaviourEntry>,
}

impl BehaviourChangelog {
    /// A changelog of exactly these entries.
    #[must_use]
    pub fn new(entries: Vec<BehaviourEntry>) -> Self {
        Self { entries }
    }

    /// Parses the changelog from TOML.
    pub fn from_toml(text: &str) -> Result<Self, Error> {
        let file: File = toml::from_str(text).map_err(|e| Error::BadChangelog {
            message: e.to_string(),
        })?;
        Ok(Self {
            entries: file.entries,
        })
    }

    /// The deterministic text form: entries sorted by key, fields in declaration order.
    #[must_use]
    pub fn render(&self) -> String {
        let mut entries = self.entries.clone();
        entries.sort_by_key(|a| a.key.text());
        toml::to_string(&File { entries }).unwrap_or_default()
    }

    /// The entry for a key, if the release declares one.
    #[must_use]
    pub fn entry(&self, key: &BehaviourKey) -> Option<&BehaviourEntry> {
        self.entries.iter().find(|e| &e.key == key)
    }

    /// The divergences the L5 report found that no entry names, sorted by key.
    ///
    /// A divergence matches an entry when their keys are equal; the summary is for a human. An
    /// empty result is the acceptance condition: every behaviour change the release makes is
    /// declared.
    #[must_use]
    pub fn diff_against(&self, report: &dyn DivergenceSource) -> Vec<Divergence> {
        let declared: BTreeSet<String> = self.entries.iter().map(|e| e.key.text()).collect();
        let mut out: Vec<Divergence> = report
            .divergences()
            .into_iter()
            .filter(|d| !declared.contains(&d.key.text()))
            .collect();
        out.sort_by_key(|a| a.key.text());
        out
    }
}
