//! The real adapters behind the approval service's traits (WP-MIG-04.T2). Each section runs
//! only with its feature (`cargo test --all-features` runs them all).
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    unused_imports,
    dead_code,
    deprecated
)]

use spot_migration_approval::{
    Act, Approval, ApprovalPolicy, ApprovalService, ApprovalStore, AuditSink, AuthorshipCheck,
    Checks, Decision, Identity, InMemoryApprovalStore, PlanDigest, Refusal, SignatureHook,
    authorise_apply, codes,
};
use spot_migration_plan::{
    BlockingClass, Change, ChangeKind, ReleasePlan, Risk, SchemaPlan, SystemDescription, plan,
};

const SECOND: i64 = 1_000_000;
const HOUR: i64 = 3_600 * SECOND;
const T0: i64 = 1_000 * HOUR;

fn plan_with_risk(risk: Risk) -> ReleasePlan {
    let empty = SystemDescription::new();
    let mut release = plan(&empty, &empty, &SchemaPlan::default());
    release.changes.push(Change::new(
        "schema/example",
        ChangeKind::SchemaAddColumn,
        "schema/add_column",
        "invoice.note",
        "Add a column",
        risk,
        BlockingClass::Online,
    ));
    release.risk = risk;
    release
}

fn approval(plan: &ReleasePlan, who: &str, signature: Option<String>) -> Approval {
    Approval {
        plan_digest: PlanDigest::of(plan),
        approver: who.to_owned(),
        role: "release_manager".to_owned(),
        decision: Decision::Approve,
        comment: String::new(),
        at: T0,
        signature,
    }
}

fn act<'a>(who: &'a str, signature: Option<String>, now: i64) -> Act<'a> {
    Act {
        identity: Identity::new(who, &["release_manager"]),
        role: "release_manager",
        comment: "ok",
        signature,
        now,
    }
}

#[cfg(feature = "sod")]
mod sod {
    use super::*;
    use std::sync::Arc;

    use spot_migration_approval::adapters::SodAuthorship;

    #[test]
    #[allow(deprecated)]
    fn the_legacy_in_memory_record_still_answers() {
        let release = plan_with_risk(Risk::NeedsReview);
        let digest = PlanDigest::of(&release);
        let sod = SodAuthorship::standard().unwrap();
        assert!(!sod.is_author(&digest, "ann"), "nobody is an author yet");
        sod.record_author(&digest, "ann").unwrap();
        assert!(sod.is_author(&digest, "ann"));
        assert!(!sod.is_author(&digest, "bob"));
        let other = plan_with_risk(Risk::Destructive);
        assert!(!sod.is_author(&PlanDigest::of(&other), "ann"));
    }

    #[test]
    fn authorship_is_read_from_the_persisted_plan_record_across_a_restart() {
        let release = plan_with_risk(Risk::NeedsReview);
        let digest = PlanDigest::of(&release);
        let store = Arc::new(InMemoryApprovalStore::new());
        let service = ApprovalService::new(
            store.clone(),
            spot_migration_approval::MemoryAuditSink::new(),
            ApprovalPolicy::default(),
        )
        .unwrap();
        service.register_plan(&digest, "ann").unwrap();
        drop(service);

        // A fresh adapter and a fresh service over the same store: the author is still known.
        let sod = SodAuthorship::standard()
            .unwrap()
            .over(store.clone() as Arc<dyn ApprovalStore>);
        assert!(sod.is_author(&digest, "ann"));
        assert!(!sod.is_author(&digest, "bob"));
        let other = plan_with_risk(Risk::Destructive);
        assert!(!sod.is_author(&PlanDigest::of(&other), "ann"));

        let service = ApprovalService::new(
            store,
            spot_migration_approval::MemoryAuditSink::new(),
            ApprovalPolicy::default(),
        )
        .unwrap();
        let checks = Checks {
            authorship: &sod,
            signature: None,
        };
        let refused = service
            .approve(&release, &act("ann", None, T0), &checks)
            .unwrap_err();
        assert_eq!(refused.code(), codes::NOT_PERMITTED);
        assert!(service.store().all().unwrap().is_empty());
        service
            .approve(&release, &act("bob", None, T0), &checks)
            .unwrap();
    }

