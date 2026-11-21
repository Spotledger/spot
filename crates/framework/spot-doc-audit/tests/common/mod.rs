//! Shared fixtures: a real field guard over one protected DocType, a fake backend, and the
//! in-memory audit and appender stores the tests drive.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spot_app_api::prelude::{Name, RecordId, Value};
use spot_app_api::{Actor, Invocation};
use spot_core::{ManualClock, SiteId};
use spot_doc_audit::event::{ActorRef, AuditSource, DocAuditEvent, DocOperation, DocumentImage};
use spot_field_guard::source::{DocProtection, StaticProtection};
use spot_field_guard::{
    AuditRecord as GuardAuditRecord, BoundedQueue, FieldAccessRecord, FieldGuard, ProtectionSource,
};
use spot_meta::{
    BuildOptions, Definition, EffectiveMeta, FieldSpec, FieldType, MaskSpec, ProtectionGroupSpec,
    Registry,
};
use spot_permissions::model::Principal;
use spot_permissions::{
    DocFacts, FieldFacts, GroupName, Model, ModelTexts, PermissionEngine, Snapshot, load,
};

pub const DOCTYPE: &str = "note";
pub const SECRET: &str = "secret";
pub const TITLE: &str = "title";

fn definitions() -> Vec<Definition> {
    let mut note = Definition::new(DOCTYPE);
    let mut secret = FieldSpec::new(SECRET, FieldType::Text);
    secret.protection_group = Some("confidential".to_owned());
    secret.masking = Some(MaskSpec::placeholder());
    note.fields = vec![FieldSpec::new(TITLE, FieldType::Text), secret];
    note.protection_groups = vec![ProtectionGroupSpec::new("confidential", "Confidential")];
    vec![note]
}

pub fn meta() -> Arc<EffectiveMeta> {
    Registry::with_options(definitions(), BuildOptions::new())
        .unwrap_or_else(|e| panic!("fixture meta: {e:?}"))
        .effective()
}

pub fn protection() -> Arc<StaticProtection> {
    Arc::new(StaticProtection::new(vec![DocProtection {
        name: DOCTYPE.to_owned(),
        fields: vec![FieldFacts {
            path: SECRET.to_owned(),
            groups: vec![GroupName("confidential".to_owned())],
            tags: vec![],
            mask: Some(spot_permissions::MaskFormat::Placeholder),
        }],
        tables: BTreeMap::new(),
        trace_groups: vec![],
    }]))
}

const GROUPS: &str = r#"
[[entries]]
name = "confidential"
"#;

fn grants() -> String {
    let mut out = String::new();
    for role in ["Reader", "MaskedReader"] {
        out.push_str(&format!(
            "[[entries]]\nrole = \"{role}\"\ndoctype = \"{DOCTYPE}\"\nrights = [\"read\"]\n"
        ));
    }
    out
}

fn field_grants() -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "[[entries]]\nrole = \"Reader\"\ndoctype = \"{DOCTYPE}\"\ntarget = {{ group = \"confidential\" }}\naccess = [\"read\"]\n"
    ));
    out.push_str(&format!(
        "[[entries]]\nrole = \"MaskedReader\"\ndoctype = \"{DOCTYPE}\"\ntarget = {{ group = \"confidential\" }}\naccess = [\"read_masked\"]\n"
    ));
    out
}

pub fn model() -> Model {
    load(&ModelTexts {
        grants: grants(),
        field_grants: field_grants(),
        groups: GROUPS.to_owned(),
        shares: String::new(),
    })
    .unwrap_or_else(|e| panic!("fixture model: {:?}", e.as_slice()))
}

pub struct Fixture {
    pub guard: Arc<FieldGuard>,
    pub protection: Arc<StaticProtection>,
}

