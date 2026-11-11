//! The optional persistence capability: pending changes are stored, the session token never is.
#![cfg(feature = "storage")]
#![allow(missing_docs)]

use serde_json::json;
use spot_client_core::{Core, Event, Input, Output};

fn input(value: serde_json::Value) -> Result<Input, String> {
    serde_json::from_value(value).map_err(|e| e.to_string())
}

#[test]
fn queue_is_written_without_the_token_and_restored_on_start() -> Result<(), String> {
    let mut core = Core::new();
    assert!(matches!(
        core.start("site-1").as_slice(),
        [Output::StorageRead { .. }]
    ));
    core.handle(input(
        json!({"type":"request","op":"sign_in","req_id":1,"user":"u","password":"pw"}),
    )?);
    core.handle(input(json!({"type":"transport_reply","id":1,"reply":{"status":200,"body":"{\"data\":{\"token\":\"tok-secret\",\"expires_at\":\"x\",\"user\":{\"id\":\"u\"}}}"}}))?);
    // The core has seen that Invoice holds no secret field, so its writes may be stored.
    core.handle(input(json!({"type":"request","op":"call","req_id":3,"method":"GET","path":"/api/v1/meta/Invoice"}))?);
    let meta = json!({"data":{"doctype":"Invoice","label":"Invoice","version":1,"fields":[{"name":"t","label":"T","type":"text"}]}}).to_string();
    core.handle(input(
        json!({"type":"transport_reply","id":2,"reply":{"status":200,"body":meta}}),
    )?);
    let out = core.handle(input(json!({"type":"request","op":"call","req_id":2,"method":"POST","path":"/api/v1/resource/Invoice","body":"{}","idempotency_key":"k-1"}))?);
    let write = out.iter().find_map(|o| match o {
        Output::StorageWrite { value: Some(v), .. } => Some(v.clone()),
        _ => None,
    });
    let text = write.ok_or("no StorageWrite")?;
    assert!(text.contains("k-1") && !text.contains("tok-secret") && !text.contains("pw"));
    assert!(
        text.starts_with(r#"{"v":2,"owner":{"site":"site-1","user_id":"u"},"calls":["#),
        "{text}"
    );

    let mut fresh = Core::new();
    fresh.start("site-1");
    let out = fresh.handle(input(json!({"type":"storage_reply","id":1,"value":text}))?);
    let reauth = out
        .iter()
        .filter(|o| matches!(o, Output::Emit(Event::ReauthRequired { .. })))
        .count();
    assert_eq!(reauth, 1, "one held call, one ReauthRequired");
    assert!(
        !out.iter().any(|o| matches!(o, Output::SendHttp { .. })),
        "nothing is sent before a sign-in has established the user"
    );
    Ok(())
}

fn sign_in(core: &mut Core, user: &str) -> Result<Vec<Output>, String> {
    core.handle(input(
        json!({"type":"request","op":"sign_in","req_id":9,"user":user,"password":"pw"}),
    )?);
    let body = json!({"data":{"token":"t","expires_at":"x","user":{"id":user}}}).to_string();
    // The sign-in is the first request, so its transport id is 1 on a fresh core.
    Ok(core.handle(input(
        json!({"type":"transport_reply","id":1,"reply":{"status":200,"body":body}}),
    )?))
}

fn queue_of(site: &str, user: &str) -> String {
    json!({"v":2,"owner":{"site":site,"user_id":user},"calls":[
        {"req_id":2,"method":"POST","path":"/api/v1/method/invoice_ping","body":"{}",
         "if_match":null,"idempotency_key":"k-1"}]})
    .to_string()
}

fn restore(site: &str, stored: &str) -> Result<Core, String> {
    let mut core = Core::new();
    core.start(site);
    core.handle(input(
        json!({"type":"storage_reply","id":1,"value":stored}),
    )?);
    Ok(core)
}

fn discarded(out: &[Output]) -> Option<(u64, String)> {
    out.iter().find_map(|o| match o {
        Output::Emit(Event::QueueDiscarded { count, reason }) => {
            Some((*count, format!("{reason:?}")))
        }
        _ => None,
    })
}

#[test]
fn another_users_queue_is_discarded_unsent() -> Result<(), String> {
    let mut core = restore("site-1", &queue_of("site-1", "alice"))?;
    let out = sign_in(&mut core, "bob")?;
    assert_eq!(discarded(&out), Some((1, "OwnerChanged".to_owned())));
    assert!(out.iter().any(|o| matches!(
        o,
        Output::Emit(Event::Failed { req_id: 2, problem, .. }) if problem.code == "auth.session_changed"
    )));
    assert!(!out.iter().any(|o| matches!(o, Output::SendHttp { .. })));
    assert!(
        out.iter()
            .any(|o| matches!(o, Output::StorageWrite { value: None, .. }))
    );
    Ok(())
}

#[test]
fn the_same_users_queue_goes_out_once_and_is_not_rewritten() -> Result<(), String> {
    let mut core = restore("site-1", &queue_of("site-1", "alice"))?;
    let out = sign_in(&mut core, "alice")?;
    assert_eq!(discarded(&out), None);
    let sent = out
        .iter()
        .filter(|o| matches!(o, Output::SendHttp { request, .. } if request.path == "/api/v1/method/invoice_ping"))
        .count();
    assert_eq!(sent, 1);
    assert!(!out.iter().any(|o| matches!(o, Output::StorageWrite { .. })));
    Ok(())
}

#[test]
fn a_queue_for_another_site_is_discarded() -> Result<(), String> {
    let mut core = restore("site-2", &queue_of("site-1", "alice"))?;
    let out = sign_in(&mut core, "alice")?;
    assert_eq!(discarded(&out), Some((1, "OwnerChanged".to_owned())));
    Ok(())
}

#[test]
fn untagged_and_unreadable_values_are_discarded() -> Result<(), String> {
    let old = json!([{"req_id":2,"method":"POST","path":"/api/v1/method/invoice_ping","body":"{}",
        "if_match":null,"idempotency_key":"k"}])
    .to_string();
    let mut core = restore("site-1", &old)?;
    let out = sign_in(&mut core, "alice")?;
    assert_eq!(discarded(&out), Some((1, "Untagged".to_owned())));
    assert!(!out.iter().any(|o| matches!(o, Output::SendHttp { .. })));
    for garbage in [
        "",
        "null",
        "{",
        "[1,2]",
        r#"{"v":2}"#,
        r#"{"v":1,"owner":{"site":"site-1","user_id":"alice"},"calls":[]}"#,
    ] {
        let mut core = restore("site-1", garbage)?;
        let out = sign_in(&mut core, "alice")?;
        assert_eq!(
            discarded(&out),
            Some((0, "Unreadable".to_owned())),
            "{garbage}"
        );
        assert!(
            out.iter()
                .any(|o| matches!(o, Output::StorageWrite { value: None, .. }))
        );
    }
    Ok(())
}

#[test]
fn a_late_storage_reply_is_judged_against_the_signed_in_user() -> Result<(), String> {
    let mut core = Core::new();
    core.start("site-1");
    sign_in(&mut core, "bob")?;
    let out = core.handle(input(
        json!({"type":"storage_reply","id":1,"value":queue_of("site-1","alice")}),
    )?);
    assert_eq!(discarded(&out), Some((1, "OwnerChanged".to_owned())));
    assert!(!out.iter().any(|o| matches!(o, Output::SendHttp { .. })));
    Ok(())
}

#[test]
fn no_queue_is_written_without_a_site_or_a_user() -> Result<(), String> {
    {
        let mut core = Core::new();
        core.start("");
        sign_in(&mut core, "alice")?;
        let out = core.handle(input(
            json!({"type":"request","op":"call","req_id":2,"method":"POST","path":"/api/v1/method/invoice_ping","body":"{}"}),
        )?);
        assert!(out.iter().any(|o| matches!(o, Output::SendHttp { .. })));
        assert!(!out.iter().any(|o| matches!(o, Output::StorageWrite { .. })));
    }
    // A sign-in answer with no user id leaves the owner unknown.
    let mut core = Core::new();
    core.start("site-1");
    core.handle(input(
        json!({"type":"request","op":"sign_in","req_id":9,"user":"a","password":"pw"}),
    )?);
    let body = r#"{"data":{"token":"t","expires_at":"x","user":{}}}"#;
    core.handle(input(
        json!({"type":"transport_reply","id":1,"reply":{"status":200,"body":body}}),
    )?);
    let out = core.handle(input(
        json!({"type":"request","op":"call","req_id":2,"method":"POST","path":"/api/v1/method/invoice_ping","body":"{}"}),
    )?);
    assert!(!out.iter().any(|o| matches!(o, Output::StorageWrite { .. })));
    Ok(())
}
