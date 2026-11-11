//! Effect declarations in the registry (WP-EXT-11.T2; ADR-080 6.3, 6.4).
#![allow(clippy::unwrap_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{codes, names, ov, sn};
use proptest::prelude::*;
use spot_app_api::prelude::{OffsetDateTime, Version};
use spot_app_api::{
    ActionRef, AppResult, BoxFuture, Controller, EffectClassName, EffectDecl, EffectKind,
    EffectReceipt, Hex32, HookContext, HookPoint, OpaqueRef, Operation, OperationReceipt,
    OverrideKind, Phase, ReceiptState, Reversal, StepDecl, StepEffects, StepName, StoreName,
    Target,
};
use spot_seams::{Registry, RegistryBuilder, UndeclaredReason, check_receipt};

struct Ctrl {
    doctype: String,
    steps: Vec<StepDecl>,
    effects: Vec<StepEffects>,
}

impl Controller for Ctrl {
    fn doctype(&self) -> &str {
        &self.doctype
    }
    fn steps(&self) -> Vec<StepDecl> {
        self.steps.clone()
    }
    fn step_effects(&self) -> Vec<StepEffects> {
        self.effects.clone()
    }
    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        _ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

fn at(phase: Phase, op: Operation) -> HookPoint {
    HookPoint::of(phase, Target::Op(op))
}

fn step(name: &str, point: HookPoint, priority: i32) -> StepDecl {
    StepDecl {
        name: sn(name),
        point,
        priority,
        budget: Duration::from_millis(100),
    }
}

fn class(s: &str) -> EffectClassName {
    EffectClassName::new(s).unwrap()
}

fn store(s: &str) -> StoreName {
    StoreName::new(s).unwrap()
}

/// An internal effect writing `stores`, reversible by `by`.
fn internal(class_name: &str, stores: &[&str], by: Operation) -> EffectDecl {
    EffectDecl::new(
        EffectKind::Internal,
        class(class_name),
        stores.iter().map(|s| store(s)).collect(),
        Reversal::ReversibleBy(by),
    )
}

fn mail(reversal: Reversal) -> EffectDecl {
    EffectDecl::new(
        EffectKind::External,
        class("mail"),
        vec![store("outbound_mail")],
        reversal,
    )
}

fn fx(step_name: &str, effects: Vec<EffectDecl>) -> StepEffects {
    StepEffects::new(sn(step_name), effects)
}

fn ctrl(doctype: &str, steps: Vec<StepDecl>, effects: Vec<StepEffects>) -> Arc<dyn Controller> {
    Arc::new(Ctrl {
        doctype: doctype.to_owned(),
        steps,
        effects,
    })
}

const INVOICE: &str = "sales_invoice";

/// The forward step: writes `ledger_entry` at `after_post`, reversible by cancel.
fn forward() -> (StepDecl, StepEffects) {
    (
        step(
            "ledger:write_entries",
            at(Phase::After, Operation::Post),
            10,
        ),
        fx(
            "ledger:write_entries",
            vec![internal(
                "balance_movement",
                &["ledger_entry"],
                Operation::Cancel,
            )],
        ),
    )
}

/// A reversing step at `after_cancel` writing `ledger_entry`.
fn reverse(name: &str, app_stores: &[&str]) -> (StepDecl, StepEffects) {
    (
        step(name, at(Phase::After, Operation::Cancel), 10),
        fx(
            name,
            vec![internal("balance_movement", app_stores, Operation::Post)],
        ),
    )
}

fn paired() -> RegistryBuilder {
    let (f, fe) = forward();
    let (r, re) = reverse("ledger:reverse_entries", &["ledger_entry"]);
    let mut b = RegistryBuilder::new();
    b.add_controller("ledger", ctrl(INVOICE, vec![f, r], vec![fe, re]));
    b
}

fn step_names(r: &Registry, op: Operation) -> Vec<String> {
    r.effects(INVOICE, op)
        .iter()
        .map(|e| e.step.to_string())
        .collect()
}

#[test]
fn a_paired_post_and_cancel_build_and_report_their_effects() {
    let r = paired().build().unwrap();
    let post = r.effects(INVOICE, Operation::Post);
    assert_eq!(post.len(), 1);
    assert_eq!(post[0].step, sn("ledger:write_entries"));
    assert_eq!(post[0].app, "ledger");
    assert_eq!(post[0].point.name(), "after_post");
    assert_eq!(post[0].decl.kind, EffectKind::Internal);
    assert_eq!(
        step_names(&r, Operation::Cancel),
        ["ledger:reverse_entries"]
    );
    assert!(r.effects(INVOICE, Operation::Create).is_empty());
    assert!(r.effects("unknown", Operation::Post).is_empty());
}

#[test]
fn a_forward_effect_without_a_reversing_step_is_refused() {
    let (f, fe) = forward();
    let mut b = RegistryBuilder::new();
    b.add_controller("ledger", ctrl(INVOICE, vec![f], vec![fe]));
    assert_eq!(codes(b.build()), ["effects.no_paired_reversal"]);
}

#[test]
fn a_reversing_step_for_another_store_does_not_pair() {
    let (f, fe) = forward();
    let (r, re) = reverse("ledger:reverse_other", &["stock_entry"]);
    let mut b = RegistryBuilder::new();
    b.add_controller("ledger", ctrl(INVOICE, vec![f, r], vec![fe, re]));
    assert_eq!(codes(b.build()), ["effects.no_paired_reversal"]);
}

#[test]
fn every_store_of_a_forward_effect_needs_a_reversal() {
    let f = step("ledger:write", at(Phase::After, Operation::Post), 10);
    let fe = fx(
        "ledger:write",
        vec![internal(
            "balance_movement",
            &["ledger_entry", "stock_entry"],
            Operation::Cancel,
        )],
    );
    let (r, re) = reverse("ledger:reverse_one", &["ledger_entry"]);
    let mut b = RegistryBuilder::new();
    b.add_controller("ledger", ctrl(INVOICE, vec![f, r], vec![fe, re]));
    let err = b.build().err().unwrap();
    assert_eq!(err.len(), 1);
    assert_eq!(err[0].code, "effects.no_paired_reversal");
    assert!(err[0].message.contains("stock_entry"));
    assert!(!err[0].message.contains("`ledger_entry`, "));
}

#[test]
fn pairing_is_per_doctype() {
    let (f, fe) = forward();
    let (r, re) = reverse("other:reverse", &["ledger_entry"]);
    let mut b = RegistryBuilder::new();
    b.add_controller("ledger", ctrl(INVOICE, vec![f], vec![fe]));
    b.add_controller("other", ctrl("purchase_invoice", vec![r], vec![re]));
    assert_eq!(codes(b.build()), ["effects.no_paired_reversal"]);
}

#[test]
fn disabling_the_reversing_step_is_override_drops_reversal() {
    let mut b = paired();
    b.add_override(
        "erp",
        ov("ledger:reverse_entries", OverrideKind::Disable, None),
    );
    let err = b.build().err().unwrap();
    assert_eq!(err.len(), 1);
    assert_eq!(err[0].code, "effects.override_drops_reversal");
    assert_eq!(err[0].apps, ["erp", "ledger"]);
}

#[test]
fn replacing_the_reversing_step_with_one_that_declares_nothing_drops_the_reversal() {
    let mut b = paired();
    b.add_controller(
        "ext",
        ctrl(
            INVOICE,
            vec![step("ext:quiet", at(Phase::After, Operation::Cancel), 5)],
            vec![],
        ),
    );
    b.add_override(
        "ext",
        ov(
            "ledger:reverse_entries",
            OverrideKind::Replace,
            Some("ext:quiet"),
        ),
    );
    assert_eq!(codes(b.build()), ["effects.override_drops_reversal"]);
}

#[test]
fn replacing_the_reversing_step_with_one_that_declares_the_reversal_keeps_it() {
    let mut b = paired();
    let st = step("ext:better_reverse", at(Phase::After, Operation::Cancel), 5);
    b.add_controller(
        "ext",
        ctrl(
            INVOICE,
            vec![st],
            vec![fx(
                "ext:better_reverse",
                vec![internal(
                    "balance_movement",
                    &["ledger_entry"],
                    Operation::Post,
                )],
            )],
        ),
    );
    b.add_override(
        "ext",
        ov(
            "ledger:reverse_entries",
            OverrideKind::Replace,
            Some("ext:better_reverse"),
        ),
    );
    let r = b.build().unwrap();
    // The replacement's declaration, not the replaced step's.
    assert_eq!(step_names(&r, Operation::Cancel), ["ext:better_reverse"]);
    let rows = r.effect_changing_overrides();
    assert!(
        rows.is_empty(),
        "same declaration, nothing changed: {rows:?}"
    );
}

#[test]
fn replacing_the_forward_step_with_an_unpaired_one_is_no_paired_reversal() {
    let mut b = paired();
    b.add_controller(
        "ext",
        ctrl(
            INVOICE,
            vec![step(
                "ext:other_write",
                at(Phase::After, Operation::Post),
                5,
            )],
            vec![fx(
                "ext:other_write",
                vec![internal(
                    "balance_movement",
                    &["bonus_entry"],
                    Operation::Cancel,
                )],
            )],
        ),
    );
    b.add_override(
        "ext",
        ov(
            "ledger:write_entries",
            OverrideKind::Replace,
            Some("ext:other_write"),
        ),
    );
    assert_eq!(codes(b.build()), ["effects.no_paired_reversal"]);
}

#[test]
fn disabling_the_forward_step_with_the_reversal_left_is_fine() {
    let mut b = paired();
    b.add_override(
        "erp",
        ov("ledger:write_entries", OverrideKind::Disable, None),
    );
    let r = b.build().unwrap();
    assert!(r.effects(INVOICE, Operation::Post).is_empty());
    assert_eq!(
        step_names(&r, Operation::Cancel),
        ["ledger:reverse_entries"]
    );
}

#[test]
fn a_wrap_contributes_the_wrapper_then_the_wrapped_effects() {
    let mut b = paired();
    b.add_controller(
        "ext",
        ctrl(
            INVOICE,
            vec![step("ext:guard", at(Phase::After, Operation::Post), 5)],
            vec![fx(
                "ext:guard",
                vec![internal("audit_note", &["audit_log"], Operation::Cancel)],
            )],
        ),
    );
    // The audit store needs its own reversal.
    let (r, re) = reverse("audit_rev:reverse_audit", &["audit_log"]);
    b.add_controller("audit_rev", ctrl(INVOICE, vec![r], vec![re]));
    b.add_override(
        "ext",
        ov(
            "ledger:write_entries",
            OverrideKind::Wrap,
            Some("ext:guard"),
        ),
    );
    let reg = b.build().unwrap();
    let post = reg.effects(INVOICE, Operation::Post);
    let order: Vec<String> = post.iter().map(|e| e.step.to_string()).collect();
    assert_eq!(order, ["ext:guard", "ledger:write_entries"]);
    let plan = reg.plan(INVOICE, &at(Phase::After, Operation::Post));
    assert_eq!(names(plan), ["ext:guard"]);
    assert_eq!(plan[0].effects.len(), 1);
    assert_eq!(plan[0].wrapped.as_ref().unwrap().effects.len(), 1);
    let rows = reg.effect_changing_overrides();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].removed.is_empty());
    assert_eq!(rows[0].added.len(), 1);
    assert_eq!(rows[0].target_step, "ledger:write_entries");
    assert_eq!(rows[0].hook_point, "after_post");
}

