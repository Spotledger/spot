//! WP-AI-03.T8: the engine-backed document reader. The permission core, the protection facts and
//! the field guard are the real ones (built from real metadata and a real permission model); only
//! the document loader is scripted, so the tests decide which user may load which record and can
//! make the engine fail. The last test builds the LLM gateway over the provided reader and checks
//! what a prompt contains.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use spot_app_api::Document;
use spot_app_api::context::{Actor, Invocation};
use spot_app_api::prelude::{Name, RecordId, Value};
use spot_cli::services::egress::SharedGuard;
use spot_cli::services::llm::config::TenantConfig;
use spot_cli::services::llm::gateway::{
    Caller, Deps, LlmGateway, ProviderFactory, TransportFactory,
};
use spot_cli::services::llm::prompt::{DocumentReader, FieldClass, PromptPolicy, ReadRefusal};
use spot_cli::services::llm::store::Store;
use spot_cli::services::llm_reader::reader::{EngineReader, Fut, Loaded, Seams};
use spot_core::{Clock, ManualClock, SiteId};
use spot_document::secrets::SecretVault;
use spot_egress::{EgressPolicy, Guard, InMemorySink, Resolver};
use spot_events_contract::PrincipalRef;
use spot_field_guard::{
    AuditRecord, BoundedQueue, FieldAccessRecord, FieldGuard, MetaProtection, ProtectionSource,
};
use spot_llm::transport::HttpTransport;
use spot_llm::transport::mock::MockTransport;
use spot_llm::{
    ApiKey, Message, MockAction, MockProvider, MockStep, Provider, Response, StopReason, Usage,
};
use spot_meta::{
    BuildOptions, Definition, EffectiveMeta, FieldSpec, FieldType, MaskSpec, ProtectionGroupSpec,
    Registry,
};
use spot_permissions::{Model, ModelTexts, PermissionEngine, Principal, Snapshot, load};
use spot_redact::{RedactionPolicy, Redactor};
use spot_secrets::{MemoryKeyring, SecretBox, SecretString};
use time::OffsetDateTime;

const DOCTYPE: &str = "ticket";
const SUBJECT: &str = "ship the printer early";
const DETAILS: &str = "the cable is under the desk";
const CARD: &str = "4111111111111111";
const NOTE: &str = "PROTECTED-HR-NOTE-XYZ";

// --- the real permission fixture ---------------------------------------------------------------

fn grouped(name: &str, group: &str, mask: MaskSpec) -> FieldSpec {
    let mut field = FieldSpec::new(name, FieldType::Text);
    field.protection_group = Some(group.to_owned());
    field.masking = Some(mask);
    field
}

fn meta() -> Arc<EffectiveMeta> {
    let mut ticket = Definition::new(DOCTYPE);
    ticket.fields = vec![
        FieldSpec::new("subject", FieldType::Text),
        FieldSpec::new("details", FieldType::LongText),
        grouped(
            "card",
            "pci",
            MaskSpec {
                placeholder: false,
                last_chars: Some(4),
            },
        ),
        grouped("hr_note", "hr", MaskSpec::placeholder()),
    ];
    ticket.protection_groups = vec![
        ProtectionGroupSpec::new("pci", "Payment cards"),
        ProtectionGroupSpec::new("hr", "Compensation"),
    ];
    Registry::with_options(vec![ticket], BuildOptions::new())
        .unwrap_or_else(|e| panic!("fixture meta: {e:?}"))
        .effective()
}

