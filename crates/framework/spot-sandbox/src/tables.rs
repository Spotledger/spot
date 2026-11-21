//! What happens to each framework table in a sandbox clone (WP-ALM-05.T4).
//!
//! A sandbox is a restore of production with the data masked. Masking (`mask`, `scan`) works on
//! the classified fields of **documents**. The platform also keeps tables that are not documents:
//! the audit chain, per-subject keys, nonces, replay guards, the job queue and outbox, mail logs,
//! link keys, coordination state, sync identity, conditions, the migration control schema. Copied
//! into a clone they are wrong (a clone claiming to be the production node) or a leak (keys,
//! nonces, mail history, the audit of real people). This module is the **registry that must be
//! complete**: every table has one of three treatments.
//!
//! - [`TableTreatment::Keep`]: definitions and configuration with no personal or financial data,
//!   copied as is.
//! - [`TableTreatment::Empty`]: truncated in the clone: identity, secret, history, queue or
//!   per-node state.
//! - [`TableTreatment::Rebuild`]: derived data (search index, embeddings, caches, feed
//!   projections), truncated and rebuilt by the clone's own services after activation.
//!
//! **A table this registry does not know is emptied** (fail towards less data) and reported as
//! unclassified; silence never makes it `Keep`. A table that holds **document rows** is not in
//! this registry: the clone knows the metadata's definitions (including child tables), and those
//! tables are governed by field classification and masking. Everything else in the schema is
//! framework state.
//!
//! `tests/tables_complete.rs` scans the repository for `CREATE TABLE` and fails when a crate adds
//! a table that is not classified here.

use std::collections::BTreeSet;

/// What a sandbox clone does with a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TableTreatment {
    /// Copied as is.
    Keep,
    /// Truncated in the clone.
    Empty,
    /// Truncated, and rebuilt by the clone's own services after activation.
    Rebuild,
}

impl TableTreatment {
    /// The word reports use.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            TableTreatment::Keep => "keep",
            TableTreatment::Empty => "empty",
            TableTreatment::Rebuild => "rebuild",
        }
    }
}

/// One table and what to do with it.
#[derive(Clone, Copy, Debug)]
pub struct TableRule {
    /// The exact name, schema-qualified when it is not in `public` (`spot_migration_control.gate`).
    pub name: &'static str,
    /// What the clone does.
    pub treatment: TableTreatment,
    /// Why, in one line.
    pub why: &'static str,
    /// A `Keep` of a name that looks sensitive (`key`, `secret`, `token`, `nonce`, `audit`,
    /// `outbox`, `mail`) must say why that is safe.
    pub allow_keep_because: Option<&'static str>,
    /// Set when the table is created by SQL the source scan cannot see (a generated name, a
    /// migration file): why it is listed without being found.
    pub external: Option<&'static str>,
}

/// A family of tables with one treatment, for names that cannot be listed exactly.
#[derive(Clone, Copy, Debug)]
pub struct PrefixRule {
    /// The name prefix (schema-qualified for a schema family).
    pub prefix: &'static str,
    /// What the clone does.
    pub treatment: TableTreatment,
    /// Why this family is a prefix and not a list.
    pub why: &'static str,
}

const fn rule(name: &'static str, treatment: TableTreatment, why: &'static str) -> TableRule {
    TableRule {
        name,
        treatment,
        why,
        allow_keep_because: None,
        external: None,
    }
}

const fn empty(name: &'static str, why: &'static str) -> TableRule {
    rule(name, TableTreatment::Empty, why)
}

const fn rebuild(name: &'static str, why: &'static str) -> TableRule {
    rule(name, TableTreatment::Rebuild, why)
}

const fn keep(name: &'static str, why: &'static str) -> TableRule {
    rule(name, TableTreatment::Keep, why)
}

impl TableRule {
    /// Marks the table as created outside what the source scan sees.
    #[must_use]
    pub const fn external(mut self, reason: &'static str) -> Self {
        self.external = Some(reason);
        self
    }
}

