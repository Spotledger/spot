//! Publish: reading the multipart upload, and the blocking steps that follow.
//!
//! The upload is read without a parser of our own (axum's `multipart`, which is `multer`). The
//! bundle part is streamed to a private temporary file and cut off the moment it passes the size
//! cap, so an oversized upload is never held whole. Small text parts are capped separately. Part
//! names, repeat counts and the content type of a certificate part are checked as the parts
//! arrive; the order is free, as the contract leaves it.
//!
//! The registry core (`Registry::publish`) validates the bundle, the signature chain, the
//! revocation list, name ownership and the monotonic version, and only then writes the blob into
//! the content-addressed store; a refused upload therefore leaves no blob, and the temporary file
//! is removed in every case. The listing starts as `pending_review`; nothing is public until an
//! operator approves it.

use std::path::{Path, PathBuf};

use axum::extract::multipart::{Field, Multipart, MultipartError};
use axum::http::StatusCode;
use base64::Engine as _;
use sha2::{Digest as _, Sha256};
use spot_bundle::{Bundle, BundleError};
use spot_bundle_signing::{BundleSignature, CertificateChain, SignedCertificate};
use spot_registry::{AppName, IndexStore as _, PublisherStatus, Release};
use spot_store_state::{
    AuditEntry, AuditKind, BAD_TRANSITION, Begin, Fingerprint, ListingState, ReviewRecord,
    ReviewStatus, ReviewUpdate,
};
use tokio::io::AsyncWriteExt;

use crate::auth::Principal;
use crate::config::Limits;
use crate::error::{ApiError, Wire, bundle_error_code, codes, publish_error};
use crate::exec::block_on;
use crate::keys::hex;
use crate::metrics::Metrics;
use crate::ops::clip;
use crate::review::ReviewJob;
use crate::service::Core;

/// The most parts a publish may carry: the bundle, the signature and at most 8 certificates,
/// with room for nothing else.
const MAX_PARTS: usize = 10;
/// The contract's longest certificate chain.
const MAX_CHAIN: usize = 8;

/// A temporary upload file, removed when dropped.
pub(crate) struct TempFile {
    path: PathBuf,
}

impl Drop for TempFile {
    fn drop(&mut self) {
        // Nothing to do when the file is already gone.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A fully read upload.
pub(crate) struct Upload {
    pub temp: TempFile,
    pub size: u64,
    pub sha256: [u8; 32],
    pub signature: String,
    pub chain: Vec<String>,
}

fn multipart_error(error: &MultipartError) -> ApiError {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        ApiError::wire(Wire::TooLarge)
    } else {
        ApiError::invalid("the multipart body is malformed")
    }
}

fn is_text_plain(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|value| {
        value
            .split(';')
            .next()
            .is_some_and(|media| media.trim().eq_ignore_ascii_case("text/plain"))
    })
}

