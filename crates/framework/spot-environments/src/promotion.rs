//! Promotion records and the import a receiving environment runs.
//!
//! A [`PromotionRecord`] is appended to a [`PromotionChain`] after every successful import. The
//! chain links each record to the one before it with SHA-256, the same tamper-evidence shape the
//! audit chain uses, so cutting a record out or editing one is detectable. [`verify_chain`]
//! proves the acceptance property: one bundle digest traversed dev -> test -> prod with an
//! identical captured result at every hop.

use spot_custom_bundle::{
    Bundle, CaptureOptions, Limits, SectionRegistry, SiteReader, SiteWriter, apply, capture_with,
    digest, plan_apply,
};

use crate::error::{EnvironmentError, Result, codes};
use crate::model::{Environment, EnvironmentSet, Role};
use crate::transport::{EnvRef, TransportPackage, Trust};
use crate::variables::Variables;

/// A site the promotion can read and write, such as `spot_custom_bundle::FakeSite`.
pub trait Site: SiteReader + SiteWriter {
    /// The site as the reader capture and planning take.
    fn as_reader(&self) -> &dyn SiteReader;
    /// The site as the writer apply takes.
    fn as_writer(&mut self) -> &mut dyn SiteWriter;
}

impl<T: SiteReader + SiteWriter> Site for T {
    fn as_reader(&self) -> &dyn SiteReader {
        self
    }

    fn as_writer(&mut self) -> &mut dyn SiteWriter {
        self
    }
}

/// The previous-hash of the first record in a chain.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// The domain prefix of a promotion record's hash.
const HASH_PREFIX: &[u8] = b"spotledger-environment-promotion-v1\0";

/// One successful promotion, as appended to the chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromotionRecord {
    /// The position in the chain, starting at 1.
    pub seq: u64,
    /// The previous record's hash, or [`GENESIS`].
    pub prev: String,
    /// This record's hash over every field below.
    pub hash: String,
    /// The source bundle's canonical digest, lower-case hex.
    pub bundle_digest: String,
    /// The target capture's digest after variables are folded back, lower-case hex.
    pub result_digest: String,
    /// The apply plan's deterministic hash.
    pub plan_hash: String,
    /// The environment the bundle came from.
    pub source: EnvRef,
    /// The environment it was applied to.
    pub target: EnvRef,
    /// Who ran the import.
    pub actor: String,
    /// Unix milliseconds, supplied by the caller.
    pub at: u64,
}

impl PromotionRecord {
    /// A record before it is appended; [`PromotionChain::append`] assigns `seq`, `prev` and
    /// `hash`.
    ///
    /// # Errors
    /// [`codes::CHAIN_BROKEN`] for a digest that is not 64 hex characters, an empty plan hash
    /// or an empty actor.
    pub fn new(
        source: EnvRef,
        target: EnvRef,
        bundle_digest: impl Into<String>,
        result_digest: impl Into<String>,
        plan_hash: impl Into<String>,
        actor: impl Into<String>,
        at: u64,
    ) -> Result<Self> {
        let record = Self {
            seq: 0,
            prev: String::new(),
            hash: String::new(),
            bundle_digest: bundle_digest.into(),
            result_digest: result_digest.into(),
            plan_hash: plan_hash.into(),
            source,
            target,
            actor: actor.into(),
            at,
        };
        record.check_fields()?;
        Ok(record)
    }

    /// Checks the fields that do not depend on the chain position.
    fn check_fields(&self) -> Result<()> {
        if !is_hex64(&self.bundle_digest) {
            return Err(EnvironmentError::about(
                codes::CHAIN_BROKEN,
                &self.bundle_digest,
                "a bundle digest is 64 hexadecimal characters",
            ));
        }
        if !is_hex64(&self.result_digest) {
            return Err(EnvironmentError::about(
                codes::CHAIN_BROKEN,
                &self.result_digest,
                "a result digest is 64 hexadecimal characters",
            ));
        }
        if self.plan_hash.is_empty() {
            return Err(EnvironmentError::new(
                codes::CHAIN_BROKEN,
                "a promotion record names its plan hash",
            ));
        }
        if self.actor.is_empty() {
            return Err(EnvironmentError::new(
                codes::CHAIN_BROKEN,
                "a promotion record names the actor",
            ));
        }
        Ok(())
    }

