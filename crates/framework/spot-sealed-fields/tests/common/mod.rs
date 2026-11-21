//! Shared by the channel-leak tests: the marker, the leak detector and the fixtures.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_meta::{Definition, FieldSpec, FieldType, ProtectionGroupSpec, Registry};
use spot_permissions::{
    DocFacts, FieldFacts, GroupName as PermGroup, MaskFormat, Model, ModelTexts, PermissionEngine,
    Principal, Snapshot, load,
};
use spot_protected_keys::{
    GroupName, Hub, KeyEpoch, MemoryRootKeyProvider, MonoTime, NodeId, NodeKeyStore, NodeWrapKey,
    TenantId, Tier, TierTable,
};
use spot_sealed_fields::{
    AuditSink, HubPublicKey, Loaded, NodeFieldStore, PUBLIC_KEY_LEN, RevealService, SealTarget,
    Sealed, SealedAd, SealedFieldsError, SyncClass, ValueSource,
};
use spot_secrets::SecretBytes;

pub const MARKER: &str = "MARKER-SECRET-VALUE-DE89";
pub const DOCTYPE: &str = "bank_account";

pub fn marker() -> &'static [u8] {
    MARKER.as_bytes()
}

pub fn tenant() -> TenantId {
    TenantId::new("acme").expect("tenant")
}

pub fn group(name: &str) -> GroupName {
    GroupName::new(name).expect("group")
}

pub fn node(name: &str) -> NodeId {
    NodeId::new(name).expect("node")
}

// ------------------------------------------------------------------------------- detector

pub fn hex_encode(bytes: &[u8], upper: bool) -> String {
    let alphabet: &[u8] = if upper {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(alphabet[(byte >> 4) as usize] as char);
        out.push(alphabet[(byte & 0x0f) as usize] as char);
    }
    out
}

