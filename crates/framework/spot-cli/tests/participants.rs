//! The participants one site installs, in the order the engine requires, and the configurations
//! it refuses to start (WP-PRD-01.T11).
//!
//! No PostgreSQL is needed: the owners, the masker and the metadata source are the real ones, and
//! only the transaction a participant would write through is absent — which is why the installer
//! reports the names in order rather than leaving the order to be inferred from a fake write.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, RwLock};

use spot_activity::FieldChange;
use spot_app_api::prelude::OffsetDateTime;
use spot_app_api::prelude::Value;
use spot_controllers::ControllerRegistry;
use spot_core::{ManualClock, SiteId};
use spot_doc_audit::{AuditStep, FailingWriter, NoBeforeImages};
use spot_edge_sync::MetaSource;
use spot_field_guard::{AuditRecord, BoundedQueue, FieldAccessRecord, ProtectionSource};
use spot_meta::{
    BuildOptions, Definition, EffectiveMeta, FieldSpec, FieldType, MaskSpec, ProtectionGroupSpec,
    Registry,
};
use spot_partition::MemoryPartitionRegistry;
use spot_permissions::{Model, ModelTexts, PermissionEngine, Snapshot, load};
use spot_site_masker::{GuardDeps, SiteMasker};

use spot_cli::config;
use spot_cli::services::participants::{self, Audit, Deps};

const DOCTYPE: &str = "invoice";
const LATER: &str = "credit_note";

// --- fixtures ---------------------------------------------------------------------------

/// Generation one has `invoice` with a plain `memo`; generation two protects `memo`, adds a
/// second DocType and is the metadata the site serves after a swap.
fn definitions(second: bool) -> Vec<Definition> {
    let mut invoice = Definition::new(DOCTYPE);
    let mut memo = FieldSpec::new("memo", FieldType::Text);
    if second {
        memo.protection_group = Some("pci".to_owned());
        memo.masking = Some(MaskSpec::placeholder());
        invoice.protection_groups = vec![ProtectionGroupSpec::new("pci", "Payment cards")];
    }
    invoice.fields = vec![FieldSpec::new("number", FieldType::Text), memo];
    let mut out = vec![invoice];
    if second {
        let mut note = Definition::new(LATER);
        note.fields = vec![FieldSpec::new("body", FieldType::Text)];
        out.push(note);
    }
    out
}

fn meta(second: bool) -> Arc<EffectiveMeta> {
    Registry::with_options(definitions(second), BuildOptions::new())
        .unwrap_or_else(|e| panic!("fixture meta: {e:?}"))
        .effective()
}

fn model(meta: &EffectiveMeta) -> Model {
    let mut grants = String::new();
    for view in meta.doctypes() {
        let doctype = view.name.as_str();
        grants.push_str(&format!(
            "[[entries]]\nrole = \"Reader\"\ndoctype = \"{doctype}\"\nrights = [\"read\"]\n"
        ));
    }
    load(&ModelTexts {
        grants,
        field_grants: format!(
            "[[entries]]\nrole = \"Reader\"\ndoctype = \"{DOCTYPE}\"\ntarget = {{ group = \"pci\" }}\n\
             access = [\"read\"]\n"
        ),
        groups: "[[entries]]\nname = \"pci\"\n".to_owned(),
        shares: String::new(),
    })
    .unwrap_or_else(|e| panic!("fixture model: {:?}", e.as_slice()))
}

fn deps_for(meta: &EffectiveMeta) -> GuardDeps {
    let facts = spot_field_guard::MetaProtection::new(meta);
    let mut snap = Snapshot::build(meta, &model(meta)).unwrap_or_else(|e| panic!("{e:?}"));
    for view in meta.doctypes() {
        let doctype = view.name.as_str();
        if let Some(protection) = facts.protection(doctype) {
            snap = snap
                .with_field_facts(doctype, protection.fields.clone())
                .unwrap_or_else(|e| panic!("{e:?}"));
        }
    }
    GuardDeps {
        engine: Arc::new(PermissionEngine::new(Arc::new(snap))),
        trace: Arc::new(BoundedQueue::<FieldAccessRecord>::new(64)),
        audit: Arc::new(BoundedQueue::<AuditRecord>::new(64)),
        clock: Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH)),
    }
}

fn masker(meta: &Arc<EffectiveMeta>) -> Arc<dyn spot_doc_audit::FieldMasker> {
    Arc::new(SiteMasker::new(meta, deps_for(meta)))
}

