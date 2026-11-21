//! The TOML text form (brief §2): a process version is a reviewable document
//! that round-trips, so it can be reviewed like code and later carried by
//! `spot-custom-bundle` as a section.
//!
//! The canonical form writes object keys sorted, one scalar per line, nested
//! tables and arrays inline, and no comments beyond the leading one. The same
//! writer feeds the content hash, so identical content produces identical text
//! and identical hashes.
//!
//! # Extensibility (ADR-080)
//!
//! A `kind` this build does not know is **not** a parse failure: it parses into
//! [`NodeKind::Unknown`] with its config table kept whole, round-trips, and
//! [`validate`](crate::validate) reports `bpm_model.unknown_node_kind`. A
//! top-level section this build does not know is ignored on read, so adding one
//! is a pure addition.
//!
//! WP-BPM-12 adds the `form`, `send_message` and `wait_message` kinds and the
//! optional `[participants]` section, read by
//! [`from_text_with_participants`] and written by
//! [`to_text_with_participants`]. Both are additions: [`to_text`] of a version
//! with no participants is byte for byte the text WP-BPM-01 wrote.

use toml::Value;

use crate::error::TextError;
use crate::id::NodeId;
use crate::model::{
    AgentConfig, DecisionConfig, DocumentConfig, Edge, FormConfig, IntegrationConfig, JoinRule,
    KIND_AGENT, KIND_DECISION, KIND_DOCUMENT, KIND_FORM, KIND_INTEGRATION, KIND_LOGIC,
    KIND_SEND_MESSAGE, KIND_SUB_PROCESS, KIND_TRIGGER, KIND_WAIT, KIND_WAIT_MESSAGE, LogicConfig,
    Node, NodeKind, Participant, ParticipantKind, Participants, ProcessVersion, RetryPolicy, Rule,
    SendMessageConfig, SubProcessConfig, TriggerConfig, UnknownNode, VersionStatus, WaitConfig,
    WaitMessageConfig,
};
use crate::tomlutil::{quote, render_key_values};

/// The node keys that are not part of the kind's config; removed before the
/// rest of the table is read as config.
const NODE_COMMON_KEYS: [&str; 9] = [
    "id",
    "kind",
    "join",
    "join_count",
    "terminal",
    "error",
    "timeout_secs",
    "retry_attempts",
    "retry_delay_secs",
];

/// Serialises a version to its canonical TOML text.
///
/// The participants of a process contract are process-level data, so a version
/// on its own carries none; use [`to_text_with_participants`] to write the
/// `[participants]` section a bundle section carries.
#[must_use]
pub fn to_text(version: &ProcessVersion) -> String {
    to_text_with_participants(version, None)
}

