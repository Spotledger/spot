//! The ADR-063 preset model (WP-SYN-01.T1): what a DocType's sync policy *is* once ADR-063 is
//! accepted, as a value that is validated for internal consistency and readable as a sentence.
//!
//! This is **additive** to the EDG-08 [`crate::SyncPolicy`]: no existing type, function, text form
//! or caller changes. The preset policy has its own DocType property ([`SYNC_PRESET_KEY`]) so the
//! old `sync_policy` property is untouched. Mapping the old vocabulary onto this one is the
//! lead's decision and is listed under `## Open questions` in the task report, not done here.
//!
//! * [`Preset`], [`Selector`], [`AdoptMode`], [`WhenEmpty`], [`Obligation`], [`StalePolicy`],
//!   [`ExcludedFields`] and [`ClosureOverrides`] are the vocabulary of ADR-063 section 1.
//! * [`PresetPolicy`] holds every setting of the section 1 table with its documented default.
//! * [`PresetPolicy::validate`] checks the rules that need no catalog ([`validate`]); the
//!   catalog-wide lints L1 to L7 are WP-SYN-01.T2.
//! * [`PresetPolicy::parse_text`] / [`PresetPolicy::to_text`] are the metadata text form
//!   ([`text`]).
//! * [`PresetPolicy::human_reading`] returns translation keys with parameters ([`reading`]).
//! * [`changes`] lists the effects of a policy change, ADR-063 section 7 ([`change`]).

pub mod change;
pub mod reading;
pub mod text;
pub mod validate;

use std::fmt;

use crate::policy::{FieldName, ScopeKeyType};

pub use change::{PresetChange, changes};
pub use reading::{Names, ReadingPart};
pub use text::SYNC_PRESET_KEY;
pub use validate::{PresetIssue, codes};

/// Declares a small closed enum with a stable text form.
macro_rules! text_enum {
    (
        $(#[$meta:meta])*
        $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident => $text:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum $name {
            $( $(#[$vmeta])* $variant ),+
        }

        impl $name {
            /// The text form used in metadata.
            #[must_use]
            pub fn as_str(self) -> &'static str {
                match self { $( $name::$variant => $text ),+ }
            }

            /// Parses the text form. Case-sensitive.
            #[must_use]
            pub fn parse(text: &str) -> Option<Self> {
                match text { $( $text => Some($name::$variant), )+ _ => None }
            }

            /// The accepted texts, for error messages.
            pub const TEXTS: &'static [&'static str] = &[ $( $text ),+ ];
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

text_enum! {
    /// The four presets of ADR-063 section 1. Everything else is an optional setting on one.
    Preset {
        /// Owned by the node where it lives; never sent. **The default.**
        Local => "local",
        /// Owned by the hub; sent down to nodes in scope.
        Master => "master",
        /// Owned by the node that created it; sent up to the hub.
        Record => "record",
        /// Owned by the creating node until the hub adopts it, then by the hub.
        Handover => "handover",
    }
}

impl Preset {
    /// Every preset, in declaration order.
    pub const ALL: [Preset; 4] = [
        Preset::Local,
        Preset::Master,
        Preset::Record,
        Preset::Handover,
    ];

    /// Whether the preset syncs at all (every preset but [`Preset::Local`]).
    #[must_use]
    pub fn is_synced(self) -> bool {
        !matches!(self, Preset::Local)
    }
}

text_enum! {
    /// What `adopt` does for a `handover` record (ADR-063 section 5 and amendment 1).
    AdoptMode {
        /// The hub reviews records created at branches and adopts them by hand. **The default.**
        Manual => "manual",
        /// The hub issues the adoption request as soon as the record arrives.
        Auto => "auto",
        /// The record stays owned by its creating node for ever.
        Never => "never",
    }
}

text_enum! {
    /// What a scope selector does when the field it reads is empty (ADR-063 section 1).
    WhenEmpty {
        /// Every edge in the selector's class receives the record. **The default.**
        All => "all",
        /// No node receives it.
        None => "none",
    }
}

text_enum! {
    /// Delivery obligation (ADR-065 section 2).
    Obligation {
        /// Delivered when the node connects; lag raises an alert, nothing is tracked.
        Normal => "normal",
        /// Each version is an obligation with a deadline and evidence of delivery.
        Mandatory => "mandatory",
    }
}

text_enum! {
    /// What posting does on a node that is stale for a freshness class (ADR-065 section 4).
    StalePolicy {
        /// The document posts and is stamped stale. **The default.**
        AllowStamp => "allow_stamp",
        /// The user must confirm a plain warning.
        Warn => "warn",
        /// Posting is refused, unless a supervisor overrides.
        Block => "block",
    }
}

/// A scope key reference: the field of the record that names the key, and the key type it is
/// looked up under. Used by [`Selector::Key`] and by `owner_key`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct KeyRef {
    /// The record field whose value is the scope key value.
    pub field: FieldName,
    /// The registered key type, for example `warehouse`.
    pub key_type: ScopeKeyType,
}

/// A scope selector (ADR-063 section 3): which nodes a record version goes to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Selector {
    /// Every edge.
    All,
    /// Edges with this group tag.
    Group(String),
    /// Edges holding the scope key `(key_type, <value of field in the record>)`.
    Key {
        /// The record field whose value names the key.
        field: FieldName,
        /// The registered key type.
        key_type: ScopeKeyType,
    },
}

/// Fields that never leave the origin (`local`), never go down (`down`) or never go up (`up`).
///
/// Lists keep their written order and duplicates, so [`PresetPolicy::validate`] can report them;
/// a field may appear in only one of the three lists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExcludedFields {
    /// Never leave the origin.
    pub local: Vec<FieldName>,
    /// Never go down to a node.
    pub down: Vec<FieldName>,
    /// Never go up to the hub.
    pub up: Vec<FieldName>,
}

impl ExcludedFields {
    /// Whether nothing is excluded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.local.is_empty() && self.down.is_empty() && self.up.is_empty()
    }

    /// Every entry of the three lists, in the order local, down, up.
    pub fn iter_all(&self) -> impl Iterator<Item = &FieldName> {
        self.local
            .iter()
            .chain(self.down.iter())
            .chain(self.up.iter())
    }
}