/// Families listed by prefix. Each prefix is a family whose members are generated, per tenant or
/// per schema, so an exact list would always be stale.
pub const PREFIX_RULES: &[PrefixRule] = &[
    PrefixRule {
        prefix: "spot_embedding",
        treatment: TableTreatment::Rebuild,
        why: "embedding tables are created per model and dimension; derived from documents",
    },
    PrefixRule {
        prefix: "spot_cache",
        treatment: TableTreatment::Rebuild,
        why: "cache tables are derived: they hold copies of values and are refilled on use",
    },
    PrefixRule {
        prefix: "spot_search_",
        treatment: TableTreatment::Rebuild,
        why: "search copies are derived from documents and rebuilt by the indexer",
    },
    PrefixRule {
        prefix: "spot_graph_edge",
        treatment: TableTreatment::Rebuild,
        why: "graph edges and their snapshots are projections of documents",
    },
    PrefixRule {
        prefix: "spot_control.",
        treatment: TableTreatment::Empty,
        why: "the control-plane schema (tenants, registry, store state) is not part of a site",
    },
    PrefixRule {
        prefix: "spot_migration_control.",
        treatment: TableTreatment::Empty,
        why: "the migration gate, snapshots and timings of the source: a copied closed gate would close the clone",
    },
    PrefixRule {
        prefix: "spot_rescue_control.",
        treatment: TableTreatment::Empty,
        why: "the rescue journal of the source: a clone starts its own",
    },
];