pub fn fixture() -> Fixture {
    let meta = meta();
    let protection = protection();
    let mut snap =
        Snapshot::build(&meta, &model()).unwrap_or_else(|e| panic!("{:?}", e.as_slice()));
    let prot = protection.protection(DOCTYPE).unwrap();
    snap = snap
        .with_field_facts(DOCTYPE, prot.fields.clone())
        .unwrap_or_else(|e| panic!("{:?}", e.as_slice()));
    let engine = Arc::new(PermissionEngine::new(Arc::new(snap)));
    let trace = Arc::new(BoundedQueue::<FieldAccessRecord>::new(64));
    let audit = Arc::new(BoundedQueue::<GuardAuditRecord>::new(64));
    let clock = Arc::new(ManualClock::new(
        spot_app_api::prelude::OffsetDateTime::UNIX_EPOCH,
    ));
    let guard = Arc::new(FieldGuard::new(
        engine,
        protection.clone(),
        trace,
        audit,
        clock,
    ));
    Fixture { guard, protection }
}

pub fn masker() -> Arc<spot_doc_audit::GuardMasker> {
    let f = fixture();
    Arc::new(spot_doc_audit::GuardMasker::new(f.guard, f.protection))
}

pub fn principal(name: &str) -> Principal {
    match name {
        "reader" => Principal::with_roles("reader", &["Reader"]),
        "masked" => Principal::with_roles("masked", &["MaskedReader"]),
        "nobody" => Principal::with_roles("nobody", &[]),
        other => panic!("unknown principal {other}"),
    }
}

pub fn doc_facts() -> DocFacts {
    DocFacts {
        doctype: DOCTYPE.to_owned(),
        ..DocFacts::default()
    }
}

// ----------------------------------------------------------------- images and events

pub fn name(s: &str) -> Name {
    Name::new(s).unwrap()
}

pub fn image(id: &str, version: u64, title: &str, secret: &str) -> DocumentImage {
    let mut img = DocumentImage::new(DOCTYPE, id, version);
    img.fields
        .insert(name(TITLE), Value::Text(title.to_owned()));
    img.fields
        .insert(name(SECRET), Value::Text(secret.to_owned()));
    img
}

pub fn image_title_only(id: &str, version: u64, title: &str) -> DocumentImage {
    let mut img = DocumentImage::new(DOCTYPE, id, version);
    img.fields
        .insert(name(TITLE), Value::Text(title.to_owned()));
    img
}

pub fn user_actor(user: &str) -> ActorRef {
    ActorRef {
        actor: format!("user:{user}"),
        on_behalf_of: None,
        component: None,
    }
}

pub fn event(id: &str, version: u64, operation: DocOperation, title: &str) -> DocAuditEvent {
    DocAuditEvent {
        site: "test.spotledger.app".to_owned(),
        doctype: DOCTYPE.to_owned(),
        id: id.to_owned(),
        version,
        operation,
        actor: ActorRef {
            actor: format!("user:{}", id_of(1_000)),
            on_behalf_of: None,
            component: None,
        },
        at: 1_700_000_000_000_000,
        changed_fields: vec![TITLE.to_owned()],
        diff: vec![spot_doc_audit::DiffChange {
            path: TITLE.to_owned(),
            before: None,
            after: Some(spot_doc_audit::DiffValue::Text(title.to_owned())),
        }],
        hidden_changes: false,
        command_id: Some("cmd-1".to_owned()),
        source: AuditSource::Api,
        truncated: false,
        diff_digest: None,
        method: None,
        correction_half: None,
        via: None,
        node: None,
        outcome: None,
        error_class: None,
        error_code: None,
    }
}

pub fn invocation(_user: &str) -> Invocation {
    Invocation {
        site: SiteId::new("test.spotledger.app").unwrap(),
        actor: Actor::User {
            id: RecordId::new_v7(),
            via: None,
        },
        command_id: None,
        language: None,
        now: spot_app_api::prelude::OffsetDateTime::UNIX_EPOCH,
    }
}

/// A writer that records every event it is given.
#[derive(Default)]
pub struct RecordingWriter {
    events: std::sync::Mutex<Vec<DocAuditEvent>>,
}

impl RecordingWriter {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn events(&self) -> Vec<DocAuditEvent> {
        self.events.lock().unwrap().clone()
    }
}

impl spot_doc_audit::AuditWriter for RecordingWriter {
    fn write(&self, event: &DocAuditEvent) -> Result<(), spot_doc_audit::DocAuditError> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }
}

pub fn id_of(n: u64) -> String {
    format!("0190c0a0-0000-7000-8000-{n:012}")
}

pub fn names(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|s| (*s).to_owned()).collect()
}