// Small standard base64 encoder, so the tests do not add a dependency.
pub fn base64_encode(bytes: &[u8]) -> String {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHA[((n >> 18) & 63) as usize] as char);
        out.push(ALPHA[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHA[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHA[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// The base64 text a value leaves inside a longer base64 stream, at each of the three byte
/// alignments: the characters that depend only on the value's own bytes.
pub fn base64_forms(value: &[u8]) -> Vec<String> {
    let mut forms = Vec::new();
    for shift in 0..3usize {
        let mut prefixed = vec![0u8; shift];
        prefixed.extend_from_slice(value);
        let text = base64_encode(&prefixed);
        let text = text.trim_end_matches('=');
        // Drop the leading characters that mix in the prefix bits, and the final character,
        // which may mix in whatever follows.
        let skip = (shift * 8).div_ceil(6);
        if text.len() > skip + 1 {
            forms.push(text[skip..text.len() - 1].to_owned());
        }
    }
    forms
}

/// Whether `text` holds `prefix` followed by exactly the number `n` (not a longer number).
pub fn has_number(text: &str, prefix: &str, n: usize) -> bool {
    let needle = format!("{prefix}{n}");
    text.match_indices(&needle).any(|(at, _)| {
        !text[at + needle.len()..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
    })
}

/// What leaks of `value` appear in `text`: the value, its base64, its hex, or its length given
/// as a number next to a length label. Empty means nothing leaks.
pub fn leaks_in(text: &str, value: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    let plain = String::from_utf8_lossy(value).into_owned();
    if text.contains(&plain) {
        found.push("plaintext".to_owned());
    }
    // A byte slice printed with `{:?}`, the way a careless log line shows a `&[u8]`.
    if text.contains(&format!("{value:?}")) {
        found.push("byte-list".to_owned());
    }
    for form in base64_forms(value) {
        if text.contains(&form) {
            found.push("base64".to_owned());
        }
    }
    if text.contains(&hex_encode(value, false)) || text.contains(&hex_encode(value, true)) {
        found.push("hex".to_owned());
    }
    let n = value.len();
    for label in ["len", "length", "size"] {
        for sep in [": ", "=", ":", " = "] {
            for quote in ["", "\""] {
                if has_number(text, &format!("{quote}{label}{quote}{sep}"), n) {
                    found.push("length".to_owned());
                }
            }
        }
    }
    if text.contains(&format!("{n} bytes")) && !text.contains(&format!("{n}0 bytes")) {
        found.push("length".to_owned());
    }
    found
}

pub fn assert_clean(channel: &str, text: &str) {
    let found = leaks_in(text, marker());
    assert!(
        found.is_empty(),
        "channel `{channel}` leaked the marker ({found:?}) in: {text}"
    );
}

pub fn assert_clean_bytes(channel: &str, bytes: &[u8]) {
    assert!(
        !bytes.windows(marker().len()).any(|w| w == marker()),
        "channel `{channel}` carries the marker as raw bytes"
    );
    assert_clean(channel, &String::from_utf8_lossy(bytes));
}

// ------------------------------------------------------------------------------- fixtures

pub fn meta() -> Arc<spot_meta::EffectiveMeta> {
    let mut doctype = Definition::new(DOCTYPE);
    doctype.fields = vec![FieldSpec::new("iban", FieldType::Text)];
    doctype.protection_groups = vec![ProtectionGroupSpec::new("bank", "Bank")];
    Arc::new(Registry::build(vec![doctype], &spot_meta::Customisations::new()).expect("meta"))
}

pub fn model() -> Model {
    load(&ModelTexts {
        grants: r#"
[[entries]]
role = "Clerk"
doctype = "bank_account"
rights = ["read"]
[[entries]]
role = "MaskedReader"
doctype = "bank_account"
rights = ["read"]
"#
        .to_owned(),
        field_grants: r#"
[[entries]]
role = "Clerk"
doctype = "bank_account"
target = { group = "bank" }
access = ["read", "reveal"]
[[entries]]
role = "MaskedReader"
doctype = "bank_account"
target = { group = "bank" }
access = ["read_masked"]
"#
        .to_owned(),
        groups: "[[entries]]\nname = \"bank\"\n".to_owned(),
        shares: String::new(),
    })
    .expect("model")
}

pub fn engine() -> Arc<PermissionEngine> {
    let snapshot = Snapshot::build(&meta(), &model())
        .expect("snapshot")
        .with_field_facts(
            DOCTYPE,
            vec![FieldFacts {
                path: "iban".to_owned(),
                groups: vec![PermGroup("bank".to_owned())],
                tags: Vec::new(),
                mask: Some(MaskFormat::LastChars(4)),
            }],
        )
        .expect("facts");
    Arc::new(PermissionEngine::new(Arc::new(snapshot)))
}

/// The hub's value store: every document holds the marker.
pub struct MarkerSource;

impl ValueSource for MarkerSource {
    fn load(&self, _doctype: &str, id: &str, _field: &str) -> Option<Loaded> {
        Some(Loaded {
            value: SecretBytes::new(marker().to_vec()),
            doc_facts: DocFacts {
                doctype: DOCTYPE.to_owned(),
                id: Some(id.to_owned()),
                created_by: Some("alice".to_owned()),
                ..DocFacts::default()
            },
        })
    }
}

pub fn service_with(audit: Arc<dyn AuditSink>) -> RevealService {
    RevealService::new(engine(), Arc::new(MarkerSource), audit)
}

pub fn clerk() -> Principal {
    Principal::with_roles("alice", &["Clerk"])
}

pub fn masked_reader() -> Principal {
    Principal::with_roles("bob", &["MaskedReader"])
}

/// A node needs only a public key to write; the private half is not needed to test a node.
pub fn some_hub_key(seed: u8) -> HubPublicKey {
    HubPublicKey::from_bytes([seed; PUBLIC_KEY_LEN])
}

pub fn node_store(key: HubPublicKey) -> NodeFieldStore {
    NodeFieldStore::new(node("branch-1"), tenant(), DOCTYPE, key)
}

pub fn iban_ad() -> SealedAd {
    SealedAd::of_field(tenant(), DOCTYPE, "iban", "BA-1")
}

pub fn write_marker(
    store: &mut NodeFieldStore,
    ad: &SealedAd,
) -> Result<Sealed, SealedFieldsError> {
    let (g, key) = (group("bank_details"), store.hub_key());
    store
        .write(
            &SealTarget {
                group: &g,
                class: SyncClass::Sealed,
                field: "iban",
                document: "BA-1",
                hub_key: &key,
            },
            marker(),
            ad,
        )
        .map(|stored| stored.sealed)
}

/// A node with a live one-hour lease on `bank_details`, and the group's epoch.
pub fn leased_node(now: MonoTime) -> (NodeKeyStore, GroupName, KeyEpoch) {
    let mut hub = Hub::new(MemoryRootKeyProvider::new(tenant()).expect("root"));
    let g = group("bank_details");
    let epoch = hub.create_group(&g, MonoTime::ZERO).expect("group");
    let n = node("branch-1");
    let hub_copy = NodeWrapKey::generate(n.clone()).expect("wrap");
    let node_copy = hub_copy.duplicate().expect("duplicate");
    hub.register_node(&n, hub_copy).expect("register");
    let mut tiers = TierTable::new();
    tiers.grant(n.clone(), g.clone(), Tier::one());
    let release = hub.release(&n, &g, now, &tiers).expect("release");
    let mut store = NodeKeyStore::new(n, node_copy);
    store.accept(&release, now).expect("accept");
    (store, g, epoch)
}

pub fn show<E: std::fmt::Display + std::fmt::Debug>(error: &E) -> String {
    format!("{error} || {error:?}")
}