    #[test]
    #[allow(deprecated)]
    fn the_deprecated_record_author_registers_through_the_store() {
        let release = plan_with_risk(Risk::NeedsReview);
        let digest = PlanDigest::of(&release);
        let store = Arc::new(InMemoryApprovalStore::new());
        let sod = SodAuthorship::standard()
            .unwrap()
            .over(store.clone() as Arc<dyn ApprovalStore>);
        sod.record_author(&digest, "ann").unwrap();
        assert_eq!(store.author_of(&digest).unwrap().as_deref(), Some("ann"));
        assert_eq!(
            sod.record_author(&digest, "bob").unwrap_err().code(),
            codes::AUTHOR_CONFLICT
        );
    }

    #[test]
    fn a_store_that_fails_makes_the_person_the_author() {
        struct Down;
        impl ApprovalStore for Down {
            fn append(&self, _a: Approval) -> Result<(), spot_migration_approval::ApprovalError> {
                Ok(())
            }
            fn for_digest(
                &self,
                _d: &PlanDigest,
            ) -> Result<Vec<Approval>, spot_migration_approval::ApprovalError> {
                Ok(Vec::new())
            }
            fn all(&self) -> Result<Vec<Approval>, spot_migration_approval::ApprovalError> {
                Ok(Vec::new())
            }
            fn author_of(
                &self,
                _d: &PlanDigest,
            ) -> Result<Option<String>, spot_migration_approval::ApprovalError> {
                Err(spot_migration_approval::ApprovalError::Backend {
                    message: "down".to_owned(),
                })
            }
        }
        let release = plan_with_risk(Risk::NeedsReview);
        let sod = SodAuthorship::standard()
            .unwrap()
            .over(Arc::new(Down) as Arc<dyn ApprovalStore>);
        assert!(sod.is_author(&PlanDigest::of(&release), "anyone"));
    }
}

#[cfg(feature = "signed-command")]
mod signed {
    use super::*;
    use serde_json::json;
    use spot_migration_approval::adapters::{
        APPROVE_OP, ApproverKey, SignedCommandHook, approval_params, encode_signature,
    };
    use spot_signed_command::{Command, Signer, SoftwareSigner, VerifyPolicy, sign};

    const AUDIENCE: &str = "site-1";

    /// The one key each person holds.
    fn signer_for(who: &str) -> SoftwareSigner {
        let seed = match who {
            "ann" => 9,
            "bob" => 10,
            _ => 11,
        };
        SoftwareSigner::from_seed(&[seed; 32]).unwrap()
    }

    fn keys() -> Vec<ApproverKey> {
        ["ann", "bob"]
            .iter()
            .map(|who| ApproverKey::bound(signer_for(who).public_key_bytes(), *who))
            .collect()
    }

    fn hook() -> SignedCommandHook {
        SignedCommandHook::new(&keys(), VerifyPolicy::default(), AUDIENCE).unwrap()
    }

    /// A signature by `key` over `op` with `params`, valid at T0.
    fn signature(key: &SoftwareSigner, op: &str, params: serde_json::Value) -> String {
        let now = T0 / SECOND;
        let command = Command::new(op, params, AUDIENCE, key.key_id(), now - 10, now + 300);
        encode_signature(&sign(&command, key).unwrap()).unwrap()
    }

    fn good(release: &ReleasePlan, who: &str) -> String {
        signature(
            &signer_for(who),
            APPROVE_OP,
            approval_params(&PlanDigest::of(release), who),
        )
    }

    fn destructive_policy() -> ApprovalPolicy {
        let mut policy = ApprovalPolicy::default();
        policy.destructive.require_hardware_signature = true;
        policy
    }

