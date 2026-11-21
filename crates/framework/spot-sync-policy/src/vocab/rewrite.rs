//! The rewrite specification: what the retired class, direction and owner scope vocabulary
//! becomes under ADR-063, and what a person must decide first (decisions C1 to C7 of 31 Oct
//! 2026; the rows are M1 to M8 of `docs/research/sync-policy-vocabulary-mapping.md`).
//!
//! [`rewrite_candidate`] is a pure function. It is the specification WP-SYN-01.T7 builds the
//! rewrite tool against and the text the lint flag prints. It never drops a word silently: a
//! declaration that does not map is refused with every reason, and the tool leaves it alone.

use crate::policy::{Class, Direction, ScopeKeyType, SyncPolicy};
use crate::preset::{Preset, PresetPolicy, WhenEmpty};

/// Why a declaration needs a person. A closed set: one variant per refusal rule, so a rewrite
/// tool can match on it and never parse a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// C1 (row M3): `owner_scope` on a `master` meant that only the key holder writes it, so
    /// the declaration is a `handover` or an ownership migration; the key is never dropped.
    OwnerScopeOnMaster {
        /// The key type the declaration names.
        key_type: ScopeKeyType,
    },
    /// C2 (rows M4 and M5): `owner_key` needs the record field that names the key, and the old
    /// form stored only the key type; the declarer names the field.
    OwnerKeyNeedsField {
        /// The key type the declaration names.
        key_type: ScopeKeyType,
    },
    /// C3 (row M5): `shared` plus `up` holds ownership per key, `record` gives it to the
    /// creator; the rewrite is only safe when just the key holder creates the record, and that
    /// cannot be shown from the declaration. `Class::Shared` is not retired.
    KeyHolderNotProvable {
        /// The key type the declaration names.
        key_type: ScopeKeyType,
    },
    /// C4 (row M8): `shared` plus `both` is not mapped mechanically; the two candidate
    /// rewrites differ in who may edit after the first hub contact. Both are printed.
    SharedBoth {
        /// The key type the declaration names.
        key_type: ScopeKeyType,
        /// The candidate rewrites, as text with `<...>` placeholders for what only a person
        /// knows: the `handover` first, then the `record` plus `relay`.
        candidates: Vec<String>,
    },
    /// Row M7 (not a numbered decision): `shared` plus `down` with a key. Rewriting it as a
    /// `master` either drops the key or narrows the reach, which is a routing change.
    KeyedSharedDown {
        /// The key type the declaration names.
        key_type: ScopeKeyType,
    },
    /// C7: `when_empty` is set away from the default, which is `none`; no behaviour changes
    /// without a written reason.
    WhenEmptyAwayFromDefault {
        /// The value that was set.
        set: WhenEmpty,
    },
    /// The combination is not one of the eight legal rows (the old lint already rejects it), so
    /// there is nothing to rewrite.
    NotALegalDeclaration,
    /// The candidate would not pass [`PresetPolicy::validate`]; carries the issue codes.
    CandidateInvalid {
        /// The `sync_policy.preset_*` codes of the issues.
        codes: Vec<&'static str>,
    },
}

impl Refusal {
    /// The decision of 31 Oct 2026 that fixes this refusal, or `""` for a refusal that follows
    /// from no numbered decision.
    #[must_use]
    pub fn decision(&self) -> &'static str {
        match self {
            Refusal::OwnerScopeOnMaster { .. } => "C1",
            Refusal::OwnerKeyNeedsField { .. } => "C2",
            Refusal::KeyHolderNotProvable { .. } => "C3",
            Refusal::SharedBoth { .. } => "C4",
            Refusal::WhenEmptyAwayFromDefault { .. } => "C7",
            Refusal::KeyedSharedDown { .. }
            | Refusal::NotALegalDeclaration
            | Refusal::CandidateInvalid { .. } => "",
        }
    }

    /// The question the declarer must answer, in plain words.
    #[must_use]
    pub fn question(&self) -> String {
        match self {
            Refusal::OwnerScopeOnMaster { key_type } => format!(
                "owner scope `{key_type}` on a master means only its holder writes it, so this \
                 is a handover or an ownership migration, not a master; the key is not dropped"
            ),
            Refusal::OwnerKeyNeedsField { key_type } => format!(
                "name the record field that holds the `{key_type}` key; the creating node must \
                 be its holder"
            ),
            Refusal::KeyHolderNotProvable { key_type } => format!(
                "a shared record is owned per `{key_type}` key, a record by its creator; confirm \
                 that only the holder of the key ever creates it, or it has no preset"
            ),
            Refusal::SharedBoth { key_type, .. } => format!(
                "a shared record flowing both ways under `{key_type}` is either a handover (the \
                 hub adopts it) or a record plus relay (each node owns its own); they differ in \
                 who may edit after the first hub contact"
            ),
            Refusal::KeyedSharedDown { key_type } => format!(
                "a shared record flowing down under `{key_type}`: a master reaches every node, \
                 so say whether it should reach only the holders of the key (and which field \
                 names it) or every node"
            ),
            Refusal::WhenEmptyAwayFromDefault { set } => format!(
                "`when_empty` is `{set}`, not the default `none`; say why the behaviour differs"
            ),
            Refusal::NotALegalDeclaration => {
                "this class, direction and owner scope combination is not a legal declaration; \
                 correct it before it is rewritten"
                    .to_owned()
            }
            Refusal::CandidateInvalid { codes } => format!(
                "the mechanical rewrite would not be a valid policy ({}); fix the declaration \
                 first",
                codes.join(", ")
            ),
        }
    }
}

