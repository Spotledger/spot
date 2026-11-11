//! Collect structural diagnostics without echoing input values (URLs/digests may be secret).
use toml::Value;

type Field = (&'static str, &'static str, bool);
fn fields(schema: &str) -> &'static [Field] {
    match schema {
        "common" => &[
            ("apps", "strings", true),
            ("server", "server", true),
            ("postgres", "postgres", true),
            ("telemetry", "telemetry", false),
            ("services", "services", false),
            ("audit", "audit", false),
            ("trust", "trust", false),
        ],
        "trust" => &[
            ("roots", "string", false),
            ("revocations", "string", false),
            ("require_signed", "boolean", false),
        ],
        "site" => &[
            ("site", "string", true),
            ("database", "string", true),
            ("collation", "string", false),
            ("aliases", "strings", false),
            ("api_keys", "keys", false),
            ("apps", "strings", false),
            ("credentials", "credentials", false),
            ("partition", "partition", false),
        ],
        "partition" => &[
            ("node", "string", true),
            ("doctypes", "strings", false),
            ("scope", "scope_fields", false),
        ],
        "scope_field" => &[
            ("doctype", "string", true),
            ("key", "string", true),
            ("field", "string", true),
        ],
        "credential" => &[
            ("user", "string", true),
            ("password_hash", "string", true),
            ("first_sign_in", "boolean", false),
            ("expires_at", "integer", false),
        ],
        "key" => &[
            ("key_id", "string", true),
            ("digest", "string", true),
            ("user", "string", true),
            ("expires_at", "integer", false),
        ],
        "server" => &[
            ("bind", "strings", false),
            ("max_body_bytes", "integer", false),
            ("request_timeout", "integer", false),
            ("shutdown_grace", "integer", false),
            ("trusted_proxies", "strings", false),
            ("fallback_site", "string", false),
        ],
        "postgres" => &[
            ("admin_url", "string", true),
            ("pool_max_size", "integer", false),
        ],
        "telemetry" => &[
            ("filter", "string", false),
            ("format", "string", false),
            ("service_name", "string", false),
            ("node_id", "string", false),
            ("otlp", "otlp", false),
        ],
        "audit" => &[
            ("identity_file_mirror", "boolean", false),
            ("identity_chain_strict", "boolean", false),
        ],
        "otlp" => &[
            ("endpoint", "string", true),
            ("sample_ppm", "integer", true),
        ],
        _ => &[],
    }
}
fn empty(kind: &str) -> Value {
    match kind {
        "string" => Value::String(String::new()),
        "integer" => Value::Integer(0),
        "boolean" => Value::Boolean(false),
        "strings" | "keys" | "credentials" | "scope_fields" => Value::Array(Vec::new()),
        _ => Value::Table(toml::map::Map::new()),
    }
}
pub(crate) fn clean(value: &mut Value, schema: &str, path: &str, errors: &mut Vec<String>) {
    if !value.is_table() {
        errors.push(format!("{path}: expected table"));
        *value = empty(schema);
    }
    let Some(table) = value.as_table_mut() else {
        return;
    };
    let fields = fields(schema);
    // `services` is checked setting by setting below, against the typed options.
    table.retain(|key, _| {
        if schema == "services" {
            return true;
        }
        let known = fields.iter().any(|(name, _, _)| key == *name);
        if !known {
            errors.push(format!("{path}: unknown field {key}"));
        }
        known
    });
    for (name, kind, required) in fields {
        let child = format!("{path}.{name}");
        if !table.contains_key(*name) && *required {
            errors.push(format!("{child}: missing required field"));
            table.insert((*name).into(), empty(kind));
        }
        let Some(value) = table.get_mut(*name) else {
            continue;
        };
        let valid = match *kind {
            "string" => value.is_str(),
            "integer" => value.as_integer().is_some_and(|i| i >= 0),
            "boolean" => value.as_bool().is_some(),
            "strings" | "keys" | "credentials" | "scope_fields" => value.is_array(),
            _ => value.is_table(),
        };
        if !valid {
            errors.push(format!("{child}: invalid {kind}"));
            *value = empty(kind);
        }
        match *kind {
            "strings" => {
                if let Some(values) = value.as_array_mut() {
                    for (i, value) in values.iter_mut().enumerate() {
                        if !value.is_str() {
                            errors.push(format!("{child}[{i}]: expected string"));
                            *value = empty("string");
                        }
                    }
                }
            }
            "keys" => {
                if let Some(values) = value.as_array_mut() {
                    for (i, value) in values.iter_mut().enumerate() {
                        clean(value, "key", &format!("{child}[{i}]"), errors);
                    }
                }
            }
            "credentials" => {
                if let Some(values) = value.as_array_mut() {
                    for (i, value) in values.iter_mut().enumerate() {
                        clean(value, "credential", &format!("{child}[{i}]"), errors);
                    }
                }
            }
            "scope_fields" => {
                if let Some(values) = value.as_array_mut() {
                    for (i, value) in values.iter_mut().enumerate() {
                        clean(value, "scope_field", &format!("{child}[{i}]"), errors);
                    }
                }
            }
            "server" | "postgres" | "telemetry" | "otlp" | "services" | "partition" | "audit" => {
                clean(value, kind, &child, errors)
            }
            _ => {}
        }
    }
    if schema == "services" {
        // Each top-level setting is checked on its own against the typed options, so one bad
        // setting is named without echoing any value (an SMTP password is a secret).
        table.retain(|key, value| {
            let mut one = toml::map::Map::new();
            one.insert(key.to_owned(), value.clone());
            let valid = Value::Table(one)
                .try_into::<crate::config::Services>()
                .is_ok();
            if !valid {
                errors.push(format!("{path}.{key}: invalid services setting"));
            }
            valid
        });
    }
    if schema == "server" {
        table.retain(|key, value| {
            let mut one = toml::map::Map::new();
            one.insert(key.to_owned(), value.clone());
            let valid = Value::Table(one)
                .try_into::<spot_http::ServerConfig>()
                .is_ok();
            if !valid {
                errors.push(format!("{path}.{key}: invalid server setting"));
            }
            valid
        });
    }
}