    #[test]
    fn a_destructive_plan_needs_a_signature_that_verifies_and_binds_the_digest() {
        let release = plan_with_risk(Risk::Destructive);
        let hook = hook();
        let checks = Checks {
            authorship: &spot_migration_approval::NoAuthors,
            signature: Some(&hook),
        };
        let policy = destructive_policy();
        let go =
            |approvals: &[Approval]| authorise_apply(&release, approvals, &policy, T0, &checks);

        let ann = approval(&release, "ann", Some(good(&release, "ann")));
        let bob = approval(&release, "bob", Some(good(&release, "bob")));
        assert!(go(&[ann.clone(), bob.clone()]).is_ok());

        // No signature.
        let unsigned = approval(&release, "bob", None);
        assert_eq!(
            go(&[ann.clone(), unsigned]),
            Err(Refusal::SignatureRequired {
                approver: "bob".to_owned()
            })
        );

        // A valid signature, but over another plan's digest.
        let other = plan_with_risk(Risk::NeedsReview);
        let wrong_digest = approval(&release, "bob", Some(good(&other, "bob")));
        assert!(matches!(
            go(&[ann.clone(), wrong_digest]),
            Err(Refusal::SignatureRequired { .. })
        ));

        // Ann's signature presented as Bob's.
        let borrowed = approval(&release, "bob", Some(good(&release, "ann")));
        assert!(matches!(
            go(&[ann.clone(), borrowed]),
            Err(Refusal::SignatureRequired { .. })
        ));

        // A signature over some other command (right key, right params, wrong operation).
        let wrong_op = signature(
            &signer_for("bob"),
            "other_operation",
            approval_params(&PlanDigest::of(&release), "bob"),
        );
        assert!(matches!(
            go(&[ann.clone(), approval(&release, "bob", Some(wrong_op))]),
            Err(Refusal::SignatureRequired { .. })
        ));

        // Extra params smuggled in.
        let extra = signature(
            &signer_for("bob"),
            APPROVE_OP,
            json!({"approver": "bob", "plan_digest": PlanDigest::of(&release).as_hex(), "x": 1}),
        );
        assert!(matches!(
            go(&[ann.clone(), approval(&release, "bob", Some(extra))]),
            Err(Refusal::SignatureRequired { .. })
        ));

        // A key nobody trusts.
        let stranger = SoftwareSigner::from_seed(&[3; 32]).unwrap();
        let untrusted = signature(
            &stranger,
            APPROVE_OP,
            approval_params(&PlanDigest::of(&release), "bob"),
        );
        assert!(matches!(
            go(&[ann.clone(), approval(&release, "bob", Some(untrusted))]),
            Err(Refusal::SignatureRequired { .. })
        ));

        // Garbage and truncation never verify.
        for junk in ["", "zz", "00", &good(&release, "bob")[..40]] {
            assert!(
                go(&[
                    ann.clone(),
                    approval(&release, "bob", Some(junk.to_owned()))
                ])
                .is_err(),
                "junk {junk:?} verified"
            );
        }

        // A tampered signature byte.
        let mut tampered = good(&release, "bob").into_bytes();
        let last = tampered.len() - 1;
        tampered[last] = if tampered[last] == b'0' { b'1' } else { b'0' };
        let tampered = String::from_utf8(tampered).unwrap();
        assert!(go(&[ann, approval(&release, "bob", Some(tampered))]).is_err());
    }

    #[test]
    fn an_approval_outside_the_command_window_or_for_another_site_is_refused() {
        let release = plan_with_risk(Risk::Destructive);
        let hook = hook();
        let digest = PlanDigest::of(&release);
        let mut late = approval(&release, "bob", Some(good(&release, "bob")));
        assert!(hook.verify(&digest, &late));
        late.at = T0 + HOUR;
        assert!(!hook.verify(&digest, &late), "the command had expired");

        let other_site =
            SignedCommandHook::new(&keys(), VerifyPolicy::default(), "site-2").unwrap();
        assert!(!other_site.verify(
            &digest,
            &approval(&release, "bob", Some(good(&release, "bob")))
        ));
    }

    #[test]
    fn a_revoked_key_is_refused() {
        let release = plan_with_risk(Risk::Destructive);
        let mut hook = hook();
        let digest = PlanDigest::of(&release);
        let signed = approval(&release, "bob", Some(good(&release, "bob")));
        assert!(hook.verify(&digest, &signed));
        hook.revoke(&signer_for("bob").public_key_bytes(), T0 / SECOND - 100);
        assert!(!hook.verify(&digest, &signed));
    }