/// The outcome of [`rewrite_candidate`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rewrite {
    /// The declaration maps with no change of meaning: the policy to declare instead. Every
    /// setting the old words never said is left at the preset's default.
    Mechanical(Box<PresetPolicy>),
    /// A person must decide. Every reason is listed, none is dropped.
    NeedsHuman {
        /// The refusals, in a fixed order: the row's reasons, then C7.
        reasons: Vec<Refusal>,
    },
}

/// The rewrite of an old declaration into the ADR-063 vocabulary.
///
/// | Row | Old | Result |
/// | --- | --- | --- |
/// | M1 | `local`, `none` | `local` |
/// | M2 | `master`, `down` | `master` |
/// | M3 | `master`, `down`, owner scope | refused, C1 |
/// | M4 | `transactional`, `up`, owner scope | refused, C2 |
/// | M5 | `shared`, `up`, owner scope | refused, C2 and C3 |
/// | M6 | `shared`, `down` | `master` |
/// | M7 | `shared`, `down`, owner scope | refused |
/// | M8 | `shared`, `both`, owner scope | refused, C4 |
///
/// `exclude_fields` maps to `excluded_fields.local` (the old set is applied on the sender only).
/// `trigger` is accepted and ignored (C5); the lint warns about it.
#[must_use]
pub fn rewrite_candidate(old: &SyncPolicy) -> Rewrite {
    rewrite_candidate_with(old, WhenEmpty::None)
}

/// As [`rewrite_candidate`], for a DocType that also carries the new `when_empty` setting.
/// Anything but the default `none` is refused (C7): the old words never said it, so a value
/// there is behaviour the rewrite must not change without a written reason.
#[must_use]
pub fn rewrite_candidate_with(old: &SyncPolicy, when_empty: WhenEmpty) -> Rewrite {
    let mut reasons = Vec::new();
    let preset = match (old.class, old.direction, old.owner_scope.as_ref()) {
        (Class::Local, Direction::None, None) => Some(Preset::Local),
        (Class::Master | Class::Shared, Direction::Down, None) => Some(Preset::Master),
        (Class::Master, Direction::Down, Some(key)) => {
            reasons.push(Refusal::OwnerScopeOnMaster {
                key_type: key.clone(),
            });
            None
        }
        (Class::Transactional, Direction::Up, Some(key)) => {
            reasons.push(Refusal::OwnerKeyNeedsField {
                key_type: key.clone(),
            });
            None
        }
        (Class::Shared, Direction::Up, Some(key)) => {
            reasons.push(Refusal::OwnerKeyNeedsField {
                key_type: key.clone(),
            });
            reasons.push(Refusal::KeyHolderNotProvable {
                key_type: key.clone(),
            });
            None
        }
        (Class::Shared, Direction::Down, Some(key)) => {
            reasons.push(Refusal::KeyedSharedDown {
                key_type: key.clone(),
            });
            None
        }
        (Class::Shared, Direction::Both, Some(key)) => {
            reasons.push(Refusal::SharedBoth {
                key_type: key.clone(),
                candidates: vec![
                    "preset = \"handover\"".to_owned(),
                    format!(
                        "preset = \"record\"\nowner_key = \"<field>@{key}\"\n\
                         relay = \"<selector>\""
                    ),
                ],
            });
            None
        }
        _ => {
            reasons.push(Refusal::NotALegalDeclaration);
            None
        }
    };
    if when_empty != WhenEmpty::None {
        reasons.push(Refusal::WhenEmptyAwayFromDefault { set: when_empty });
    }
    let Some(preset) = preset.filter(|_| reasons.is_empty()) else {
        return Rewrite::NeedsHuman { reasons };
    };

    let mut candidate = PresetPolicy::new(preset);
    candidate.excluded_fields.local = old.exclude_fields.iter().cloned().collect();
    match candidate.validate() {
        Ok(()) => Rewrite::Mechanical(Box::new(candidate)),
        Err(issues) => Rewrite::NeedsHuman {
            reasons: vec![Refusal::CandidateInvalid {
                codes: issues.iter().map(|i| i.code).collect(),
            }],
        },
    }
}