    /// The hash of this record's fields for `seq`, `prev` and every content field.
    #[must_use]
    pub fn calculate_hash(&self, seq: u64, prev: &str) -> String {
        let mut input = Vec::new();
        input.extend_from_slice(HASH_PREFIX);
        input.extend_from_slice(&seq.to_be_bytes());
        for part in [
            prev,
            &self.bundle_digest,
            &self.result_digest,
            &self.plan_hash,
            &self.source.set,
            self.source.role.name(),
            &self.source.tenant.to_string(),
            &self.target.set,
            self.target.role.name(),
            &self.target.tenant.to_string(),
            &self.actor,
        ] {
            input.extend_from_slice(part.as_bytes());
            input.push(0);
        }
        input.extend_from_slice(&self.at.to_be_bytes());
        hex(&digest::sha256(&input))
    }
}

/// An append-only chain of promotion records for one promotion run.
#[derive(Clone, Debug, Default)]
pub struct PromotionChain {
    records: Vec<PromotionRecord>,
}

impl PromotionChain {
    /// An empty chain.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a record, assigning its sequence number, previous hash and own hash.
    ///
    /// # Errors
    /// [`codes::CHAIN_BROKEN`] when the record's fields do not hold.
    pub fn append(&mut self, mut record: PromotionRecord) -> Result<PromotionRecord> {
        record.check_fields()?;
        let seq = self.records.len() as u64 + 1;
        let prev = self
            .records
            .last()
            .map_or_else(|| GENESIS.to_owned(), |last| last.hash.clone());
        record.seq = seq;
        record.prev = prev.clone();
        record.hash = record.calculate_hash(seq, &prev);
        self.records.push(record.clone());
        Ok(record)
    }

    /// The records, oldest first.
    #[must_use]
    pub fn records(&self) -> &[PromotionRecord] {
        &self.records
    }

    /// The chain proof, or why the chain does not hold.
    ///
    /// # Errors
    /// [`codes::CHAIN_BROKEN`] as [`verify_chain`] documents.
    pub fn verify(&self) -> Result<ChainProof> {
        verify_chain(&self.records)
    }
}

/// What a verified chain proves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainProof {
    /// The one digest every hop carried.
    pub digest: String,
    /// The environments the digest traversed, in order.
    pub environments: Vec<EnvRef>,
}

/// Verifies that the records form one unbroken chain and that one digest traversed dev, test and
/// prod with an identical result at every hop.
///
/// # Errors
/// [`codes::CHAIN_BROKEN`] for a bad sequence, link or hash; a mixed bundle digest; a result
/// digest that differs from the bundle digest; a broken source/target hand-off; or a ladder that
/// does not start at dev, end at prod and advance one role at a time.
pub fn verify_chain(records: &[PromotionRecord]) -> Result<ChainProof> {
    let first = records.first().ok_or_else(|| {
        EnvironmentError::new(
            codes::CHAIN_BROKEN,
            "a promotion chain has at least one record",
        )
    })?;
    let digest = first.bundle_digest.clone();
    let mut environments = vec![first.source.clone()];
    let mut previous_hash = GENESIS.to_owned();
    let mut previous: Option<&PromotionRecord> = None;
    for (index, record) in records.iter().enumerate() {
        record.check_fields()?;
        let expected_seq = index as u64 + 1;
        if record.seq != expected_seq {
            return Err(EnvironmentError::about(
                codes::CHAIN_BROKEN,
                record.seq.to_string(),
                "a promotion record's sequence number does not follow the previous one",
            ));
        }
        if record.prev != previous_hash {
            return Err(EnvironmentError::about(
                codes::CHAIN_BROKEN,
                record.seq.to_string(),
                "a promotion record does not link to the one before it",
            ));
        }
        if record.hash != record.calculate_hash(record.seq, &record.prev) {
            return Err(EnvironmentError::about(
                codes::CHAIN_BROKEN,
                record.seq.to_string(),
                "a promotion record's hash does not cover its contents",
            ));
        }
        if record.bundle_digest != digest {
            return Err(EnvironmentError::about(
                codes::CHAIN_BROKEN,
                record.seq.to_string(),
                "the chain carries more than one bundle digest",
            ));
        }
        if record.result_digest != record.bundle_digest {
            return Err(EnvironmentError::about(
                codes::RESULT_MISMATCH,
                record.seq.to_string(),
                "the applied result does not capture back to the source digest",
            ));
        }
        if record.target.role.rank() != record.source.role.rank() + 1 {
            return Err(EnvironmentError::about(
                codes::CHAIN_BROKEN,
                record.target.role.name(),
                "a promotion advances exactly one role",
            ));
        }
        if let Some(previous) = previous
            && record.source != previous.target
        {
            return Err(EnvironmentError::about(
                codes::CHAIN_BROKEN,
                record.seq.to_string(),
                "a promotion starts somewhere other than where the previous one ended",
            ));
        }
        environments.push(record.target.clone());
        previous_hash = record.hash.clone();
        previous = Some(record);
    }
    if environments.first().map(|env| env.role) != Some(Role::Dev)
        || environments.last().map(|env| env.role) != Some(Role::Prod)
    {
        return Err(EnvironmentError::new(
            codes::CHAIN_BROKEN,
            "the chain runs from dev to prod",
        ));
    }
    Ok(ChainProof {
        digest,
        environments,
    })
}

