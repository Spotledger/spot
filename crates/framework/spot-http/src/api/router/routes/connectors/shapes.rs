//! The response shapes of the connector operations: the contract's `Connector`, the invoke
//! answer and a run-log line, each checked against the contract's bounds. A port answer outside
//! them is `None` (the route answers `500`), so a port that misbehaves cannot make the route
//! emit a body the contract does not describe.

use serde_json::{Map, Value as Json, json};

use crate::api::ports::connectors::{ConnectorView, InvokeOut, OperationView, RunView};

/// A UTC RFC 3339 instant of the bounded length the contract allows.
fn utc(s: &str) -> bool {
    s.ends_with('Z') && s.len() <= 40
}

/// The contract's `RecordId`: a lower-case hyphenated UUIDv7.
pub(super) fn is_record_id(raw: &str) -> bool {
    let b = raw.as_bytes();
    if b.len() != 36 {
        return false;
    }
    b.iter().enumerate().all(|(i, c)| match i {
        8 | 13 | 18 | 23 => *c == b'-',
        14 => *c == b'7',
        19 => matches!(*c, b'8' | b'9' | b'a' | b'b'),
        _ => c.is_ascii_digit() || (b'a'..=b'f').contains(c),
    })
}

fn code_ok(code: &Option<String>) -> bool {
    code.as_deref()
        .is_none_or(|c| !c.is_empty() && c.chars().count() <= 100)
}

fn operation_json(op: &OperationView) -> Option<Json> {
    let ok = (1..=100).contains(&op.name.chars().count())
        && op
            .description
            .as_deref()
            .is_none_or(|d| d.chars().count() <= 500)
        && op.input_schema.len() <= 100
        && op.output_schema.len() <= 100;
    if !ok {
        return None;
    }
    let mut map = Map::new();
    map.insert("name".to_owned(), Json::from(op.name.clone()));
    if let Some(description) = &op.description {
        map.insert("description".to_owned(), Json::from(description.clone()));
    }
    map.insert(
        "input_schema".to_owned(),
        Json::Object(op.input_schema.clone()),
    );
    map.insert(
        "output_schema".to_owned(),
        Json::Object(op.output_schema.clone()),
    );
    Some(Json::Object(map))
}

/// The contract's `Connector`, or `None` when the port's view breaks the contract's bounds.
pub(super) fn connector_json(view: &ConnectorView) -> Option<Json> {
    let ok = (1..=100).contains(&view.name.chars().count())
        && view
            .kind
            .as_deref()
            .is_none_or(|k| (1..=60).contains(&k.chars().count()))
        && view.operations.len() <= 200
        && view.last_run.as_ref().is_none_or(|r| {
            utc(&r.at) && (1..=100).contains(&r.operation.chars().count()) && code_ok(&r.code)
        });
    if !ok {
        return None;
    }
    let mut operations = Vec::with_capacity(view.operations.len());
    for op in &view.operations {
        operations.push(operation_json(op)?);
    }
    let mut map = Map::new();
    map.insert("name".to_owned(), Json::from(view.name.clone()));
    if let Some(kind) = &view.kind {
        map.insert("kind".to_owned(), Json::from(kind.clone()));
    }
    map.insert("state".to_owned(), Json::from(view.state.as_str()));
    map.insert("operations".to_owned(), Json::Array(operations));
    if let Some(run) = &view.last_run {
        map.insert(
            "last_run".to_owned(),
            json!({
                "at": run.at,
                "operation": run.operation,
                "outcome": run.outcome.as_str(),
                "code": run.code,
            }),
        );
    }
    Some(Json::Object(map))
}

/// The `data` of an invoke answer, or `None` when it breaks the contract's bounds.
pub(super) fn invoke_json(out: &InvokeOut) -> Option<Json> {
    if out.output.len() > 200 || out.job_id.as_deref().is_some_and(|j| !is_record_id(j)) {
        return None;
    }
    let mut map = Map::new();
    map.insert("output".to_owned(), Json::Object(out.output.clone()));
    if let Some(job) = &out.job_id {
        map.insert("job_id".to_owned(), Json::from(job.clone()));
    }
    Some(Json::Object(map))
}

/// One run-log line, or `None` when it breaks the contract's bounds.
pub(super) fn run_json(run: &RunView) -> Option<Json> {
    let ok = utc(&run.at)
        && (1..=100).contains(&run.operation.chars().count())
        && code_ok(&run.code)
        && i64::try_from(run.duration_ms).is_ok();
    ok.then(|| {
        json!({
            "at": run.at,
            "operation": run.operation,
            "outcome": run.outcome.as_str(),
            "duration_ms": run.duration_ms,
            "code": run.code,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::is_record_id;

    #[test]
    fn a_record_id_is_a_lower_case_uuid_v7() {
        assert!(is_record_id("01950000-0000-7000-8000-000000000001"));
        assert!(!is_record_id("01950000-0000-4000-8000-000000000001"));
        assert!(!is_record_id("01950000-0000-7000-c000-000000000001"));
        assert!(!is_record_id("01950000-0000-7000-8000-00000000000G"));
        assert!(!is_record_id("job-1"));
    }
}