fn create_temp(dir: &Path) -> Result<(TempFile, tokio::fs::File), ApiError> {
    // The name is random; no part of it comes from the request.
    let path = dir.join(format!("upload-{}.part", uuid::Uuid::new_v4().simple()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .map_err(|_| ApiError::unavailable("the upload area is not writable"))?;
    Ok((TempFile { path }, tokio::fs::File::from_std(file)))
}

async fn stream_bundle(
    field: &mut Field<'_>,
    max: u64,
    dir: &Path,
) -> Result<(TempFile, u64, [u8; 32]), ApiError> {
    let (temp, mut file) = create_temp(dir)?;
    let write_failed = |_| ApiError::unavailable("the upload area is not writable");
    let mut size = 0_u64;
    let mut hash = Sha256::new();
    while let Some(chunk) = field.chunk().await.map_err(|e| multipart_error(&e))? {
        size = size.saturating_add(chunk.len() as u64);
        if size > max {
            // Cut off at once: nothing past the cap is read or written.
            return Err(ApiError::wire(Wire::TooLarge));
        }
        hash.update(&chunk);
        file.write_all(&chunk).await.map_err(write_failed)?;
    }
    file.flush().await.map_err(write_failed)?;
    drop(file);
    if size == 0 {
        return Err(ApiError::invalid("the bundle part is empty"));
    }
    let mut digest = [0_u8; 32];
    digest.copy_from_slice(&hash.finalize());
    Ok((temp, size, digest))
}

async fn read_text(field: &mut Field<'_>, max: usize) -> Result<String, ApiError> {
    let mut buffer: Vec<u8> = Vec::new();
    while let Some(chunk) = field.chunk().await.map_err(|e| multipart_error(&e))? {
        if buffer.len() + chunk.len() > max {
            return Err(ApiError::invalid("a text part is too long"));
        }
        buffer.extend_from_slice(&chunk);
    }
    String::from_utf8(buffer).map_err(|_| ApiError::invalid("a text part is not UTF-8"))
}

/// Reads the multipart body into a temporary file and the two kinds of text part.
pub(crate) async fn read_upload(
    mut multipart: Multipart,
    limits: &Limits,
    temp_dir: &Path,
) -> Result<Upload, ApiError> {
    let mut bundle: Option<(TempFile, u64, [u8; 32])> = None;
    let mut signature: Option<String> = None;
    let mut chain: Vec<String> = Vec::new();
    let mut parts = 0_usize;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| multipart_error(&e))?
    {
        parts += 1;
        if parts > MAX_PARTS {
            return Err(ApiError::invalid("the upload has too many parts"));
        }
        let name = field.name().unwrap_or_default().to_owned();
        let plain = is_text_plain(field.content_type());
        let untyped = field.content_type().is_none();
        match name.as_str() {
            "bundle" => {
                if bundle.is_some() {
                    return Err(ApiError::invalid("the bundle part is repeated"));
                }
                bundle = Some(stream_bundle(&mut field, limits.max_bundle_bytes, temp_dir).await?);
            }
            "signature" => {
                if signature.is_some() {
                    return Err(ApiError::invalid("the signature part is repeated"));
                }
                if !(plain || untyped) {
                    return Err(ApiError::invalid("the signature part must be text"));
                }
                signature = Some(read_text(&mut field, limits.max_text_part_bytes).await?);
            }
            "certificate_chain" => {
                if chain.len() >= MAX_CHAIN {
                    return Err(ApiError::invalid("the chain has more than 8 certificates"));
                }
                if !plain {
                    return Err(ApiError::invalid(
                        "each certificate_chain part must be text/plain",
                    ));
                }
                chain.push(read_text(&mut field, limits.max_text_part_bytes).await?);
            }
            _ => {
                return Err(ApiError::invalid(
                    "the upload has a part the contract does not define",
                ));
            }
        }
    }
    let (temp, size, sha256) =
        bundle.ok_or_else(|| ApiError::invalid("the bundle part is missing"))?;
    let signature = signature.ok_or_else(|| ApiError::invalid("the signature part is missing"))?;
    if chain.is_empty() {
        return Err(ApiError::invalid("the certificate_chain part is missing"));
    }
    Ok(Upload {
        temp,
        size,
        sha256,
        signature,
        chain,
    })
}

/// The answer of a publish.
pub(crate) struct Accepted {
    pub review_id: String,
    pub replayed: bool,
}

fn decode_base64(text: &str) -> Result<Vec<u8>, ApiError> {
    base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .map_err(|_| ApiError::invalid("a part is not standard base64"))
}

fn decode_signature(text: &str) -> Result<BundleSignature, ApiError> {
    BundleSignature::from_json(&decode_base64(text)?)
        .map_err(|_| ApiError::wire(Wire::SignatureInvalid))
}

fn decode_chain(parts: &[String]) -> Result<CertificateChain, ApiError> {
    let mut certificates = Vec::with_capacity(parts.len());
    for part in parts {
        let certificate = SignedCertificate::from_json(&decode_base64(part)?)
            .map_err(|_| ApiError::wire(Wire::SignatureInvalid))?;
        certificates.push(certificate);
    }
    if certificates.len() > 1 {
        return Err(ApiError::invalid(
            "this store accepts a chain of one certificate: the leaf, issued by a trust root",
        ));
    }
    certificates
        .pop()
        .map(CertificateChain::new)
        .ok_or_else(|| ApiError::invalid("the certificate chain is empty"))
}

fn fingerprint(app: &AppName, upload: &Upload) -> Fingerprint {
    let mut hash = Sha256::new();
    for field in [
        app.as_str().to_owned(),
        hex(&upload.sha256),
        upload.signature.clone(),
        upload.chain.join(","),
    ] {
        hash.update((field.len() as u64).to_le_bytes());
        hash.update(field.as_bytes());
    }
    let mut out = [0_u8; 32];
    out.copy_from_slice(&hash.finalize());
    Fingerprint(out)
}

fn open_error(errors: &[BundleError]) -> ApiError {
    let code = errors.first().map_or("bundle.bad_archive", |e| e.code);
    if code == "bundle.too_large" {
        ApiError::wire(Wire::TooLarge)
    } else {
        bundle_error_code(code)
    }
}

fn read_temp(temp: &TempFile, size: u64) -> Result<Vec<u8>, ApiError> {
    let bytes = std::fs::read(&temp.path).map_err(|_| ApiError::internal())?;
    if bytes.len() as u64 != size {
        return Err(ApiError::internal());
    }
    Ok(bytes)
}

/// Publishes `upload` for `who` under `app`, audited whether it succeeds or not. The registry's
/// own sink writes the audit entry of a successful publish; a refusal is audited here.
pub(crate) fn publish_sync(
    core: &Core,
    who: &Principal,
    app: &AppName,
    upload: &Upload,
    idempotency_key: Option<&str>,
) -> Result<Accepted, ApiError> {
    let now = core.clock.now();
    let outcome = publish_inner(core, who, app, upload, idempotency_key, now);
    match &outcome {
        Ok(_) => Metrics::bump(&core.metrics.publishes_accepted),
        Err(error) => {
            Metrics::bump(&core.metrics.publishes_refused);
            core.audit(&AuditEntry {
                kind: AuditKind::Publish,
                actor: who.publisher_id.clone(),
                publisher_id: Some(who.publisher_id.clone()),
                app: Some(app.clone()),
                version: None,
                digest: None,
                reason: clip(&format!("refused: {}", error.code), 512),
                at: now,
            });
        }
    }
    outcome
}

fn publish_inner(
    core: &Core,
    who: &Principal,
    app: &AppName,
    upload: &Upload,
    idempotency_key: Option<&str>,
    now: i64,
) -> Result<Accepted, ApiError> {
    if !core.reviewer.accepting() {
        return Err(ApiError::unavailable(
            "every review worker is stuck; publishing is paused",
        ));
    }
    if core
        .index
        .publisher(&who.publisher_id)?
        .is_some_and(|publisher| publisher.status != PublisherStatus::Active)
    {
        return Err(ApiError::wire(Wire::PublisherRevoked));
    }
    let signature = decode_signature(&upload.signature)?;
    let chain = decode_chain(&upload.chain)?;
    // A key is bound to one publisher: the signature and the certificate must be that
    // publisher's own, or nothing is stored.
    if signature.publisher_id != who.publisher_id
        || chain.leaf.body.publisher_id != who.publisher_id
    {
        return Err(ApiError::wire_with(
            Wire::Forbidden,
            "this key cannot publish for the publisher named in the signature",
        ));
    }
    if let Some(key) = idempotency_key {
        match core.state.idempotency_begin(
            &who.publisher_id,
            key,
            &fingerprint(app, upload),
            now,
        )? {
            Begin::Conflict => {
                return Err(ApiError::invalid(
                    "the idempotency key was used with a different upload",
                ));
            }
            Begin::Replay {
                review_id: Some(review_id),
            } => {
                return Ok(Accepted {
                    review_id,
                    replayed: true,
                });
            }
            // First sight, or a first request that never finished: process it. Everything below
            // is idempotent for the same bytes.
            Begin::New | Begin::Replay { review_id: None } => {}
        }
    }
    let bytes = read_temp(&upload.temp, upload.size)?;
    let limits = &core.policy.limits;
    let bundle = Bundle::open(&bytes, limits, &core.policy.host).map_err(|e| open_error(&e))?;
    if bundle.manifest().app.name.as_str() != app.as_str() {
        return Err(ApiError::wire_with(
            Wire::InvalidManifest,
            "the manifest names a different app than the path",
        ));
    }
    if bundle.manifest_bytes().len() as u64 > core.config.limits.max_manifest_bytes {
        return Err(ApiError::wire(Wire::InvalidManifest));
    }
    drop(bundle);
    let release = block_on(core.registry.publish(bytes, signature, chain, now))
        .map_err(|e| publish_error(&e))?;
    let review_id = register_review(core, who, &release, now)?;
    if let Some(key) = idempotency_key {
        // A second writer that finished the same key first wins; the answer is the same review.
        let _ = core
            .state
            .idempotency_finish(&who.publisher_id, key, &review_id);
    }
    Ok(Accepted {
        review_id,
        replayed: false,
    })
}

/// Starts the listing as `pending_review` and creates the review, or finds the review of a
/// version that was already accepted (a repeat of the same bytes).
fn register_review(
    core: &Core,
    who: &Principal,
    release: &Release,
    now: i64,
) -> Result<String, ApiError> {
    match core.state.set_listing(
        &release.app,
        &release.version,
        ListingState::PendingReview,
        &who.publisher_id,
        now,
    ) {
        Ok(()) => new_review(core, who, release, now),
        Err(error) if error.code == BAD_TRANSITION => {
            let existing = core
                .state
                .reviews_for(&release.app, &release.version)?
                .into_iter()
                .find(|record| {
                    record.publisher_id == who.publisher_id && record.digest == release.digest
                });
            match existing {
                Some(record) => Ok(record.review_id),
                // The listing row exists but its review does not (a crash in between).
                None => new_review(core, who, release, now),
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn new_review(
    core: &Core,
    who: &Principal,
    release: &Release,
    now: i64,
) -> Result<String, ApiError> {
    let review_id = uuid::Uuid::new_v4().simple().to_string();
    core.state.create_review(&ReviewRecord {
        review_id: review_id.clone(),
        publisher_id: who.publisher_id.clone(),
        app: release.app.clone(),
        version: release.version.clone(),
        digest: release.digest,
        status: ReviewStatus::Queued,
        verdict: None,
        summary: None,
        error_code: None,
        created_at: now,
        updated_at: now,
    })?;
    let job = ReviewJob {
        review_id: review_id.clone(),
        digest: release.digest,
    };
    if core.reviewer.enqueue(job).is_err() {
        Metrics::bump(&core.metrics.queue_refusals);
        let failed = ReviewUpdate {
            status: ReviewStatus::Failed,
            verdict: None,
            summary: None,
            error_code: Some(codes::REVIEW_UNAVAILABLE.to_owned()),
            now,
        };
        let _ = core.state.update_review(&review_id, &failed);
    }
    Ok(review_id)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::is_text_plain;

    #[test]
    fn only_text_plain_counts_as_text() {
        assert!(is_text_plain(Some("text/plain")));
        assert!(is_text_plain(Some("Text/Plain; charset=utf-8")));
        assert!(!is_text_plain(Some("application/json")));
        assert!(!is_text_plain(Some("text/plainer")));
        assert!(!is_text_plain(None));
    }
}
