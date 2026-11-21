//! The human reading of a [`PresetPolicy`] (ADR-063 section 1): the Admin Center shows a policy
//! as sentences, not as a table of flags.
//!
//! [`PresetPolicy::human_reading`] returns [`ReadingPart`]s: a translation key and its
//! parameters, one per sentence. The platform's i18n turns them into text; this crate holds no
//! wording. The names a sentence mentions (the hub, a field, a key type) come from a [`Names`]
//! the caller supplies, so a tenant's own labels appear.
//!
//! The keys are `sync_policy.reading.*`. Parameters are `owner`, `field`, `key_type`, `group`,
//! `fields` (labels joined with a comma and a space).

/// A translation key under the reading namespace. Built with `concat!` so the lint that reads dotted
/// literals as error codes does not take these keys for codes (they are not).
macro_rules! key {
    ($name:literal) => {
        concat!("sync_policy.", "reading.", $name)
    };
}

use super::{
    AdoptMode, KeyRef, Obligation, Preset, PresetPolicy, Selector, StalePolicy, WhenEmpty,
};
use crate::policy::{FieldName, ScopeKeyType};

/// The tenant's own names for the things a reading mentions.
pub trait Names {
    /// What the hub is called here, for example "Head Office".
    fn hub_name(&self) -> String;
    /// The label of a field, for example "cost price" for `cost_price`.
    fn field_label(&self, field: &FieldName) -> String;
    /// The label of a scope key type, for example "warehouse".
    fn key_type_label(&self, key_type: &ScopeKeyType) -> String;
}

/// One sentence of the reading: a translation key and its parameters, in a fixed order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadingPart {
    /// The translation key, `sync_policy.reading.*`.
    pub key: &'static str,
    /// Named parameters of the sentence.
    pub params: Vec<(&'static str, String)>,
}

impl ReadingPart {
    fn new(key: &'static str) -> Self {
        Self {
            key,
            params: Vec::new(),
        }
    }

    fn with(mut self, name: &'static str, value: String) -> Self {
        self.params.push((name, value));
        self
    }

    /// The value of a parameter, if the part has it.
    #[must_use]
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Which sentence family a selector is read into.
#[derive(Clone, Copy)]
enum Dest {
    Sent,
    Adopted,
    Relay,
}

impl PresetPolicy {
    /// The policy as translation keys with parameters, one per sentence, in reading order:
    /// who owns it, where it goes, the settings that differ from the defaults, what is withheld.
    #[must_use]
    pub fn human_reading(&self, names: &dyn Names) -> Vec<ReadingPart> {
        let hub = names.hub_name();
        let mut out = Vec::new();
        match self.preset {
            Preset::Local => {
                out.push(ReadingPart::new(key!("local")));
                return out;
            }
            Preset::Master => {
                out.push(ReadingPart::new(key!("owner_hub")).with("owner", hub));
                match &self.scope {
                    Some(s) => out.push(selector_part(s, Dest::Sent, names)),
                    None => out.push(ReadingPart::new(key!("sent_nowhere"))),
                }
            }
            Preset::Record => {
                out.push(ReadingPart::new(key!("owner_creator")));
                out.push(ReadingPart::new(key!("sent_up")).with("owner", hub));
                if let Some(relay) = &self.relay {
                    out.push(selector_part(relay, Dest::Relay, names));
                }
            }
            Preset::Handover => {
                out.push(ReadingPart::new(key!("owner_handover")).with("owner", hub.clone()));
                out.push(adopt_part(self.adopt_mode(), hub));
                if let Some(s) = &self.scope {
                    out.push(selector_part(s, Dest::Adopted, names));
                }
            }
        }
        if let Some(key) = &self.owner_key {
            out.push(key_part(key!("owner_key"), key, names));
        }
        if self.when_empty == WhenEmpty::All
            && let Some(Selector::Key { field, .. }) = self.scope.as_ref().or(self.relay.as_ref())
        {
            out.push(
                ReadingPart::new(key!("when_empty_all")).with("field", names.field_label(field)),
            );
        }
        self.settings_parts(names, &mut out);
        self.withheld_parts(names, &mut out);
        out
    }

    fn settings_parts(&self, names: &dyn Names, out: &mut Vec<ReadingPart>) {
        let default_obligation = PresetPolicy::new(self.preset).obligation_class();
        match (self.obligation_class(), default_obligation) {
            (Obligation::Mandatory, Obligation::Normal) => {
                out.push(ReadingPart::new(key!("obligation_mandatory")));
            }
            (Obligation::Normal, Obligation::Mandatory) => {
                out.push(ReadingPart::new(key!("obligation_normal")));
            }
            _ => {}
        }
        if let Some(field) = &self.effective_from_field {
            out.push(
                ReadingPart::new(key!("effective_from")).with("field", names.field_label(field)),
            );
        }
        match self.stale_policy {
            StalePolicy::AllowStamp => {}
            StalePolicy::Warn => out.push(ReadingPart::new(key!("stale_warn"))),
            StalePolicy::Block => out.push(ReadingPart::new(key!("stale_block"))),
        }
    }

