//! What a policy change does (ADR-063 section 7), for the migration plan.
//!
//! [`changes`] compares two policies of one DocType and lists the effects, in a fixed order.
//! It reads only the two policies: counting the records a backfill will send, and whether data
//! exists, are the caller's (the destructive test is [`PresetChange::is_destructive`]).

use super::{Preset, PresetPolicy, Selector, WhenEmpty};

/// One effect of changing a DocType's policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PresetChange {
    /// `local` to a synced preset: existing records are sent through the bootstrap path
    /// (ADR-066). Allowed.
    BackfillRequired {
        /// The preset the DocType moves to.
        to: Preset,
    },
    /// The preset changes between two synced presets, so the owner class changes (for instance
    /// `master` to `record`). Destructive when data exists: it needs an explicit ownership
    /// migration plan and approval, and the platform refuses it as a routine edit.
    OwnerClassChange {
        /// The preset before.
        from: Preset,
        /// The preset after.
        to: Preset,
    },
    /// The scope no longer reaches nodes it reached. Existing mirrors stay, flagged.
    ScopeNarrowed,
    /// The scope reaches nodes it did not reach. The new nodes are backfilled.
    ScopeWidened,
    /// A synced preset back to `local`. Existing mirrors remain as read-only copies, flagged.
    SyncStopped,
}

impl PresetChange {
    /// Whether the change is destructive given whether the DocType already holds records.
    /// Only an owner class change is, and only once data exists.
    #[must_use]
    pub fn is_destructive(&self, data_exists: bool) -> bool {
        data_exists && matches!(self, PresetChange::OwnerClassChange { .. })
    }
}

/// The effects of changing `old` into `new`, in the order: sync stopped / backfill / owner class,
/// then scope narrowed, then scope widened. An identical policy gives an empty list.
#[must_use]
pub fn changes(old: &PresetPolicy, new: &PresetPolicy) -> Vec<PresetChange> {
    let mut out = Vec::new();
    match (old.preset.is_synced(), new.preset.is_synced()) {
        (false, false) => return out,
        (true, false) => {
            out.push(PresetChange::SyncStopped);
            return out;
        }
        (false, true) => {
            out.push(PresetChange::BackfillRequired { to: new.preset });
            return out;
        }
        (true, true) => {}
    }
    if old.preset != new.preset {
        out.push(PresetChange::OwnerClassChange {
            from: old.preset,
            to: new.preset,
        });
    }
    let (narrowed, widened) = scope_movement(old, new);
    if narrowed {
        out.push(PresetChange::ScopeNarrowed);
    }
    if widened {
        out.push(PresetChange::ScopeWidened);
    }
    out
}