/// Serialises a version and, when given, the process's participants to their
/// canonical TOML text.
///
/// The section is written between the version's scalar keys and its nodes,
/// because a TOML table written after an array of tables would belong to the
/// last element of that array.
#[must_use]
pub fn to_text_with_participants(
    version: &ProcessVersion,
    participants: Option<&Participants>,
) -> String {
    let mut out = String::new();
    out.push_str(
        "# Spotledger process definition (spot-bpm-model). Canonical: keys sorted, one scalar per line.\n",
    );
    line(&mut out, "name", &quote(&version.name));
    line(&mut out, "app", &quote(&version.app));
    if let Some(title) = &version.title {
        line(&mut out, "title", &quote(title));
    }
    line(&mut out, "version", &version.version.to_string());
    line(&mut out, "status", status_token(version.status));

    if let Some(participants) = participants {
        out.push('\n');
        out.push_str("[participants]\n");
        line(&mut out, "plays", &quote(&participants.plays));
        for role in &participants.roles {
            out.push('\n');
            out.push_str("[[participants.roles]]\n");
            line(&mut out, "role", &quote(&role.role));
            line(&mut out, "kind", participant_kind_token(role.kind));
        }
    }

    for node in &version.nodes {
        out.push('\n');
        out.push_str("[[nodes]]\n");
        line(&mut out, "id", &quote(node.id.as_str()));
        line(&mut out, "kind", &quote(node.kind.name()));
        write_join(&mut out, node.join);
        if node.terminal {
            line(&mut out, "terminal", "true");
        }
        if let Some(error) = &node.error {
            line(&mut out, "error", &quote(error.as_str()));
        }
        if let Some(secs) = node.timeout_secs {
            line(&mut out, "timeout_secs", &secs.to_string());
        }
        if !node.retry.is_none() {
            line(
                &mut out,
                "retry_attempts",
                &node.retry.max_attempts.to_string(),
            );
            if node.retry.delay_secs > 0 {
                line(
                    &mut out,
                    "retry_delay_secs",
                    &node.retry.delay_secs.to_string(),
                );
            }
        }
        for config_line in render_key_values(&node.kind.config_table()) {
            out.push_str(&config_line);
            out.push('\n');
        }
    }

    for edge in &version.edges {
        out.push('\n');
        out.push_str("[[edges]]\n");
        line(&mut out, "from", &quote(edge.from.as_str()));
        line(&mut out, "to", &quote(edge.to.as_str()));
        if let Some(condition) = &edge.condition {
            line(&mut out, "condition", &quote(condition));
        }
        if let Some(rule) = &edge.rule {
            line(&mut out, "rule", &quote(rule));
        }
        if let Some(label) = &edge.label {
            line(&mut out, "label", &quote(label));
        }
        if edge.default {
            line(&mut out, "default", "true");
        }
    }

    for rule in &version.rules {
        out.push('\n');
        out.push_str("[[rules]]\n");
        line(&mut out, "id", &quote(rule.id.as_str()));
        line(&mut out, "expression", &quote(&rule.expression));
    }

    out
}

/// Appends one `key = value` line.
fn line(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push_str(" = ");
    out.push_str(value);
    out.push('\n');
}

/// Writes the `join` (and `join_count`) keys.
fn write_join(out: &mut String, join: JoinRule) {
    match join {
        JoinRule::All => {}
        JoinRule::Any => line(out, "join", "\"any\""),
        JoinRule::Quorum { required } => {
            line(out, "join", "\"quorum\"");
            line(out, "join_count", &required.to_string());
        }
    }
}

/// The text token of a version status.
fn status_token(status: VersionStatus) -> &'static str {
    match status {
        VersionStatus::Draft => "\"draft\"",
        VersionStatus::Published => "\"published\"",
        VersionStatus::Retired => "\"retired\"",
    }
}

/// The text token of a participant kind.
fn participant_kind_token(kind: ParticipantKind) -> &'static str {
    match kind {
        ParticipantKind::Tenant => "\"tenant\"",
        ParticipantKind::External => "\"external\"",
    }
}

/// Parses a version from TOML text.
///
/// A `[participants]` section is ignored here; [`from_text_with_participants`]
/// reads it.
///
/// # Errors
/// [`TextError::Toml`] when the text is not well-formed, [`TextError::Shape`]
/// when it is well-formed but not a process definition document, and
/// [`TextError::Name`] when a name breaks the id grammar.
pub fn from_text(text: &str) -> Result<ProcessVersion, TextError> {
    from_text_with_participants(text).map(|(version, _)| version)
}

/// Parses a version and, when the document has one, its `[participants]`
/// section.
///
/// # Errors
/// The errors of [`from_text`], and a shape error when the section is present
/// but malformed.
pub fn from_text_with_participants(
    text: &str,
) -> Result<(ProcessVersion, Option<Participants>), TextError> {
    let value: Value = toml::from_str(text).map_err(|e| TextError::Toml {
        message: e.to_string(),
    })?;
    let table = value
        .as_table()
        .ok_or_else(|| shape("the document is not a TOML table"))?;

    let name = string(table, "name")?;
    let app = string(table, "app")?;
    NodeId::new(&name)?;
    NodeId::new(&app)?;
    let title = optional_string(table, "title")?;
    let version = u32_value(table, "version")?;
    let status = parse_status(&string(table, "status")?)?;
    let participants = parse_participants(table.get("participants"))?;
    let nodes = parse_nodes(table.get("nodes"))?;
    let edges = parse_edges(table.get("edges"))?;
    let rules = parse_rules(table.get("rules"))?;

    Ok((
        ProcessVersion {
            name,
            app,
            title,
            version,
            status,
            nodes,
            edges,
            rules,
        },
        participants,
    ))
}

