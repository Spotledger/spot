//! WP-ADM-11.T3, control-plane side: request, two-person approval, the time-boxed open, revoke,
//! expiry and the recording report, over an in-memory store and fake ports.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use spot_break_glass::{FileRecorder, FrameKind, Recorder};
use spot_control_plane::CpCode;
use spot_control_plane::CpError;
use spot_control_plane::auth::{HostIdentity, OperatorCall, ScopedTenants, TenantScope};
use spot_control_plane::modules::break_glass::{
    AgentDispatch, BreakGlassApprover, Grant, GrantState as State, MemoryKv, MemoryRecordings,
    Plane, RequestBody,
};
use spot_core::{Clock, ManualClock};
use spot_mgmt_api::ports::{BoxFuture, MemoryAuditSink};
use time::OffsetDateTime;

const NOW: i64 = 1_800_000_000;

/// An approval is `sig:<key>:<tenant>:<grant>`: valid only for the tenant and grant it names.
struct Approver;

impl BreakGlassApprover for Approver {
    fn verify<'a>(
        &'a self,
        approval: &'a str,
        tenant: &'a str,
        grant: &'a str,
        _now: i64,
    ) -> BoxFuture<'a, Result<String, CpError>> {
        Box::pin(async move {
            let parts: Vec<&str> = approval.split(':').collect();
            match parts.as_slice() {
                ["sig", key, t, g] if *t == tenant && *g == grant => Ok((*key).to_owned()),
                _ => Err(CpError::new(CpCode::SignatureInvalid)),
            }
        })
    }
}

#[derive(Default)]
struct Dispatch {
    sent: Mutex<Vec<(String, String, Value)>>,
    down: Mutex<bool>,
}

impl AgentDispatch for Dispatch {
    fn send<'a>(
        &'a self,
        tenant: &'a str,
        op: &'a str,
        args: Value,
    ) -> BoxFuture<'a, Result<(), CpError>> {
        Box::pin(async move {
            if *self.down.lock().unwrap() {
                return Err(CpError::new(CpCode::Internal));
            }
            self.sent
                .lock()
                .unwrap()
                .push((tenant.to_owned(), op.to_owned(), args));
            Ok(())
        })
    }
}

struct Rig {
    plane: Arc<Plane>,
    clock: Arc<ManualClock>,
    dispatch: Arc<Dispatch>,
    audit: Arc<MemoryAuditSink>,
    recordings: Arc<MemoryRecordings>,
}

fn rig() -> Rig {
    let clock = Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
    ));
    let dispatch = Arc::new(Dispatch::default());
    let audit = Arc::new(MemoryAuditSink::new());
    let recordings = Arc::new(MemoryRecordings::default());
    let plane = Plane::new(
        Arc::new(MemoryKv::new()),
        Arc::new(Approver),
        Arc::clone(&dispatch) as _,
        Arc::clone(&recordings) as _,
        Arc::clone(&clock) as Arc<dyn Clock>,
        Arc::clone(&audit) as _,
    );
    Rig {
        plane,
        clock,
        dispatch,
        audit,
        recordings,
    }
}

fn call(principal: &str, tenants: &[&str]) -> OperatorCall {
    OperatorCall {
        principal: principal.to_owned(),
        scope: ScopedTenants::new(TenantScope::Ids(
            tenants
                .iter()
                .map(|t| (*t).to_owned())
                .collect::<BTreeSet<_>>(),
        )),
        command: None,
        request_id: "req-1".to_owned(),
    }
}

fn body(seconds: u64) -> RequestBody {
    RequestBody {
        tenant: "acme".to_owned(),
        reason: "repair a stuck invoice workflow".to_owned(),
        scope: vec!["invoices".to_owned()],
        duration_seconds: seconds,
    }
}

fn sig(key: &str, grant: &Grant) -> String {
    format!("sig:{key}:{}:{}", grant.tenant, grant.id)
}