/// Every table the platform creates, with its treatment.
pub const TABLES: &[TableRule] = &[
    // ---- access review and separation of duties
    empty("spot_access_review_campaigns", "decisions about real people's access"),
    empty("spot_access_review_history", "who reviewed whom"),
    empty("spot_access_review_items", "the people under review"),
    empty("spot_sod_override_revocations", "who was allowed to override a rule, and who took it back"),
    empty("spot_sod_overrides", "named exceptions granted to people"),
    keep("spot_sod_rule_sets", "the rules themselves: configuration without personal data"),
    // ---- audit
    empty("spot_audit_bookmark", "audit reader positions of the source"),
    empty("spot_audit_chain", "the audit of real people; a clone starts its own chain"),
    empty("spot_audit_checkpoint", "checkpoints of the source's chain"),
    empty("spot_audit_doc_index", "index into the source's audit"),
    empty("spot_audit_event_index", "index into the source's audit"),
    empty("spot_audit_site_checkpoint", "checkpoints of the source's chain"),
    // ---- processes: instances are business data of arbitrary shape; definitions are code
    empty("spot_bpm_case", "process cases are business data until case variables can be classified"),
    empty("spot_bpm_case_var", "case variables are business data of arbitrary shape"),
    empty("spot_bpm_conversation", "conversations carry messages from real people"),
    empty("spot_bpm_conversation_rejection", "rejections of conversation messages"),
    keep("spot_bpm_definition", "process definitions: configuration"),
    keep("spot_bpm_definition_head", "which definition version is current: configuration"),
    empty("spot_bpm_node_outcome", "outcomes of case nodes"),
    empty("spot_bpm_node_run_consumer", "bookmarks of the source's node-run feed"),
    empty("spot_bpm_node_run_event", "history of case node runs"),
    empty("spot_bpm_node_run_seq", "sequence of the source's node-run feed"),
    empty("spot_bpm_task", "human tasks of cases name real people"),
    empty("spot_bpm_task_event", "history of human tasks"),
    empty("spot_bpm_trigger_mark", "trigger positions of the source"),
    // ---- change feed and projections
    rebuild("spot_capture_watermark", "position of the capture invariant over the source's feed"),
    rebuild("spot_change", "the change feed; a clone starts its own"),
    rebuild("spot_change_pub", "the change feed's publication log"),
    rebuild("spot_feed_state", "the feed's watermark and generation"),
    rebuild("spot_graph_edge", "a projection of documents"),
    rebuild("spot_search_doc", "a copy of searchable values"),
    // ---- conditions, coordination, sync identity
    empty("spot_condition_history", "history of the source's conditions"),
    empty("spot_condition_open", "the source's open conditions"),
    empty("spot_coord_counter", "coordination counters of the source cluster"),
    empty("spot_coord_lease", "leases held by the source's nodes"),
    empty("spot_coord_node", "presence of the source's nodes"),
    empty("spot_coord_rate", "rate-limit buckets of the source"),
    empty("spot_coord_token", "fencing high-water marks of the source"),
    empty("spot_schedule_mark", "schedule marks of the source"),
    empty("spot_llm_send", "the record of what the source sent to a model provider"),
    empty("spot_llm_tenant", "the source's model providers, endpoints and sealed keys"),
    empty("spot_llm_usage", "the source's model usage and budget counters"),
    empty("spot_dag_node_state", "state of the source's running graphs"),
    empty("spot_dag_run", "runs of the source's graphs"),
    empty("spot_edge_drift_entry", "drift findings between the source and branch nodes"),
    empty("spot_edge_drift_report", "drift reports of the source's branch nodes"),
    empty("spot_edge_repair_audit", "repairs made on the source's branch nodes"),
    empty("spot_node_role_state", "a clone must never be a hub or edge node: no recorded role"),
    empty("spot_partition_assignment", "which node owns which partition: per-node state"),
    empty("spot_partition_history", "history of partition ownership"),
    empty("spot_range_grant", "leased number ranges: a clone must not consume the source's grants"),
    empty("spot_range_series", "number range series of the source's nodes"),
    // ---- effects, jobs, outbox, mail
    empty("spot_effect_hold_checkpoint", "the source's hold ledger; the clone gets its own hold after masking"),
    empty("spot_effect_hold_effect", "held effects of the source"),
    empty("spot_effect_hold_sequence", "ledger sequence of the source"),
    empty("spot_effect_hold_state", "the source's hold state"),
    empty("spot_event_consumer", "consumer bookmarks into the source's event streams"),
    empty("spot_event_partition", "event stream partitions of the source"),
    empty("spot_idempotency_key", "idempotency keys of the source's callers"),
    empty("spot_job", "the source's job queue"),
    empty("spot_job_intake", "the source's job intake positions"),
    empty("spot_job_outbox_seen", "which outbox events the source's jobs took"),
    empty("spot_mail_sent_log", "who the source sent mail to"),
    empty("spot_outbox", "the source's pending events"),
    empty("spot_outbox_pub", "the source's event publication log"),
    // ---- keys, nonces, replay guards
    empty("spot_journal_subject_key", "per-subject journal keys: secrets"),
    empty("spot_key_event", "key group history"),
    empty("spot_key_group", "key groups: secrets"),
    empty("spot_key_group_epoch", "key group epochs: secrets"),
    empty("spot_key_hold", "key holds"),
    empty("spot_key_node", "which node holds which key"),
    empty("spot_key_revocation", "key revocations"),
    empty("spot_signed_command_nonce", "nonces of signed commands: replay guards of the source"),
    empty("spot_sso_replay", "single sign-on replay guards of the source"),
    // ---- tenant links and the exchange
    empty("spot_link_cursor", "positions of the source's link exchange"),
    empty("spot_link_inbox", "messages received from linked tenants"),
    empty("spot_link_inbox_conv", "conversations with linked tenants"),
    empty("spot_link_notice", "notices exchanged with linked tenants"),
    empty("spot_link_notice_ack", "acknowledgements of notices"),
    empty("spot_link_outbox", "messages queued for linked tenants"),
    empty("spot_link_outbox_event", "events behind the link outbox"),
    empty("spot_link_parked", "messages parked from linked tenants"),
    empty("spot_link_seen", "message ids seen from linked tenants"),
    empty("spot_tenant_link", "who the tenant is linked to"),
    empty("spot_tenant_link_audit_ref", "audit references of links"),
    empty("spot_tenant_link_consent", "consents given to linked tenants"),
    empty("spot_tenant_link_grant", "grants to linked tenants"),
    empty("spot_tenant_link_nonce", "nonces of link handshakes"),
    empty("spot_tenant_link_offer", "link offers: they carry secrets"),
    empty("spot_tenant_link_peer_key", "keys of linked tenants"),
    empty("spot_quarantine_item", "records the source held back: they are real business data"),
    empty("spot_quarantine_payload", "the held records' images: real business data"),
    // ---- agent memory, metering, retention, migration operations
    empty("spot_deletion_proof", "proofs of deletion of real people's data"),
    empty("spot_memory_candidate", "agent memory may hold personal data"),
    empty("spot_memory_database", "the registry of agent memory databases").external(
        "named by a constant and created through a format! placeholder in spot-agent-memory-pg",
    ),
    empty("spot_memory_episode", "agent memory may hold personal data"),
    empty("spot_memory_fact", "agent memory may hold personal data"),
    empty("spot_memory_procedure", "agent memory may hold personal data"),
    empty("spot_meter_event", "usage events of the source"),
    empty("spot_meter_member", "usage by member"),
    empty("spot_meter_period", "usage periods of the source"),
    empty("spot_meter_retention", "usage retention marks of the source"),
    empty("spot_meter_total", "usage totals of the source"),
    empty("spot_migration_approval", "who approved the source's migrations"),
    empty("spot_migration_plan", "the source's migration plans"),
    empty("spot_migration_plan_author", "who authored the source's plans"),
    empty("spot_migration_rehearsal", "rehearsals of the source's migrations"),
    empty("spot_retention_checkpoint", "retention progress of the source"),
    empty("spot_retention_outcome", "retention outcomes naming deleted data"),
    // ---- agent tiers, process effects, governance and migrations of processes
    empty("spot_agent_tier_event", "history of the source's agent registrations"),
    empty("spot_agent_tier_registration", "which agents the source registered, and as whom"),
    empty("spot_agent_tier_support_session", "support sessions opened on the source"),
    empty("spot_bpm_eff_audit", "the audit of effects run for real cases"),
    empty("spot_bpm_eff_done", "which effects of the source's cases already ran: a clone must never skip or replay them"),
    empty("spot_bpm_eff_log", "the log of effects run for real cases"),
    empty("spot_bpm_eff_request", "effect requests of real cases"),
    empty("spot_bpm_eff_undo", "undo records of effects of real cases"),
    empty("spot_bpm_gov_audit", "the audit of process governance decisions about real people"),
    empty("spot_bpm_gov_close_wanted", "close requests the source's approvers made"),
    empty("spot_bpm_gov_draft", "drafts of process changes name their authors"),
    empty("spot_bpm_gov_grant_use", "which grants real people used"),
    empty("spot_bpm_gov_request", "change requests of the source's governance"),
    empty("spot_bpm_gov_reversal", "reversals decided on the source"),
    empty("spot_bpm_migration", "migrations of the source's running cases"),
    empty("spot_bpm_migration_audit", "the audit of those migrations"),
    empty("spot_bpm_version_audit", "who published or retired which process version"),
    // ---- command ledger, connectors, webhooks
    empty("spot_command_ledger", "commands of the source's callers: replay guard and results"),
    empty("spot_connector_def", "connector endpoints of the source: a clone must not call the source's partners"),
    empty("spot_connector_run", "runs of the source's connectors"),
    empty("spot_connector_secret", "connector credentials: secrets"),
    empty("spot_webhook_intake", "webhook deliveries received by the source"),
    // ---- deep access and emergency access
    empty("spot_deep_access_consents", "consents of real people to deep access"),
    empty("spot_deep_access_credentials", "deep access credentials: secrets"),
    empty("spot_deep_access_keys", "deep access keys: secrets"),
    empty("spot_deep_access_nonces", "nonces of deep access: replay guards of the source"),
    empty("spot_deep_access_requests", "who asked for deep access to what"),
    empty("spot_deep_access_revocations", "who revoked which deep access"),
    empty("spot_emergency_access", "break-glass grants of the source"),
    empty("spot_effect_hold_payload_delete", "deletions of held effect payloads of the source"),
    // ---- journal access, rebuild, shipping and audit
    empty("spot_journal_audit_result", "results of the source's journal audits"),
    empty("spot_journal_grant", "grants to read the source's journal"),
    empty("spot_journal_rebuild_state", "rebuild progress over the source's journal"),
    empty("spot_journal_ship_state", "shipping positions of the source's journal"),
    // ---- mail
    empty("spot_mail_bounce_log", "who the source's mail bounced for"),
    empty("spot_mail_intake_cursor", "mailbox positions of the source"),
    empty("spot_mail_intake_seen", "message ids the source's intake has seen"),
    // ---- settings and single sign-on
    empty("spot_settings_history", "who changed which setting"),
    empty("spot_settings_value", "the source's setting values: they may name endpoints and accounts a clone must not use"),
    empty("spot_sso_handover", "single sign-on hand-overs of real sessions"),
    empty("spot_sso_pending", "sign-ins in flight on the source"),
    empty("spot_sso_policy", "the source's single sign-on policy: a clone starts without an identity provider"),
    empty("spot_sso_provider", "identity providers of the source, with their client secrets"),
    empty("spot_sso_saml_provider", "SAML providers of the source, with their keys"),
    empty("spot_sso_session_index", "single sign-on session indexes of real people"),
    // ---- sync: nodes, enrolment, hub queues, leases, offline identity
    empty("spot_lease_grant_log", "leases granted to the source's nodes"),
    empty("spot_leased_range", "leased ranges held by the source's edge nodes"),
    empty("spot_offline_identity_state", "offline identity state of a node: a clone must not be one"),
    empty("spot_sync_catchup_progress", "catch-up progress of the source's nodes"),
    empty("spot_sync_enrol_attempt", "enrolment attempts on the source"),
    empty("spot_sync_enrol_node", "nodes enrolled with the source"),
    empty("spot_sync_enrolment", "enrolment tokens and records of the source"),
    empty("spot_sync_hub_dest_state", "delivery state of the source hub"),
    empty("spot_sync_hub_mandatory", "mandatory records queued by the source hub"),
    empty("spot_sync_hub_node_record", "records held for the source's nodes"),
    empty("spot_sync_hub_queue", "the source hub's queues: business records in flight"),
    empty("spot_sync_hub_relay", "records relayed by the source hub"),
    empty("spot_sync_node", "the source's registered nodes"),
    empty("spot_sync_node_applied", "what the source's node applied"),
    empty("spot_sync_node_cert", "node certificates: secrets"),
    empty("spot_sync_node_checkpoint", "checkpoints of the source's node"),
    empty("spot_sync_node_cursor", "cursors of the source's node"),
    empty("spot_sync_node_parked", "records the source's node parked: business data"),
    empty("spot_sync_node_repair", "repairs of the source's node"),
    empty("spot_sync_node_state", "the source node's own state: a clone must never be that node"),
    empty("spot_sync_process_definition", "process definitions received over sync by the source's node"),
    // ---- what stays
    keep("spot_installer_app", "which apps are installed: the clone runs the same apps"),
    keep("spot_installer_journal", "the install history of apps: no personal data"),
    keep("spot_installer_owned_doctype", "which app owns which DocType"),
    keep("spot_installer_site", "which version of each app the site runs"),
    keep("spot_meta", "the storage schema's own bookkeeping (catalogue version, collation)"),
    keep("spot_sequence", "numbering series positions: the clone keeps numbering consistent with its copied documents")
        .external("created through a constant name (`TABLE`) in spot-postgres sequences"),
];

