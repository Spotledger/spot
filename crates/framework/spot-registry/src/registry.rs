use crate::{
    AppName, AuditAction, AuditRecord, AuditSink, BlobStore, Caller, Digest, IndexStore, Publisher,
    PublisherStatus, RegistryError, Release, Removal, Result, SearchPage, SearchQuery,
    SignatureRef, TransferRecord, Version, Visibility, bytes_stream, collect_stream,
};
use spot_bundle::{Bundle, HostInfo, Limits, Policy};
use spot_bundle_signing::{
    BundleFacts, BundleSignature, CertificateChain, RevocationList, SigningError, TrustAnchor,
    Verified, VerifyMode, VerifyPolicy, verify,
};
use std::sync::{Arc, Mutex, MutexGuard};

/// Immutable registry validation policy.
#[derive(Clone, Debug)]
pub struct RegistryPolicy {
    /// Bounded bundle parser.
    pub limits: Limits,
    /// Host used by spot-bundle for requires_host validation.
    pub host: HostInfo,
    /// Capability ceilings enforced by spot-bundle, not duplicated here.
    pub capabilities: Policy,
}
impl Default for RegistryPolicy {
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            host: HostInfo {
                version: Version::new(0, 1, 0),
            },
            capabilities: Policy::default(),
        }
    }
}
struct TrustData {
    anchor: TrustAnchor,
    revocations: RevocationList,
}
/// Shared current trust state. All registry instances in a deployment should share this
/// object; updates serialize with final publish checks, fetch verification and mutations.
pub struct RegistryTrust {
    data: Mutex<TrustData>,
}
impl RegistryTrust {
    /// Validates the signed initial list and its trusted root before storing it.
    pub fn new(anchor: TrustAnchor, revocations: RevocationList, now: i64) -> Result<Self> {
        validate_list(&anchor, &revocations, now)?;
        Ok(Self {
            data: Mutex::new(TrustData {
                anchor,
                revocations,
            }),
        })
    }
    /// Accepts only a strictly newer, correctly signed list. Equal generations are refused
    /// to prevent replacing a generation with different contents. Anchor rotation is a
    /// separate deployment operation; publisher-supplied roots are never installed.
    pub fn update_revocations(&self, list: RevocationList, now: i64) -> Result<()> {
        let mut data = self.lock()?;
        validate_list(&data.anchor, &list, now)?;
        if list.body.generation <= data.revocations.body.generation {
            return Err(SigningError::StaleRevocationList.into());
        }
        data.revocations = list;
        Ok(())
    }
    fn lock(&self) -> Result<MutexGuard<'_, TrustData>> {
        self.data.lock().map_err(|_| crate::error::poisoned())
    }
}
fn validate_list(anchor: &TrustAnchor, list: &RevocationList, now: i64) -> Result<()> {
    let root = anchor
        .root(&list.body.issued_by)
        .ok_or(SigningError::UnknownRoot)?;
    if !root.valid_at(now) {
        return Err(SigningError::RootOutsideWindow.into());
    }
    root.public()
        .verify(&list.body.signed_bytes(), list.signature())
        .map_err(|_| SigningError::RevocationSignatureInvalid)?;
    if list.body.issued_at > now {
        return Err(RegistryError::new(
            "registry.invalid_trust",
            "revocation list issued in future",
        ));
    }
    Ok(())
}
fn check(
    data: &TrustData,
    facts: &BundleFacts<'_>,
    proof: &SignatureRef,
    now: i64,
) -> Result<Verified> {
    let verified = verify(
        facts,
        &proof.signature,
        &proof.chain,
        &data.anchor,
        &data.revocations,
        now,
        &VerifyPolicy {
            mode: VerifyMode::Install,
            allow_expired_certificate_at_startup: false,
            held_revocation_generation: data.revocations.body.generation,
        },
    )?;
    if proof.signature.signed_at > now {
        return Err(RegistryError::new(
            "registry.signature_in_future",
            "signature time is in future",
        ));
    }
    // Signing's verifier preserves historical signatures on ordinary key retirement.
    // Registry distribution is stricter: currently revoked credentials stop fetch/publish
    // even when an old signing time precedes effective_from. Compromise remains universal.
    if data
        .revocations
        .body
        .publishers
        .iter()
        .any(|p| p.publisher_id == verified.publisher_id && p.effective_from <= now)
    {
        return Err(SigningError::PublisherRevoked.into());
    }
    if data
        .revocations
        .body
        .keys
        .iter()
        .any(|k| k.key_id == verified.key_id && (k.compromised || k.effective_from <= now))
    {
        return Err(SigningError::KeyRevoked.into());
    }
    Ok(verified)
}
fn publisher(proof: &SignatureRef) -> Publisher {
    let body = &proof.chain.leaf.body;
    Publisher {
        id: body.publisher_id.clone(),
        display_name: body.display_name.clone(),
        certificate_chain_root: body.issued_by.clone(),
        status: PublisherStatus::Active,
    }
}
fn facts<'a>(digest: &'a Digest, app: &'a str, version: &'a str) -> BundleFacts<'a> {
    BundleFacts {
        digest,
        app_id: app,
        version,
    }
}
/// Registry rules over injectable storage. Blob writes precede index commit. Preflight is
/// repeated in the atomic index commit; concurrency losers leave repairable orphan bytes.
/// The index backend and audit sink are trusted deployment components, not client inputs.
pub struct Registry<I: IndexStore, B: BlobStore> {
    index: Arc<I>,
    blobs: Arc<B>,
    policy: RegistryPolicy,
    trust: Arc<RegistryTrust>,
    audit: Arc<dyn AuditSink>,
}
impl<I: IndexStore, B: BlobStore> Registry<I, B> {
    /// Wires a registry; use matching store/registry bundle host and limits.
    pub fn new(
        index: Arc<I>,
        blobs: Arc<B>,
        policy: RegistryPolicy,
        trust: Arc<RegistryTrust>,
        audit: Arc<dyn AuditSink>,
    ) -> Self {
        Self {
            index,
            blobs,
            policy,
            trust,
            audit,
        }
    }
    fn open(&self, bytes: &[u8]) -> Result<Bundle> {
        if bytes.len() as u64 > self.policy.limits.max_compressed_bytes {
            return Err(RegistryError::new(
                "registry.too_large",
                "bundle exceeds compressed size policy",
            ));
        }
        Bundle::open(bytes, &self.policy.limits, &self.policy.host)
            .map_err(crate::error::bundle_error)
    }
    fn privileges(&self, bundle: &Bundle) -> Result<()> {
        let report = bundle.check_privileges(&self.policy.capabilities);
        if report.is_ok() {
            Ok(())
        } else {
            Err(crate::error::bundle_error(report.errors))
        }
    }
    /// Publish in fixed refusal order: size, bundle/manifest, trust, ownership/status,
    /// version, parsed requirements (already established by bundle), privileges, blob,
    /// atomic index commit. Every republish passes trust/privileges, even idempotent ones.
    pub async fn publish(
        &self,
        bytes: Vec<u8>,
        signature: BundleSignature,
        chain: CertificateChain,
        now: i64,
    ) -> Result<Release> {
        let bundle = self.open(&bytes)?;
        let digest = bundle.digest();
        let manifest = bundle.manifest();
        let version = manifest.app.version.to_string();
        let proof = SignatureRef { signature, chain };
        let verified = {
            let trust = self.trust.lock()?;
            check(
                &trust,
                &facts(&digest, manifest.app.name.as_str(), &version),
                &proof,
                now,
            )?
        };
        let mut release = Release {
            app: AppName::parse(manifest.app.name.as_str())?,
            version: manifest.app.version.clone(),
            digest,
            size: bytes.len() as u64,
            requires_host: manifest.app.requires_host.clone(),
            depends: manifest.app.depends.clone(),
            published_at: now,
            publisher: verified.publisher_id,
            yanked: None,
            withdrawn: None,
            signature_ref: proof,
            title: manifest.app.title.clone(),
            description: String::new(),
            category: String::new(),
            capabilities: manifest.capabilities.clone(),
        };
        self.index.preflight(&release)?;
        self.privileges(&bundle)?;
        self.blobs
            .put_if_absent(digest, bytes_stream(bytes))
            .await?;
        release.size = self.blobs.size(digest).await?;
        // No await between final trust verification, atomic index commit and audit.
        let trust = self.trust.lock()?;
        check(
            &trust,
            &facts(&digest, release.app.as_str(), &version),
            &release.signature_ref,
            now,
        )?;
        let outcome = self
            .index
            .commit(release.clone(), publisher(&release.signature_ref))?;
        check(
            &trust,
            &facts(
                &outcome.release.digest,
                outcome.release.app.as_str(),
                &outcome.release.version.to_string(),
            ),
            &outcome.release.signature_ref,
            now,
        )?;
        if outcome.inserted {
            self.audit.record(audit_release(
                AuditAction::Publish,
                &outcome.release,
                now,
                String::new(),
                &outcome.release.publisher,
            ));
        }
        Ok(outcome.release)
    }
    /// Authenticates a separate private signature over a bundle digest, using
    /// AUTHORIZATION_APP_ID and AUTHORIZATION_VERSION, signed at the current time.
    /// Public release signatures never grant publisher authority. This short-lived
    /// bearer proof must stay private in the caller/transport; every use rechecks expiry,
    /// current trust and status. It is not a network challenge/session protocol.
    pub fn authenticate(
        &self,
        bytes: &[u8],
        signature: BundleSignature,
        chain: CertificateChain,
        now: i64,
    ) -> Result<Caller> {
        let bundle = self.open(bytes)?;
        let digest = bundle.digest();
        let app = crate::AUTHORIZATION_APP_ID.to_owned();
        let version = crate::AUTHORIZATION_VERSION.to_owned();
        let proof = SignatureRef { signature, chain };
        let trust = self.trust.lock()?;
        let verified = check(&trust, &facts(&digest, &app, &version), &proof, now)?;
        authorization_window(verified.signed_at, now)?;
        Ok(Caller {
            publisher: verified.publisher_id,
            proof,
            facts: (digest, app, version),
        })
    }
    fn caller(&self, trust: &TrustData, caller: &Caller, now: i64) -> Result<()> {
        authorization_window(caller.proof.signature.signed_at, now)?;
        let (digest, app, version) = &caller.facts;
        let verified = check(trust, &facts(digest, app, version), &caller.proof, now)?;
        if verified.publisher_id != caller.publisher {
            return Err(RegistryError::new(
                "registry.unauthorized",
                "caller identity differs",
            ));
        }
        if self
            .index
            .publisher(&caller.publisher)?
            .is_some_and(|p| p.status != PublisherStatus::Active)
        {
            return Err(RegistryError::new(
                "registry.publisher_inactive",
                "publisher suspended",
            ));
        }
        Ok(())
    }
    /// Latest available version; excludes prereleases unless requested.
    pub fn latest(&self, app: &AppName, include_pre: bool) -> Result<Option<Release>> {
        self.index.latest(app, include_pre)
    }
    /// Version listing, with current authorization for inactive rows.
    pub fn releases(
        &self,
        app: &AppName,
        visibility: &Visibility,
        caller: Option<&Caller>,
        now: i64,
    ) -> Result<Vec<Release>> {
        let trust = self.trust.lock()?;
        if visibility.include_inactive {
            self.caller(&trust, caller.ok_or_else(unauthorized)?, now)?;
        }
        self.index.releases(app, visibility, caller)
    }
    /// Ranked search with query-bound pagination and current inactive-access authorization.
    pub fn search(&self, query: &SearchQuery, now: i64) -> Result<SearchPage> {
        let trust = self.trust.lock()?;
        if query.visibility.include_inactive {
            self.caller(&trust, query.caller.as_ref().ok_or_else(unauthorized)?, now)?;
        }
        self.index.search(query)
    }
    /// Adds owner-curated description/category (not asserted as signed manifest facts).
    pub fn describe(
        &self,
        app: &AppName,
        description: String,
        category: String,
        by: &Caller,
        now: i64,
    ) -> Result<()> {
        let trust = self.trust.lock()?;
        self.caller(&trust, by, now)?;
        self.index
            .describe(app, &by.publisher, description, category)
    }
    /// Owner-authorized yank; bytes and signatures remain fetchable by digest.
    pub fn yank(
        &self,
        app: &AppName,
        version: &Version,
        reason: String,
        by: &Caller,
        now: i64,
    ) -> Result<()> {
        self.remove(app, version, reason, by, now, false)
    }
    /// Owner-authorized security withdrawal; all fetch paths refuse the release.
    pub fn withdraw(
        &self,
        app: &AppName,
        version: &Version,
        reason: String,
        by: &Caller,
        now: i64,
    ) -> Result<()> {
        self.remove(app, version, reason, by, now, true)
    }
    fn remove(
        &self,
        app: &AppName,
        version: &Version,
        reason: String,
        by: &Caller,
        now: i64,
        withdraw: bool,
    ) -> Result<()> {
        if reason.trim().is_empty() {
            return Err(RegistryError::new(
                "registry.reason_required",
                "removal needs a reason",
            ));
        }
        let trust = self.trust.lock()?;
        self.caller(&trust, by, now)?;
        if let Some(release) = self.index.remove(
            app,
            version,
            &by.publisher,
            Removal {
                at: now,
                reason: reason.clone(),
            },
            withdraw,
        )? {
            self.audit.record(audit_release(
                if withdraw {
                    AuditAction::Withdraw
                } else {
                    AuditAction::Yank
                },
                &release,
                now,
                reason,
                &by.publisher,
            ));
        }
        Ok(())
    }
    /// Transfers exclusive ownership with two current trusted signatures over exactly the
    /// request commitment. Generation/time/expiry/reason are all signed; replay is refused.
    pub fn transfer(&self, transfer: TransferRecord, now: i64) -> Result<()> {
        let request = &transfer.request;
        if request.at > now
            || now >= request.expires_at
            || request.at >= request.expires_at
            || request.reason.trim().is_empty()
        {
            return Err(RegistryError::new(
                "registry.transfer_invalid",
                "invalid transfer window or reason",
            ));
        }
        let digest = request.digest()?;
        let f = facts(&digest, crate::TRANSFER_APP_ID, crate::TRANSFER_VERSION);
        let trust = self.trust.lock()?;
        let from = check(&trust, &f, &transfer.from, now)?;
        let to = check(&trust, &f, &transfer.to, now)?;
        if from.publisher_id != request.from
            || to.publisher_id != request.to
            || from.signed_at != request.at
            || to.signed_at != request.at
        {
            return Err(RegistryError::new(
                "registry.transfer_invalid",
                "transfer signatures name different parties or times",
            ));
        }
        let audit = AuditRecord {
            action: AuditAction::Transfer,
            who: request.from.clone(),
            app: request.app.clone(),
            version: None,
            digest,
            at: now,
            reason: format!("to {}: {}", request.to, request.reason),
        };
        let recipient = publisher(&transfer.to);
        self.index.transfer(transfer, recipient)?;
        self.audit.record(audit);
        Ok(())
    }
    /// Reports complete blob addresses without any row; never silently imports unsigned
    /// releases or deletes bytes. Safe during concurrent publication (snapshot report only).
    pub async fn repair_orphans(&self) -> Result<Vec<Digest>> {
        let blobs = self.blobs.inventory().await?;
        let indexed = self.index.digests()?;
        Ok(blobs.into_iter().filter(|d| !indexed.contains(d)).collect())
    }
    /// Verified fetch for a new install. Yanked and withdrawn releases are unavailable.
    pub async fn fetch(
        &self,
        app: &AppName,
        version: &Version,
        now: i64,
    ) -> Result<crate::VerifiedBundle> {
        let r = self.index.release(app, version)?.ok_or_else(missing)?;
        self.fetch_release(r, now, false).await
    }
    /// Verified digest fetch for an existing site. Allows yank, refuses withdrawal and
    /// current revocation before returning any bytes.
    pub async fn fetch_digest(&self, digest: Digest, now: i64) -> Result<crate::VerifiedBundle> {
        let r = self.index.by_digest(digest)?.ok_or_else(missing)?;
        self.fetch_release(r, now, true).await
    }
    async fn fetch_release(
        &self,
        release: Release,
        now: i64,
        allow_yanked: bool,
    ) -> Result<crate::VerifiedBundle> {
        available(&release, allow_yanked)?;
        let bytes = collect_stream(
            self.blobs.get(release.digest).await?,
            self.policy.limits.max_compressed_bytes,
        )
        .await?;
        let bundle = self.open(&bytes).map_err(|_| {
            RegistryError::new(
                "registry.digest_mismatch",
                "transport returned invalid bundle bytes",
            )
        })?;
        if bundle.digest() != release.digest {
            return Err(RegistryError::new(
                "registry.digest_mismatch",
                "transport digest differs",
            ));
        }
        let manifest = bundle.manifest();
        // Every installer-facing signed fact must be derived from the actual bundle.
        if manifest.app.name.as_str() != release.app.as_str()
            || manifest.app.version != release.version
            || manifest.app.title != release.title
            || manifest.app.requires_host != release.requires_host
            || manifest.app.depends != release.depends
            || manifest.capabilities != release.capabilities
            || bytes.len() as u64 != release.size
        {
            return Err(RegistryError::new(
                "registry.metadata_mismatch",
                "indexed identity or manifest metadata differs from signed bundle",
            ));
        }
        self.privileges(&bundle)?;
        let trust = self.trust.lock()?;
        let version = manifest.app.version.to_string();
        let verified = check(
            &trust,
            &facts(&release.digest, manifest.app.name.as_str(), &version),
            &release.signature_ref,
            now,
        )?;
        if verified.publisher_id != release.publisher {
            return Err(RegistryError::new(
                "registry.metadata_mismatch",
                "indexed publisher differs from certificate",
            ));
        }
        // Re-read security state after I/O, preventing stale pre-I/O withdrawal decisions.
        let current = self
            .index
            .release(&release.app, &release.version)?
            .ok_or_else(missing)?;
        if current.digest != release.digest || current.signature_ref != release.signature_ref {
            return Err(RegistryError::new(
                "registry.metadata_mismatch",
                "release changed during fetch",
            ));
        }
        available(&current, allow_yanked)?;
        Ok(crate::VerifiedBundle {
            bytes,
            release: current,
            verified,
        })
    }
}
fn available(r: &Release, allow_yanked: bool) -> Result<()> {
    if r.withdrawn.is_some() {
        return Err(RegistryError::new(
            "registry.withdrawn",
            "release withdrawn for security",
        ));
    }
    if r.yanked.is_some() && !allow_yanked {
        return Err(RegistryError::new(
            "registry.yanked",
            "release unavailable for new installs",
        ));
    }
    Ok(())
}
fn audit_release(
    action: AuditAction,
    r: &Release,
    now: i64,
    reason: String,
    who: &str,
) -> AuditRecord {
    AuditRecord {
        action,
        who: who.to_owned(),
        app: r.app.clone(),
        version: Some(r.version.clone()),
        digest: r.digest,
        at: now,
        reason,
    }
}
fn unauthorized() -> RegistryError {
    RegistryError::new(
        "registry.unauthorized",
        "inactive access requires authenticated caller",
    )
}
fn authorization_window(signed_at: i64, now: i64) -> Result<()> {
    if now < signed_at || now.saturating_sub(signed_at) >= crate::AUTHORIZATION_LIFETIME_SECONDS {
        return Err(RegistryError::new(
            "registry.unauthorized",
            "publisher authorization is expired or not yet valid",
        ));
    }
    Ok(())
}
fn missing() -> RegistryError {
    RegistryError::new("registry.not_found", "release absent")
}
