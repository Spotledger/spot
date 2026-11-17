//! JSON views of catalogue operations and commands, and the conversion of wire parameters into
//! typed invocations.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use spot_mgmt_ops::{
    Catalogue, Invocation, OpId, OperationSpec, ParamSpec, ParamType, ParamValue, Risk, TargetKind,
    Validated,
};

use crate::error::MgmtError;
use crate::lifecycle::CommandRecord;

/// The wire name of a risk.
#[must_use]
pub fn risk_str(risk: Risk) -> &'static str {
    match risk {
        Risk::Read => "read",
        Risk::Low => "low",
        Risk::Medium => "medium",
        Risk::High => "high",
        Risk::Critical => "critical",
    }
}

/// Parses a wire risk.
#[must_use]
pub fn parse_risk(text: &str) -> Option<Risk> {
    match text {
        "read" => Some(Risk::Read),
        "low" => Some(Risk::Low),
        "medium" => Some(Risk::Medium),
        "high" => Some(Risk::High),
        "critical" => Some(Risk::Critical),
        _ => None,
    }
}

fn target_str(target: TargetKind) -> &'static str {
    match target {
        TargetKind::Fleet => "fleet",
        TargetKind::Group => "group",
        TargetKind::Server => "server",
        TargetKind::Tenant => "tenant",
    }
}

/// What a command must carry: `signature` above `read`, plus `approval:<kind>` entries.
#[must_use]
pub fn requires(risk: Risk, approval_kinds: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    if risk > Risk::Read {
        out.push("signature".to_owned());
    }
    for kind in approval_kinds {
        out.push(format!("approval:{kind}"));
    }
    out
}

fn default_text(value: &ParamValue) -> Option<String> {
    match value {
        ParamValue::Bool(flag) => Some(flag.to_string()),
        ParamValue::Int(number) => Some(number.to_string()),
        ParamValue::Text(text) => Some(text.clone()),
        ParamValue::Duration(seconds) => Some(format!("{seconds}s")),
        ParamValue::List(_) => None,
    }
}

fn param_json(param: &ParamSpec) -> Value {
    // The wire type names are bool, int, text and choice; durations ride as integer seconds and
    // lists as their element type.
    let mut ty = &param.ty;
    while let ParamType::List(inner) = ty {
        ty = inner;
    }
    let mut map = Map::new();
    map.insert("name".into(), json!(param.name));
    map.insert("required".into(), json!(param.required));
    map.insert("is_secret".into(), json!(param.secret));
    map.insert("doc".into(), json!(param.doc));
    match ty {
        ParamType::Bool => {
            map.insert("type".into(), json!("bool"));
        }
        ParamType::Int { min, max } => {
            map.insert("type".into(), json!("int"));
            map.insert("min".into(), json!(min));
            map.insert("max".into(), json!(max));
        }
        ParamType::Duration => {
            map.insert("type".into(), json!("int"));
            map.insert("min".into(), json!(0));
        }
        ParamType::Text { max_len, pattern } => {
            map.insert("type".into(), json!("text"));
            map.insert("max_len".into(), json!(max_len));
            map.insert("pattern".into(), json!(pattern));
        }
        ParamType::Enum(choices) => {
            map.insert("type".into(), json!("choice"));
            map.insert("choices".into(), json!(choices));
        }
        ParamType::List(_) => {}
    }
    if let Some(text) = param.default.as_ref().and_then(default_text) {
        map.insert("default".into(), json!(text));
    }
    Value::Object(map)
}

/// The `MgmtOperation` JSON of one operation.
#[must_use]
pub fn operation_json(spec: &OperationSpec, approval_kinds: &[String]) -> Value {
    json!({
        "id": spec.id.as_str(),
        "title": spec.title,
        "summary": spec.summary,
        "risk": risk_str(spec.risk),
        "target": target_str(spec.target),
        "parameters": spec.params.iter().map(param_json).collect::<Vec<_>>(),
        "requires": requires(spec.risk, approval_kinds),
        "audit_event": spec.id.as_str(),
    })
}

/// The whole catalogue as JSON, keyed by operation id (what
/// `cargo run -p spot-mgmt-api --example catalogue_json` prints for `check.py`).
#[must_use]
pub fn catalogue_json(catalogue: &Catalogue) -> Value {
    let mut map = Map::new();
    for spec in catalogue.iter() {
        map.insert(spec.id.as_str().to_owned(), operation_json(spec, &[]));
    }
    Value::Object(map)
}

fn convert(value: &Value, expected: Option<&ParamType>) -> Option<ParamValue> {
    match value {
        Value::Bool(flag) => Some(ParamValue::Bool(*flag)),
        Value::Number(number) => {
            let integer = number.as_i64()?;
            if matches!(expected, Some(ParamType::Duration)) {
                return u64::try_from(integer).ok().map(ParamValue::Duration);
            }
            Some(ParamValue::Int(integer))
        }
        Value::String(text) => Some(ParamValue::Text(text.clone())),
        Value::Array(items) => {
            let inner = match expected {
                Some(ParamType::List(inner)) => Some(inner.as_ref()),
                _ => None,
            };
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(convert(item, inner)?);
            }
            Some(ParamValue::List(out))
        }
        Value::Null | Value::Object(_) => None,
    }
}