/// A metadata source a test can swap under the participants, like `SiteLive` does for the site.
fn live_meta() -> (Arc<RwLock<Arc<EffectiveMeta>>>, Arc<dyn MetaSource>) {
    let handle = Arc::new(RwLock::new(meta(false)));
    let source: Arc<dyn MetaSource> = {
        let handle = handle.clone();
        Arc::new(move || Arc::clone(&handle.read().unwrap_or_else(|p| p.into_inner())))
    };
    (handle, source)
}

fn site() -> SiteId {
    SiteId::new("acme").unwrap()
}

fn partition(doctypes: &[&str], scope: &[(&str, &str, &str)]) -> config::Partition {
    config::Partition {
        node: "node-a".to_owned(),
        doctypes: doctypes.iter().map(|d| (*d).to_owned()).collect(),
        scope: scope
            .iter()
            .map(|(doctype, key, field)| config::ScopeField {
                doctype: (*doctype).to_owned(),
                key: (*key).to_owned(),
                field: (*field).to_owned(),
            })
            .collect(),
    }
}

fn site_config(partition: Option<config::Partition>) -> config::Site {
    let mut site = config::Site::named("acme", "acme", "und", vec!["platform".to_owned()]);
    site.partition = partition;
    site
}

fn deps(
    audit: Option<Audit>,
    partitions: Option<Arc<dyn spot_partition::PartitionRegistry>>,
) -> Deps {
    let (_handle, source) = live_meta();
    Deps {
        audit,
        controllers: Arc::new(ControllerRegistry::new()),
        partitions,
        meta: source,
    }
}

fn audit(second: bool) -> Audit {
    let meta = meta(second);
    Audit::new(&meta, masker(&meta))
}

// --- the order --------------------------------------------------------------------------

#[test]
fn a_partitioned_site_installs_audit_then_owner_then_edge() {
    let config = site_config(Some(partition(&[DOCTYPE], &[(DOCTYPE, "branch", "memo")])));
    let registry: Arc<dyn spot_partition::PartitionRegistry> =
        Arc::new(MemoryPartitionRegistry::new());
    let installed =
        participants::build(&site(), &config, &deps(Some(audit(false)), Some(registry)))
            .expect("a covered partition profile installs");
    assert_eq!(installed.names, vec!["audit", "owner", "edge"]);
    assert_eq!(installed.participants.len(), 3);
    assert_eq!(installed.names_in_order(), "audit, owner, edge");
}

#[test]
fn a_single_node_site_installs_audit_then_edge_and_says_partition_off() {
    let config = site_config(None);
    let installed = participants::build(&site(), &config, &deps(Some(audit(false)), None))
        .expect("a single-node site installs");
    assert_eq!(installed.names, vec!["audit", "edge"]);
    let ready = participants::ParticipantsReady::new(&config.site, None, &installed.names);
    assert!(
        ready.detail().contains("partition: off"),
        "{}",
        ready.detail()
    );
    assert!(
        ready.detail().contains("docaudit: on"),
        "{}",
        ready.detail()
    );
}

#[test]
fn without_an_audit_chain_the_owner_check_still_runs_before_edge() {
    let config = site_config(Some(partition(&[DOCTYPE], &[(DOCTYPE, "branch", "memo")])));
    let registry: Arc<dyn spot_partition::PartitionRegistry> =
        Arc::new(MemoryPartitionRegistry::new());
    let installed = participants::build(&site(), &config, &deps(None, Some(registry)))
        .expect("a covered partition profile installs without an audit participant");
    assert_eq!(installed.names, vec!["owner", "edge"]);
    let ready = participants::ParticipantsReady::new(
        &config.site,
        config.partition.as_ref(),
        &installed.names,
    );
    assert!(
        ready.detail().contains("docaudit: off"),
        "{}",
        ready.detail()
    );
    assert!(
        ready
            .detail()
            .contains("partition: node node-a, 1 DocTypes"),
        "{}",
        ready.detail()
    );
}

// --- the refusals -----------------------------------------------------------------------

#[test]
fn a_partitioned_doctype_with_no_scope_field_refuses_to_start() {
    let config = site_config(Some(partition(
        &[DOCTYPE, LATER],
        &[(DOCTYPE, "branch", "memo")],
    )));
    let registry: Arc<dyn spot_partition::PartitionRegistry> =
        Arc::new(MemoryPartitionRegistry::new());
    let error = participants::build(&site(), &config, &deps(Some(audit(true)), Some(registry)))
        .expect_err("an uncovered DocType never starts");
    let text = error.to_string();
    assert!(text.starts_with("partition.scope_uncovered:"), "{text}");
    assert!(
        text.contains(LATER),
        "the refusal names every uncovered DocType: {text}"
    );
    assert!(
        !text.contains(DOCTYPE),
        "the covered one is not named: {text}"
    );
}

