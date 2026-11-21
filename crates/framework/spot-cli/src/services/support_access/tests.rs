//! Tests of the support-access decisions over an in-memory store and recording fakes.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use ed25519_dalek::{Signer, SigningKey};
use spot_deep_access::{AccessRequest, ConsentRecord, to_hex};
use spot_deep_access_pg::{
    ConsentRow, CredentialRow, KeyRow, RequestRow, RequestStatus, StoreError,
};
use spot_iam_access::stepup::AuthMethod;
use spot_support_session::{
    ActionScope, DocTypeScope, Scope, SessionSpec, Subject, SubjectKind, Tier,
};

use super::engine::{SFut, SupportAccess, SupportStore};
use super::issuance::Issuance;
use super::*;

const TENANT: &str = "tenant-a";

#[derive(Default)]
struct Mem {
    requests: Mutex<Vec<RequestRow>>,
    consents: Mutex<Vec<ConsentRow>>,
    credentials: Mutex<Vec<(CredentialRow, bool)>>,
    keys: Mutex<Vec<KeyRow>>,
}

impl SupportStore for Mem {
    fn put_request<'a>(&'a self, row: &'a RequestRow, _: i64) -> SFut<'a, bool> {
        Box::pin(async move {
            let mut all = self.requests.lock().unwrap();
            if all.iter().any(|r| r.id == row.id) {
                return Ok(false);
            }
            all.push(row.clone());
            Ok(true)
        })
    }
    fn get_request<'a>(&'a self, id: &'a str) -> SFut<'a, Option<RequestRow>> {
        Box::pin(async move {
            Ok(self
                .requests
                .lock()
                .unwrap()
                .iter()
                .find(|r| r.id == id)
                .cloned())
        })
    }
    fn list_requests<'a>(
        &'a self,
        _: Option<RequestStatus>,
        after: Option<&'a str>,
        limit: u32,
    ) -> SFut<'a, Vec<RequestRow>> {
        Box::pin(async move {
            let mut rows: Vec<_> = self
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| after.is_none_or(|a| r.id.as_str() > a))
                .cloned()
                .collect();
            rows.sort_by(|a, b| a.id.cmp(&b.id));
            rows.truncate(limit as usize);
            Ok(rows)
        })
    }
    fn transition<'a>(
        &'a self,
        id: &'a str,
        from: &'a [RequestStatus],
        to: RequestStatus,
        _: i64,
    ) -> SFut<'a, bool> {
        Box::pin(async move {
            let mut all = self.requests.lock().unwrap();
            match all
                .iter_mut()
                .find(|r| r.id == id && from.contains(&r.status))
            {
                Some(row) => {
                    row.status = to;
                    Ok(true)
                }
                None => Ok(false),
            }
        })
    }
    fn record_consent<'a>(&'a self, row: &'a ConsentRow) -> SFut<'a, bool> {
        Box::pin(async move {
            let mut all = self.consents.lock().unwrap();
            if all
                .iter()
                .any(|c| c.request_id == row.request_id && c.approver == row.approver)
            {
                return Ok(false);
            }
            all.push(row.clone());
            Ok(true)
        })
    }
    fn consents<'a>(&'a self, request_id: &'a str) -> SFut<'a, Vec<ConsentRow>> {
        Box::pin(async move {
            Ok(self
                .consents
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.request_id == request_id)
                .cloned()
                .collect())
        })
    }
    fn revoke<'a>(&'a self, session: [u8; 16], _: &'a str, _: &'a str, _: i64) -> SFut<'a, ()> {
        Box::pin(async move {
            for (c, revoked) in self.credentials.lock().unwrap().iter_mut() {
                if c.session_id == session {
                    *revoked = true;
                }
            }
            Ok(())
        })
    }
    fn record_credential<'a>(
        &'a self,
        row: &'a CredentialRow,
        _secret_digest: [u8; 32],
    ) -> SFut<'a, ()> {
        Box::pin(async move {
            self.credentials.lock().unwrap().push((row.clone(), false));
            Ok(())
        })
    }
    fn credentials<'a>(&'a self, _: u32) -> SFut<'a, Vec<(CredentialRow, bool)>> {
        Box::pin(async move { Ok(self.credentials.lock().unwrap().clone()) })
    }
    fn keys<'a>(&'a self) -> SFut<'a, Vec<KeyRow>> {
        Box::pin(async move { Ok(self.keys.lock().unwrap().clone()) })
    }
    fn enrol_key<'a>(&'a self, row: &'a KeyRow) -> SFut<'a, bool> {
        Box::pin(async move {
            self.keys.lock().unwrap().push(row.clone());
            Ok(true)
        })
    }
}