/// The exact rule for `name`.
fn exact(name: &str) -> Option<&'static TableRule> {
    TABLES.iter().find(|rule| rule.name == name)
}

/// The treatment of `name`: the exact rule, else the longest matching prefix family; `None` when
/// the registry does not know the table (the clone then empties it and reports it).
#[must_use]
pub fn treatment_of(name: &str) -> Option<TableTreatment> {
    if let Some(rule) = exact(name) {
        return Some(rule.treatment);
    }
    PREFIX_RULES
        .iter()
        .filter(|rule| name.starts_with(rule.prefix))
        .max_by_key(|rule| rule.prefix.len())
        .map(|rule| rule.treatment)
}

/// A table of the clone: its schema and its name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TableRef {
    /// The schema (`public` for the site's own tables).
    pub schema: String,
    /// The table name.
    pub name: String,
}

impl TableRef {
    /// A table of the `public` schema.
    #[must_use]
    pub fn public(name: &str) -> Self {
        Self {
            schema: "public".to_owned(),
            name: name.to_owned(),
        }
    }

    /// The name the registry uses: bare in `public`, `schema.table` elsewhere.
    #[must_use]
    pub fn key(&self) -> String {
        if self.schema == "public" {
            self.name.clone()
        } else {
            format!("{}.{}", self.schema, self.name)
        }
    }
}