#[test]
fn an_audit_step_beside_the_audit_participant_refuses_to_start() {
    let config = site_config(None);
    let meta = meta(false);
    let mut controllers = ControllerRegistry::new();
    spot_doc_audit::install::register(
        &mut controllers,
        "sample",
        AuditStep::new(
            DOCTYPE,
            Arc::new(FailingWriter),
            masker(&meta),
            Arc::new(NoBeforeImages),
            Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH)),
        ),
    );
    let (_handle, source) = live_meta();
    let error = participants::build(
        &site(),
        &config,
        &Deps {
            audit: Some(Audit::new(&meta, masker(&meta))),
            controllers: Arc::new(controllers),
            partitions: None,
            meta: source,
        },
    )
    .expect_err("double auditing never starts");
    let text = error.to_string();
    assert!(text.starts_with("docaudit.double_registration:"), "{text}");
    assert!(text.contains(DOCTYPE), "{text}");
}

#[test]
fn a_node_profile_without_a_registry_refuses_to_start() {
    let config = site_config(Some(partition(&[DOCTYPE], &[(DOCTYPE, "branch", "memo")])));
    let error = participants::build(&site(), &config, &deps(Some(audit(false)), None))
        .expect_err("a node never runs unpartitioned");
    let text = error.to_string();
    assert!(
        text.starts_with("cli.partition_registry_missing:"),
        "{text}"
    );
    assert!(text.contains("node-a"), "{text}");
}

// --- the metadata generation ------------------------------------------------------------

#[test]
fn every_generation_gets_its_own_masker_and_its_own_edge_metadata() {
    let config = site_config(None);
    let (handle, source) = live_meta();
    let first_meta = meta(false);
    let first = participants::build(
        &site(),
        &config,
        &Deps {
            audit: Some(Audit::new(&first_meta, masker(&first_meta))),
            controllers: Arc::new(ControllerRegistry::new()),
            partitions: None,
            meta: source.clone(),
        },
    )
    .expect("the first generation installs");
    let first_audit = Audit::new(&first_meta, masker(&first_meta));
    assert!(!first_audit.doctypes().contains(&LATER.to_owned()));
    assert_eq!(first.meta.effective().doctypes().count(), 1);

    // The swap: the live handle now serves generation two.
    let second_meta = meta(true);
    *handle.write().unwrap_or_else(|p| p.into_inner()) = second_meta.clone();
    let second = participants::build(
        &site(),
        &config,
        &Deps {
            audit: Some(Audit::new(&second_meta, masker(&second_meta))),
            controllers: Arc::new(ControllerRegistry::new()),
            partitions: None,
            meta: source,
        },
    )
    .expect("the second generation installs");
    // The edge participant reads the live handle, so a swap changes what it derives from.
    assert_eq!(second.meta.effective().doctypes().count(), 2);
    // And the audit participant built for generation two audits the DocType generation two added.
    let audited = Audit::new(&second_meta, masker(&second_meta));
    assert!(audited.doctypes().contains(&DOCTYPE.to_owned()));
    assert!(audited.doctypes().contains(&LATER.to_owned()));
    assert_eq!(audited.doctypes().len(), 2);
}

#[test]
fn the_masker_follows_the_generation_and_refuses_one_it_does_not_know() {
    let first = meta(false);
    let second = meta(true);
    let change = FieldChange {
        path: "memo".to_owned(),
        before: Some(Value::Text("before".to_owned())),
        after: Some(Value::Text("after".to_owned())),
    };
    // Generation one leaves `memo` alone; generation two masks it.
    let plain = spot_doc_audit::FieldMasker::mask_for_audit(
        &*masker(&first),
        DOCTYPE,
        std::slice::from_ref(&change),
    )
    .expect("a known DocType masks");
    assert_eq!(
        plain.changes[0].after,
        Some(Value::Text("after".to_owned()))
    );
    let masked = spot_doc_audit::FieldMasker::mask_for_audit(&*masker(&second), DOCTYPE, &[change])
        .expect("a known DocType masks");
    assert_ne!(
        masked.changes[0].after,
        Some(Value::Text("after".to_owned())),
        "the generation that protects `memo` masks it"
    );
    // A DocType the masker does not know is refused, not written unmasked: the participant
    // propagates that error, so the write that was being audited fails closed.
    let error = spot_doc_audit::FieldMasker::mask_for_audit(
        &*masker(&first),
        LATER,
        &[FieldChange {
            path: "memo".to_owned(),
            before: None,
            after: Some(Value::Text("x".to_owned())),
        }],
    )
    .expect_err("an unknown DocType is refused");
    assert!(error.code().starts_with("docaudit."), "{}", error.code());
}