fn model() -> Model {
    let mut grants = String::new();
    for (role, rights) in [
        ("Clerk", r#"["read"]"#),
        ("MaskedReader", r#"["read", "report"]"#),
        ("Analyst", r#"["read", "report"]"#),
    ] {
        grants.push_str(&format!(
            "[[entries]]\nrole = \"{role}\"\ndoctype = \"{DOCTYPE}\"\nrights = {rights}\n"
        ));
    }
    let mut field_grants = String::new();
    for group in ["pci", "hr"] {
        for (role, access) in [
            ("MaskedReader", r#"["read_masked", "report"]"#),
            ("Analyst", r#"["read", "report"]"#),
        ] {
            field_grants.push_str(&format!(
                "[[entries]]\nrole = \"{role}\"\ndoctype = \"{DOCTYPE}\"\ntarget = {{ group = \"{group}\" }}\naccess = {access}\n"
            ));
        }
    }
    load(&ModelTexts {
        grants,
        field_grants,
        groups: "[[entries]]\nname = \"pci\"\n[[entries]]\nname = \"hr\"\n".to_owned(),
        shares: String::new(),
    })
    .unwrap_or_else(|e| panic!("fixture model: {:?}", e.as_slice()))
}

/// What one site serves: the guard, the protection facts and who holds which role.
struct World {
    guard: Arc<FieldGuard>,
    protection: Arc<MetaProtection>,
}

fn world() -> World {
    let meta = meta();
    let protection = Arc::new(MetaProtection::new(&meta));
    let snapshot = Snapshot::build(&meta, &model())
        .unwrap_or_else(|e| panic!("{:?}", e.as_slice()))
        .with_field_facts(
            DOCTYPE,
            protection.protection(DOCTYPE).unwrap().fields.clone(),
        )
        .unwrap_or_else(|e| panic!("{:?}", e.as_slice()));
    let engine = Arc::new(PermissionEngine::new(Arc::new(snapshot)));
    let trace: Arc<BoundedQueue<FieldAccessRecord>> = Arc::new(BoundedQueue::new(1024));
    let audit: Arc<BoundedQueue<AuditRecord>> = Arc::new(BoundedQueue::new(1024));
    let guard = Arc::new(FieldGuard::new(
        engine,
        protection.clone(),
        trace,
        audit,
        Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH)),
    ));
    World { guard, protection }
}

fn name(text: &str) -> Name {
    Name::new(text).unwrap()
}

fn ticket() -> Document {
    let mut doc = Document::new(name(DOCTYPE), RecordId::new_v7());
    doc.set(name("subject"), Value::Text(SUBJECT.into()));
    doc.set(name("details"), Value::Text(DETAILS.into()));
    doc.set(name("card"), Value::Text(CARD.into()));
    doc.set(name("hr_note"), Value::Text(NOTE.into()));
    doc
}

// --- the scripted seams --------------------------------------------------------------------------

/// The real guard and facts, a scripted loader and a scripted directory.
struct Scripted {
    world: World,
    /// Whether the site serves metadata.
    served: AtomicBool,
    /// Whether the document engine fails every load.
    outage: AtomicBool,
    /// Whether the principal directory cannot answer.
    directory_down: AtomicBool,
    docs: Mutex<HashMap<RecordId, Document>>,
    /// `(user, document)` pairs the engine lets through: the document-level grants and shares.
    allowed: Mutex<HashSet<(RecordId, RecordId)>>,
    roles: Mutex<HashMap<RecordId, &'static str>>,
    loads: AtomicUsize,
}

impl Scripted {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            world: world(),
            served: AtomicBool::new(true),
            outage: AtomicBool::new(false),
            directory_down: AtomicBool::new(false),
            docs: Mutex::new(HashMap::new()),
            allowed: Mutex::new(HashSet::new()),
            roles: Mutex::new(HashMap::new()),
            loads: AtomicUsize::new(0),
        })
    }

    /// A user holding `role`.
    fn user(&self, role: &'static str) -> RecordId {
        let id = RecordId::new_v7();
        self.roles.lock().unwrap().insert(id, role);
        id
    }

    /// A stored ticket.
    fn store(&self) -> RecordId {
        let doc = ticket();
        let id = doc.id();
        self.docs.lock().unwrap().insert(id, doc);
        id
    }

    fn allow(&self, user: RecordId, doc: RecordId) {
        self.allowed.lock().unwrap().insert((user, doc));
    }

    fn revoke(&self, user: RecordId, doc: RecordId) {
        self.allowed.lock().unwrap().remove(&(user, doc));
    }

    fn loads(&self) -> usize {
        self.loads.load(Ordering::SeqCst)
    }
}

