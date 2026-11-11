//! A call read back from the host's storage stays in the next `StorageWrite` while it is held,
//! until the core sees metadata that shows a secret for its DocType (or a child table), a user
//! change, a sign-out, or an owner mismatch (WP-WEB-10.T4). Calls created in this session keep
//! the stricter rule of WP-WEB-10.T3. Exercised through the public API only.
#![cfg(feature = "storage")]
#![allow(missing_docs)]

use serde_json::{Value, json};
use spot_client_core::{Core, Event, Input, Output};

const SECRET: &str = "S3CR3T-field-value";

fn feed(core: &mut Core, value: Value) -> Result<Vec<Output>, String> {
    let input: Input = serde_json::from_value(value).map_err(|e| e.to_string())?;
    Ok(core.handle(input))
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

fn holds(out: &[Output], needle: &str) -> bool {
    written(out)
        .iter()
        .any(|w| w.as_deref().is_some_and(|t| t.contains(needle)))
}

fn sign_in_body(user: &str) -> String {
    json!({"data":{"token":"tok","expires_at":"x","user":{"id":user}}}).to_string()
}

fn stored_text(site: &str, user: &str, path: &str) -> String {
    json!({"v":2,"owner":{"site":site,"user_id":user},"calls":[
        {"req_id":7,"method":"POST","path":path,"body":format!("{{\"api_key\":\"{SECRET}\"}}"),
         "if_match":null,"idempotency_key":"k-7"}]})
    .to_string()
}

struct Host {
    core: Core,
    req: u64,
}

impl Host {
    /// A core that has read `value` from storage, not yet signed in.
    fn restored(value: &str) -> Result<Self, String> {
        let mut core = Core::new();
        core.start("site-1");
        feed(
            &mut core,
            json!({"type":"storage_reply","id":1,"value":value}),
        )?;
        Ok(Self { core, req: 10 })
    }

    fn sign_in(&mut self, user: &str) -> Result<Vec<Output>, String> {
        self.req += 1;
        let out = feed(
            &mut self.core,
            json!({"type":"request","op":"sign_in","req_id":self.req,"user":user,"password":"pw"}),
        )?;
        let id = *http_ids(&out).first().ok_or("sign-in not sent")?;
        self.reply(id, 200, &sign_in_body(user))
    }

    fn reply(&mut self, id: u64, status: u16, body: &str) -> Result<Vec<Output>, String> {
        feed(
            &mut self.core,
            json!({"type":"transport_reply","id":id,"reply":{"status":status,"body":body}}),
        )
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
        feed(&mut self.core, request)
    }

    /// The host reads `/api/v1/meta/{doctype}` and the answer arrives; returns the outputs of
    /// the reply (where a rewrite of the queue would appear).
    fn meta(&mut self, doctype: &str, fields: Value) -> Result<Vec<Output>, String> {
        let out = self.call("GET", &format!("/api/v1/meta/{doctype}"), None)?;
        let id = *http_ids(&out).first().ok_or("meta read not sent")?;
        let body = json!({"data":{"doctype":doctype,"label":doctype,"version":1,"fields":fields}})
            .to_string();
        self.reply(id, 200, &body)
    }

    fn plain(&mut self, doctype: &str) -> Result<Vec<Output>, String> {
        self.meta(doctype, json!([{"name":"title","label":"T","type":"text"}]))
    }

    fn secret(&mut self, doctype: &str) -> Result<Vec<Output>, String> {
        self.meta(
            doctype,
            json!([{"name":"api_key","label":"K","type":"secret"}]),
        )
    }

    /// Any later write of the queue; a fresh call to a plain path makes the core look again.
    fn poke(&mut self) -> Result<Vec<Output>, String> {
        let out = self.call("POST", "/api/v1/method/ping", None)?;
        let id = *http_ids(&out).first().ok_or("poke not sent")?;
        let mut all = out;
        all.extend(self.reply(id, 200, "{}")?);
        Ok(all)
    }
}

fn customer() -> String {
    stored_text("site-1", "u", "/api/v1/resource/Customer")
}

#[test]
fn a_restored_call_is_kept_across_two_writes() -> Result<(), String> {
    let mut host = Host::restored(&customer())?;
    host.sign_in("u")?;
    // Two further writes of the queue (each poke changes it) both still hold the call.
    let first = host.poke()?;
    assert!(holds(&first, "k-7"), "kept in the first write");
    let second = host.poke()?;
    assert!(holds(&second, "k-7"), "and in the second");
    assert!(
        holds(&second, SECRET),
        "the stored value is exactly what the host gave"
    );
    Ok(())
}

#[test]
fn a_meta_reply_with_a_secret_drops_the_restored_call_and_it_never_returns() -> Result<(), String> {
    let mut host = Host::restored(&customer())?;
    host.sign_in("u")?;
    let out = host.secret("Customer")?;
    assert!(
        !holds(&out, SECRET),
        "dropped from the write the reply causes"
    );
    assert!(!holds(&host.poke()?, SECRET));
    // A reply that does not parse forgets the metadata; the call does not come back.
    let probe = host.call("GET", "/api/v1/meta/Customer", None)?;
    let id = *http_ids(&probe).first().ok_or("not sent")?;
    let out = host.reply(id, 200, "not json")?;
    assert!(!holds(&out, SECRET));
    assert!(!holds(&host.poke()?, SECRET));
    Ok(())
}

#[test]
fn a_secret_learned_before_the_sign_in_drops_the_call_from_storage_but_not_from_memory()
-> Result<(), String> {
    // The host reads meta before anyone signs in: a read with no session fails locally, so the
    // reply comes after the sign-in. The restored call is replayed exactly as in T3.
    let mut host = Host::restored(&customer())?;
    let out = host.sign_in("u")?;
    assert_eq!(http_ids(&out).len(), 1, "replayed once");
    assert!(!holds(&host.secret("Customer")?, SECRET));
    Ok(())
}

#[test]
fn a_secret_in_a_child_table_drops_the_restored_call() -> Result<(), String> {
    let mut host = Host::restored(&customer())?;
    host.sign_in("u")?;
    host.meta(
        "Customer",
        json!([{"name":"rows","label":"R","type":"table","child_doctype":"Row"}]),
    )?;
    // The child is not seen yet: not known to be unsafe, so the call stays.
    assert!(holds(&host.poke()?, "k-7"));
    let out = host.secret("Row")?;
    assert!(!holds(&out, SECRET));
    assert!(!holds(&host.poke()?, SECRET));
    Ok(())
}

#[test]
fn a_bytes_field_counts_as_a_secret() -> Result<(), String> {
    let mut host = Host::restored(&customer())?;
    host.sign_in("u")?;
    let out = host.meta("Customer", json!([{"name":"k","label":"K","type":"bytes"}]))?;
    assert!(!holds(&out, SECRET));
    Ok(())
}

#[test]
fn a_plain_meta_reply_leaves_the_restored_call_in_place() -> Result<(), String> {
    let mut host = Host::restored(&customer())?;
    host.sign_in("u")?;
    host.plain("Customer")?;
    assert!(holds(&host.poke()?, "k-7"));
    Ok(())
}

#[test]
fn an_unseen_doctype_is_never_admitted_fresh() -> Result<(), String> {
    let mut host = Host::restored(&customer())?;
    host.sign_in("u")?;
    // A call made in this session to another, unseen DocType is sent but not stored.
    let body = format!("{{\"api_key\":\"{SECRET}\"}}");
    let out = host.call("POST", "/api/v1/resource/Invoice", Some(&body))?;
    assert_eq!(http_ids(&out).len(), 1);
    assert!(!holds(&out, "Invoice"));
    // Even the same DocType as the restored call: a fresh call is not marked restored. Once
    // the restored one is dropped by a secret, the fresh one is not stored either.
    host.secret("Customer")?;
    let out = host.call("POST", "/api/v1/resource/Customer", Some(&body))?;
    assert!(!holds(&out, SECRET));
    assert!(!holds(&host.poke()?, SECRET));
    Ok(())
}

#[test]
fn sign_out_drops_the_restored_call() -> Result<(), String> {
    let mut host = Host::restored(&customer())?;
    host.sign_in("u")?;
    host.req += 1;
    let out = feed(
        &mut host.core,
        json!({"type":"request","op":"sign_out","req_id":host.req}),
    )?;
    let id = *http_ids(&out).first().ok_or("sign-out not sent")?;
    host.reply(id, 200, "{}")?;
    host.sign_in("u")?;
    assert!(!holds(&host.poke()?, SECRET));
    Ok(())
}

#[test]
fn a_user_change_drops_the_restored_call() -> Result<(), String> {
    let mut host = Host::restored(&customer())?;
    host.sign_in("u")?;
    let out = host.sign_in("v")?;
    assert!(!holds(&out, SECRET));
    assert!(!holds(&host.poke()?, SECRET));
    Ok(())
}

#[test]
fn a_restored_call_of_another_owner_or_site_is_discarded_and_never_stored() -> Result<(), String> {
    for (site, user) in [("site-1", "other"), ("site-2", "u")] {
        let mut host = Host::restored(&stored_text(site, user, "/api/v1/resource/Customer"))?;
        let out = host.sign_in("u")?;
        assert!(
            out.iter()
                .any(|o| matches!(o, Output::Emit(Event::QueueDiscarded { count: 1, .. })))
        );
        assert!(http_ids(&out).is_empty(), "never sent");
        assert!(!holds(&out, SECRET));
        assert!(!holds(&host.poke()?, SECRET));
    }
    Ok(())
}

#[test]
fn a_tampered_stored_call_on_a_denied_path_is_refused() -> Result<(), String> {
    for path in [
        "/api/v1/auth/login",
        "/api/v1/auth/password",
        "/api/v1/resource/%2e%2e/auth/login",
    ] {
        let mut host = Host::restored(&stored_text("site-1", "u", path))?;
        let out = host.sign_in("u")?;
        assert!(!holds(&out, SECRET), "{path}");
        assert!(!holds(&host.poke()?, SECRET), "{path}");
    }
    Ok(())
}