/// Dependency-closure overrides (ADR-064 section 4). Each is optional; an absent value means the
/// platform's setting applies (ADR-064 states 500 records, 4 MB and 5,000 as starting values,
/// which are tunable settings and so are never hidden constants here).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClosureOverrides {
    /// Most records one bundle's closure may hold before it is split.
    pub max_records: Option<u32>,
    /// Most bytes of images one bundle's closure may hold before it is split.
    pub max_bytes: Option<u64>,
    /// A closure larger than this many records is a condition, not a split.
    pub hard_limit_records: Option<u32>,
}

impl ClosureOverrides {
    /// Whether no override is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// One DocType's sync policy under ADR-063: a preset and its optional settings.
///
/// Settings whose default depends on the preset are stored as explicit options and read through
/// an accessor ([`PresetPolicy::adopt_mode`], [`PresetPolicy::obligation_class`]), so
/// [`PresetPolicy::validate`] can tell "set where it does not belong" from "left at the default".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresetPolicy {
    /// The preset.
    pub preset: Preset,
    /// Which nodes receive the record. `Some(All)` for a `master` by default, `None` otherwise.
    pub scope: Option<Selector>,
    /// For `record` and `handover`: the scope key the creating node must hold.
    pub owner_key: Option<KeyRef>,
    /// For `record` only: the selector that passes each version on to counterparties.
    pub relay: Option<Selector>,
    /// Fields that never leave the origin, never go down, or never go up.
    pub excluded_fields: ExcludedFields,
    /// For `handover` only: `None` reads as [`AdoptMode::Manual`].
    pub adopt: Option<AdoptMode>,
    /// What a selector does when its field is empty.
    pub when_empty: WhenEmpty,
    /// `None` reads as the preset's default: mandatory for `record`, normal otherwise.
    pub obligation: Option<Obligation>,
    /// A date field the record is in force from (ADR-065 section 6). Needs `Mandatory`.
    pub effective_from_field: Option<FieldName>,
    /// What posting does on a stale node.
    pub stale_policy: StalePolicy,
    /// Dependency closure overrides.
    pub closure: ClosureOverrides,
}