#[derive(Default)]
struct Audit {
    events: Mutex<Vec<SupportAuditEvent>>,
    fail: Mutex<bool>,
}

impl SupportAudit for Audit {
    fn record<'a>(
        &'a self,
        event: SupportAuditEvent,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        Box::pin(async move {
            if *self.fail.lock().unwrap() {
                return Err("down".to_owned());
            }
            self.events.lock().unwrap().push(event);
            Ok(())
        })
    }
}

struct Recordings;

impl SupportRecordings for Recordings {
    fn actions<'a>(
        &'a self,
        _: &'a str,
        _: u32,
    ) -> BoxFuture<'a, std::result::Result<Vec<RecordedAction>, String>> {
        Box::pin(async {
            Ok(vec![RecordedAction {
                at: "2026-11-13T10:00:00Z".to_owned(),
                action: "read".to_owned(),
                target: None,
            }])
        })
    }
}

struct Rig {
    engine: SupportAccess,
    store: Arc<Mem>,
    audit: Arc<Audit>,
    alice: SigningKey,
    bob: SigningKey,
}

fn admin(user: &str) -> Option<CallerInfo> {
    Some(CallerInfo {
        user_id: user.to_owned(),
        is_security_admin: true,
        step_up_current: true,
    })
}

fn rig(settings: Settings) -> Rig {
    let store = Arc::new(Mem::default());
    let audit = Arc::new(Audit::default());
    let (alice, bob) = (
        SigningKey::from_bytes(&[1; 32]),
        SigningKey::from_bytes(&[2; 32]),
    );
    for (name, key) in [("alice", &alice), ("bob", &bob)] {
        store.keys.lock().unwrap().push(KeyRow {
            key_id: name.to_owned(),
            identity: name.to_owned(),
            public_key: key.verifying_key().to_bytes(),
            is_owner: true,
            enrolled_by: "admin".to_owned(),
            enrolled_at: 1,
        });
    }
    let engine = SupportAccess {
        store: store.clone(),
        audit: audit.clone(),
        recordings: Some(Arc::new(Recordings)),
        settings,
        tenant: TENANT.to_owned(),
    };
    Rig {
        engine,
        store,
        audit,
        alice,
        bob,
    }
}

fn scope() -> Scope {
    Scope {
        doctypes: DocTypeScope::Named(vec!["Invoice".into()]),
        actions: ActionScope::Any,
    }
}

fn request(id: &str, tier: Tier, tenant: &str) -> AccessRequest {
    let subject = Subject::new(SubjectKind::Agent, "support-agent");
    AccessRequest {
        id: id.into(),
        requester: subject.clone(),
        reason: "Repair invoice".into(),
        ticket_ref: "SUP-42".into(),
        requested_at: 100,
        spec: SessionSpec {
            id: "grant".into(),
            tenant: tenant.into(),
            subject,
            tier_ceiling: tier,
            scope: scope(),
            reason: "Repair invoice".into(),
            requested_by: "support-requester".into(),
            starts_at: 100,
            expires_at: 10_000,
        },
    }
}

