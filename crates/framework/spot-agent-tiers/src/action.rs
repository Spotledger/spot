//! Actions and the catalogue load: what an action is, its tier, its domain and its effect.
//!
//! The catalogue is TOML (`[[actions]]`: `name`, `tier`, `domain`, `effect`), hand-editable,
//! loaded through [`ActionCatalogue::from_toml`]. A load problem
//! ([`TierError::DuplicateAction`], [`TierError::EffectBelowFloor`], an empty name) is an
//! error on the spot: the authoriser refuses every name a broken catalogue cannot answer for.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::TierError;
use crate::tier::Tier;

/// What an action does to the system (blueprint WS13, the hard floor's subject).
///
/// Ordered above `read`: the hard floor's journal, snapshot and financial-route checks bind
/// every effect strictly above [`Effect::Read`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// No change at all: reads, diagnosis, explanation.
    Read,
    /// A change that can be undone without residue (settings within policy, retries,
    /// print formats).
    Reversible,
    /// A change to business documents through a catalogued remedy.
    DataRepair,
    /// A change to customizations, scripts, apps or migrations.
    Structural,
}

impl Effect {
    /// The stable snake_case text form, which is also the serde and TOML form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Effect::Read => "read",
            Effect::Reversible => "reversible",
            Effect::DataRepair => "data_repair",
            Effect::Structural => "structural",
        }
    }

    /// The minimum tier this effect may be declared at: the one table the catalogue load
    /// enforces, read off the blueprint's tier table.
    ///
    /// Only reading is T0 ("T0 Explain: read and diagnose"); a reversible change is T1
    /// ("T1 Configure: reversible settings"); a change that repairs business data is T2
    /// ("T2 Repair data: remedies from the catalogue only"); a structural one is T3 ("T3
    /// Structural: customizations, scripts, app installs, migrations"). A `structural`
    /// effect declared as T1 is therefore a load error.
    #[must_use]
    pub fn min_tier(self) -> Tier {
        match self {
            Effect::Read => Tier::T0,
            Effect::Reversible => Tier::T1,
            Effect::DataRepair => Tier::T2,
            Effect::Structural => Tier::T3,
        }
    }
}

impl fmt::Display for Effect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The support versus operations separation (blueprint WS13): a support agent works only
/// support actions, an operations agent only operations actions; they never cross.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Domain {
    /// The support domain (the support agent's home).
    Support,
    /// The operations domain (accounting and similar operations agents).
    Operations,
}

impl Domain {
    /// The stable snake_case text form, which is also the serde and TOML form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Domain::Support => "support",
            Domain::Operations => "operations",
        }
    }
}

impl fmt::Display for Domain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One catalogued action: its name is its stable address.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionSpec {
    /// The action's name, as the request names it (`"explain_failed_job"`).
    pub name: String,
    /// The tier the action needs.
    pub tier: Tier,
    /// The domain the action belongs to.
    pub domain: Domain,
    /// What the action does to the system.
    pub effect: Effect,
}

impl ActionSpec {
    /// Whether this action changes anything: any effect above [`Effect::Read`].
    #[must_use]
    pub fn is_change(&self) -> bool {
        self.effect > Effect::Read
    }
}

/// The load-time shape of one TOML row: identical to [`ActionSpec`], kept separate so an
/// internal representation change never moves the file format.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionRow {
    name: String,
    tier: Tier,
    domain: Domain,
    effect: Effect,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogueFile {
    actions: Vec<ActionRow>,
}

/// The catalogued actions, keyed by name; the authoriser refuses any name not here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActionCatalogue {
    actions: BTreeMap<String, ActionSpec>,
}

impl ActionCatalogue {
    /// A catalogue with no actions (every name is refused, as unknown).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Loads a catalogue from TOML and checks it in one pass.
    ///
    /// # Errors
    ///
    /// [`TierError::Parse`] when the text is not valid TOML or an entry has an unknown or
    /// missing key; [`TierError::ValueInvalid`] when a name is empty;
    /// [`TierError::DuplicateAction`] when two entries declare the same name;
    /// [`TierError::EffectBelowFloor`] when an entry's tier is below its declared effect's
    /// [`Effect::min_tier`] (a `structural` effect declared as T1, for instance). The first
    /// problem found is returned; a broken catalogue never loads partially.
    pub fn from_toml(text: &str) -> Result<Self, TierError> {
        let file: CatalogueFile = toml::from_str(text).map_err(|e| TierError::Parse {
            at: "actions".to_owned(),
            detail: e.to_string(),
        })?;
        let mut actions: BTreeMap<String, ActionSpec> = BTreeMap::new();
        for (i, row) in file.actions.into_iter().enumerate() {
            let src = format!("actions[{i}]");
            let name = row.name.trim().to_owned();
            if name.is_empty() {
                return Err(TierError::ValueInvalid {
                    at: src,
                    detail: "`name` is required".to_owned(),
                });
            }
            let minimum = row.effect.min_tier();
            if row.tier < minimum {
                return Err(TierError::EffectBelowFloor {
                    at: src,
                    name: name.clone(),
                    declared: row.tier.as_str().to_owned(),
                    effect: row.effect.as_str().to_owned(),
                    minimum: minimum.as_str().to_owned(),
                });
            }
            let spec = ActionSpec {
                name: name.clone(),
                tier: row.tier,
                domain: row.domain,
                effect: row.effect,
            };
            let replaced = actions.insert(name.clone(), spec);
            if replaced.is_some() {
                return Err(TierError::DuplicateAction { at: src, name });
            }
        }
        Ok(Self { actions })
    }

    /// The number of actions in the catalogue.
    #[must_use]
    pub fn len(&self) -> usize {
        self.actions.len()
    }

    /// Whether the catalogue is empty (every name is then refused as unknown).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// The spec for an action name, if catalogued.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ActionSpec> {
        self.actions.get(name)
    }
}