impl Seams for Scripted {
    fn site(&self) -> SiteId {
        SiteId::new("site-1").unwrap()
    }

    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH
    }

    fn guard(&self) -> Option<Arc<FieldGuard>> {
        self.served
            .load(Ordering::SeqCst)
            .then(|| Arc::clone(&self.world.guard))
    }

    fn protection(&self, doctype: &str) -> Option<Arc<spot_field_guard::DocProtection>> {
        if !self.served.load(Ordering::SeqCst) {
            return None;
        }
        self.world.protection.protection(doctype)
    }

    fn principal(&self, id: RecordId) -> Fut<'_, Option<Principal>> {
        Box::pin(async move {
            if self.directory_down.load(Ordering::SeqCst) {
                return None;
            }
            // A user the directory does not know is a principal with no roles.
            let roles: Vec<&str> = self
                .roles
                .lock()
                .unwrap()
                .get(&id)
                .copied()
                .into_iter()
                .collect();
            Some(Principal::with_roles(&id.to_string(), &roles))
        })
    }

    fn load<'a>(&'a self, inv: &'a Invocation, doctype: &'a str, id: RecordId) -> Fut<'a, Loaded> {
        Box::pin(async move {
            self.loads.fetch_add(1, Ordering::SeqCst);
            if self.outage.load(Ordering::SeqCst) {
                return Loaded::Unavailable;
            }
            let Actor::User { id: user, .. } = &inv.actor else {
                return Loaded::Absent;
            };
            if !self.allowed.lock().unwrap().contains(&(*user, id)) {
                return Loaded::Absent;
            }
            match self.docs.lock().unwrap().get(&id) {
                Some(doc) if doc.doctype().as_str() == doctype => Loaded::Document(doc.clone()),
                _ => Loaded::Absent,
            }
        })
    }
}

fn reader(seams: &Arc<Scripted>) -> EngineReader {
    EngineReader::new(Arc::clone(seams) as Arc<dyn Seams>, 4_000)
}

fn who(user: RecordId) -> String {
    PrincipalRef::user(user).to_string()
}

async fn read(
    r: &EngineReader,
    user: RecordId,
    doc: RecordId,
) -> Result<spot_cli::services::llm::prompt::DocumentView, ReadRefusal> {
    r.read(&who(user), DOCTYPE, &doc.to_string()).await
}

fn names(view: &spot_cli::services::llm::prompt::DocumentView) -> Vec<(&str, FieldClass)> {
    let mut list: Vec<(&str, FieldClass)> = view
        .fields
        .iter()
        .map(|f| (f.name.as_str(), f.class))
        .collect();
    list.sort();
    list
}

// --- tests ---------------------------------------------------------------------------------------

#[tokio::test]
async fn a_readable_document_comes_back_with_honest_classes() {
    let s = Scripted::new();
    let analyst = s.user("Analyst");
    let doc = s.store();
    s.allow(analyst, doc);
    let view = read(&reader(&s), analyst, doc).await.unwrap();
    assert_eq!(view.doctype, DOCTYPE);
    assert_eq!(view.name, doc.to_string());
    // Everything the analyst may read, each labelled: plain stays plain, the protection groups
    // are protected (the prompt builder, not the reader, strips them).
    assert_eq!(
        names(&view),
        vec![
            ("card", FieldClass::Protected),
            ("details", FieldClass::Plain),
            ("hr_note", FieldClass::Protected),
            ("subject", FieldClass::Plain),
        ]
    );
    let subject = view.fields.iter().find(|f| f.name == "subject").unwrap();
    assert_eq!(subject.value, SUBJECT);
}