#[test]
fn disabling_an_external_step_drops_its_effects_and_is_marked() {
    let notify = step("mail:notify", at(Phase::Committed, Operation::Post), 10);
    let mut b = paired();
    b.add_controller(
        "mail",
        ctrl(
            INVOICE,
            vec![notify],
            vec![fx(
                "mail:notify",
                vec![mail(Reversal::CompensableBy(
                    ActionRef::new("mail:send_correction").unwrap(),
                ))],
            )],
        ),
    );
    let with = b.build().unwrap();
    assert_eq!(
        step_names(&with, Operation::Post),
        ["ledger:write_entries", "mail:notify"]
    );

    b.add_override("erp", ov("mail:notify", OverrideKind::Disable, None));
    let r = b.build().unwrap();
    assert_eq!(step_names(&r, Operation::Post), ["ledger:write_entries"]);
    let rows = r.effect_changing_overrides();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].app, "erp");
    assert_eq!(rows[0].removed.len(), 1);
    assert!(rows[0].added.is_empty());
    // The explanation names the effects of the disabled line, and says it does not run.
    let lines = r.explain(INVOICE, &at(Phase::Committed, Operation::Post));
    assert_eq!(lines.len(), 1);
    assert!(!lines[0].runs);
    assert_eq!(lines[0].effects.len(), 1);
}