    #[test]
    fn a_key_is_bound_to_one_approver() {
        let release = plan_with_risk(Risk::Destructive);
        let hook = hook();
        let digest = PlanDigest::of(&release);

        // The valid binding.
        assert!(hook.verify(
            &digest,
            &approval(&release, "bob", Some(good(&release, "bob")))
        ));

        // Bob's key signs an approval that names Ann, params and all: valid for the digest, but
        // the key is bound to Bob, so Ann's approval is refused.
        let for_ann_by_bob = signature(
            &signer_for("bob"),
            APPROVE_OP,
            approval_params(&digest, "ann"),
        );
        assert!(!hook.verify(&digest, &approval(&release, "ann", Some(for_ann_by_bob))));

        // The same signature presented as Bob's approval: the params name Ann, so refused too.
        let same = signature(
            &signer_for("bob"),
            APPROVE_OP,
            approval_params(&digest, "ann"),
        );
        assert!(!hook.verify(&digest, &approval(&release, "bob", Some(same))));

        // Through the policy: Ann's key cannot make Bob's approval count.
        let policy = destructive_policy();
        let checks = Checks {
            authorship: &spot_migration_approval::NoAuthors,
            signature: Some(&hook),
        };
        let ann = approval(&release, "ann", Some(good(&release, "ann")));
        let forged = approval(
            &release,
            "bob",
            Some(signature(
                &signer_for("ann"),
                APPROVE_OP,
                approval_params(&digest, "bob"),
            )),
        );
        assert!(matches!(
            authorise_apply(&release, &[ann, forged], &policy, T0, &checks),
            Err(Refusal::SignatureRequired { .. })
        ));
    }

    #[test]
    fn a_key_without_a_subject_is_not_trusted() {
        let key = signer_for("ann").public_key_bytes();
        for keys in [
            vec![ApproverKey::unbound(key)],
            vec![ApproverKey::bound(key, ""), ApproverKey::bound(key, "ann")],
            vec![
                ApproverKey::bound(signer_for("bob").public_key_bytes(), "bob"),
                ApproverKey::unbound(key),
            ],
        ] {
            let error = SignedCommandHook::new(&keys, VerifyPolicy::default(), AUDIENCE)
                .err()
                .unwrap();
            assert_eq!(error.code(), codes::KEY_NOT_BOUND);
        }
    }

    #[test]
    fn the_same_signed_approval_can_be_evaluated_repeatedly() {
        // status, then authorise: re-checking must not be taken for a replay.
        let release = plan_with_risk(Risk::Destructive);
        let hook = hook();
        let digest = PlanDigest::of(&release);
        let signed = approval(&release, "bob", Some(good(&release, "bob")));
        assert!(hook.verify(&digest, &signed));
        assert!(hook.verify(&digest, &signed));
    }
}

#[cfg(feature = "settings")]
mod settings {
    use super::*;
    use std::sync::Arc;

    use spot_migration_approval::SETTING_KEY;
    use spot_migration_approval::adapters::{PolicySource, SettingsPolicySource, strictest_policy};
    use spot_settings::{MemoryStore, ParamValue, ScopeRef, StoredValue, ValueStore};

    fn source() -> (Arc<MemoryStore>, SettingsPolicySource) {
        let store = Arc::new(MemoryStore::new());
        let source = SettingsPolicySource::new(store.clone(), ScopeRef::Fleet);
        (store, source)
    }

    #[test]
    fn the_policy_round_trips_through_settings() {
        let (store, source) = source();
        // Nothing stored is the strictest policy, never the loose default.
        assert_eq!(source.load(), (strictest_policy(), PolicySource::Unset));
        assert_ne!(source.policy(), ApprovalPolicy::default());
        let mut policy = ApprovalPolicy::default();
        policy.needs_review.approvers = 3;
        policy.expiry_micros = 5 * HOUR;
        source.store(&policy).unwrap();
        assert_eq!(source.load(), (policy.clone(), PolicySource::Stored));
        assert!(store.get(&ScopeRef::Fleet, SETTING_KEY).is_some());
    }

    #[test]
    fn an_explicit_loose_policy_still_loads_loose() {
        let (_, source) = source();
        let loose = ApprovalPolicy::default();
        assert!(loose.additive.auto_approve);
        source.store(&loose).unwrap();
        let (policy, origin) = source.load();
        assert_eq!(policy, loose);
        assert_eq!(origin, PolicySource::Stored);
        assert!(policy.additive.auto_approve);
    }

    #[test]
    fn an_absent_unreadable_and_invalid_policy_each_give_the_strictest() {
        let strictest = strictest_policy();
        let (store, source) = source();
        // Absent.
        assert_eq!(source.policy(), strictest);
        // Unreadable (a secret, not text).
        store.put(
            &ScopeRef::Fleet,
            SETTING_KEY,
            StoredValue::Secret(spot_settings::SecretValue::new("x")),
        );
        assert_eq!(source.policy(), strictest);
        // Invalid (does not parse).
        store.put(
            &ScopeRef::Fleet,
            SETTING_KEY,
            StoredValue::Value(ParamValue::Text("{".to_owned())),
        );
        assert_eq!(source.policy(), strictest);
        // Invalid (parses, fails validation: one destructive approver).
        let mut weak = ApprovalPolicy::default();
        weak.destructive.approvers = 1;
        store.put(
            &ScopeRef::Fleet,
            SETTING_KEY,
            StoredValue::Value(ParamValue::Text(weak.to_setting())),
        );
        assert_eq!(source.policy(), strictest);
        assert_eq!(strictest, ApprovalPolicy::strictest());
    }