/// Builds a [`TextError::Shape`].
fn shape(message: impl Into<String>) -> TextError {
    TextError::Shape {
        message: message.into(),
    }
}

/// A required string field.
fn string(table: &toml::Table, key: &str) -> Result<String, TextError> {
    table
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| shape(format!("`{key}` must be a string")))
}

/// An optional string field.
fn optional_string(table: &toml::Table, key: &str) -> Result<Option<String>, TextError> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(shape(format!("`{key}` must be a string"))),
    }
}

/// A required `u32` field.
fn u32_value(table: &toml::Table, key: &str) -> Result<u32, TextError> {
    let raw = table
        .get(key)
        .and_then(Value::as_integer)
        .ok_or_else(|| shape(format!("`{key}` must be an integer")))?;
    u32::try_from(raw).map_err(|_| shape(format!("`{key}` is out of range")))
}

/// An optional `u32` field.
fn optional_u32(table: &toml::Table, key: &str) -> Result<Option<u32>, TextError> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::Integer(raw)) => u32::try_from(*raw)
            .map(Some)
            .map_err(|_| shape(format!("`{key}` is out of range"))),
        Some(_) => Err(shape(format!("`{key}` must be an integer"))),
    }
}

/// An optional `u64` field.
fn optional_u64(table: &toml::Table, key: &str) -> Result<Option<u64>, TextError> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::Integer(raw)) => u64::try_from(*raw)
            .map(Some)
            .map_err(|_| shape(format!("`{key}` is out of range"))),
        Some(_) => Err(shape(format!("`{key}` must be an integer"))),
    }
}

/// The text token of a version status, parsed.
fn parse_status(raw: &str) -> Result<VersionStatus, TextError> {
    match raw {
        "draft" => Ok(VersionStatus::Draft),
        "published" => Ok(VersionStatus::Published),
        "retired" => Ok(VersionStatus::Retired),
        other => Err(shape(format!("unknown status `{other}`"))),
    }
}

/// Parses the `participants` table.
fn parse_participants(value: Option<&Value>) -> Result<Option<Participants>, TextError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let table = value
        .as_table()
        .ok_or_else(|| shape("`participants` must be a table"))?;
    let plays = string(table, "plays")?;
    let mut roles = Vec::new();
    if let Some(items_value) = table.get("roles") {
        let items = items_value
            .as_array()
            .ok_or_else(|| shape("`participants.roles` must be an array of tables"))?;
        for (i, item) in items.iter().enumerate() {
            let role_table = item
                .as_table()
                .ok_or_else(|| shape(format!("participants.roles[{i}] is not a table")))?;
            reject_extra(role_table, &["role", "kind"])?;
            let role = string(role_table, "role")
                .map_err(|e| shape(format!("participants.roles[{i}]: {e}")))?;
            let kind = match optional_string(role_table, "kind")?.as_deref() {
                None | Some("tenant") => ParticipantKind::Tenant,
                Some("external") => ParticipantKind::External,
                Some(other) => {
                    return Err(shape(format!(
                        "participants.roles[{i}]: unknown participant kind `{other}`"
                    )));
                }
            };
            roles.push(Participant::new(role, kind));
        }
    }
    for key in table.keys() {
        if key != "plays" && key != "roles" {
            return Err(shape(format!("unknown key `{key}` in `participants`")));
        }
    }
    Ok(Some(Participants::new(roles, plays)))
}

/// Parses the `nodes` array.
fn parse_nodes(value: Option<&Value>) -> Result<Vec<Node>, TextError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| shape("`nodes` must be an array of tables"))?;
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let table = item
                .as_table()
                .ok_or_else(|| shape(format!("nodes[{i}] is not a table")))?;
            parse_node(table).map_err(|e| shape(format!("nodes[{i}]: {e}")))
        })
        .collect()
}