/// Whether the reach of the scope got smaller and whether it got larger. Two unrelated selectors
/// do both: some nodes leave and others join.
fn scope_movement(old: &PresetPolicy, new: &PresetPolicy) -> (bool, bool) {
    match (&old.scope, &new.scope) {
        (None, None) => (false, false),
        (None, Some(_)) => (false, true),
        (Some(_), None) => (true, false),
        (Some(a), Some(b)) if a == b => match (old.when_empty, new.when_empty, a) {
            // for a key selector the empty-field rule decides who gets records with no value
            (WhenEmpty::All, WhenEmpty::None, Selector::Key { .. }) => (true, false),
            (WhenEmpty::None, WhenEmpty::All, Selector::Key { .. }) => (false, true),
            _ => (false, false),
        },
        (Some(Selector::All), Some(_)) => (true, false),
        (Some(_), Some(Selector::All)) => (false, true),
        (Some(_), Some(_)) => (true, true),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::policy::{FieldName, ScopeKeyType};

    fn key_scope(field: &str) -> Selector {
        Selector::Key {
            field: FieldName::new(field).expect("field"),
            key_type: ScopeKeyType::new("company").expect("kt"),
        }
    }

    fn with_scope(preset: Preset, scope: Option<Selector>) -> PresetPolicy {
        let mut p = PresetPolicy::new(preset);
        p.scope = scope;
        p
    }

    #[test]
    fn local_to_each_synced_preset_requires_a_backfill() {
        for to in [Preset::Master, Preset::Record, Preset::Handover] {
            assert_eq!(
                changes(&PresetPolicy::local(), &PresetPolicy::new(to)),
                vec![PresetChange::BackfillRequired { to }]
            );
        }
    }

    #[test]
    fn any_synced_preset_back_to_local_stops_sync() {
        for from in [Preset::Master, Preset::Record, Preset::Handover] {
            assert_eq!(
                changes(&PresetPolicy::new(from), &PresetPolicy::local()),
                vec![PresetChange::SyncStopped]
            );
        }
    }

    #[test]
    fn changing_the_owner_class_is_destructive_once_data_exists() {
        let change = changes(
            &PresetPolicy::new(Preset::Master),
            &PresetPolicy::new(Preset::Record),
        );
        // master (scope all) to record (no scope) also narrows what the nodes receive
        assert_eq!(
            change,
            vec![
                PresetChange::OwnerClassChange {
                    from: Preset::Master,
                    to: Preset::Record
                },
                PresetChange::ScopeNarrowed
            ]
        );
        assert!(change[0].is_destructive(true));
        assert!(!change[0].is_destructive(false));
        assert!(!PresetChange::ScopeNarrowed.is_destructive(true));
        assert!(!PresetChange::BackfillRequired { to: Preset::Master }.is_destructive(true));

        let h = changes(
            &PresetPolicy::new(Preset::Record),
            &PresetPolicy::new(Preset::Handover),
        );
        assert_eq!(
            h,
            vec![PresetChange::OwnerClassChange {
                from: Preset::Record,
                to: Preset::Handover
            }]
        );
    }

    #[test]
    fn scope_narrowing_and_widening() {
        let all = with_scope(Preset::Master, Some(Selector::All));
        let group = with_scope(Preset::Master, Some(Selector::Group("north".to_owned())));
        let none = with_scope(Preset::Master, None);
        assert_eq!(changes(&all, &group), vec![PresetChange::ScopeNarrowed]);
        assert_eq!(changes(&group, &all), vec![PresetChange::ScopeWidened]);
        assert_eq!(changes(&all, &none), vec![PresetChange::ScopeNarrowed]);
        assert_eq!(changes(&none, &group), vec![PresetChange::ScopeWidened]);
        // two unrelated selectors: some nodes leave, others join
        let other = with_scope(Preset::Master, Some(key_scope("company")));
        assert_eq!(
            changes(&group, &other),
            vec![PresetChange::ScopeNarrowed, PresetChange::ScopeWidened]
        );
    }

    #[test]
    fn the_empty_field_rule_moves_the_reach_of_a_key_scope() {
        let mut a = with_scope(Preset::Master, Some(key_scope("company")));
        a.when_empty = WhenEmpty::All;
        let mut b = a.clone();
        b.when_empty = WhenEmpty::None;
        assert_eq!(changes(&a, &b), vec![PresetChange::ScopeNarrowed]);
        assert_eq!(changes(&b, &a), vec![PresetChange::ScopeWidened]);
        // ignored for a selector that never reads a field
        a.scope = Some(Selector::All);
        b.scope = Some(Selector::All);
        assert_eq!(changes(&a, &b), vec![]);
    }

    #[test]
    fn nothing_that_does_not_move_sync_is_a_change() {
        let mut a = PresetPolicy::new(Preset::Master);
        assert_eq!(changes(&a, &a.clone()), vec![]);
        assert_eq!(
            changes(&PresetPolicy::local(), &PresetPolicy::local()),
            vec![]
        );
        a.stale_policy = super::super::StalePolicy::Warn;
        assert_eq!(changes(&a, &PresetPolicy::new(Preset::Master)), vec![]);
    }
}