    #[test]
    fn an_invalid_policy_is_not_stored() {
        let (store, source) = source();
        let mut weak = ApprovalPolicy::default();
        weak.destructive.approvers = 1;
        assert_eq!(source.store(&weak).unwrap_err().code(), codes::BAD_POLICY);
        assert!(store.get(&ScopeRef::Fleet, SETTING_KEY).is_none());
    }

    #[test]
    fn an_unreadable_or_invalid_value_falls_back_to_the_strictest_policy() {
        let strictest = strictest_policy();
        strictest.validate().unwrap();
        // The strictest is at least as strict as the default on every axis.
        let default = ApprovalPolicy::default();
        assert!(!strictest.additive.auto_approve);
        assert!(strictest.needs_review.approvers >= default.needs_review.approvers);
        assert!(strictest.destructive.approvers >= default.destructive.approvers);
        assert!(strictest.destructive.require_hardware_signature);
        assert!(strictest.expiry_micros <= default.expiry_micros);
        assert!(strictest.forbid_author_approval);

        let (store, source) = source();
        let mut weak = ApprovalPolicy::default();
        weak.destructive.approvers = 1; // loosest-looking and invalid
        let bad_values = [
            StoredValue::Value(ParamValue::Text(weak.to_setting())),
            StoredValue::Value(ParamValue::Text("not json".to_owned())),
            StoredValue::Value(ParamValue::Text(String::new())),
            StoredValue::Value(ParamValue::Bool(true)),
            StoredValue::Value(ParamValue::Int(0)),
            StoredValue::Secret(spot_settings::SecretValue::new("x")),
        ];
        for value in bad_values {
            store.put(&ScopeRef::Fleet, SETTING_KEY, value);
            let (policy, origin) = source.load();
            assert_eq!(policy, strictest);
            assert!(matches!(origin, PolicySource::Strictest { .. }));
        }
        // A good value restores normal reading.
        source.store(&default).unwrap();
        assert_eq!(source.load(), (default, PolicySource::Stored));
    }
}

#[cfg(feature = "audit-chain")]
mod chain {
    use super::*;
    use std::future::Future;
    use std::sync::Arc;