/// Parses the `edges` array.
fn parse_edges(value: Option<&Value>) -> Result<Vec<Edge>, TextError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| shape("`edges` must be an array of tables"))?;
    let mut edges = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let table = item
            .as_table()
            .ok_or_else(|| shape(format!("edges[{i}] is not a table")))?;
        edges.push(parse_edge(table).map_err(|e| shape(format!("edges[{i}]: {e}")))?);
    }
    Ok(edges)
}

/// Parses the `rules` array.
fn parse_rules(value: Option<&Value>) -> Result<Vec<Rule>, TextError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| shape("`rules` must be an array of tables"))?;
    let mut rules = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let table = item
            .as_table()
            .ok_or_else(|| shape(format!("rules[{i}] is not a table")))?;
        rules.push(parse_rule(table).map_err(|e| shape(format!("rules[{i}]: {e}")))?);
    }
    Ok(rules)
}

/// Parses one node table.
fn parse_node(table: &toml::Table) -> Result<Node, TextError> {
    let id = NodeId::new(&string(table, "id")?)?;
    let kind_text = string(table, "kind")?;
    if kind_text.is_empty() {
        return Err(shape("`kind` must not be empty"));
    }
    let join = parse_join(table)?;
    let terminal = match table.get("terminal") {
        None => false,
        Some(Value::Boolean(b)) => *b,
        Some(_) => return Err(shape("`terminal` must be a boolean")),
    };
    let error = match optional_string(table, "error")? {
        Some(raw) => Some(NodeId::new(&raw)?),
        None => None,
    };
    let timeout_secs = optional_u64(table, "timeout_secs")?;
    let max_attempts = optional_u32(table, "retry_attempts")?.unwrap_or(1);
    if max_attempts == 0 {
        return Err(shape("`retry_attempts` must be at least 1"));
    }
    let retry = RetryPolicy {
        max_attempts,
        delay_secs: optional_u64(table, "retry_delay_secs")?.unwrap_or(0),
    };

    let mut config = table.clone();
    for key in NODE_COMMON_KEYS {
        config.remove(key);
    }
    let kind = parse_kind(&kind_text, config)?;
    Ok(Node {
        id,
        kind,
        join,
        terminal,
        error,
        timeout_secs,
        retry,
    })
}

/// Parses the join rule.
fn parse_join(table: &toml::Table) -> Result<JoinRule, TextError> {
    match optional_string(table, "join")?.as_deref() {
        None | Some("all") => Ok(JoinRule::All),
        Some("any") => Ok(JoinRule::Any),
        Some("quorum") => {
            let required = optional_u32(table, "join_count")?
                .ok_or_else(|| shape("`join_count` is required when `join = \"quorum\"`"))?;
            if required == 0 {
                return Err(shape("`join_count` must be at least 1"));
            }
            Ok(JoinRule::Quorum { required })
        }
        Some(other) => Err(shape(format!("unknown join rule `{other}`"))),
    }
}

/// Builds a [`NodeKind`] from the kind token and the node's config table.
///
/// A known kind parses its typed config (an unknown key in it is an error); an
/// unknown kind is preserved whole.
pub fn parse_kind(kind: &str, config: toml::Table) -> Result<NodeKind, TextError> {
    let value = Value::Table(config);
    let result = match kind {
        KIND_TRIGGER => value.try_into::<TriggerConfig>().map(NodeKind::Trigger),
        KIND_DOCUMENT => value.try_into::<DocumentConfig>().map(NodeKind::Document),
        KIND_DECISION => value.try_into::<DecisionConfig>().map(NodeKind::Decision),
        KIND_INTEGRATION => value
            .try_into::<IntegrationConfig>()
            .map(NodeKind::Integration),
        KIND_LOGIC => value.try_into::<LogicConfig>().map(NodeKind::Logic),
        KIND_WAIT => value.try_into::<WaitConfig>().map(NodeKind::Wait),
        KIND_SUB_PROCESS => value
            .try_into::<SubProcessConfig>()
            .map(NodeKind::SubProcess),
        KIND_AGENT => value.try_into::<AgentConfig>().map(NodeKind::Agent),
        KIND_FORM => value.try_into::<FormConfig>().map(NodeKind::Form),
        KIND_SEND_MESSAGE => value
            .try_into::<SendMessageConfig>()
            .map(NodeKind::SendMessage),
        KIND_WAIT_MESSAGE => value
            .try_into::<WaitMessageConfig>()
            .map(NodeKind::WaitMessage),
        other => {
            let config = match value {
                Value::Table(table) => table,
                _ => toml::Table::new(),
            };
            return Ok(NodeKind::Unknown(UnknownNode {
                kind: other.to_owned(),
                config,
            }));
        }
    };
    result.map_err(|e| shape(format!("invalid `{kind}` config: {e}")))
}

