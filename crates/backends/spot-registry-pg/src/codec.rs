//! Turning a release and a transfer into columns plus a `jsonb` evidence document, and back.
//!
//! The model types of `spot-registry`, `spot-bundle-signing` and `spot-manifest` carry no serde
//! derives, and this crate must not add a second authoritative encoding, so the evidence document
//! is built field by field from their public parts and rebuilt through their public constructors
//! (`from_parts`, `parse`, `from_bytes`). Anything that no longer rebuilds is a storage failure,
//! never a guess.

use std::collections::BTreeSet;
use std::str::FromStr;

use serde_json::{Map, Value, json};
use spot_bundle_signing::{
    BundleSignature, CertificateChain, KeyId, PublicKey, PublisherCertificate, SignedCertificate,
};
use spot_manifest::{AppDependency, Capabilities, DocTypeAccess, Endpoint, FileAccess};
use spot_registry::{
    AppName, Digest, Release, Removal, Result, SignatureRef, TransferRecord, TransferRequest,
    Version, VersionReq,
};
use spot_storage_contract::Name;

use crate::error::corrupt;

/// The tokens of a text: alphanumeric runs, lowercased. The same rule the in-memory index uses.
pub(crate) fn tokens(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The searchable corpus of a release: its app name, title and description.
pub(crate) fn corpus(app: &AppName, title: &str, description: &str) -> BTreeSet<String> {
    tokens(&format!("{} {} {}", app.as_str(), title, description))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Result<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return Err(corrupt());
    }
    (0..text.len() / 2)
        .map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).map_err(|_| corrupt()))
        .collect()
}

fn array<const N: usize>(text: &str) -> Result<[u8; N]> {
    <[u8; N]>::try_from(unhex(text)?.as_slice()).map_err(|_| corrupt())
}

fn text<'a>(object: &'a Value, key: &str) -> Result<&'a str> {
    object.get(key).and_then(Value::as_str).ok_or_else(corrupt)
}

fn int(object: &Value, key: &str) -> Result<i64> {
    object.get(key).and_then(Value::as_i64).ok_or_else(corrupt)
}

fn uint(object: &Value, key: &str) -> Result<u64> {
    object.get(key).and_then(Value::as_u64).ok_or_else(corrupt)
}

fn flag(object: &Value, key: &str) -> Result<bool> {
    object.get(key).and_then(Value::as_bool).ok_or_else(corrupt)
}

fn list<'a>(object: &'a Value, key: &str) -> Result<&'a Vec<Value>> {
    object
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(corrupt)
}

fn strings(object: &Value, key: &str) -> Result<Vec<String>> {
    list(object, key)?
        .iter()
        .map(|v| v.as_str().map(str::to_owned).ok_or_else(corrupt))
        .collect()
}

/// The evidence of a signature and its certificate chain.
pub(crate) fn signature_ref_json(proof: &SignatureRef) -> Value {
    let s = &proof.signature;
    let c = &proof.chain.leaf;
    json!({
        "signature": {
            "app_id": s.app_id,
            "app_version": s.app_version,
            "publisher_id": s.publisher_id,
            "key_id": s.key_id.as_str(),
            "signed_at": s.signed_at,
            "bytes": hex(s.signature()),
        },
        "leaf": {
            "publisher_id": c.body.publisher_id,
            "display_name": c.body.display_name,
            "key": hex(c.body.key.as_bytes()),
            "valid_from": c.body.valid_from,
            "valid_to": c.body.valid_to,
            "issued_by": c.body.issued_by.as_str(),
            "serial": c.body.serial,
            "bytes": hex(c.signature()),
        },
    })
}

/// Rebuilds the evidence written by [`signature_ref_json`].
pub(crate) fn signature_ref_from(value: &Value) -> Result<SignatureRef> {
    let s = value.get("signature").ok_or_else(corrupt)?;
    let c = value.get("leaf").ok_or_else(corrupt)?;
    let key_id = |v: &Value, k: &str| KeyId::parse(text(v, k)?).map_err(|_| corrupt());
    let signature = BundleSignature::from_parts(
        text(s, "app_id")?.to_owned(),
        text(s, "app_version")?.to_owned(),
        text(s, "publisher_id")?.to_owned(),
        key_id(s, "key_id")?,
        int(s, "signed_at")?,
        array(text(s, "bytes")?)?,
    );
    let body = PublisherCertificate {
        publisher_id: text(c, "publisher_id")?.to_owned(),
        display_name: text(c, "display_name")?.to_owned(),
        key: PublicKey::from_bytes(&unhex(text(c, "key")?)?).map_err(|_| corrupt())?,
        valid_from: int(c, "valid_from")?,
        valid_to: int(c, "valid_to")?,
        issued_by: key_id(c, "issued_by")?,
        serial: uint(c, "serial")?,
    };
    let leaf = SignedCertificate::from_parts(body, array(text(c, "bytes")?)?);
    Ok(SignatureRef {
        signature,
        chain: CertificateChain::new(leaf),
    })
}