#[test]
fn explain_lines_carry_the_effects_of_their_step() {
    let r = paired().build().unwrap();
    let lines = r.explain(INVOICE, &at(Phase::After, Operation::Post));
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].effects.len(), 1);
    assert_eq!(lines[0].effects[0].class, class("balance_movement"));
}

#[test]
fn a_controller_without_declarations_behaves_as_before() {
    let mut b = RegistryBuilder::new();
    b.add_controller(
        "core",
        common::ctrl(
            INVOICE,
            vec![
                common::decl("core:a", at(Phase::After, Operation::Post), 10),
                common::decl("core:b", at(Phase::After, Operation::Post), 20),
            ],
        ),
    );
    b.add_override("erp", ov("core:b", OverrideKind::Disable, None));
    let r = b.build().unwrap();
    assert_eq!(
        names(r.plan(INVOICE, &at(Phase::After, Operation::Post))),
        ["core:a"]
    );
    assert!(r.effects(INVOICE, Operation::Post).is_empty());
    assert!(r.effect_changing_overrides().is_empty());
    for line in r.explain(INVOICE, &at(Phase::After, Operation::Post)) {
        assert!(line.effects.is_empty());
    }
}

#[test]
fn an_internal_effect_that_is_not_reversible_is_refused() {
    let s = step("ledger:write", at(Phase::After, Operation::Post), 10);
    let bad = EffectDecl::new(
        EffectKind::Internal,
        class("balance_movement"),
        vec![store("ledger_entry")],
        Reversal::CompensableBy(ActionRef::new("ledger:fix").unwrap()),
    );
    let mut b = RegistryBuilder::new();
    b.add_controller(
        "ledger",
        ctrl(INVOICE, vec![s], vec![fx("ledger:write", vec![bad])]),
    );
    assert_eq!(codes(b.build()), ["effects.internal_not_reversible"]);
}