/// What the clone does with every table it found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TablePlan {
    /// Tables to truncate because they are `Empty`, or unknown (see `unclassified`).
    pub empty: Vec<TableRef>,
    /// Tables to truncate because they are derived and rebuilt after activation.
    pub rebuild: Vec<TableRef>,
    /// Tables copied as they are.
    pub keep: Vec<TableRef>,
    /// Document tables: governed by field classification and masking, not by this registry.
    pub documents: Vec<TableRef>,
    /// The tables the registry does not know. They are also in `empty`.
    pub unclassified: Vec<TableRef>,
}

impl TablePlan {
    /// Every table to truncate, `Empty` first.
    #[must_use]
    pub fn to_truncate(&self) -> Vec<&TableRef> {
        self.empty.iter().chain(self.rebuild.iter()).collect()
    }
}

/// Decides every table of the clone. `documents` is the set of storage names of the metadata's
/// definitions (child tables included): a `public` table in it is a document table.
#[must_use]
pub fn plan_tables(existing: &[TableRef], documents: &BTreeSet<String>) -> TablePlan {
    let mut plan = TablePlan::default();
    let mut sorted: Vec<&TableRef> = existing.iter().collect();
    sorted.sort();
    for table in sorted {
        if table.schema == "public" && documents.contains(&table.name) {
            plan.documents.push(table.clone());
            continue;
        }
        match treatment_of(&table.key()) {
            Some(TableTreatment::Keep) => plan.keep.push(table.clone()),
            Some(TableTreatment::Empty) => plan.empty.push(table.clone()),
            Some(TableTreatment::Rebuild) => plan.rebuild.push(table.clone()),
            None => {
                plan.unclassified.push(table.clone());
                plan.empty.push(table.clone());
            }
        }
    }
    plan
}

