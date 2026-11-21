//! A write under `/api/v1/resource/` that carries a body is stored only when the core has seen
//! the DocType's metadata (and its child tables') in a passing `GET /api/v1/meta/{doctype}`
//! reply and no field is a `secret`. Fails closed: unseen, unparsed, ambiguous means not stored.
//! Exercised through the public API only.
#![cfg(feature = "storage")]
#![allow(missing_docs)]

use serde_json::{Value, json};
use spot_client_core::{Core, Event, Input, Output};

const SECRET: &str = "S3CR3T-field-value";

fn feed(core: &mut Core, value: Value) -> Result<Vec<Output>, String> {
    let input: Input = serde_json::from_value(value).map_err(|e| e.to_string())?;
    Ok(core.handle(input))
}

/// The id the next request the core sends will carry (it counts every send).
struct Host {
    core: Core,
    sent: u64,
    req: u64,
}

fn http_ids(out: &[Output]) -> Vec<u64> {
    out.iter()
        .filter_map(|o| match o {
            Output::SendHttp { id, .. } => Some(*id),
            _ => None,
        })
        .collect()
}

fn written(out: &[Output]) -> Vec<Option<String>> {
    out.iter()
        .filter_map(|o| match o {
            Output::StorageWrite { value, .. } => Some(value.clone()),
            _ => None,
        })
        .collect()
}

fn sign_in_body(user: &str) -> String {
    json!({"data":{"token":"tok","expires_at":"x","user":{"id":user}}}).to_string()
}

impl Host {
    fn new() -> Result<Self, String> {
        let mut host = Self {
            core: Core::new(),
            sent: 0,
            req: 10,
        };
        host.core.start("site-1");
        host.sign_in("u")?;
        Ok(host)
    }