fn files_name(access: FileAccess) -> &'static str {
    match access {
        FileAccess::None => "none",
        FileAccess::Read => "read",
        FileAccess::ReadWrite => "read_write",
    }
}

fn files_from(name: &str) -> Result<FileAccess> {
    match name {
        "none" => Ok(FileAccess::None),
        "read" => Ok(FileAccess::Read),
        "read_write" => Ok(FileAccess::ReadWrite),
        _ => Err(corrupt()),
    }
}

/// The immutable part of a release that the query columns do not hold.
pub(crate) fn evidence_json(release: &Release) -> Value {
    let caps = &release.capabilities;
    json!({
        "depends": release.depends.iter()
            .map(|d| json!({"app": d.app.as_str(), "version": d.version.to_string()}))
            .collect::<Vec<_>>(),
        "capabilities": {
            "network": caps.network.iter()
                .map(|e| json!({"host": e.host, "port": e.port}))
                .collect::<Vec<_>>(),
            "email": caps.email,
            "files": files_name(caps.files),
            "read": caps.doctypes.read,
            "write": caps.doctypes.write,
        },
        "proof": signature_ref_json(&release.signature_ref),
    })
}

/// Everything the insert statement binds for a release.
pub(crate) struct ReleaseRow {
    pub app: String,
    pub version: String,
    pub digest: String,
    pub size: i64,
    pub requires_host: Option<String>,
    pub published_at: i64,
    pub publisher: String,
    pub yanked: Option<(i64, String)>,
    pub withdrawn: Option<(i64, String)>,
    pub title: String,
    pub description: String,
    pub category: String,
    pub category_key: String,
    pub is_pre: bool,
    pub cap_network: bool,
    pub cap_email: bool,
    pub cap_files: &'static str,
    pub cap_doc_read: bool,
    pub cap_doc_write: bool,
    pub evidence: String,
    pub tokens: Vec<String>,
    pub vector_text: String,
}

impl ReleaseRow {
    pub(crate) fn of(release: &Release) -> Result<Self> {
        let removal = |r: &Option<Removal>| r.as_ref().map(|r| (r.at, r.reason.clone()));
        let words = corpus(&release.app, &release.title, &release.description);
        let caps = &release.capabilities;
        Ok(Self {
            app: release.app.as_str().to_owned(),
            version: release.version.to_string(),
            digest: release.digest.to_hex(),
            size: i64::try_from(release.size).map_err(|_| corrupt())?,
            requires_host: release.requires_host.as_ref().map(ToString::to_string),
            published_at: release.published_at,
            publisher: release.publisher.clone(),
            yanked: removal(&release.yanked),
            withdrawn: removal(&release.withdrawn),
            title: release.title.clone(),
            description: release.description.clone(),
            category: release.category.clone(),
            category_key: release.category.to_ascii_lowercase(),
            is_pre: !release.version.pre.is_empty(),
            cap_network: !caps.network.is_empty(),
            cap_email: caps.email,
            cap_files: files_name(caps.files),
            cap_doc_read: !caps.doctypes.read.is_empty(),
            cap_doc_write: !caps.doctypes.write.is_empty(),
            evidence: evidence_json(release).to_string(),
            vector_text: words.iter().cloned().collect::<Vec<_>>().join(" "),
            tokens: words.into_iter().collect(),
        })
    }
}

/// The columns [`release_from_row`] reads, in order.
pub(crate) const RELEASE_COLUMNS: &str = "app, version, digest, size, requires_host, published_at, \
     publisher, yanked_at, yanked_reason, withdrawn_at, withdrawn_reason, title, description, \
     category, evidence::text";