/// What an import did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Imported {
    /// The appended record.
    pub record: PromotionRecord,
    /// The plan's hash.
    pub plan_hash: String,
    /// The target capture's digest after folding variables back.
    pub result_digest: String,
    /// How many changes the plan applied.
    pub applied: usize,
}

/// Everything an import needs besides the package and the site.
pub struct Context<'a> {
    /// The set the source and target belong to.
    pub set: &'a EnvironmentSet,
    /// The receiving environment.
    pub target: &'a Environment,
    /// The section registry the bundle is read and checked with.
    pub registry: &'a SectionRegistry,
    /// The bundle limits.
    pub limits: &'a Limits,
    /// The roots, revocations and time the receiver trusts.
    pub trust: Trust<'a>,
    /// The target's variable values.
    pub variables: &'a Variables,
    /// Who runs the import.
    pub actor: &'a str,
    /// Unix milliseconds, supplied by the caller.
    pub at: u64,
}

/// Verifies a package, resolves its variables, plans, applies, captures the target back and
/// appends the promotion record.
///
/// # Errors
/// The verification errors of [`TransportPackage::verify`], [`codes::UNRESOLVED_VARIABLE`]
/// before planning, [`codes::PLAN_FAILED`], [`codes::APPLY_FAILED`],
/// [`codes::CAPTURE_FAILED`], and [`codes::RESULT_MISMATCH`] when the applied target does not
/// capture back to the source digest. Nothing is applied when verification or variable
/// validation fails.
pub fn import(
    context: &Context<'_>,
    package: &TransportPackage,
    chain: &mut PromotionChain,
    site: &mut dyn Site,
) -> Result<Imported> {
    let verified = package.verify(
        context.set,
        context.target,
        &context.trust,
        context.registry,
        context.limits,
    )?;
    let source_digest = verified.bundle().digest().to_hex();

    // Values are checked before planning, so an unbound placeholder never reaches a plan.
    context.variables.validate(verified.bundle())?;
    let resolved = context
        .variables
        .resolve(verified.bundle(), context.limits)?;

    let plan = plan_apply(&resolved, site.as_reader(), context.registry).map_err(|errors| {
        let detail = errors
            .first()
            .map_or("custom_bundle.apply_failed", |err| err.code());
        EnvironmentError::about(
            codes::PLAN_FAILED,
            detail,
            "the bundle was refused against the target site",
        )
    })?;
    let plan_hash = plan.hash().to_owned();
    let report = apply(&plan, site.as_writer(), context.registry).map_err(|err| {
        EnvironmentError::about(
            codes::APPLY_FAILED,
            err.code(),
            "the plan could not be written to the target site",
        )
    })?;

    let header = verified.bundle().header();
    let options = CaptureOptions::new(
        header.name.clone(),
        header.bundle_version.clone(),
        header.created_by.clone(),
        header.created_at.clone(),
    );
    let captured = capture_with(site.as_reader(), &options, context.registry, context.limits)
        .map_err(|err| {
            EnvironmentError::about(
                codes::CAPTURE_FAILED,
                err.code(),
                "the target site could not be captured after the apply",
            )
        })?;

    // Fold this environment's values back to placeholders and pack under the *source* header,
    // so the comparison is about content: the applied result must be the source template with
    // this environment's values filled in.
    let sections = context.variables.normalise_sections(captured.sections())?;
    let result = Bundle::pack(header.clone(), sections, context.limits).map_err(|err| {
        EnvironmentError::about(
            codes::CAPTURE_FAILED,
            err.code(),
            "the folded target capture could not be packed",
        )
    })?;
    let result_digest = result.digest().to_hex();
    if result_digest != source_digest {
        return Err(EnvironmentError::about(
            codes::RESULT_MISMATCH,
            context.target.tenant.to_string(),
            "the applied target does not capture back to the source digest",
        ));
    }

    let record = PromotionRecord::new(
        EnvRef::new(context.set, verified.source()),
        EnvRef::new(context.set, context.target),
        source_digest,
        result_digest.clone(),
        plan_hash.clone(),
        context.actor,
        context.at,
    )?;
    let record = chain.append(record)?;
    Ok(Imported {
        record,
        plan_hash,
        result_digest,
        applied: report.applied().len(),
    })
}

/// Whether `text` is exactly 64 lower-case hexadecimal characters.
fn is_hex64(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Lower-case hex of a SHA-256 result.
fn hex(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