#[test]
fn declarations_the_contract_refuses_become_conflicts() {
    let build = |point: HookPoint, e: EffectDecl| {
        let mut b = RegistryBuilder::new();
        b.add_controller(
            "x",
            ctrl(
                INVOICE,
                vec![step("x:s", point, 1)],
                vec![fx("x:s", vec![e])],
            ),
        );
        codes(b.build())
    };
    // External effect before commit.
    assert_eq!(
        build(
            at(Phase::After, Operation::Update),
            mail(Reversal::Irreversible {
                reason: "sent".into()
            })
        ),
        ["effects.wrong_phase"]
    );
    // Irreversible without a reason.
    assert_eq!(
        build(
            at(Phase::Committed, Operation::Update),
            mail(Reversal::Irreversible { reason: " ".into() })
        ),
        ["effects.irreversible_reason"]
    );
    // Internal effect at post reversible by something other than cancel.
    assert_eq!(
        build(
            at(Phase::After, Operation::Post),
            internal("balance_movement", &["a"], Operation::Delete)
        ),
        ["effects.wrong_reversal"]
    );
    // Internal effect writing nothing.
    assert_eq!(
        build(
            at(Phase::After, Operation::Update),
            internal("balance_movement", &[], Operation::Delete)
        ),
        ["effects.internal_writes_nothing"]
    );
}

#[test]
fn effects_for_an_undeclared_or_repeated_step_are_conflicts() {
    let mut b = RegistryBuilder::new();
    b.add_controller(
        "x",
        ctrl(
            INVOICE,
            vec![step("x:s", at(Phase::Committed, Operation::Update), 1)],
            vec![fx("x:ghost", vec![]), fx("x:s", vec![]), fx("x:s", vec![])],
        ),
    );
    assert_eq!(
        codes(b.build()),
        ["effects.duplicate_step", "effects.unknown_step"]
    );
}

#[test]
fn an_effect_class_outside_the_known_set_is_refused() {
    let s = step("mail:notify", at(Phase::Committed, Operation::Post), 1);
    let build = |known: Option<&[&str]>| {
        let mut b = RegistryBuilder::new();
        b.add_controller(
            "mail",
            ctrl(
                INVOICE,
                vec![s.clone()],
                vec![fx(
                    "mail:notify",
                    vec![mail(Reversal::Irreversible {
                        reason: "sent".into(),
                    })],
                )],
            ),
        );
        if let Some(k) = known {
            let classes: Vec<EffectClassName> = k.iter().map(|c| class(c)).collect();
            b.known_effect_classes(&classes);
        }
        codes(b.build())
    };
    assert!(
        build(None).is_empty(),
        "without a known set any class passes"
    );
    assert!(build(Some(&["mail", "link_message"])).is_empty());
    assert_eq!(build(Some(&["link_message"])), ["effects.unknown_class"]);
}

fn receipt(op: Operation, doctype: &str, effects: Vec<EffectReceipt>) -> OperationReceipt {
    OperationReceipt {
        operation: op,
        doctype: doctype.to_owned(),
        name: "SINV-0001".to_owned(),
        version: Version(2),
        effects,
        committed_at: OffsetDateTime::UNIX_EPOCH,
    }
}