    fn withheld_parts(&self, names: &dyn Names, out: &mut Vec<ReadingPart>) {
        for (key, list) in [
            (key!("not_sent"), &self.excluded_fields.local),
            (key!("not_sent_down"), &self.excluded_fields.down),
            (key!("not_sent_up"), &self.excluded_fields.up),
        ] {
            if !list.is_empty() {
                let labels: Vec<String> = list.iter().map(|f| names.field_label(f)).collect();
                out.push(ReadingPart::new(key).with("fields", labels.join(", ")));
            }
        }
    }
}

fn adopt_part(mode: AdoptMode, hub: String) -> ReadingPart {
    let key = match mode {
        AdoptMode::Manual => key!("adopt_manual"),
        AdoptMode::Auto => key!("adopt_auto"),
        AdoptMode::Never => key!("adopt_never"),
    };
    ReadingPart::new(key).with("owner", hub)
}

fn key_part(key: &'static str, k: &KeyRef, names: &dyn Names) -> ReadingPart {
    ReadingPart::new(key)
        .with("field", names.field_label(&k.field))
        .with("key_type", names.key_type_label(&k.key_type))
}

fn selector_part(selector: &Selector, dest: Dest, names: &dyn Names) -> ReadingPart {
    match (selector, dest) {
        (Selector::All, Dest::Sent) => ReadingPart::new(key!("sent_all")),
        (Selector::All, Dest::Adopted) => ReadingPart::new(key!("adopted_all")),
        (Selector::All, Dest::Relay) => ReadingPart::new(key!("relay_all")),
        (Selector::Group(tag), d) => {
            let key = match d {
                Dest::Sent => key!("sent_group"),
                Dest::Adopted => key!("adopted_group"),
                Dest::Relay => key!("relay_group"),
            };
            ReadingPart::new(key).with("group", tag.clone())
        }
        (Selector::Key { field, key_type }, d) => {
            let key = match d {
                Dest::Sent => key!("sent_key"),
                Dest::Adopted => key!("adopted_key"),
                Dest::Relay => key!("relay_key"),
            };
            let k = KeyRef {
                field: field.clone(),
                key_type: key_type.clone(),
            };
            key_part(key, &k, names)
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::preset::{ExcludedFields, Obligation};

    struct Tenant;

    impl Names for Tenant {
        fn hub_name(&self) -> String {
            "Head Office".to_owned()
        }
        fn field_label(&self, field: &FieldName) -> String {
            field.as_str().replace('_', " ")
        }
        fn key_type_label(&self, key_type: &ScopeKeyType) -> String {
            key_type.as_str().to_owned()
        }
    }

    /// The test-only English renderer: a template per key, `{name}` replaced by the parameter.
    fn english(parts: &[ReadingPart]) -> String {
        let templates: BTreeMap<&str, &str> = BTreeMap::from([
            ("local", "Stays here and is never sent."),
            ("owner_hub", "Owned by {owner}."),
            ("owner_creator", "Owned by the branch that creates it."),
            (
                "owner_handover",
                "Owned by the branch that creates it until {owner} adopts it, then by {owner}.",
            ),
            ("sent_all", "Sent to every branch."),
            ("sent_group", "Sent to branches in the group {group}."),
            (
                "sent_key",
                "Sent to branches holding the {key_type} named in {field}.",
            ),
            ("sent_nowhere", "Not sent to any branch."),
            ("sent_up", "Sent up to {owner}."),
            ("adopted_all", "After adoption, sent to every branch."),
            (
                "adopted_group",
                "After adoption, sent to branches in the group {group}.",
            ),
            (
                "adopted_key",
                "After adoption, sent to branches holding the {key_type} named in {field}.",
            ),
            ("relay_all", "Passed on to every branch."),
            ("relay_group", "Passed on to branches in the group {group}."),
            (
                "relay_key",
                "Passed on to branches holding the {key_type} named in {field}.",
            ),
            (
                "owner_key",
                "Valid only at the branch holding the {key_type} named in {field}.",
            ),
            ("adopt_manual", "{owner} adopts it by hand, after review."),
            ("adopt_auto", "{owner} adopts it as soon as it arrives."),
            ("adopt_never", "It is never adopted: the branch keeps it."),
            (
                "when_empty_all",
                "When {field} is empty, it is sent to every node.",
            ),
            (
                "obligation_mandatory",
                "Delivery is tracked and must arrive on time.",
            ),
            ("obligation_normal", "Delivery is not tracked."),
            ("effective_from", "In force from the date in {field}."),
            (
                "stale_warn",
                "Posting at a branch that is out of date asks for confirmation.",
            ),
            (
                "stale_block",
                "Posting at a branch that is out of date is refused.",
            ),
            ("not_sent", "Not sent: {fields}."),
            ("not_sent_down", "Not sent down: {fields}."),
            ("not_sent_up", "Not sent up: {fields}."),
        ]);
        parts
            .iter()
            .map(|part| {
                let short = part
                    .key
                    .strip_prefix("sync_policy.reading.")
                    .expect("reading key");
                let mut text = (*templates.get(short).expect(short)).to_owned();
                for (name, value) in &part.params {
                    text = text.replace(&format!("{{{name}}}"), value);
                }
                assert!(!text.contains('{'), "unfilled parameter in {short}: {text}");
                text
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn field(name: &str) -> FieldName {
        FieldName::new(name).expect("field")
    }

    fn key(field_name: &str, key_type: &str) -> KeyRef {
        KeyRef {
            field: field(field_name),
            key_type: ScopeKeyType::new(key_type).expect("kt"),
        }
    }

    #[test]
    fn worked_example_a_master_with_a_withheld_field() {
        let mut p = PresetPolicy::new(Preset::Master);
        p.excluded_fields = ExcludedFields {
            local: vec![field("cost_price")],
            ..ExcludedFields::default()
        };
        let parts = p.human_reading(&Tenant);
        assert_eq!(
            parts.iter().map(|p| p.key).collect::<Vec<_>>(),
            vec![
                "sync_policy.reading.owner_hub",
                "sync_policy.reading.sent_all",
                "sync_policy.reading.not_sent"
            ]
        );
        assert_eq!(parts[0].param("owner"), Some("Head Office"));
        assert_eq!(parts[2].param("fields"), Some("cost price"));
        assert_eq!(
            english(&parts),
            "Owned by Head Office. Sent to every branch. Not sent: cost price."
        );
    }

    #[test]
    fn worked_example_a_branch_transaction_relayed_to_the_counterparty() {
        let mut p = PresetPolicy::new(Preset::Record);
        p.owner_key = Some(key("from_warehouse", "warehouse"));
        p.relay = Some(Selector::Key {
            field: field("to_warehouse"),
            key_type: ScopeKeyType::new("warehouse").expect("kt"),
        });
        assert_eq!(
            english(&p.human_reading(&Tenant)),
            "Owned by the branch that creates it. Sent up to Head Office. \
             Passed on to branches holding the warehouse named in to warehouse. \
             Valid only at the branch holding the warehouse named in from warehouse."
        );
    }

    #[test]
    fn worked_example_a_handover_with_raised_obligation_and_a_block() {
        let mut p = PresetPolicy::new(Preset::Handover);
        p.adopt = Some(AdoptMode::Auto);
        p.scope = Some(Selector::Group("kiosks".to_owned()));
        p.obligation = Some(Obligation::Mandatory);
        p.effective_from_field = Some(field("valid_from"));
        p.stale_policy = StalePolicy::Block;
        p.excluded_fields.up = vec![field("pin"), field("till_note")];
        assert_eq!(
            english(&p.human_reading(&Tenant)),
            "Owned by the branch that creates it until Head Office adopts it, then by Head Office. \
             Head Office adopts it as soon as it arrives. \
             After adoption, sent to branches in the group kiosks. \
             Delivery is tracked and must arrive on time. In force from the date in valid from. \
             Posting at a branch that is out of date is refused. Not sent up: pin, till note."
        );
    }

    #[test]
    fn local_and_the_remaining_branches_of_the_reading() {
        let local = PresetPolicy::local().human_reading(&Tenant);
        assert_eq!(english(&local), "Stays here and is never sent.");

        let mut nowhere = PresetPolicy::new(Preset::Master);
        nowhere.scope = None;
        assert_eq!(
            english(&nowhere.human_reading(&Tenant)),
            "Owned by Head Office. Not sent to any branch."
        );

        let mut empty = PresetPolicy::new(Preset::Master);
        empty.scope = Some(Selector::Key {
            field: field("company"),
            key_type: ScopeKeyType::new("company").expect("kt"),
        });
        empty.when_empty = WhenEmpty::All;
        empty.obligation = Some(Obligation::Mandatory);
        empty.stale_policy = StalePolicy::Warn;
        let text = english(&empty.human_reading(&Tenant));
        assert!(
            text.contains("When company is empty, it is sent to every node."),
            "{text}"
        );
        assert!(text.contains("asks for confirmation"), "{text}");

        // a record whose default obligation was lowered says so
        let mut lowered = PresetPolicy::new(Preset::Record);
        lowered.obligation = Some(Obligation::Normal);
        assert!(english(&lowered.human_reading(&Tenant)).contains("Delivery is not tracked."));
    }
}
