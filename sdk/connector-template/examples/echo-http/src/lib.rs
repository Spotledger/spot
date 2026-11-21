//! `echo-http`: a sample connector built from the template alone.
//!
//! It has two operations: `echo` returns its input unchanged, and `fetch` makes one outbound
//! `GET` through the host's `http-request` (and so through the egress policy) and returns the
//! response. It declares no auth; add a `[connector] auth` entry to inject a credential.

wit_bindgen::generate!({
    world: "connector",
    path: "wit",
});

/// The public-style REST fixture the sample talks to. A real connector declares the host in
/// `connector.toml` and the host's egress policy must allow it.
const FIXTURE_URL: &str = "https://api.example.test/ping";

struct EchoHttp;

impl Guest for EchoHttp {
    fn describe() -> ConnectorInfo {
        ConnectorInfo {
            name: "echo-http".to_owned(),
            version: "0.1.0".to_owned(),
            operations: vec![
                Operation {
                    name: "echo".to_owned(),
                    input_schema: r#"{"type":"object","description":"returned unchanged"}"#
                        .to_owned(),
                    output_schema: r#"{"type":"object"}"#.to_owned(),
                },
                Operation {
                    name: "fetch".to_owned(),
                    input_schema: r#"{"type":"object","properties":{}}"#.to_owned(),
                    output_schema: r#"{"type":"object","description":"the JSON HTTP response"}"#
                        .to_owned(),
                },
            ],
            auth: vec![AuthKind::None],
        }
    }

    fn invoke(op: String, input: String, ctx: CallContext) -> Result<String, Error> {
        match op.as_str() {
            "echo" => Ok(input),
            "fetch" => {
                log(Level::Info, &format!("fetch at {}", ctx.now_micros));
                let request = format!(
                    r#"{{"method":"GET","url":"{FIXTURE_URL}","headers":[],"body":null}}"#
                );
                http_request(&request).map_err(|message| Error {
                    code: "connectors.egress_denied".to_owned(),
                    message,
                    retryable: true,
                })
            }
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
            message: "echo-http accepts no webhooks".to_owned(),
            retryable: false,
        })
    }
}

export!(EchoHttp);