impl PresetPolicy {
    /// A policy with the documented defaults of `preset`: scope `all` for a `master` and none
    /// otherwise, `when_empty = none`, `stale_policy = allow_stamp`, nothing else set.
    #[must_use]
    pub fn new(preset: Preset) -> Self {
        Self {
            preset,
            scope: (preset == Preset::Master).then_some(Selector::All),
            owner_key: None,
            relay: None,
            excluded_fields: ExcludedFields::default(),
            adopt: None,
            when_empty: WhenEmpty::None,
            obligation: None,
            effective_from_field: None,
            stale_policy: StalePolicy::AllowStamp,
            closure: ClosureOverrides::default(),
        }
    }

    /// The default policy: `local`. *Nothing syncs unless declared* (ADR-062 rule 1).
    #[must_use]
    pub fn local() -> Self {
        Self::new(Preset::Local)
    }

    /// The adoption mode in force: the setting, else [`AdoptMode::Manual`].
    #[must_use]
    pub fn adopt_mode(&self) -> AdoptMode {
        self.adopt.unwrap_or(AdoptMode::Manual)
    }

    /// The obligation in force: the setting, else the preset's default (ADR-065 section 2).
    #[must_use]
    pub fn obligation_class(&self) -> Obligation {
        self.obligation.unwrap_or(match self.preset {
            Preset::Record => Obligation::Mandatory,
            _ => Obligation::Normal,
        })
    }
}

impl Default for PresetPolicy {
    fn default() -> Self {
        Self::local()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn local_is_the_default_and_syncs_nothing() {
        let p = PresetPolicy::default();
        assert_eq!(p.preset, Preset::Local);
        assert!(!p.preset.is_synced());
        assert_eq!(p.scope, None);
    }

    #[test]
    fn defaults_per_preset_are_pinned() {
        let master = PresetPolicy::new(Preset::Master);
        assert_eq!(master.scope, Some(Selector::All));
        assert_eq!(master.obligation_class(), Obligation::Normal);

        let record = PresetPolicy::new(Preset::Record);
        assert_eq!(record.scope, None);
        assert_eq!(record.relay, None);
        assert_eq!(record.obligation_class(), Obligation::Mandatory);

        let handover = PresetPolicy::new(Preset::Handover);
        assert_eq!(handover.scope, None);
        assert_eq!(handover.adopt_mode(), AdoptMode::Manual);
        assert_eq!(handover.obligation_class(), Obligation::Normal);

        for preset in Preset::ALL {
            let p = PresetPolicy::new(preset);
            assert_eq!(p.when_empty, WhenEmpty::None, "{preset}");
            assert_eq!(p.stale_policy, StalePolicy::AllowStamp, "{preset}");
            assert!(p.excluded_fields.is_empty(), "{preset}");
            assert!(p.closure.is_empty(), "{preset}");
            assert_eq!(p.owner_key, None, "{preset}");
            assert_eq!(p.effective_from_field, None, "{preset}");
        }
    }

    #[test]
    fn enum_text_forms_round_trip() {
        for preset in Preset::ALL {
            assert_eq!(Preset::parse(preset.as_str()), Some(preset));
        }
        for text in AdoptMode::TEXTS {
            assert_eq!(AdoptMode::parse(text).map(AdoptMode::as_str), Some(*text));
        }
        for text in StalePolicy::TEXTS {
            assert_eq!(
                StalePolicy::parse(text).map(StalePolicy::as_str),
                Some(*text)
            );
        }
        assert_eq!(Preset::parse("Master"), None);
        assert_eq!(WhenEmpty::parse("some"), None);
        assert_eq!(Obligation::parse("mandatory"), Some(Obligation::Mandatory));
    }
}
