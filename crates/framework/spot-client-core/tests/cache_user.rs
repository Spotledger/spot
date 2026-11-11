//! One user's cached read never answers another user's request.
#![cfg(not(feature = "storage"))]
#![allow(missing_docs)]

use serde_json::json;
use spot_client_core::{Core, Event, Input, Output};

fn feed(core: &mut Core, value: serde_json::Value) -> Result<Vec<Output>, String> {
    let input: Input = serde_json::from_value(value).map_err(|e| e.to_string())?;
    Ok(core.handle(input))
}

fn sign_in(core: &mut Core, req_id: u64, http_id: u64, user: &str) -> Result<(), String> {
    feed(
        core,
        json!({"type":"request","op":"sign_in","req_id":req_id,"user":user,"password":"pw"}),
    )?;
    let body = json!({"data":{"token":format!("tok-{user}"),"expires_at":"x","user":{"id":user}}});
    feed(
        core,
        json!({"type":"transport_reply","id":http_id,"reply":{"status":200,"body":body.to_string()}}),
    )?;
    Ok(())
}

fn has_if_none_match(out: &[Output]) -> bool {
    out.iter().any(|o| match o {
        Output::SendHttp { request, .. } => request
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("If-None-Match")),
        _ => false,
    })
}

#[test]
fn a_different_user_starts_with_an_empty_cache() -> Result<(), String> {
    let mut core = Core::new();
    sign_in(&mut core, 1, 1, "alice")?;
    feed(
        &mut core,
        json!({"type":"request","op":"resource_get","req_id":2,"doctype":"Invoice","id":"I1"}),
    )?;
    feed(
        &mut core,
        json!({"type":"transport_reply","id":2,"reply":{"status":200,"headers":[["ETag","\"3\""]],"body":"{\"a\":1}"}}),
    )?;
    let again = feed(
        &mut core,
        json!({"type":"request","op":"resource_get","req_id":3,"doctype":"Invoice","id":"I1"}),
    )?;
    assert!(has_if_none_match(&again), "same user revalidates");

    sign_in(&mut core, 4, 4, "bob")?;
    let bob = feed(
        &mut core,
        json!({"type":"request","op":"resource_get","req_id":5,"doctype":"Invoice","id":"I1"}),
    )?;
    assert!(
        !has_if_none_match(&bob),
        "another user must not reuse the cache"
    );
    assert!(
        !bob.iter()
            .any(|o| matches!(o, Output::Emit(Event::Resource { .. })))
    );
    Ok(())
}