    fn call(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<Vec<Output>, String> {
        self.req += 1;
        let mut request = json!({"type":"request","op":"call","req_id":self.req,
            "method":method,"path":path,"idempotency_key":"k"});
        if let (Some(body), Some(map)) = (body, request.as_object_mut()) {
            map.insert("body".to_owned(), json!(body));
        }
        let out = feed(&mut self.core, request)?;
        self.sent += u64::try_from(http_ids(&out).len()).map_err(|e| e.to_string())?;
        Ok(out)
    }

    fn reply(&mut self, id: u64, status: u16, body: &str) -> Result<Vec<Output>, String> {
        feed(
            &mut self.core,
            json!({"type":"transport_reply","id":id,"reply":{"status":status,"body":body}}),
        )
    }

    fn sign_in(&mut self, user: &str) -> Result<Vec<Output>, String> {
        self.req += 1;
        let out = feed(
            &mut self.core,
            json!({"type":"request","op":"sign_in","req_id":self.req,"user":user,"password":"pw"}),
        )?;
        self.sent += 1;
        let id = *http_ids(&out).first().ok_or("sign-in not sent")?;
        self.reply(id, 200, &sign_in_body(user))
    }

    fn sign_out(&mut self) -> Result<Vec<Output>, String> {
        self.req += 1;
        let out = feed(
            &mut self.core,
            json!({"type":"request","op":"sign_out","req_id":self.req}),
        )?;
        let id = *http_ids(&out).first().ok_or("sign-out not sent")?;
        self.reply(id, 200, "{}")
    }

    /// The host reads `/api/v1/meta/{path_doctype}` and the answer arrives.
    fn meta_raw(
        &mut self,
        path_doctype: &str,
        status: u16,
        body: &str,
    ) -> Result<Vec<Output>, String> {
        let out = self.call("GET", &format!("/api/v1/meta/{path_doctype}"), None)?;
        let id = *http_ids(&out).first().ok_or("meta read not sent")?;
        self.reply(id, status, body)
    }

    fn meta(&mut self, doctype: &str, fields: Value) -> Result<Vec<Output>, String> {
        let body = json!({"data":{"doctype":doctype,"label":doctype,"version":1,"fields":fields}})
            .to_string();
        self.meta_raw(doctype, 200, &body)
    }

    fn plain(&mut self, doctype: &str) -> Result<Vec<Output>, String> {
        self.meta(doctype, json!([{"name":"title","label":"T","type":"text"}]))
    }

    fn secret(&mut self, doctype: &str) -> Result<Vec<Output>, String> {
        self.meta(
            doctype,
            json!([{"name":"title","label":"T","type":"text"},{"name":"api_key","label":"K","type":"secret"}]),
        )
    }

    fn parent(&mut self, doctype: &str, child: &str) -> Result<Vec<Output>, String> {
        self.meta(
            doctype,
            json!([{"name":"rows","label":"R","type":"table","child_doctype":child}]),
        )
    }

    /// True when the write is sent and, in the same output, is also stored.
    fn stored(&mut self, method: &str, path: &str) -> Result<bool, String> {
        let body = format!("{{\"api_key\":\"{SECRET}\"}}");
        let out = self.call(method, path, Some(&body))?;
        let ids = http_ids(&out);
        let [id] = ids.as_slice() else {
            return Err(format!("{path}: the call must still be sent from memory"));
        };
        let stored = written(&out)
            .iter()
            .any(|w| w.as_deref().is_some_and(|t| t.contains(SECRET)));
        // The answer settles it, so the next call to the same path is not parked behind it.
        self.reply(*id, 200, "{}")?;
        Ok(stored)
    }
}

#[test]
fn unseen_doctype_is_sent_but_not_stored() -> Result<(), String> {
    let mut host = Host::new()?;
    assert!(!host.stored("POST", "/api/v1/resource/Customer")?);
    Ok(())
}

#[test]
fn a_plain_meta_makes_the_doctype_storable() -> Result<(), String> {
    let mut host = Host::new()?;
    host.plain("Customer")?;
    assert!(host.stored("POST", "/api/v1/resource/Customer")?);
    assert!(host.stored("PATCH", "/api/v1/resource/Customer/C-1")?);
    assert!(host.stored("PUT", "/api/v1/resource/Customer/C%2D1")?);
    assert!(host.stored("POST", "/api/v1/resource/Customer/C-1/methods/recalc")?);
    // Another DocType is still unknown.
    assert!(!host.stored("POST", "/api/v1/resource/Supplier")?);
    // A DocType name is exact: the same name in another case is another DocType.
    assert!(!host.stored("POST", "/api/v1/resource/customer")?);
    Ok(())
}

#[test]
fn a_secret_field_keeps_the_doctype_out_of_storage() -> Result<(), String> {
    let mut host = Host::new()?;
    host.secret("Customer")?;
    assert!(!host.stored("POST", "/api/v1/resource/Customer")?);
    assert!(!host.stored("PATCH", "/api/v1/resource/Customer/C-1")?);
    Ok(())
}

#[test]
fn a_child_table_must_be_seen_and_safe_whatever_the_order() -> Result<(), String> {
    // Parent first, child later.
    let mut host = Host::new()?;
    host.parent("Order", "OrderLine")?;
    assert!(
        !host.stored("POST", "/api/v1/resource/Order")?,
        "child unseen"
    );
    assert!(
        !host.stored("PATCH", "/api/v1/resource/Order/O-1")?,
        "child unseen"
    );
    host.plain("OrderLine")?;
    assert!(
        host.stored("POST", "/api/v1/resource/Order")?,
        "child now seen"
    );
    assert!(host.stored("PATCH", "/api/v1/resource/Order/O-1")?);
    // A later child reply with a secret makes the parent unsafe at once.
    host.secret("OrderLine")?;
    assert!(!host.stored("PATCH", "/api/v1/resource/Order/O-1")?);

    // Child first, parent later.
    let mut host = Host::new()?;
    host.plain("OrderLine")?;
    assert!(host.stored("POST", "/api/v1/resource/OrderLine")?);
    host.parent("Order", "OrderLine")?;
    assert!(host.stored("POST", "/api/v1/resource/Order")?);

    // A table without a child DocType, and a multiselect, are judged the same way.
    let mut host = Host::new()?;
    host.meta("Loose", json!([{"name":"rows","label":"R","type":"table"}]))?;
    assert!(!host.stored("POST", "/api/v1/resource/Loose")?);
    host.meta(
        "Tagged",
        json!([{"name":"tags","label":"T","type":"table_multiselect","child_doctype":"Tag","link_field":"tag"}]),
    )?;
    assert!(!host.stored("POST", "/api/v1/resource/Tagged")?);
    host.plain("Tag")?;
    assert!(host.stored("POST", "/api/v1/resource/Tagged")?);
    Ok(())
}

#[test]
fn nested_and_cyclic_children_are_followed_and_terminate() -> Result<(), String> {
    let mut host = Host::new()?;
    host.parent("A", "B")?;
    host.parent("B", "C")?;
    assert!(!host.stored("POST", "/api/v1/resource/A")?);
    host.plain("C")?;
    assert!(host.stored("POST", "/api/v1/resource/A")?);
    host.parent("C", "A")?;
    assert!(
        host.stored("POST", "/api/v1/resource/A")?,
        "a cycle adds no secret"
    );
    host.secret("B")?;
    assert!(!host.stored("POST", "/api/v1/resource/A")?);
    Ok(())
}

#[test]
fn a_later_reply_with_a_secret_removes_the_call_from_the_next_write() -> Result<(), String> {
    let mut host = Host::new()?;
    host.plain("Customer")?;
    let out = host.call(
        "POST",
        "/api/v1/resource/Customer",
        Some(&format!("{{\"api_key\":\"{SECRET}\"}}")),
    )?;
    assert!(
        written(&out)
            .iter()
            .any(|w| w.as_deref().is_some_and(|t| t.contains(SECRET)))
    );
    let out = host.secret("Customer")?;
    let writes = written(&out);
    assert!(
        writes
            .iter()
            .all(|w| w.as_deref().is_none_or(|t| !t.contains(SECRET))),
        "{writes:?}"
    );
    assert!(
        writes.contains(&None),
        "the stored queue is emptied: {writes:?}"
    );
    Ok(())
}

#[test]
fn a_reply_that_does_not_count_leaves_the_doctype_unknown() -> Result<(), String> {
    let good = json!({"data":{"doctype":"Customer","label":"C","version":1,
        "fields":[{"name":"t","label":"T","type":"text"}]}})
    .to_string();
    let wrong_name = good.replace("\"Customer\"", "\"Other\"");
    let mut bad: Vec<(u16, String)> = vec![
        (404, good.clone()),
        (200, "not json".to_owned()),
        (200, "{}".to_owned()),
        (200, wrong_name),
        (
            200,
            r#"{"data":{"doctype":"Customer","fields":"x"}}"#.to_owned(),
        ),
        (
            200,
            r#"{"data":{"doctype":"Customer","fields":[{"name":"x"}]}}"#.to_owned(),
        ),
        (
            200,
            r#"{"data":{"doctype":"Customer","fields":[{"type":5}]}}"#.to_owned(),
        ),
        (200, r#"{"data":{"doctype":"Customer"}}"#.to_owned()),
    ];
    bad.push((204, good.clone()));
    for (status, body) in bad {
        let mut host = Host::new()?;
        host.meta_raw("Customer", status, &body)?;
        assert!(
            !host.stored("POST", "/api/v1/resource/Customer")?,
            "{status} {body}"
        );
    }
    // A reply that does not parse forgets what was known.
    let mut host = Host::new()?;
    host.plain("Customer")?;
    host.meta_raw("Customer", 200, "garbage")?;
    assert!(!host.stored("POST", "/api/v1/resource/Customer")?);
    // A failed read (the server errors) does not change what was learned.
    host.plain("Customer")?;
    host.meta_raw("Customer", 500, "")?;
    assert!(host.stored("POST", "/api/v1/resource/Customer")?);
    // A reply to a path other than `/api/v1/meta/{doctype}` teaches nothing.
    let mut host = Host::new()?;
    for path in [
        "/api/v1/meta/Customer/x",
        "/api/v1/meta",
        "/api/v1/Meta/Customer",
        "/api/v1/meta/Customer%2F..%2FOther",
        "/api/v1/resource/Customer",
    ] {
        let out = host.call("GET", path, None)?;
        if let Some(id) = http_ids(&out).first() {
            host.reply(*id, 200, &good)?;
        }
    }
    assert!(!host.stored("POST", "/api/v1/resource/Customer")?);
    Ok(())
}

#[test]
fn calls_without_a_body_are_stored_whatever_the_doctype() -> Result<(), String> {
    let mut host = Host::new()?;
    for (method, path) in [
        ("DELETE", "/api/v1/resource/Customer/C-1"),
        ("POST", "/api/v1/resource/Customer/C-1/post"),
        ("POST", "/api/v1/resource/Customer/C-1/cancel"),
    ] {
        let out = host.call(method, path, None)?;
        assert!(
            written(&out)
                .iter()
                .any(|w| w.as_deref().is_some_and(|t| t.contains(path))),
            "{method} {path}"
        );
    }
    // A query can carry a value, so a bodiless write with one needs a safe DocType.
    let mut host = Host::new()?;
    let out = host.call("DELETE", "/api/v1/resource/Customer/C-1?token=abc", None)?;
    assert!(written(&out).is_empty());
    host.plain("Customer")?;
    let out = host.call("DELETE", "/api/v1/resource/Customer/C-2?x=1", None)?;
    assert!(!written(&out).is_empty());
    Ok(())
}

#[test]
fn paths_that_cannot_be_read_strictly_are_refused_even_for_a_safe_doctype() -> Result<(), String> {
    let mut host = Host::new()?;
    host.plain("Customer")?;
    host.plain("x")?;
    for path in [
        "/api/v1/resource/Customer%2f..%2fx",
        "/api/v1/resource/Customer%2Fx",
        "/api/v1/resource/Customer/../Customer",
        "/api/v1/resource/./Customer",
        "/api/v1//resource/Customer",
        "/api/v1/resource//Customer",
        "/api/v1/resource/Customer/",
        "/api/v1/resource/Customer.",
        "/api/v1/resource/Customer;x=1",
        "/api/v1/resource/Customer%00",
        "/api/v1/resource/Customer%zz",
        "/api/v1/resource/%43ustomer%253F",
        "/api/v1/resource/Customer\\x",
        "/api/v1/RESOURCE/Customer",
        "/API/v1/resource/Customer",
        "/api/v1/resource/",
        "/api/v1/resource",
    ] {
        assert!(!host.stored("POST", path)?, "{path}");
    }
    // The decoded name is the DocType: an escaped letter is the same DocType.
    assert!(host.stored("POST", "/api/v1/resource/%43ustomer")?);
    assert!(host.stored("POST", "/api/v1/resource/Customer?dry=1")?);
    Ok(())
}

#[test]
fn the_deny_list_still_applies_to_a_safe_doctype() -> Result<(), String> {
    let mut host = Host::new()?;
    host.plain("Customer")?;
    assert!(!host.stored("POST", "/api/v1/resource/Customer/C-1/reveal")?);
    assert!(!host.stored("POST", "/api/v1/auth/password")?);
    assert!(!host.stored("POST", "/api/v1/events/consumers/c1/ack")?);
    assert!(!host.stored("GET", "/api/v1/resource/Customer")?);
    Ok(())
}

#[test]
fn a_function_call_is_judged_as_before() -> Result<(), String> {
    let mut host = Host::new()?;
    assert!(host.stored("POST", "/api/v1/method/some_function")?);
    Ok(())
}

#[test]
fn a_user_change_or_sign_out_clears_what_was_learned() -> Result<(), String> {
    let mut host = Host::new()?;
    host.plain("Customer")?;
    assert!(host.stored("POST", "/api/v1/resource/Customer")?);
    // The same user signing in again keeps it.
    host.sign_in("u")?;
    assert!(host.stored("POST", "/api/v1/resource/Customer")?);
    // Another user starts from nothing.
    host.sign_in("v")?;
    assert!(!host.stored("POST", "/api/v1/resource/Customer")?);
    host.plain("Customer")?;
    assert!(host.stored("POST", "/api/v1/resource/Customer")?);
    host.sign_out()?;
    host.sign_in("v")?;
    assert!(!host.stored("POST", "/api/v1/resource/Customer")?);
    Ok(())
}

fn restored_text(path: &str) -> String {
    json!({"v":2,"owner":{"site":"site-1","user_id":"u"},"calls":[
        {"req_id":7,"method":"POST","path":path,"body":format!("{{\"api_key\":\"{SECRET}\"}}"),
         "if_match":null,"idempotency_key":"k-7"}]})
    .to_string()
}

#[test]
fn a_restored_call_of_an_unseen_doctype_is_replayed_and_kept_in_the_next_write()
-> Result<(), String> {
    // WP-WEB-10.T4 changed this on purpose: T3 dropped a restored call from the very next
    // `StorageWrite` even when its DocType was never shown to hold a secret, which made the
    // offline queue non-durable. Now a restored call stays admitted (marked on `Pending`)
    // until the core actually learns a secret for its DocType, a user change, a sign-out, or
    // an owner mismatch; this test's old assertion (`written(&out) == vec![None]`, dropped at
    // once) is replaced by the opposite: the queue is unchanged, so no `StorageWrite` at all.
    let mut core = Core::new();
    core.start("site-1");
    let out = feed(
        &mut core,
        json!({"type":"storage_reply","id":1,"value":restored_text("/api/v1/resource/Customer")}),
    )?;
    assert!(
        out.iter()
            .any(|o| matches!(o, Output::Emit(Event::ReauthRequired { req_id: 7 })))
    );
    assert!(
        written(&out).is_empty(),
        "kept: the queue did not change, so nothing is rewritten: {:?}",
        written(&out)
    );
    // After the owner's sign-in it is replayed exactly as before.
    feed(
        &mut core,
        json!({"type":"request","op":"sign_in","req_id":1,"user":"u","password":"pw"}),
    )?;
    let out = feed(
        &mut core,
        json!({"type":"transport_reply","id":1,"reply":{"status":200,"body":sign_in_body("u")}}),
    )?;
    let sent: Vec<_> = out
        .iter()
        .filter_map(|o| match o {
            Output::SendHttp { request, .. } => Some(request.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent.first().map(|r| r.path.as_str()),
        Some("/api/v1/resource/Customer")
    );
    assert_eq!(
        sent.first().and_then(|r| r.body.as_deref()),
        Some(format!("{{\"api_key\":\"{SECRET}\"}}").as_str())
    );
    // The sign-in replay changes nothing about the stored queue either: it was already
    // written with this call before the sign-in, so no further StorageWrite appears here.
    assert!(
        written(&out).is_empty(),
        "still kept, still no rewrite: {:?}",
        written(&out)
    );
    Ok(())
}

#[test]
fn a_restored_call_of_a_non_resource_path_is_kept() -> Result<(), String> {
    let mut core = Core::new();
    core.start("site-1");
    let out = feed(
        &mut core,
        json!({"type":"storage_reply","id":1,"value":restored_text("/api/v1/method/ping")}),
    )?;
    assert!(written(&out).is_empty());
    Ok(())
}

/// A small deterministic generator (xorshift); the crate itself has no random source.
struct Rng(u64);
impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n.max(1)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Seen {
    None,
    Plain,
    Secret,
}

/// For many orders of events, no secret value is ever stored while the model says the DocType
/// (`Order`, with a child table `Line`) is not known to be plain, nor appears anywhere but the
/// request sent and the stored queue.
#[test]
fn the_secret_is_never_stored_for_any_order_of_events() -> Result<(), String> {
    for seed in 1..=60_u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut host = Host::new()?;
        let (mut order, mut line) = (Seen::None, Seen::None);
        let mut next_marker = 0_u32;
        for _ in 0..40 {
            match rng.below(8) {
                0 => {
                    host.parent("Order", "Line")?;
                    order = Seen::Plain;
                }
                1 => {
                    host.meta("Order", json!([{"name":"k","label":"K","type":"secret"}]))?;
                    order = Seen::Secret;
                }
                2 => {
                    host.plain("Line")?;
                    line = Seen::Plain;
                }
                3 => {
                    host.secret("Line")?;
                    line = Seen::Secret;
                }
                4 => {
                    host.meta_raw("Order", 200, "not json")?;
                    order = Seen::None;
                }
                5 => {
                    host.sign_out()?;
                    host.sign_in("u")?;
                    (order, line) = (Seen::None, Seen::None);
                }
                _ => {
                    next_marker += 1;
                    let marker = format!("{SECRET}-{seed}-{next_marker}");
                    let body = format!("{{\"k\":\"{marker}\"}}");
                    let path = if rng.below(2) == 0 {
                        "/api/v1/resource/Order"
                    } else {
                        "/api/v1/resource/Order/O-1"
                    };
                    let out = host.call("POST", path, Some(&body))?;
                    let safe = order == Seen::Plain && line == Seen::Plain;
                    check(&out, &marker, safe, "send")?;
                    // The call is pending until its reply; later events may change the set.
                    if let Some(id) = http_ids(&out).first() {
                        let out = host.reply(*id, 200, "{}")?;
                        check(&out, &marker, false, "reply")?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// A marker may be in `SendHttp` (the call itself) and, when `safe`, in a `StorageWrite`;
/// never in an event, and never in a write when not safe.
fn check(out: &[Output], marker: &str, safe: bool, when: &str) -> Result<(), String> {
    for o in out {
        match o {
            Output::SendHttp { .. } => {}
            Output::StorageWrite { value: Some(v), .. } if v.contains(marker) => {
                if !safe {
                    return Err(format!("{when}: {marker} stored while not known safe"));
                }
            }
            other => {
                if format!("{other:?}").contains(marker) {
                    return Err(format!("{when}: {marker} leaked in {other:?}"));
                }
            }
        }
    }
    Ok(())
}