fn reported(step_name: &str, class_name: &str, kind: EffectKind) -> EffectReceipt {
    EffectReceipt {
        step: sn(step_name),
        class: class(class_name),
        kind,
        store_refs: vec![OpaqueRef::new("row-1").unwrap()],
        count: 1,
        digest: Hex32::new(&"cd".repeat(32)).unwrap(),
        state: ReceiptState::Applied,
    }
}

#[test]
fn a_receipt_is_checked_against_the_declarations() {
    let r = paired().build().unwrap();
    let ok = reported(
        "ledger:write_entries",
        "balance_movement",
        EffectKind::Internal,
    );
    assert!(
        check_receipt(
            &r,
            INVOICE,
            Operation::Post,
            &receipt(Operation::Post, INVOICE, vec![ok.clone()])
        )
        .is_empty()
    );
    assert!(
        check_receipt(
            &r,
            INVOICE,
            Operation::Post,
            &receipt(Operation::Post, INVOICE, vec![])
        )
        .is_empty()
    );

    let bad = vec![
        reported(
            "ledger:write_entries",
            "stock_movement",
            EffectKind::Internal,
        ),
        reported(
            "ledger:write_entries",
            "balance_movement",
            EffectKind::External,
        ),
        reported("ghost:step", "balance_movement", EffectKind::Internal),
        ok,
    ];
    let found = check_receipt(
        &r,
        INVOICE,
        Operation::Post,
        &receipt(Operation::Post, INVOICE, bad),
    );
    let reasons: Vec<UndeclaredReason> = found.iter().map(|u| u.reason).collect();
    assert_eq!(
        reasons,
        [
            UndeclaredReason::ClassNotDeclared,
            UndeclaredReason::KindMismatch,
            UndeclaredReason::NoDeclaration
        ]
    );
    assert_eq!(found[2].step, sn("ghost:step"));
}

#[test]
fn a_receipt_for_another_operation_or_doctype_covers_nothing() {
    let r = paired().build().unwrap();
    let e = reported(
        "ledger:write_entries",
        "balance_movement",
        EffectKind::Internal,
    );
    for rc in [
        receipt(Operation::Cancel, INVOICE, vec![e.clone()]),
        receipt(Operation::Post, "purchase_invoice", vec![e.clone()]),
    ] {
        let found = check_receipt(&r, INVOICE, Operation::Post, &rc);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].reason, UndeclaredReason::ReceiptMismatch);
    }
}

#[test]
fn an_override_replacing_a_step_makes_the_replaced_steps_reports_undeclared() {
    let mut b = RegistryBuilder::new();
    let notify = step("mail:notify", at(Phase::Committed, Operation::Post), 10);
    b.add_controller(
        "mail",
        ctrl(
            INVOICE,
            vec![notify],
            vec![fx(
                "mail:notify",
                vec![mail(Reversal::Irreversible {
                    reason: "sent".into(),
                })],
            )],
        ),
    );
    b.add_override("erp", ov("mail:notify", OverrideKind::Disable, None));
    let r = b.build().unwrap();
    let found = check_receipt(
        &r,
        INVOICE,
        Operation::Post,
        &receipt(
            Operation::Post,
            INVOICE,
            vec![reported("mail:notify", "mail", EffectKind::External)],
        ),
    );
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].reason, UndeclaredReason::NoDeclaration);
}