#[tokio::test]
async fn a_field_the_user_may_not_read_is_absent_not_masked() {
    let s = Scripted::new();
    let masked = s.user("MaskedReader");
    let clerk = s.user("Clerk");
    let doc = s.store();
    s.allow(masked, doc);
    s.allow(clerk, doc);
    let r = reader(&s);

    // `read_masked` on the groups: the plain fields only, and no masked form of the others.
    let view = read(&r, masked, doc).await.unwrap();
    assert_eq!(
        names(&view),
        vec![
            ("details", FieldClass::Plain),
            ("subject", FieldClass::Plain)
        ]
    );
    for field in &view.fields {
        assert!(
            !field.value.contains(NOTE)
                && !field.value.contains("1111")
                && !field.value.contains('*'),
            "{field:?}"
        );
    }

    // `read` without `report` is not enough for the search channel: nothing is released.
    let view = read(&r, clerk, doc).await.unwrap();
    assert!(view.fields.is_empty(), "{view:?}");
}

#[tokio::test]
async fn an_unreadable_document_and_a_missing_one_are_the_same_refusal() {
    let s = Scripted::new();
    let analyst = s.user("Analyst");
    let stranger = s.user("Analyst");
    let doc = s.store();
    s.allow(analyst, doc);
    let r = reader(&s);

    // The stranger holds the role but not this document; another record does not exist.
    let unreadable = read(&r, stranger, doc).await;
    let missing = read(&r, analyst, RecordId::new_v7()).await;
    // A user the directory does not know has no roles and no grant.
    let unknown = read(&r, RecordId::new_v7(), doc).await;
    assert_eq!(unreadable.unwrap_err(), ReadRefusal::Denied);
    assert_eq!(missing.unwrap_err(), ReadRefusal::Denied);
    assert_eq!(unknown.unwrap_err(), ReadRefusal::Denied);

    // A hidden or unknown DocType and a name that is no record id are refused alike.
    let by_name = r
        .read(&who(analyst), "no_such_doctype", &doc.to_string())
        .await;
    assert_eq!(by_name.unwrap_err(), ReadRefusal::Denied);
    let bad_name = r.read(&who(analyst), DOCTYPE, "T-1").await;
    assert_eq!(bad_name.unwrap_err(), ReadRefusal::Denied);
    // None of those was an outage.
    assert_eq!(r.unavailable_total(), 0);
}

#[tokio::test]
async fn a_revoked_share_is_refused_on_the_next_call_with_no_restart() {
    let s = Scripted::new();
    let analyst = s.user("Analyst");
    let doc = s.store();
    s.allow(analyst, doc);
    let r = reader(&s);
    assert!(read(&r, analyst, doc).await.is_ok());
    s.revoke(analyst, doc);
    assert_eq!(
        read(&r, analyst, doc).await.unwrap_err(),
        ReadRefusal::Denied
    );
    s.allow(analyst, doc);
    assert!(read(&r, analyst, doc).await.is_ok());
}

#[tokio::test]
async fn a_platform_a_consumer_and_every_other_actor_is_refused_before_anything_is_loaded() {
    let s = Scripted::new();
    let analyst = s.user("Analyst");
    let doc = s.store();
    s.allow(analyst, doc);
    let r = reader(&s);
    let name = doc.to_string();
    for actor in [
        PrincipalRef::consumer(analyst).to_string(),
        "system".to_owned(),
        "system-indexer".to_owned(),
        "operator".to_owned(),
        "agent:abc".to_owned(),
        "user:not-an-id".to_owned(),
        analyst.to_string(),
        String::new(),
    ] {
        let refused = r.read(&actor, DOCTYPE, &name).await;
        assert_eq!(refused.unwrap_err(), ReadRefusal::Denied, "{actor:?}");
    }
    assert_eq!(s.loads(), 0, "the engine was never asked");
    // The user reference itself works.
    assert!(r.read(&who(analyst), DOCTYPE, &name).await.is_ok());
    assert_eq!(s.loads(), 1);
}