fn removal(at: Option<i64>, reason: Option<String>) -> Result<Option<Removal>> {
    match (at, reason) {
        (Some(at), Some(reason)) => Ok(Some(Removal { at, reason })),
        (None, None) => Ok(None),
        _ => Err(corrupt()),
    }
}

/// Rebuilds a release from a row selected with [`RELEASE_COLUMNS`].
pub(crate) fn release_from_row(row: &tokio_postgres::Row) -> Result<Release> {
    let get = |i: usize| -> Result<String> { row.try_get(i).map_err(|_| corrupt()) };
    let opt = |i: usize| -> Result<Option<String>> { row.try_get(i).map_err(|_| corrupt()) };
    let int = |i: usize| -> Result<i64> { row.try_get(i).map_err(|_| corrupt()) };
    let opt_int = |i: usize| -> Result<Option<i64>> { row.try_get(i).map_err(|_| corrupt()) };
    let evidence: Value = serde_json::from_str(&get(14)?).map_err(|_| corrupt())?;
    let caps = evidence.get("capabilities").ok_or_else(corrupt)?;
    let capabilities = Capabilities {
        network: list(caps, "network")?
            .iter()
            .map(|e| {
                Ok(Endpoint {
                    host: text(e, "host")?.to_owned(),
                    port: u16::try_from(uint(e, "port")?).map_err(|_| corrupt())?,
                })
            })
            .collect::<Result<_>>()?,
        email: flag(caps, "email")?,
        files: files_from(text(caps, "files")?)?,
        doctypes: DocTypeAccess {
            read: strings(caps, "read")?,
            write: strings(caps, "write")?,
        },
    };
    let depends = list(&evidence, "depends")?
        .iter()
        .map(|d| {
            Ok(AppDependency {
                app: Name::new(text(d, "app")?).map_err(|_| corrupt())?,
                version: VersionReq::parse(text(d, "version")?).map_err(|_| corrupt())?,
            })
        })
        .collect::<Result<_>>()?;
    Ok(Release {
        app: AppName::parse(&get(0)?).map_err(|_| corrupt())?,
        version: Version::parse(&get(1)?).map_err(|_| corrupt())?,
        digest: Digest::from_str(&get(2)?).map_err(|_| corrupt())?,
        size: u64::try_from(int(3)?).map_err(|_| corrupt())?,
        requires_host: opt(4)?
            .map(|r| VersionReq::parse(&r).map_err(|_| corrupt()))
            .transpose()?,
        depends,
        published_at: int(5)?,
        publisher: get(6)?,
        yanked: removal(opt_int(7)?, opt(8)?)?,
        withdrawn: removal(opt_int(9)?, opt(10)?)?,
        signature_ref: signature_ref_from(evidence.get("proof").ok_or_else(corrupt)?)?,
        title: get(11)?,
        description: get(12)?,
        category: get(13)?,
        capabilities,
    })
}

/// The evidence document of a transfer: both signatures.
pub(crate) fn transfer_evidence(record: &TransferRecord) -> Value {
    let mut map = Map::new();
    map.insert("from".to_owned(), signature_ref_json(&record.from));
    map.insert("to".to_owned(), signature_ref_json(&record.to));
    Value::Object(map)
}

/// The columns [`transfer_from_row`] reads, in order.
pub(crate) const TRANSFER_COLUMNS: &str =
    "app, from_publisher, to_publisher, generation, at, expires_at, reason, evidence::text";

/// Rebuilds a transfer from a row selected with [`TRANSFER_COLUMNS`].
pub(crate) fn transfer_from_row(row: &tokio_postgres::Row) -> Result<TransferRecord> {
    let get = |i: usize| -> Result<String> { row.try_get(i).map_err(|_| corrupt()) };
    let int = |i: usize| -> Result<i64> { row.try_get(i).map_err(|_| corrupt()) };
    let evidence: Value = serde_json::from_str(&get(7)?).map_err(|_| corrupt())?;
    Ok(TransferRecord {
        request: TransferRequest {
            app: AppName::parse(&get(0)?).map_err(|_| corrupt())?,
            from: get(1)?,
            to: get(2)?,
            generation: u64::try_from(int(3)?).map_err(|_| corrupt())?,
            at: int(4)?,
            expires_at: int(5)?,
            reason: get(6)?,
        },
        from: signature_ref_from(evidence.get("from").ok_or_else(corrupt)?)?,
        to: signature_ref_from(evidence.get("to").ok_or_else(corrupt)?)?,
    })
}