// Registration order never changes what is declared.
fn build_in_order(order: &[u8]) -> Result<Registry, Vec<spot_seams::Conflict>> {
    let mut b = RegistryBuilder::new();
    for k in order {
        match k {
            0 => {
                let (f, fe) = forward();
                b.add_controller("ledger", ctrl(INVOICE, vec![f], vec![fe]));
            }
            1 => {
                let (r, re) = reverse("rev:reverse_entries", &["ledger_entry"]);
                b.add_controller("rev", ctrl(INVOICE, vec![r], vec![re]));
            }
            2 => {
                b.add_controller(
                    "mail",
                    ctrl(
                        INVOICE,
                        vec![step(
                            "mail:notify",
                            at(Phase::Committed, Operation::Post),
                            10,
                        )],
                        vec![fx(
                            "mail:notify",
                            vec![mail(Reversal::Irreversible {
                                reason: "sent".into(),
                            })],
                        )],
                    ),
                );
            }
            3 => {
                b.add_controller(
                    "ext",
                    ctrl(
                        INVOICE,
                        vec![step("ext:guard", at(Phase::After, Operation::Post), 5)],
                        vec![fx(
                            "ext:guard",
                            vec![internal("audit_note", &["ledger_entry"], Operation::Cancel)],
                        )],
                    ),
                );
            }
            4 => {
                b.add_override(
                    "ext",
                    ov(
                        "ledger:write_entries",
                        OverrideKind::Wrap,
                        Some("ext:guard"),
                    ),
                );
            }
            _ => {
                b.add_override("erp", ov("mail:notify", OverrideKind::Disable, None));
            }
        }
    }
    b.build()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, rng_seed: proptest::test_runner::RngSeed::Fixed(0xEFF_0011), ..ProptestConfig::default() })]

    #[test]
    fn registration_order_never_changes_the_declared_effects(keys in prop::array::uniform6(any::<u8>())) {
        let mut idx: Vec<usize> = (0..6).collect();
        idx.sort_by_key(|&i| (keys[i], i));
        let shuffled: Vec<u8> = idx.into_iter().map(|i| i as u8).collect();
        let canonical = [0u8, 1, 2, 3, 4, 5];
        let a = build_in_order(&canonical).unwrap();
        let b = build_in_order(&shuffled).unwrap();
        for op in [Operation::Post, Operation::Cancel, Operation::Update] {
            prop_assert_eq!(a.effects(INVOICE, op), b.effects(INVOICE, op));
        }
        prop_assert_eq!(a.effect_changing_overrides(), b.effect_changing_overrides());
        let point = at(Phase::After, Operation::Post);
        prop_assert_eq!(a.explain(INVOICE, &point), b.explain(INVOICE, &point));
    }

    #[test]
    fn registration_order_never_changes_the_effect_conflicts(keys in prop::array::uniform3(any::<u8>())) {
        // Forward step, the disable of the reversal, and the reversal itself, in any order.
        let items = [0u8, 1, 6];
        let mut idx: Vec<usize> = (0..3).collect();
        idx.sort_by_key(|&i| (keys[i], i));
        let order: Vec<u8> = idx.into_iter().map(|i| items[i]).collect();
        let build = |order: &[u8]| {
            let mut b = RegistryBuilder::new();
            for k in order {
                match k {
                    0 => {
                        let (f, fe) = forward();
                        b.add_controller("ledger", ctrl(INVOICE, vec![f], vec![fe]));
                    }
                    1 => {
                        let (r, re) = reverse("rev:reverse_entries", &["ledger_entry"]);
                        b.add_controller("rev", ctrl(INVOICE, vec![r], vec![re]));
                    }
                    _ => {
                        b.add_override("erp", ov("rev:reverse_entries", OverrideKind::Disable, None));
                    }
                }
            }
            b.build().err().unwrap()
        };
        prop_assert_eq!(build(&[0, 1, 6]), build(&order));
    }
}

fn mail_ctrl() -> Arc<dyn Controller> {
    let s = step("mail:notify", at(Phase::Committed, Operation::Post), 1);
    ctrl(
        INVOICE,
        vec![s],
        vec![fx(
            "mail:notify",
            vec![mail(Reversal::Irreversible {
                reason: "sent".into(),
            })],
        )],
    )
}

#[test]
fn a_platform_registry_built_without_a_class_set_fails_closed() {
    let mut b = RegistryBuilder::for_platform();
    b.add_controller("mail", mail_ctrl());
    assert_eq!(codes(b.build()), ["effects.unknown_class"]);
    // Even with no steps at all: the host forgot the call.
    assert_eq!(
        codes(RegistryBuilder::for_platform().build()),
        ["effects.unknown_class"]
    );
}

#[test]
fn a_platform_registry_refuses_a_misspelt_effect_class() {
    let mut b = RegistryBuilder::for_platform();
    b.add_controller("mail", mail_ctrl());
    b.known_effect_classes(&[class("mial"), class("link_message")]);
    assert_eq!(codes(b.build()), ["effects.unknown_class"]);
}

#[test]
fn a_platform_registry_accepts_the_known_classes() {
    let mut b = RegistryBuilder::for_platform();
    b.add_controller("mail", mail_ctrl());
    b.known_effect_classes(&[class("mail"), class("link_message")]);
    assert!(codes(b.build()).is_empty());
}

#[test]
fn the_lenient_default_builder_still_accepts_any_class() {
    let mut b = RegistryBuilder::new();
    b.add_controller("mail", mail_ctrl());
    assert!(codes(b.build()).is_empty());
}