/// Builds an invocation from wire parameters. A value that has no typed form (null, object,
/// float) is a parameter error naming the parameter.
///
/// # Errors
/// [`MgmtError`] (`mgmt.invalid_parameter`) for a non-object or an unusable value.
pub fn invocation_from_params(
    spec: &OperationSpec,
    op: &OpId,
    params: &Value,
) -> Result<Invocation, MgmtError> {
    let Value::Object(map) = params else {
        return Err(MgmtError::parameter("params"));
    };
    let mut invocation = Invocation::new(op.clone());
    for (name, value) in map {
        let expected = spec
            .params
            .iter()
            .find(|param| param.name == *name)
            .map(|param| &param.ty);
        let converted = convert(value, expected).ok_or_else(|| MgmtError::parameter(name))?;
        invocation = invocation.with(name.clone(), converted);
    }
    Ok(invocation)
}

/// Maps the first validation problem to a parameter error.
#[must_use]
pub fn invalid_parameter(problems: &[spot_mgmt_ops::InvocationError]) -> MgmtError {
    match problems.first().and_then(|problem| problem.param()) {
        Some(name) => MgmtError::parameter(name),
        None => MgmtError::new(crate::error::MgmtCode::InvalidParameter),
    }
}

fn value_json(value: &ParamValue) -> Value {
    match value {
        ParamValue::Bool(flag) => json!(flag),
        ParamValue::Int(number) => json!(number),
        ParamValue::Text(text) => json!(text),
        ParamValue::Duration(seconds) => json!(seconds),
        ParamValue::List(items) => json!(
            items
                .iter()
                .map(|item| default_text(item).unwrap_or_default())
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

/// The `MgmtInvocation` JSON of a validated call: secret values are never echoed.
#[must_use]
pub fn invocation_json(validated: &Validated, risk: Risk, approval_kinds: &[String]) -> Value {
    let mut params: BTreeMap<String, Value> = BTreeMap::new();
    let mut redacted = Vec::new();
    for param in &validated.params {
        if param.secret {
            params.insert(param.name.clone(), json!("<secret>"));
            redacted.push(param.name.clone());
        } else {
            params.insert(param.name.clone(), value_json(&param.value));
        }
    }
    json!({
        "operation": validated.op.as_str(),
        "params": params,
        "risk": risk_str(risk),
        "requires": requires(risk, approval_kinds),
        "command_line": spot_mgmt_ops::render(validated, spot_mgmt_ops::Shell::Posix),
        "redacted": redacted,
    })
}

/// RFC 3339 UTC text of unix seconds.
#[must_use]
pub fn rfc3339(unix_seconds: i64) -> String {
    let days = unix_seconds.div_euclid(86_400);
    let seconds = unix_seconds.rem_euclid(86_400);
    // Civil-from-days (proleptic Gregorian), after H. Hinnant.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

/// The `MgmtCommandStatus` JSON of a stored command.
#[must_use]
pub fn status_json(record: &CommandRecord) -> Value {
    let mut map = Map::new();
    map.insert("id".into(), json!(record.id));
    map.insert("operation".into(), json!(record.operation));
    map.insert("state".into(), json!(record.state.wire()));
    if let Some(line) = record.progress.last() {
        map.insert(
            "progress".into(),
            json!({ "step": line.step, "percent": line.percent }),
        );
    }
    if let Some(result) = &record.result {
        let value = if result.is_object() {
            result.clone()
        } else {
            json!({ "value": result })
        };
        map.insert("result".into(), value);
    }
    map.insert("error_code".into(), json!(record.error_code));
    let audit_ref = if record.audit_ref.is_empty() {
        "pending"
    } else {
        record.audit_ref.as_str()
    };
    map.insert("audit_ref".into(), json!(audit_ref));
    map.insert("created_at".into(), json!(rfc3339(record.created_at)));
    map.insert("finished_at".into(), json!(record.finished_at.map(rfc3339)));
    Value::Object(map)
}

/// A minimal query-string parser (`a=b&c=d`, percent-decoding, `+` as space).
#[must_use]
pub fn parse_query(query: Option<&str>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for pair in query.unwrap_or("").split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(percent_decode(key), percent_decode(value));
    }
    out
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'+' => out.push(b' '),
            b'%' => {
                let high = bytes.get(index + 1).and_then(|b| hex_digit(*b));
                let low = bytes.get(index + 2).and_then(|b| hex_digit(*b));
                if let (Some(high), Some(low)) = (high, low) {
                    out.push((high << 4) | low);
                    index += 2;
                } else {
                    out.push(b'%');
                }
            }
            other => out.push(other),
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
