//! The persisted queue never holds a credential, a lease or a revealed value: the deny list
//! decides by normalised path class and fails closed. Exercised through the public API only.
#![cfg(feature = "storage")]
#![allow(missing_docs)]

use serde_json::json;
use spot_client_core::{Core, Input, Output};

const SECRET: &str = "S3CR3T-body-marker";

fn feed(core: &mut Core, value: serde_json::Value) -> Result<Vec<Output>, String> {
    let input: Input = serde_json::from_value(value).map_err(|e| e.to_string())?;
    Ok(core.handle(input))
}

fn signed_in() -> Result<Core, String> {
    let mut core = Core::new();
    core.start("site-1");
    feed(
        &mut core,
        json!({"type":"request","op":"sign_in","req_id":1,"user":"u","password":"pw"}),
    )?;
    let body = r#"{"data":{"token":"tok","expires_at":"x","user":{"id":"u"}}}"#;
    feed(
        &mut core,
        json!({"type":"transport_reply","id":1,"reply":{"status":200,"body":body}}),
    )?;
    Ok(core)
}

/// What one call did: whether it was sent and every text written to storage.
struct Effect {
    sent: bool,
    written: Vec<String>,
}

/// Feeds the meta reply of a DocType with only plain fields, as the host's own `GET` would
/// bring it through the core. `id` is the transport id the core gave the read.
fn learn_plain(core: &mut Core, doctype: &str, id: u64) -> Result<(), String> {
    let path = format!("/api/v1/meta/{doctype}");
    feed(
        core,
        json!({"type":"request","op":"call","req_id":100 + id,"method":"GET","path":path}),
    )?;
    let body = json!({"data":{"doctype":doctype,"label":doctype,"version":1,
        "fields":[{"name":"title","label":"Title","type":"text"}]}})
    .to_string();
    feed(
        core,
        json!({"type":"transport_reply","id":id,"reply":{"status":200,"body":body}}),
    )?;
    Ok(())
}

fn call(method: &str, path: &str) -> Result<Effect, String> {
    let mut core = signed_in()?;
    // The DocTypes the tables below write to are known to hold no secret (WP-WEB-10.T3).
    learn_plain(&mut core, "Invoice", 2)?;
    learn_plain(&mut core, "Rechnung", 3)?;
    let out = feed(
        &mut core,
        json!({"type":"request","op":"call","req_id":2,"method":method,"path":path,
               "body":format!("{{\"secret\":\"{SECRET}\"}}"),"idempotency_key":"k"}),
    )?;
    let sent = out.iter().any(|o| matches!(o, Output::SendHttp { .. }));
    let written = out
        .iter()
        .filter_map(|o| match o {
            Output::StorageWrite { value: Some(v), .. } => Some(v.clone()),
            _ => None,
        })
        .collect();
    Ok(Effect { sent, written })
}

/// True when the call was sent and a stored copy of its body exists.
fn persisted(method: &str, path: &str) -> Result<bool, String> {
    let effect = call(method, path)?;
    if path.starts_with('/') && !effect.sent {
        return Err(format!("{path}: the call must still be sent from memory"));
    }
    Ok(effect.written.iter().any(|w| w.contains(SECRET)))
}

#[test]
fn deny_list_table() -> Result<(), String> {
    let denied = [
        "/api/v1/auth/login",
        "/api/v1/auth/password",
        "/api/v1/auth/logout",
        "/api/v1/auth/session",
        "/api/v1/auth/sessions",
        "/api/v1/auth/sessions/s1",
        "/api/v1/auth/consumer/renew",
        "/api/v1/auth",
        "/api/v1/AUTH/login",
        "/api/v1/Auth/Login",
        "/api/v1/auth/login?x=1",
        "/api/v1/auth%3Flogin?x=1",
        "/api/v1/resource/x/y/reveal%23",
        "/api/v1/resource/x/y/reveal%3fa=b",
        "/api/v1/resource/x/y/reveal%253F",
        "/api/v1/auth/login#frag",
        "/api/v1/auth/login/",
        "/api/v1//auth/login",
        "/api/v1/./auth/login",
        "/api/v1/resource/../auth/login",
        "/api/v1/resource/x/../../auth/login",
        "/api/v1/%61uth/login",
        "/api/v1/auth%2Flogin",
        "/api/v1%2Fauth%2Flogin",
        "/api/v1/resource/%2e%2e/auth/login",
        "/api/v1/resource/%2E%2E%2Fauth/login",
        "/api/v1/%2561uth/login",
        "/api/v1/auth./login",
        "/api/v1/auth /login",
        "/api/v1/auth%20/login",
        "/api/v1/auth;x=1/login",
        "/api/v1\\auth\\login",
        "/api/v1/auth%5Clogin",
        "/api/v1/resource/Invoice/INV-1/reveal",
        "/api/v1/resource/Invoice/INV-1/Reveal",
        "/api/v1/resource/Invoice/INV-1/reveal?field=secret",
        "/api/v1/resource/Invoice/INV-1/%72eveal",
        "/api/v1/events/consumers/c1/ack",
        "/api/v1/events/consumers/c1/pull",
        "/api/v1/events/consumers/c1/restart",
        "/api/v1/events/stream",
        "/api/v1/events",
        "/api/v1/EVENTS/consumers",
        // Fail closed: what cannot be normalised or is not under /api/v1/.
        "",
        "/",
        "/api/v1",
        "/api/v1/",
        "/../api/v1/resource/Invoice",
        "/api/v1/resource/%zz",
        "/api/v1/resource/%",
        "/api/v1/resource/%C3%28",
        "/api/v1/resource/%00",
        "/api/v1/resource/Invoice\n",
        "api/v1/resource/Invoice",
        "/api/v2/resource/Invoice",
        "/other/path",
        "/api/v1/resource/%25252525252e",
        // Look-alikes of fixed names.
        "/api/v1/\u{FF41}uth/login",
        "/api/v1/auth\u{FF0F}login",
        "/api/v1/resource/Invoice/INV-1/r\u{0435}veal",
    ];
    for path in denied {
        assert!(!persisted("POST", path)?, "must not be persisted: {path:?}");
    }
    Ok(())
}

