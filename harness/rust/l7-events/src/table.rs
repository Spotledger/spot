//! The per-topic table: events seen, schema family, whether they conformed.

use std::collections::BTreeMap;

use serde::Serialize;
use spot_events_contract::PayloadDocument;
use spot_storage_contract::OutboxRecord;

use crate::classify::{self, Family};

/// One topic's row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TopicRow {
    /// The topic.
    pub topic: String,
    /// Events seen.
    pub events: u64,
    /// The schema family (`none` when there is no schema).
    pub family: String,
    /// Events that validated (or `events` when the topic has no schema to fail).
    pub ok: u64,
    /// The topic has a registry entry but no schema anywhere.
    pub unschematised: bool,
}

/// The table over the whole outbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TopicTable {
    /// The rows, by topic.
    pub rows: Vec<TopicRow>,
}

impl TopicTable {
    /// Builds the table from the outbox.
    pub fn of(records: &[OutboxRecord]) -> TopicTable {
        let mut rows: BTreeMap<String, TopicRow> = BTreeMap::new();
        for rec in records {
            let topic = rec.event.topic.as_str().to_owned();
            let family = classify::family_of(&topic);
            let row = rows.entry(topic.clone()).or_insert_with(|| TopicRow {
                topic: topic.clone(),
                events: 0,
                family: family.name().to_owned(),
                ok: 0,
                unschematised: family == Family::None
                    && spot_events_contract::registry::lookup(&topic).is_some(),
            });
            row.events += 1;
            let valid = PayloadDocument::from_outbox_event(&rec.event)
                .ok()
                .is_some_and(|d| classify::validate(&topic, &d.data).is_ok());
            if valid {
                row.ok += 1;
            }
        }
        TopicTable {
            rows: rows.into_values().collect(),
        }
    }

    /// The table as text.
    pub fn to_text(&self) -> String {
        let mut out = String::from(
            "topic                          events  family           ok  unschematised\n",
        );
        for r in &self.rows {
            out.push_str(&format!(
                "{:<30} {:>6}  {:<15} {:>4}  {}\n",
                r.topic, r.events, r.family, r.ok, r.unschematised
            ));
        }
        out
    }

    /// The table as JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}
