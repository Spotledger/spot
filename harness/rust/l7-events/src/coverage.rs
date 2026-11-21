//! The topics a run is expected to emit, each with the reason.

/// One topic a `standard` engine run must produce.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpectedTopic {
    /// The topic.
    pub topic: &'static str,
    /// Why this run must produce it.
    pub reason: &'static str,
}

/// The topics the L7 engine workload (the sample world: no audit step, no job engine on the
/// outbox) emits. To add a topic, append a line with the reason a run must produce it, and make
/// sure the workload really does; the registry check in the tests keeps the names honest.
pub fn expected_for_engine_world() -> Vec<ExpectedTopic> {
    [
        ("doc_created", "every create operation"),
        ("doc_updated", "update and method call operations"),
        ("doc_posted", "post of a Time Entry"),
        ("doc_cancelled", "cancel of a posted Time Entry"),
        ("doc_deleted", "delete of a Task or a Time Entry draft"),
        ("doc_renamed", "rename of a Task"),
    ]
    .into_iter()
    .map(|(topic, reason)| ExpectedTopic { topic, reason })
    .collect()
}

/// Topics the engine could emit that this world does not, with why. Not a failure when absent;
/// listed so a reader sees what is not covered.
pub const NOT_IN_THIS_WORLD: [(&str, &str); 4] = [
    (
        "sys_doc_audit",
        "the audit step is not installed in the L7 world",
    ),
    (
        "sys_job_enqueued",
        "the L7 job workload uses the in-memory store, not the outbox",
    ),
    (
        "doc_updated_posted",
        "the workload never updates a posted document",
    ),
    (
        "sys_operation_receipt",
        "receipts are written only for operations that report effects",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use spot_events_contract::registry;

    #[test]
    fn every_expected_topic_is_registered_and_listed_once() {
        let list = expected_for_engine_world();
        let mut seen = std::collections::BTreeSet::new();
        for t in &list {
            assert!(
                registry::lookup(t.topic).is_some(),
                "{} is not registered",
                t.topic
            );
            assert!(seen.insert(t.topic), "{} listed twice", t.topic);
        }
        for (topic, _) in NOT_IN_THIS_WORLD {
            assert!(
                registry::lookup(topic).is_some(),
                "{topic} is not registered"
            );
        }
    }
}