/// Parses one edge table.
fn parse_edge(table: &toml::Table) -> Result<Edge, TextError> {
    reject_extra(
        table,
        &["from", "to", "condition", "rule", "label", "default"],
    )?;
    let from = NodeId::new(&string(table, "from")?)?;
    let to = NodeId::new(&string(table, "to")?)?;
    let condition = optional_string(table, "condition")?;
    let rule = optional_string(table, "rule")?;
    let label = optional_string(table, "label")?;
    let default = match table.get("default") {
        None => false,
        Some(Value::Boolean(b)) => *b,
        Some(_) => return Err(shape("`default` must be a boolean")),
    };
    Ok(Edge {
        from,
        to,
        condition,
        rule,
        label,
        default,
    })
}

/// Parses one rule table.
fn parse_rule(table: &toml::Table) -> Result<Rule, TextError> {
    reject_extra(table, &["id", "expression"])?;
    let id = NodeId::new(&string(table, "id")?)?;
    let expression = string(table, "expression")?;
    Ok(Rule { id, expression })
}

/// Rejects a table key that is not in `allowed` (a typo, not a future field).
fn reject_extra(table: &toml::Table, allowed: &[&str]) -> Result<(), TextError> {
    for key in table.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(shape(format!("unknown key `{key}`")));
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::{
        AllowAllMeta, LogicType, Participant, ParticipantKind, Participants, TriggerType,
    };

    fn sample() -> ProcessVersion {
        ProcessVersion {
            name: "order_to_cash".to_owned(),
            app: "selling".to_owned(),
            title: Some("Order to Cash".to_owned()),
            version: 1,
            status: VersionStatus::Draft,
            nodes: vec![
                Node::new(
                    NodeId::new("start").unwrap(),
                    NodeKind::Trigger(TriggerConfig {
                        trigger: TriggerType::DocumentEvent,
                        doctype: Some("Sales Order".to_owned()),
                        event: Some("post".to_owned()),
                        ..TriggerConfig::default()
                    }),
                ),
                Node::new(
                    NodeId::new("end").unwrap(),
                    NodeKind::Logic(LogicConfig {
                        logic: LogicType::Branch,
                        ..LogicConfig::default()
                    }),
                )
                .as_terminal(),
            ],
            edges: vec![Edge::new(
                NodeId::new("start").unwrap(),
                NodeId::new("end").unwrap(),
            )],
            rules: vec![],
        }
    }

    #[test]
    fn round_trips_text() {
        let version = sample();
        let text = to_text(&version);
        let parsed = from_text(&text).unwrap();
        assert_eq!(parsed, version);
        assert_eq!(to_text(&parsed), text, "the text form is idempotent");
        // Sanity: the sample validates against a permissive metadata view.
        assert!(crate::validate_version(&version, &AllowAllMeta, &crate::Limits::new()).is_empty());
    }

    #[test]
    fn unknown_kind_round_trips_and_is_preserved() {
        // Exactly the canonical text: the unknown node's keys come back byte
        // for byte.
        let text = concat!(
            "# Spotledger process definition (spot-bpm-model). Canonical: keys sorted, one scalar per line.\n",
            "name = \"x\"\n",
            "app = \"demo\"\n",
            "version = 1\n",
            "status = \"draft\"\n",
            "\n",
            "[[nodes]]\n",
            "id = \"start\"\n",
            "kind = \"trigger\"\n",
            "trigger = \"manual\"\n",
            "\n",
            "[[nodes]]\n",
            "id = \"future\"\n",
            "kind = \"approval_matrix\"\n",
            "channel = \"#ops\"\n",
            "retries = 3\n",
        );
        let version = from_text(text).unwrap();
        let unknown = version
            .nodes
            .iter()
            .find(|n| n.id.as_str() == "future")
            .unwrap();
        match &unknown.kind {
            NodeKind::Unknown(raw) => {
                assert_eq!(raw.kind, "approval_matrix");
                assert_eq!(raw.config["channel"].as_str(), Some("#ops"));
                assert_eq!(raw.config["retries"].as_integer(), Some(3));
            }
            other => panic!("expected unknown, got {other:?}"),
        }
        // It survives a round trip byte for byte.
        let round = to_text(&version);
        assert_eq!(round, text);
        let again = from_text(&round).unwrap();
        assert_eq!(again, version);
    }

    #[test]
    fn rejects_unknown_key_in_edge() {
        let text = r#"
name = "x"
app = "demo"
version = 1
status = "draft"

[[edges]]
from = "a"
to = "b"
colour = "red"
"#;
        assert!(from_text(text).is_err());
    }

    #[test]
    fn ignores_unknown_top_level_section() {
        let text = r#"
name = "x"
app = "demo"
version = 1
status = "draft"

[approvals]
buyer = "acme"
"#;
        let version = from_text(text).unwrap();
        assert_eq!(version.name, "x");
    }

    #[test]
    fn participants_round_trip_in_the_text_form() {
        let version = sample();
        let participants = Participants::new(
            vec![
                Participant::new("requester", ParticipantKind::Tenant),
                Participant::new("approver", ParticipantKind::External),
            ],
            "requester",
        );
        let text = to_text_with_participants(&version, Some(&participants));
        assert!(text.contains("[participants]\nplays = \"requester\"\n"));
        let (parsed, parsed_participants) = from_text_with_participants(&text).unwrap();
        assert_eq!(parsed, version);
        assert_eq!(parsed_participants, Some(participants));
        // A version with no participants writes no section, and the old text
        // form is unchanged.
        assert_eq!(to_text(&version), to_text_with_participants(&version, None));
        let (_, none) = from_text_with_participants(&to_text(&version)).unwrap();
        assert_eq!(none, None);
    }

    #[test]
    fn the_new_node_kinds_round_trip() {
        let text = concat!(
            "# Spotledger process definition (spot-bpm-model). Canonical: keys sorted, one scalar per line.\n",
            "name = \"x\"\n",
            "app = \"demo\"\n",
            "version = 1\n",
            "status = \"draft\"\n",
            "\n",
            "[[nodes]]\n",
            "id = \"start\"\n",
            "kind = \"trigger\"\n",
            "trigger = \"manual\"\n",
            "\n",
            "[[nodes]]\n",
            "id = \"enter\"\n",
            "kind = \"form\"\n",
            "assignee = \"role:clerk\"\n",
            "draft_mode = \"create\"\n",
            "fields = [\"amount\", \"reason\"]\n",
            "record_type = \"Expense Claim\"\n",
            "\n",
            "[[nodes]]\n",
            "id = \"send\"\n",
            "kind = \"send_message\"\n",
            "contract = \"expense_approval\"\n",
            "mapping = \"claim_to_request\"\n",
            "message = \"request\"\n",
            "on_reject = \"error_branch\"\n",
            "\n",
            "[[nodes]]\n",
            "id = \"await\"\n",
            "kind = \"wait_message\"\n",
            "timeout_secs = 86400\n",
            "contract = \"expense_approval\"\n",
            "message = \"approve\"\n",
            "on_timeout = \"timeout_edge\"\n",
            "\n",
            "[[nodes]]\n",
            "id = \"done\"\n",
            "kind = \"logic\"\n",
            "terminal = true\n",
            "logic = \"branch\"\n",
        );
        let version = from_text(text).unwrap();
        let kinds: Vec<&str> = version.nodes.iter().map(|n| n.kind.name()).collect();
        assert_eq!(
            kinds,
            ["trigger", "form", "send_message", "wait_message", "logic"]
        );
        assert!(version.nodes.iter().all(|n| n.kind.is_known()));
        assert_eq!(to_text(&version), text, "the new kinds write canonically");
        assert_eq!(from_text(&to_text(&version)).unwrap(), version);
    }
}