#[tokio::test]
async fn an_outage_is_unavailable_counted_and_reported_not_a_denial() {
    let s = Scripted::new();
    let analyst = s.user("Analyst");
    let doc = s.store();
    s.allow(analyst, doc);
    let r = reader(&s);
    assert!(r.health().is_ok());

    s.outage.store(true, Ordering::SeqCst);
    assert_eq!(
        read(&r, analyst, doc).await.unwrap_err(),
        ReadRefusal::Unavailable
    );
    s.outage.store(false, Ordering::SeqCst);
    s.directory_down.store(true, Ordering::SeqCst);
    assert_eq!(
        read(&r, analyst, doc).await.unwrap_err(),
        ReadRefusal::Unavailable
    );
    s.directory_down.store(false, Ordering::SeqCst);

    assert_eq!(r.unavailable_total(), 2);
    let report = r.health().unwrap_err();
    assert!(report.contains("llm_reader.reads_failed"), "{report}");
    assert!(report.contains("2 read(s)"), "{report}");
    // The report clears once a check finds no new failure, and the reader works again.
    assert!(r.health().is_ok());
    assert!(read(&r, analyst, doc).await.is_ok());
}

#[tokio::test]
async fn a_site_that_serves_no_metadata_reads_nothing_and_says_so() {
    let s = Scripted::new();
    let analyst = s.user("Analyst");
    let doc = s.store();
    s.allow(analyst, doc);
    let r = reader(&s);
    s.served.store(false, Ordering::SeqCst);
    assert_eq!(
        read(&r, analyst, doc).await.unwrap_err(),
        ReadRefusal::Denied
    );
    assert_eq!(s.loads(), 0);
    let report = r.health().unwrap_err();
    assert!(report.contains("llm_reader.not_ready"), "{report}");
    s.served.store(true, Ordering::SeqCst);
    assert!(r.health().is_ok());
}

#[tokio::test]
async fn two_sites_never_mix() {
    let a = Scripted::new();
    let b = Scripted::new();
    let (user_a, user_b) = (a.user("Analyst"), b.user("Analyst"));
    let (doc_a, doc_b) = (a.store(), b.store());
    a.allow(user_a, doc_a);
    b.allow(user_b, doc_b);
    let (ra, rb) = (reader(&a), reader(&b));
    assert!(read(&ra, user_a, doc_a).await.is_ok());
    assert!(read(&rb, user_b, doc_b).await.is_ok());
    // Each site's reader knows only its own users and documents.
    assert_eq!(
        read(&rb, user_a, doc_a).await.unwrap_err(),
        ReadRefusal::Denied
    );
    assert_eq!(
        read(&ra, user_b, doc_b).await.unwrap_err(),
        ReadRefusal::Denied
    );
    assert_eq!(
        read(&ra, user_a, doc_b).await.unwrap_err(),
        ReadRefusal::Denied
    );
}

// --- the LLM gateway over the provided reader ---------------------------------------------------

const POLICY: &str = r#"
[tenants.acme]
[[tenants.acme.allow]]
host = "api.anthropic.test"
purpose = "llm"
"#;

struct Resolve;

#[async_trait::async_trait]
impl Resolver for Resolve {
    async fn resolve(&self, _host: &str) -> Result<Vec<IpAddr>, std::io::Error> {
        Ok(vec!["93.184.216.34".parse().unwrap()])
    }
}

struct NoNetwork;

impl TransportFactory for NoNetwork {
    fn transport(&self, _guard: Arc<Guard>, _tenant: &str) -> Arc<dyn HttpTransport> {
        Arc::new(MockTransport::new(Vec::new()))
    }
}

struct Mock(Arc<MockProvider>);

impl ProviderFactory for Mock {
    fn build(
        &self,
        _config: &TenantConfig,
        _key: Option<ApiKey>,
        _transport: Arc<dyn HttpTransport>,
    ) -> Arc<dyn Provider> {
        Arc::clone(&self.0) as Arc<dyn Provider>
    }
}