fn b64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0_u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(A[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A consent record and signature by `key` as `approver`.
fn consent_for(
    req: &AccessRequest,
    approver: &str,
    key: &SigningKey,
    scope: Scope,
    valid_to: i64,
) -> (String, String) {
    let record = ConsentRecord {
        request_hash: to_hex(&req.hash().unwrap()),
        approver: approver.to_owned(),
        approver_public_key: to_hex(&key.verifying_key().to_bytes()),
        method: AuthMethod::Hardware,
        approved_scope: scope,
        valid_from: 100,
        valid_to,
        nonce: to_hex(&[7; 16]),
    };
    let receipt = record.clone().into_receipt(vec![]).unwrap();
    let signature = key.sign(&receipt.signing_bytes().unwrap()).to_bytes();
    (serde_json::to_string(&record).unwrap(), b64(&signature))
}

async fn receive(rig: &Rig, req: &AccessRequest) {
    rig.engine
        .receive(&serde_json::to_string(req).unwrap(), 150)
        .await
        .expect("received");
}

fn outcomes(rig: &Rig) -> Vec<&'static str> {
    rig.audit
        .events
        .lock()
        .unwrap()
        .iter()
        .map(|e| e.outcome)
        .collect()
}

#[tokio::test]
async fn a_request_waits_pending_and_conveys_no_authority() {
    let rig = rig(Settings::default());
    let req = request("r1", Tier::T0, TENANT);
    receive(&rig, &req).await;
    let rows = rig
        .engine
        .list_requests(&admin("alice"), None, 10, 200)
        .await
        .unwrap();
    assert_eq!(rows[0]["status"], "pending");
    assert_eq!(rows[0]["approvals"], 0);
    // Not approved, so nothing can be issued from it, on any path.
    let issuance = Issuance {
        bridge: None,
        entropy: None,
        logs: None,
        verifier: None,
        policy: spot_deep_access::Policy::default(),
    };
    let attestation = spot_deep_access::OperatorAttestation {
        operator_key: [0; 32],
        vendor_key: spot_signed_command::KeyId::from_public_key(&[0; 32]),
        kind: "hardware".into(),
        valid_from: 0,
        valid_to: 1,
        signature: vec![],
    };
    let refused = issuance
        .issue(
            &rig.engine,
            IssueRequest {
                request_id: "r1".into(),
                operator_key: [0; 32],
                attestation,
            },
            200,
        )
        .await;
    assert!(matches!(
        refused,
        Err(IntakeError::Refused(Refusal::NotPending))
    ));
}

#[tokio::test]
async fn a_request_for_another_tenant_or_a_duplicate_is_refused() {
    let rig = rig(Settings::default());
    let other = request("r1", Tier::T0, "tenant-b");
    assert!(matches!(
        rig.engine
            .receive(&serde_json::to_string(&other).unwrap(), 150)
            .await,
        Err(Refusal::Invalid(_))
    ));
    assert!(rig.store.requests.lock().unwrap().is_empty());
    let req = request("r1", Tier::T0, TENANT);
    receive(&rig, &req).await;
    assert!(
        rig.engine
            .receive(&serde_json::to_string(&req).unwrap(), 160)
            .await
            .is_err()
    );
    assert!(rig.engine.receive("not json", 160).await.is_err());
}

#[tokio::test]
async fn a_valid_consent_approves_a_read_only_request() {
    let rig = rig(Settings::default());
    let req = request("r1", Tier::T0, TENANT);
    receive(&rig, &req).await;
    let (record, signature) = consent_for(&req, "alice", &rig.alice, scope(), 5_000);
    let reply = rig
        .engine
        .consent(&admin("alice"), "r1", &record, &signature, 200)
        .await
        .unwrap();
    assert_eq!(reply["status"], "approved");
    assert!(outcomes(&rig).contains(&"approved"));
    // It is no longer pending: a second consent is refused.
    assert!(matches!(
        rig.engine
            .consent(&admin("alice"), "r1", &record, &signature, 201)
            .await,
        Err(Refusal::NotPending)
    ));
}

#[tokio::test]
async fn a_consent_that_is_wrong_in_any_way_is_refused_with_its_code_and_audited() {
    let rig = rig(Settings::default());
    let req = request("r1", Tier::T0, TENANT);
    receive(&rig, &req).await;
    let code = |r: std::result::Result<serde_json::Value, Refusal>| match r {
        Err(Refusal::ConsentInvalid(code)) => code,
        other => panic!("expected a refused consent, got {other:?}"),
    };
    // Signed by the wrong key (bob signs, alice's identity).
    let (record, _) = consent_for(&req, "alice", &rig.alice, scope(), 5_000);
    let (_, bobs_signature) = consent_for(&req, "alice", &rig.bob, scope(), 5_000);
    assert_eq!(
        code(
            rig.engine
                .consent(&admin("alice"), "r1", &record, &bobs_signature, 200)
                .await
        ),
        "deep_access.bad_signature"
    );
    // Expired.
    let (record, signature) = consent_for(&req, "alice", &rig.alice, scope(), 150);
    assert_eq!(
        code(
            rig.engine
                .consent(&admin("alice"), "r1", &record, &signature, 200)
                .await
        ),
        "deep_access.expired_receipt"
    );
    // A wider scope than requested.
    let wider = Scope {
        doctypes: DocTypeScope::AllRead,
        actions: ActionScope::Any,
    };
    let (record, signature) = consent_for(&req, "alice", &rig.alice, wider, 5_000);
    assert_eq!(
        code(
            rig.engine
                .consent(&admin("alice"), "r1", &record, &signature, 200)
                .await
        ),
        "deep_access.widened_scope"
    );
    // For a different request.
    let mut other = req.clone();
    other.reason = "Something else".into();
    other.spec.reason = "Something else".into();
    let (record, signature) = consent_for(&other, "alice", &rig.alice, scope(), 5_000);
    assert_eq!(
        code(
            rig.engine
                .consent(&admin("alice"), "r1", &record, &signature, 200)
                .await
        ),
        "deep_access.wrong_request"
    );
    // Signed in as bob, approving as alice: not the caller's own identity.
    let (record, signature) = consent_for(&req, "alice", &rig.alice, scope(), 5_000);
    assert_eq!(
        code(
            rig.engine
                .consent(&admin("bob"), "r1", &record, &signature, 200)
                .await
        ),
        "deep_access.unknown_approver"
    );
    // Nothing was approved, and every refusal wrote an audit record.
    assert_eq!(
        rig.store.get_request("r1").await.unwrap().unwrap().status,
        RequestStatus::Pending
    );
    assert!(outcomes(&rig).len() >= 5);
    assert!(!outcomes(&rig).contains(&"approved"));
}

#[tokio::test]
async fn the_requester_cannot_approve_its_own_request() {
    let rig = rig(Settings::default());
    let mut req = request("r1", Tier::T0, TENANT);
    req.spec.requested_by = "alice".into();
    receive(&rig, &req).await;
    let (record, signature) = consent_for(&req, "alice", &rig.alice, scope(), 5_000);
    assert!(matches!(
        rig.engine
            .consent(&admin("alice"), "r1", &record, &signature, 200)
            .await,
        Err(Refusal::ConsentInvalid("deep_access.operator_approval"))
    ));
}

#[tokio::test]
async fn a_request_that_may_write_needs_two_different_administrators() {
    let rig = rig(Settings::default());
    let req = request("w1", Tier::T1, TENANT);
    receive(&rig, &req).await;
    let (record, signature) = consent_for(&req, "alice", &rig.alice, scope(), 5_000);
    let first = rig
        .engine
        .consent(&admin("alice"), "w1", &record, &signature, 200)
        .await
        .unwrap();
    assert_eq!(
        (first["status"].clone(), first["approvals_needed"].clone()),
        ("pending".into(), 2.into())
    );
    // The same administrator again is not a second approval.
    assert!(
        rig.engine
            .consent(&admin("alice"), "w1", &record, &signature, 201)
            .await
            .is_err()
    );
    let (record_b, signature_b) = consent_for(&req, "bob", &rig.bob, scope(), 5_000);
    let second = rig
        .engine
        .consent(&admin("bob"), "w1", &record_b, &signature_b, 202)
        .await
        .unwrap();
    assert_eq!(second["status"], "approved");
}

#[tokio::test]
async fn only_a_security_administrator_with_a_step_up_decides() {
    let rig = rig(Settings::default());
    let req = request("r1", Tier::T0, TENANT);
    receive(&rig, &req).await;
    let (record, signature) = consent_for(&req, "alice", &rig.alice, scope(), 5_000);
    let plain = Some(CallerInfo {
        user_id: "alice".into(),
        is_security_admin: false,
        step_up_current: true,
    });
    assert_eq!(
        rig.engine
            .consent(&plain, "r1", &record, &signature, 200)
            .await,
        Err(Refusal::Denied)
    );
    assert_eq!(
        rig.engine
            .consent(&None, "r1", &record, &signature, 200)
            .await,
        Err(Refusal::Denied)
    );
    let no_step_up = Some(CallerInfo {
        user_id: "alice".into(),
        is_security_admin: true,
        step_up_current: false,
    });
    assert_eq!(
        rig.engine
            .consent(&no_step_up, "r1", &record, &signature, 200)
            .await,
        Err(Refusal::StepUp)
    );
    assert_eq!(
        rig.engine.list_requests(&plain, None, 10, 200).await,
        Err(Refusal::Denied)
    );
    assert!(outcomes(&rig).iter().filter(|o| **o == "denied").count() >= 3);
}

#[tokio::test]
async fn nothing_is_decided_when_the_audit_record_cannot_be_written() {
    let rig = rig(Settings::default());
    let req = request("r1", Tier::T0, TENANT);
    receive(&rig, &req).await;
    *rig.audit.fail.lock().unwrap() = true;
    let (record, signature) = consent_for(&req, "alice", &rig.alice, scope(), 5_000);
    assert_eq!(
        rig.engine
            .consent(&admin("alice"), "r1", &record, &signature, 200)
            .await,
        Err(Refusal::Unavailable)
    );
    assert_eq!(
        rig.engine.revoke(&admin("alice"), "r1", "no", 201).await,
        Err(Refusal::Unavailable)
    );
    assert_eq!(
        rig.store.get_request("r1").await.unwrap().unwrap().status,
        RequestStatus::Pending
    );
}

fn credential(session: u8, request: &str) -> (CredentialRow, bool) {
    (
        CredentialRow {
            session_id: [session; 16],
            request_id: request.to_owned(),
            request_hash: "ab".repeat(32),
            receipt_hash: "cd".repeat(32),
            scope: "read".to_owned(),
            starts_at: 100,
            expires_at: 5_000,
            issued_at: 100,
        },
        false,
    )
}

#[tokio::test]
async fn revocation_ends_the_sessions_at_once_and_is_audited_first() {
    let rig = rig(Settings::default());
    let req = request("r1", Tier::T0, TENANT);
    receive(&rig, &req).await;
    rig.store.requests.lock().unwrap()[0].status = RequestStatus::Issued;
    rig.store
        .credentials
        .lock()
        .unwrap()
        .push(credential(1, "r1"));
    rig.store
        .credentials
        .lock()
        .unwrap()
        .push(credential(2, "other"));
    let reply = rig
        .engine
        .revoke(&admin("alice"), "r1", "no longer needed", 300)
        .await
        .unwrap();
    assert_eq!(reply["sessions_ended"], 1);
    let credentials = rig.store.credentials.lock().unwrap().clone();
    assert!(credentials[0].1);
    assert!(!credentials[1].1);
    assert!(outcomes(&rig).contains(&"revoked"));
    let sessions = rig
        .engine
        .list_sessions(&admin("alice"), 10, 301)
        .await
        .unwrap();
    assert_eq!(sessions[0]["state"], "revoked");
    assert_eq!(sessions[1]["state"], "active");
    // A revoked request cannot be revoked or approved again.
    assert!(matches!(
        rig.engine.revoke(&admin("alice"), "r1", "again", 302).await,
        Err(Refusal::NotPending)
    ));
}

#[tokio::test]
async fn a_recording_shows_the_actions_of_a_known_session_only() {
    let rig = rig(Settings::default());
    rig.store
        .credentials
        .lock()
        .unwrap()
        .push(credential(1, "r1"));
    let known = to_hex(&[1; 16]);
    let rows = rig
        .engine
        .recording(&admin("alice"), &known, 10, 200)
        .await
        .unwrap();
    assert_eq!(rows[0].as_object().unwrap().len(), 3);
    assert_eq!(
        rig.engine
            .recording(&admin("alice"), &to_hex(&[9; 16]), 10, 200)
            .await,
        Err(Refusal::NotFound)
    );
    assert!(matches!(
        rig.engine
            .recording(&admin("alice"), "../etc", 10, 200)
            .await,
        Err(Refusal::Invalid(_))
    ));
    let plain = Some(CallerInfo {
        user_id: "x".into(),
        is_security_admin: false,
        step_up_current: true,
    });
    assert_eq!(
        rig.engine.recording(&plain, &known, 10, 200).await,
        Err(Refusal::Denied)
    );
}

#[test]
fn base64_decodes_exactly_the_standard_form() {
    assert_eq!(base64_decode("c2ln", 16), Some(b"sig".to_vec()));
    assert_eq!(base64_decode("c2lnbg==", 16), Some(b"sign".to_vec()));
    assert_eq!(base64_decode("c2lnbmE=", 16), Some(b"signa".to_vec()));
    assert_eq!(base64_decode("c2l", 16), None);
    assert_eq!(base64_decode("c2l*", 16), None);
    assert_eq!(base64_decode("=c2l", 16), None);
    assert_eq!(base64_decode("c2lnc2lnc2lnc2lnc2ln", 4), None);
    assert_eq!(base64_decode("", 4), None);
    assert_eq!(base64_decode(&b64(&[0xff; 64]), 64), Some(vec![0xff; 64]));
}

#[test]
fn a_cursor_is_bound_to_the_caller_and_the_query() {
    let cursor = cursor_make("alice", "digest-1", "r5");
    assert_eq!(cursor_parse(&cursor, "alice", "digest-1"), Some("r5"));
    assert_eq!(cursor_parse(&cursor, "bob", "digest-1"), None);
    assert_eq!(cursor_parse(&cursor, "alice", "digest-2"), None);
    assert_eq!(cursor_parse("garbage", "alice", "digest-1"), None);
}

#[test]
fn settings_are_validated() {
    assert!(Settings::default().validate().is_ok());
    for bad in [
        Settings {
            approvals_required: 0,
            ..Settings::default()
        },
        Settings {
            approvals_required: 3,
            ..Settings::default()
        },
        Settings {
            sweep_every_secs: 5,
            ..Settings::default()
        },
        Settings {
            nonce_keep_days: 10,
            ..Settings::default()
        },
    ] {
        assert!(bad.validate().is_err());
    }
    let _ = StoreError::Failed;
}