async fn approved(r: &Rig, seconds: u64) -> Grant {
    let asked = r
        .plane
        .request(&call("support-1", &["acme"]), body(seconds))
        .await
        .unwrap();
    r.plane
        .approve(
            &call("admin-a", &["acme"]),
            &asked.id,
            &sig("key-a", &asked),
        )
        .await
        .unwrap();
    r.plane
        .approve(
            &call("admin-b", &["acme"]),
            &asked.id,
            &sig("key-b", &asked),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn self_approval_is_refused_and_one_approval_is_not_enough() {
    let r = rig();
    let asked = r
        .plane
        .request(&call("support-1", &["acme"]), body(600))
        .await
        .unwrap();
    let me = r
        .plane
        .approve(
            &call("support-1", &["acme"]),
            &asked.id,
            &sig("key-x", &asked),
        )
        .await
        .unwrap_err();
    assert_eq!(me.code, CpCode::Forbidden);
    let one = r
        .plane
        .approve(
            &call("admin-a", &["acme"]),
            &asked.id,
            &sig("key-a", &asked),
        )
        .await
        .unwrap();
    assert_eq!(
        one.state,
        State::Requested,
        "a single approval does not approve"
    );
    let open = r
        .plane
        .open(&call("support-1", &["acme"]), &asked.id)
        .await
        .unwrap_err();
    assert_eq!(open.code, CpCode::Conflict);
    assert!(
        r.dispatch.sent.lock().unwrap().is_empty(),
        "nothing was sent to an agent"
    );
}

#[tokio::test]
async fn the_same_principal_or_key_cannot_approve_twice_and_a_wrong_binding_is_refused() {
    let r = rig();
    let asked = r
        .plane
        .request(&call("support-1", &["acme"]), body(600))
        .await
        .unwrap();
    r.plane
        .approve(
            &call("admin-a", &["acme"]),
            &asked.id,
            &sig("key-a", &asked),
        )
        .await
        .unwrap();
    let same_principal = r
        .plane
        .approve(
            &call("admin-a", &["acme"]),
            &asked.id,
            &sig("key-b", &asked),
        )
        .await
        .unwrap_err();
    assert_eq!(same_principal.code, CpCode::Forbidden);
    let same_key = r
        .plane
        .approve(
            &call("admin-b", &["acme"]),
            &asked.id,
            &sig("key-a", &asked),
        )
        .await
        .unwrap_err();
    assert_eq!(same_key.code, CpCode::Forbidden);
    let wrong = r
        .plane
        .approve(
            &call("admin-b", &["acme"]),
            &asked.id,
            "sig:key-b:other:xxxxxxxxxxxxxxxxxxxxxxxx",
        )
        .await
        .unwrap_err();
    assert_eq!(wrong.code, CpCode::SignatureInvalid);
    let two = r
        .plane
        .approve(
            &call("admin-b", &["acme"]),
            &asked.id,
            &sig("key-b", &asked),
        )
        .await
        .unwrap();
    assert_eq!(two.state, State::Approved);
    assert_eq!(two.expires_at, Some(NOW + 600));
}

#[tokio::test]
async fn open_sends_the_remaining_seconds_and_a_window_that_passed_can_never_open() {
    let r = rig();
    let grant = approved(&r, 600).await;
    r.clock.advance(Duration::from_secs(100));
    let opened = r
        .plane
        .open(&call("support-1", &["acme"]), &grant.id)
        .await
        .unwrap();
    assert_eq!(opened.state, State::Opened);
    let sent = r.dispatch.sent.lock().unwrap().clone();
    assert_eq!(sent[0].1, "breakglass.open");
    assert_eq!(sent[0].2["duration_seconds"], 500);
    assert_eq!(sent[0].2["session"], grant.id.as_str());

    let late = approved(&r, 120).await;
    r.clock.advance(Duration::from_secs(121));
    let refused = r
        .plane
        .open(&call("support-1", &["acme"]), &late.id)
        .await
        .unwrap_err();
    assert_eq!(refused.code, CpCode::Conflict);
    let again = r
        .plane
        .open(&call("support-1", &["acme"]), &late.id)
        .await
        .unwrap_err();
    assert_eq!(
        again.code,
        CpCode::Conflict,
        "an expired grant cannot be reopened"
    );
}

#[tokio::test]
async fn an_unreachable_agent_leaves_the_grant_approved_and_fails_closed() {
    let r = rig();
    let grant = approved(&r, 600).await;
    *r.dispatch.down.lock().unwrap() = true;
    assert!(
        r.plane
            .open(&call("support-1", &["acme"]), &grant.id)
            .await
            .is_err()
    );
    *r.dispatch.down.lock().unwrap() = false;
    assert_eq!(
        r.plane
            .open(&call("support-1", &["acme"]), &grant.id)
            .await
            .unwrap()
            .state,
        State::Opened
    );
}

#[tokio::test]
async fn revoke_closes_the_session_and_a_close_that_could_not_be_sent_is_retried() {
    let r = rig();
    let grant = approved(&r, 600).await;
    r.plane
        .open(&call("support-1", &["acme"]), &grant.id)
        .await
        .unwrap();
    *r.dispatch.down.lock().unwrap() = true;
    let revoked = r
        .plane
        .revoke(&call("admin-a", &["acme"]), &grant.id)
        .await
        .unwrap();
    assert_eq!(revoked.state, State::Revoked);
    assert!(revoked.close_pending);
    *r.dispatch.down.lock().unwrap() = false;
    r.plane.sweep().await;
    let sent = r.dispatch.sent.lock().unwrap().clone();
    assert!(
        sent.iter().any(|s| s.1 == "breakglass.close"),
        "the sweep delivered the close"
    );
    let again = r
        .plane
        .revoke(&call("admin-a", &["acme"]), &grant.id)
        .await
        .unwrap_err();
    assert_eq!(again.code, CpCode::Conflict, "revocation is final");
}

#[tokio::test]
async fn the_sweep_ends_a_grant_past_its_window_and_asks_the_agent_to_close() {
    let r = rig();
    let grant = approved(&r, 300).await;
    r.plane
        .open(&call("support-1", &["acme"]), &grant.id)
        .await
        .unwrap();
    r.clock.advance(Duration::from_secs(301));
    assert_eq!(r.plane.sweep().await, 1);
    let listed = r.plane.list(&call("support-1", &["acme"])).await.unwrap();
    assert_eq!(listed[0].state, State::Expired);
    assert!(
        r.dispatch
            .sent
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.1 == "breakglass.close")
    );
    assert_eq!(r.plane.sweep().await, 0);
}

#[tokio::test]
async fn another_tenants_operator_sees_nothing_and_a_bad_request_is_invalid() {
    let r = rig();
    let grant = approved(&r, 600).await;
    let other = call("support-2", &["globex"]);
    assert_eq!(
        r.plane.open(&other, &grant.id).await.unwrap_err().code,
        CpCode::NotFound
    );
    assert_eq!(
        r.plane.revoke(&other, &grant.id).await.unwrap_err().code,
        CpCode::NotFound
    );
    assert!(r.plane.list(&other).await.unwrap().is_empty());
    assert_eq!(
        r.plane.request(&other, body(600)).await.unwrap_err().code,
        CpCode::NotFound
    );
    let operator = call("support-1", &["acme"]);
    for seconds in [30, 14_401] {
        assert_eq!(
            r.plane
                .request(&operator, body(seconds))
                .await
                .unwrap_err()
                .code,
            CpCode::Invalid
        );
    }
    let mut empty = body(600);
    empty.scope.clear();
    assert_eq!(
        r.plane.request(&operator, empty).await.unwrap_err().code,
        CpCode::Invalid
    );
    let mut short = body(600);
    short.reason = "x".to_owned();
    assert_eq!(
        r.plane.request(&operator, short).await.unwrap_err().code,
        CpCode::Invalid
    );
}

fn recording(tamper: bool) -> (Vec<u8>, String) {
    let dir = std::env::temp_dir().join(format!("spot_cp_bg_{}_{tamper}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut rec = FileRecorder::create(&dir, "s", "acme", NOW, 1 << 20).unwrap();
    rec.append(FrameKind::Input, NOW, b"ls\n", true).unwrap();
    let digest = spot_break_glass::hex(&rec.finish().unwrap());
    let mut bytes = std::fs::read(FileRecorder::recording_path(&dir, "s")).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    if tamper {
        let at = bytes.len() - 3;
        bytes[at] ^= 0xff;
    }
    (bytes, digest)
}

#[tokio::test]
async fn the_recording_is_verified_against_the_reported_digest_before_it_is_kept() {
    let r = rig();
    let grant = approved(&r, 600).await;
    r.plane
        .open(&call("support-1", &["acme"]), &grant.id)
        .await
        .unwrap();
    let host = HostIdentity {
        tenant: "acme".into(),
        host_id: "host-1".into(),
        fingerprint: "ff".into(),
    };
    let (good, digest) = recording(false);
    // Not reported yet.
    assert_eq!(
        r.plane
            .upload(&host, &grant.id, good.clone())
            .await
            .unwrap_err()
            .code,
        CpCode::Conflict
    );
    let closed = r.plane.closed(&host, &grant.id, &digest).await.unwrap();
    assert_eq!(closed.state, State::Closed);
    let (bad, _) = recording(true);
    assert_eq!(
        r.plane
            .upload(&host, &grant.id, bad)
            .await
            .unwrap_err()
            .code,
        CpCode::Invalid
    );
    assert!(r.recordings.get("acme", &grant.id).is_none());
    r.plane
        .upload(&host, &grant.id, good.clone())
        .await
        .unwrap();
    assert_eq!(r.recordings.get("acme", &grant.id).unwrap(), good);
    assert_eq!(
        r.plane
            .upload(&host, &grant.id, good)
            .await
            .unwrap_err()
            .code,
        CpCode::Conflict,
        "kept once"
    );
    let foreign = HostIdentity {
        tenant: "globex".into(),
        host_id: "host-2".into(),
        fingerprint: "ee".into(),
    };
    assert_eq!(
        r.plane
            .closed(&foreign, &grant.id, &digest)
            .await
            .unwrap_err()
            .code,
        CpCode::NotFound
    );
    assert_eq!(
        r.plane
            .closed(&host, &grant.id, "zz")
            .await
            .unwrap_err()
            .code,
        CpCode::Invalid
    );
}

#[tokio::test]
async fn every_step_is_audited_and_no_record_carries_the_reason_scope_or_signature() {
    let r = rig();
    let grant = approved(&r, 600).await;
    let _ = r
        .plane
        .approve(&call("support-1", &["acme"]), &grant.id, "sig:k:acme:x")
        .await;
    let records = r.audit.records();
    assert!(
        records.len() >= 4,
        "request, two approvals and a refusal at least"
    );
    let text = format!("{records:?}");
    for secret in [
        "repair a stuck invoice workflow",
        "invoices",
        "sig:key-a",
        "sig:key-b",
    ] {
        assert!(!text.contains(secret), "{secret} reached an audit record");
    }
    assert!(
        records
            .iter()
            .any(|r| r.outcome == spot_mgmt_api::ports::AuditOutcome::Refused)
    );
}
