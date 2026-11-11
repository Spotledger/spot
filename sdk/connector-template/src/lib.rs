//! The connector guest skeleton.
//!
//! `wit_bindgen::generate!` reads `wit/connector.wit` and generates the world's types, the
//! host imports as free functions (`log`, `http_request`, `secrets_get`, `kv_get`, `kv_set`)
//! and the `Guest` trait a connector implements. Fill in the three methods and `export!` your
//! type.

wit_bindgen::generate!({
    world: "connector",
    path: "wit",
});

/// The connector. Rename it, and rename the crate in `Cargo.toml`.
struct MyConnector;

impl Guest for MyConnector {
    fn describe() -> ConnectorInfo {
        ConnectorInfo {
            name: "my-connector".to_owned(),
            version: "0.1.0".to_owned(),
            operations: vec![Operation {
                name: "noop".to_owned(),
                input_schema: r#"{"type":"object"}"#.to_owned(),
                output_schema: r#"{"type":"object"}"#.to_owned(),
            }],
            auth: vec![AuthKind::None],
        }
    }

    fn invoke(op: String, input: String, ctx: CallContext) -> Result<String, Error> {
        // The invocation time is an argument; never read a clock. `ctx.now_micros` is the only
        // time a connector sees.
        let _ = ctx;
        match op.as_str() {
            "noop" => Ok(input),
            other => Err(Error {
                code: "connectors.unknown_operation".to_owned(),
                message: format!("no operation named {other}"),
                retryable: false,
            }),
        }
    }

    fn webhook(_request: String) -> Result<String, Error> {
        Err(Error {
            code: "connectors.unsupported".to_owned(),
            message: "this connector accepts no webhooks".to_owned(),
            retryable: false,
        })
    }
}

export!(MyConnector);