#[test]
fn ordinary_writes_are_persisted() -> Result<(), String> {
    for (method, path) in [
        ("POST", "/api/v1/resource/Invoice"),
        ("PATCH", "/api/v1/resource/Invoice/INV-1"),
        ("DELETE", "/api/v1/resource/Invoice/INV-1"),
        ("PUT", "/api/v1/resource/Invoice/INV%2D1"),
        ("POST", "/api/v1/resource/Invoice/INV-1/post"),
        ("POST", "/api/v1/resource/Invoice?dry=1"),
        ("POST", "/api/v1/resource/Rechnung/%C3%9Cberweisung-1"),
        ("post", "/api/v1/resource/Invoice"),
    ] {
        assert!(
            persisted(method, path)?,
            "should be persisted: {method} {path}"
        );
    }
    Ok(())
}

#[test]
fn a_denied_call_is_sent_and_retried_from_memory_only() -> Result<(), String> {
    let mut core = signed_in()?;
    let path = "/api/v1/auth/password";
    let body = format!("{{\"old\":\"{SECRET}\",\"new\":\"n\"}}");
    let out = feed(
        &mut core,
        json!({"type":"request","op":"call","req_id":2,"method":"POST","path":path,"body":body}),
    )?;
    assert!(out.iter().any(|o| matches!(o, Output::SendHttp { .. })));
    // The 401 holds it for the one re-auth retry; still nothing is stored.
    let out = feed(
        &mut core,
        json!({"type":"transport_reply","id":2,"reply":{"status":401,"body":""}}),
    )?;
    let all = format!("{out:?}");
    assert!(!all.contains(SECRET) && !all.contains("auth/password"));
    assert!(
        out.iter()
            .all(|o| !matches!(o, Output::StorageWrite { value: Some(_), .. }))
    );
    Ok(())
}

/// A small deterministic generator (xorshift); the crate itself has no random source.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> usize {
        usize::try_from(self.next() % n.max(1)).unwrap_or(0)
    }
}

fn encode_some(rng: &mut Rng, text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() && rng.below(3) == 0 {
            out.push_str(&format!("%{:02X}", u32::from(ch)));
        } else {
            out.push(ch);
        }
    }
    out
}

fn hostile_auth_path(rng: &mut Rng) -> String {
    let tails = [
        "login",
        "password",
        "logout",
        "session",
        "sessions/s1",
        "consumer/renew",
        "x/y",
    ];
    let tail = tails[rng.below(7)];
    let mut path = format!("/api/v1/auth/{tail}");
    if rng.below(2) == 0 {
        path = path.to_uppercase_some(rng);
    }
    if rng.below(2) == 0 {
        path = path.replace(
            "/api/v1/",
            [
                "/api/v1//",
                "/api/v1/./",
                "/api/v1/resource/../",
                "/api//v1/",
            ][rng.below(4)],
        );
    }
    if rng.below(3) == 0 {
        path = encode_some(rng, &path);
    }
    if rng.below(4) == 0 {
        path = path.replace(
            "auth/",
            ["auth./", "auth /", "auth;a=b/", "auth%2F"][rng.below(4)],
        );
    }
    if rng.below(4) == 0 {
        path = path.replace('/', "\\");
    }
    if rng.below(3) == 0 {
        path.push_str(["?a=1", "#f", "?next=/api/v1/resource/Invoice", "/"][rng.below(4)]);
    }
    path
}

trait ToUpperSome {
    fn to_uppercase_some(&self, rng: &mut Rng) -> String;
}
impl ToUpperSome for String {
    fn to_uppercase_some(&self, rng: &mut Rng) -> String {
        self.chars()
            .map(|c| {
                if rng.below(2) == 0 {
                    c.to_ascii_uppercase()
                } else {
                    c
                }
            })
            .collect()
    }
}

#[test]
fn no_authentication_path_ever_reaches_storage() -> Result<(), String> {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for _ in 0..400 {
        let path = hostile_auth_path(&mut rng);
        let effect = call("POST", &path)?;
        // A path the core refuses outright (no leading slash) is neither sent nor stored.
        assert!(
            effect.sent || !path.starts_with('/'),
            "{path:?} is still sent from memory"
        );
        for text in &effect.written {
            assert!(
                !text.contains(SECRET) && !text.contains("auth"),
                "{path:?} reached storage: {text}"
            );
        }
    }
    Ok(())
}