    use spot_audit_chain::{
        AuditChain, AuditRecord, CheckpointPolicy, InMemoryStore, KeyRing, LocalSigner, Verifier,
    };
    use spot_migration_approval::adapters::ChainAuditSink;
    use spot_migration_approval::cli::{self, Command};

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        loop {
            if let std::task::Poll::Ready(value) = future.as_mut().poll(&mut cx) {
                return value;
            }
        }
    }

    fn new_chain() -> (Arc<AuditChain<InMemoryStore>>, KeyRing) {
        let signer = LocalSigner::from_seed([7; 32]);
        let mut keys = KeyRing::new();
        keys.add(&signer.public_key()).unwrap();
        let chain = Arc::new(AuditChain::new(
            InMemoryStore::new(),
            "site-1",
            Arc::new(signer),
            CheckpointPolicy { every: 2 },
        ));
        (chain, keys)
    }

    #[test]
    fn every_approve_reject_and_status_leaves_exactly_one_verifiable_record() {
        let release = plan_with_risk(Risk::NeedsReview);
        let (chain, keys) = new_chain();
        let service = ApprovalService::new(
            InMemoryApprovalStore::new(),
            ChainAuditSink::new(chain.clone()),
            ApprovalPolicy::default(),
        )
        .unwrap();
        service
            .register_plan(&PlanDigest::of(&release), "zed")
            .unwrap();
        let checks = Checks::none();
        let count = || block_on(chain.read_range(1, u64::MAX)).unwrap().len();
        let run = |args: &[&str], who: &str, now: i64| {
            let output = cli::run(
                &service,
                &release,
                &Command::parse(args).unwrap(),
                &Identity::new(who, &["release_manager"]),
                now,
                &checks,
            );
            assert!(output.ok, "{:?}", output.data);
        };

        assert_eq!(count(), 0);
        run(&["status"], "ann", T0);
        assert_eq!(count(), 1, "status");
        run(&["approve", "--role", "release_manager"], "ann", T0 + 1);
        assert_eq!(count(), 2, "approve");
        run(&["reject", "--role", "release_manager"], "bob", T0 + 2);
        assert_eq!(count(), 3, "reject");
        run(&["plan", "show"], "ann", T0 + 3);
        assert_eq!(count(), 4, "plan show");

        let entries = block_on(chain.read_range(1, u64::MAX)).unwrap();
        let kinds: Vec<&str> = entries.iter().map(|e| e.record.kind.as_str()).collect();
        assert_eq!(
            kinds,
            [
                "migration.status_viewed",
                "migration.approved",
                "migration.rejected",
                "migration.plan_shown"
            ]
        );
        let report =
            block_on(Verifier::new(&keys).verify_range(chain.store(), "site-1", 1, u64::MAX))
                .unwrap();
        assert!(report.is_ok(), "the chain must verify: {report:?}");
    }

    #[test]
    fn a_chain_that_refuses_the_record_blocks_the_approval() {
        // A payload over the chain's size limit is refused by the chain itself.
        let (chain, _keys) = new_chain();
        let sink = ChainAuditSink::new(chain.clone());
        let oversize = AuditRecord {
            kind: "migration.approved".to_owned(),
            actor: "user:ann".to_owned(),
            on_behalf_of: None,
            subject: None,
            at: T0,
            payload: std::iter::once((
                "k".to_owned(),
                spot_audit_chain::PayloadValue::Text("x".repeat(20_000)),
            ))
            .collect(),
            masked: true,
        };
        let error = sink.record(oversize).unwrap_err();
        assert_eq!(error.code(), codes::AUDIT_UNAVAILABLE);
        assert!(block_on(chain.head()).unwrap().is_none());
    }
}

mod failing_sink {
    use super::*;

    struct Down;
    impl AuditSink for Down {
        fn record(
            &self,
            _record: spot_audit_chain::AuditRecord,
        ) -> Result<(), spot_migration_approval::ApprovalError> {
            Err(spot_migration_approval::ApprovalError::Backend {
                message: "down".to_owned(),
            })
        }
    }

    #[test]
    fn an_approval_that_cannot_be_recorded_is_not_given() {
        let release = plan_with_risk(Risk::NeedsReview);
        let service = ApprovalService::new(
            InMemoryApprovalStore::new(),
            Down,
            ApprovalPolicy::default(),
        )
        .unwrap();
        service
            .register_plan(&PlanDigest::of(&release), "zed")
            .unwrap();
        let checks = Checks::none();
        for result in [
            service.approve(&release, &act("ann", None, T0), &checks),
            service.reject(&release, &act("ann", None, T0), &checks),
        ] {
            assert_eq!(result.unwrap_err().code(), codes::AUDIT_UNAVAILABLE);
        }
        assert!(
            service.store().all().unwrap().is_empty(),
            "nothing recorded"
        );
        assert_eq!(
            service
                .record_status_viewed(&release, "ann", T0)
                .unwrap_err()
                .code(),
            codes::AUDIT_UNAVAILABLE
        );
    }

    #[test]
    fn no_apply_ticket_is_issued_when_the_audit_is_down() {
        // Approve with a working sink, then lose the audit: authorise must not issue a ticket.
        let release = plan_with_risk(Risk::NeedsReview);
        let store = InMemoryApprovalStore::new();
        let ok = ApprovalService::new(
            store,
            spot_migration_approval::MemoryAuditSink::new(),
            ApprovalPolicy::default(),
        )
        .unwrap();
        ok.register_plan(&PlanDigest::of(&release), "zed").unwrap();
        ok.approve(&release, &act("ann", None, T0), &Checks::none())
            .unwrap();
        let approvals = ok.store().all().unwrap();
        let down = ApprovalService::new(
            InMemoryApprovalStore::new(),
            Down,
            ApprovalPolicy::default(),
        )
        .unwrap();
        for a in approvals {
            down.store().append(a).unwrap();
        }
        let result = down.authorise_apply(&release, "ann", T0 + 1, &Checks::none());
        assert!(matches!(
            result,
            Err(spot_migration_approval::AuthoriseError::Approval(
                spot_migration_approval::ApprovalError::AuditUnavailable { .. }
            ))
        ));
    }
}