#[tokio::test]
async fn the_gateway_sends_the_readable_fields_and_none_of_the_hidden_ones() {
    let s = Scripted::new();
    let analyst = s.user("Analyst");
    let masked = s.user("MaskedReader");
    let doc = s.store();
    s.allow(analyst, doc);
    s.allow(masked, doc);

    let mut mock = MockProvider::new();
    mock.push_step(MockStep::new(
        None,
        MockAction::Respond(Response::new(
            Message::assistant("done"),
            StopReason::Stop,
            Usage {
                input_tokens: 10,
                output_tokens: 2,
                ..Usage::ZERO
            },
        )),
    ));
    let mock = Arc::new(mock);
    let clock = Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(1_795_262_400).unwrap(),
    ));
    let vault: Arc<dyn SecretVault> = Arc::new(SecretBox::new(
        MemoryKeyring::new("llm-reader-test").unwrap(),
    ));
    let guard = Arc::new(Guard::new(
        EgressPolicy::from_toml(POLICY).unwrap(),
        Arc::new(Resolve) as Arc<dyn Resolver>,
        Arc::new(InMemorySink::new()),
        Arc::clone(&clock) as Arc<dyn Clock>,
    ));
    let gateway = LlmGateway::new(Deps {
        site: "site-1".to_owned(),
        store: Store::memory(),
        vault,
        guard: Arc::new(SharedGuard::new(guard)),
        clock: Arc::clone(&clock) as Arc<dyn Clock>,
        providers: Arc::new(Mock(Arc::clone(&mock))),
        transports: Arc::new(NoNetwork),
        redactor: Arc::new(Redactor::from_policy(&RedactionPolicy::default()).unwrap()),
        // The engine-backed reader, not the deny-all default.
        reader: Arc::new(reader(&s)),
        max_retries: 0,
        retention_days: 1,
    });
    // Built through serde so the fields the service adds later take their defaults.
    let config: TenantConfig = serde_json::from_value(serde_json::json!({
        "tenant": "acme",
        "kind": "anthropic",
        "endpoint": "https://api.anthropic.test",
        "model": "claude-test",
        "allowed_hosts": ["api.anthropic.test"],
        "tasks": ["chat"],
        "default_max_output_tokens": 100,
    }))
    .unwrap();
    gateway.put_config(&config).await.unwrap();
    gateway
        .set_key("acme", &SecretString::new("sk-test-READER-0123456789"))
        .await
        .unwrap();

    // As the analyst: the plain fields go; the protected ones are classed and dropped by the
    // builder, so their values are in no prompt either.
    let builder = gateway
        .prompt(&who(analyst), PromptPolicy::default())
        .document(DOCTYPE, &doc.to_string())
        .await
        .unwrap();
    let (request, manifest) = builder.build("ignored", "r1");
    assert_eq!(manifest.classes.get("plain"), Some(&2));
    assert_eq!(manifest.classes.get("stripped_protected"), Some(&2));
    let caller = Caller {
        user: who(analyst),
        purpose: "summarise".to_owned(),
    };
    gateway
        .complete("acme", "chat", &caller, request, &manifest)
        .await
        .unwrap();

    // As the masked reader: only the plain fields were ever readable.
    let builder = gateway
        .prompt(&who(masked), PromptPolicy::default())
        .document(DOCTYPE, &doc.to_string())
        .await
        .unwrap();
    let (request, manifest) = builder.build("ignored", "r2");
    assert_eq!(manifest.classes.get("plain"), Some(&2));
    assert_eq!(manifest.classes.get("stripped_protected"), None);
    gateway
        .complete("acme", "chat", &caller, request, &manifest)
        .await
        .unwrap();

    let sent = mock.requests();
    assert_eq!(sent.len(), 2);
    for request in &sent {
        let text: String = request
            .messages
            .iter()
            .map(|m| m.text_content())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(SUBJECT) && text.contains(DETAILS), "{text}");
        assert!(!text.contains(CARD) && !text.contains(NOTE), "{text}");
    }

    // Without a grant the builder gets nothing at all: the document is not in the prompt.
    let stranger = s.user("Analyst");
    let refused = gateway
        .prompt(&who(stranger), PromptPolicy::default())
        .document(DOCTYPE, &doc.to_string())
        .await;
    assert!(refused.is_err());
}