/// A `Keep` rule whose name looks sensitive without the reason that makes it safe.
#[must_use]
pub fn keep_without_reason() -> Vec<&'static str> {
    const SENSITIVE: [&str; 7] = ["key", "secret", "token", "nonce", "audit", "outbox", "mail"];
    TABLES
        .iter()
        .filter(|rule| rule.treatment == TableTreatment::Keep)
        .filter(|rule| SENSITIVE.iter().any(|word| rule.name.contains(word)))
        .filter(|rule| rule.allow_keep_because.is_none())
        .map(|rule| rule.name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_treatment_is_reachable_and_unknown_is_none() {
        assert_eq!(treatment_of("spot_meta"), Some(TableTreatment::Keep));
        assert_eq!(
            treatment_of("spot_audit_chain"),
            Some(TableTreatment::Empty)
        );
        assert_eq!(
            treatment_of("spot_search_doc"),
            Some(TableTreatment::Rebuild)
        );
        assert_eq!(treatment_of("spot_test_unknown"), None);
        assert_eq!(treatment_of(""), None);
    }

    #[test]
    fn schema_qualified_names_are_exact_and_prefixes_follow_the_longest_match() {
        assert_eq!(
            treatment_of("spot_migration_control.gate"),
            Some(TableTreatment::Empty)
        );
        assert_eq!(
            treatment_of("spot_control.tenant"),
            Some(TableTreatment::Empty)
        );
        // `spot_audit_chain` is a table of `public`; the same name in another schema is unknown.
        assert_eq!(treatment_of("other.spot_audit_chain"), None);
        assert_eq!(
            treatment_of("spot_embedding_384"),
            Some(TableTreatment::Rebuild)
        );
        assert_eq!(
            treatment_of("spot_cache_entries"),
            Some(TableTreatment::Rebuild)
        );
    }

    #[test]
    fn names_are_unique_and_prefixes_do_not_shadow_exact_rules() {
        let mut seen = BTreeSet::new();
        for rule in TABLES {
            assert!(seen.insert(rule.name), "{} is listed twice", rule.name);
            assert!(!rule.why.is_empty(), "{}", rule.name);
        }
    }

    #[test]
    fn a_keep_of_a_sensitive_name_needs_a_reason() {
        assert_eq!(keep_without_reason(), Vec::<&str>::new());
    }

    #[test]
    fn the_plan_separates_documents_framework_state_and_the_unknown() {
        let documents: BTreeSet<String> = ["task".to_owned(), "task_item".to_owned()].into();
        let existing = vec![
            TableRef::public("task"),
            TableRef::public("task_item"),
            TableRef::public("spot_meta"),
            TableRef::public("spot_job"),
            TableRef::public("spot_search_doc"),
            TableRef::public("spot_test_unknown"),
            TableRef {
                schema: "spot_migration_control".to_owned(),
                name: "gate".to_owned(),
            },
        ];
        let plan = plan_tables(&existing, &documents);
        let keys = |tables: &[TableRef]| tables.iter().map(TableRef::key).collect::<Vec<_>>();
        assert_eq!(keys(&plan.documents), ["task", "task_item"]);
        assert_eq!(keys(&plan.keep), ["spot_meta"]);
        // Sorted by schema, then name: `public` comes before `spot_migration_control`.
        assert_eq!(
            keys(&plan.empty),
            [
                "spot_job",
                "spot_test_unknown",
                "spot_migration_control.gate"
            ]
        );
        assert_eq!(keys(&plan.rebuild), ["spot_search_doc"]);
        assert_eq!(keys(&plan.unclassified), ["spot_test_unknown"]);
        assert_eq!(plan.to_truncate().len(), 4);
    }

    #[test]
    fn a_document_name_in_another_schema_is_not_a_document() {
        let documents: BTreeSet<String> = ["task".to_owned()].into();
        let plan = plan_tables(
            &[TableRef {
                schema: "other".to_owned(),
                name: "task".to_owned(),
            }],
            &documents,
        );
        assert!(plan.documents.is_empty());
        assert_eq!(plan.unclassified.len(), 1);
    }
}
