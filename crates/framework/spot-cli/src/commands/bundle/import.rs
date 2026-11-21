//! The site image, the import evaluation and the reports.

use std::collections::BTreeMap;

use spot_bundle_merge::{Conflict, check_against_upgrade, merge3_with};
use spot_bundle_signing::{
    BundleFacts, BundleSignature, CertificateChain, SignedCertificate, VerifyMode, VerifyPolicy,
    verify,
};
use spot_custom_bundle::{
    ApplyPlan, Bundle, CaptureOptions, Change, ChangeAction, FakeSite, SectionRegistry, SiteReader,
    apply, capture, plan_apply,
};
use spot_environments::Role;
use spot_meta::{Customisations, Definition, FieldSpec, Property, Registry};
use spot_migration_ops::plan::PlanInputs;
use spot_migration_plan::{DocTypeDescription, SchemaPlan, SystemDescription};

use super::handover::{
    HandOverHeader, Refusal, Trust, check_direction, decode_handover, open_bundle, parse_digest,
    role_of,
};

// --- the site image ----------------------------------------------------------------------------

/// The installed app versions and the app definitions the image is built over.
pub struct ImageBase {
    pub apps: Vec<(String, String)>,
    pub host: String,
    pub definitions: Vec<Definition>,
}

/// The applied custom fields and property overrides of the site's records.
#[derive(Clone, Debug, Default)]
pub struct AppliedMeta {
    pub fields: Vec<(String, String, Option<String>)>,
    pub properties: Vec<(String, Option<String>, String, String)>,
}

/// Builds the in-memory site a capture or a plan runs against.
///
/// # Errors
/// A message when a version or a stored record does not parse.
pub fn build_image(base: &ImageBase, meta: &AppliedMeta) -> Result<FakeSite, String> {
    let host = semver::Version::parse(&base.host).map_err(|_| "the host version is not semver")?;
    let mut site = FakeSite::new().with_host(host);
    for (app, version) in &base.apps {
        let version = semver::Version::parse(version)
            .map_err(|_| format!("the version of app `{app}` is not semver"))?;
        site = site.with_app(app.clone(), version);
    }
    for definition in &base.definitions {
        site = site.with_definition(definition.clone());
    }
    for (doctype, spec, after) in &meta.fields {
        let spec: FieldSpec = toml::from_str(spec)
            .map_err(|_| format!("a stored custom field of `{doctype}` does not parse"))?;
        site = site.with_custom_field(doctype, spec, after.as_deref());
    }
    for (doctype, field, key, value) in &meta.properties {
        let property = Property::parse(key, value)
            .map_err(|_| format!("a stored property override of `{doctype}` does not parse"))?;
        site = site.with_override(doctype, field.as_deref(), property);
    }
    Ok(site)
}

// --- import ------------------------------------------------------------------------------------

/// What an import needs from the target.
pub struct ImportContext<'a> {
    pub site: &'a str,
    pub kind: Role,
    pub trust: &'a Trust,
    pub image: &'a FakeSite,
    pub now: i64,
    pub created_at: &'a str,
}

/// Everything a validated import yields. Nothing has been written.
pub struct ImportOutcome {
    pub header: HandOverHeader,
    pub bundle: Bundle,
    pub plan: ApplyPlan,
    pub after: FakeSite,
    pub conflicts: Vec<Conflict>,
    pub upgrade_conflicts: Vec<Conflict>,
}

/// Validates a hand-over file for `ctx`'s site and plans it. Writes nothing.
///
/// The order is the order of trust: the file is readable, then signed, then its digest and its
/// signature verify under the target's key policy, then the direction and the target are right,
/// then every script type-checks, and only then is a plan computed.
///
/// # Errors
/// A [`Refusal`] naming the first check that failed.
pub fn evaluate(bytes: &[u8], ctx: &ImportContext<'_>) -> Result<ImportOutcome, Refusal> {
    let registry = SectionRegistry::new();
    let (header, archive) = decode_handover(bytes)?;
    let (Some(signature), Some(certificate)) = (&header.signature, &header.certificate) else {
        return Err(Refusal::new(
            "unsigned",
            "the bundle carries no signature; an unsigned bundle is never imported",
        ));
    };
    let bundle = open_bundle(archive, &registry)?;
    if bundle.digest().to_hex() != header.digest {
        return Err(Refusal::new(
            "digest_mismatch",
            "the header's digest is not the archive's digest (the file was changed)",
        ));
    }
    let signature = BundleSignature::from_json(signature.as_bytes())
        .map_err(|e| Refusal::new("signature_invalid", e.to_string()))?;
    let certificate = SignedCertificate::from_json(certificate.as_bytes())
        .map_err(|e| Refusal::new("signature_invalid", e.to_string()))?;
    let digest = parse_digest(&header.digest)?;
    let identity = header.identity();
    let facts = BundleFacts {
        digest: &digest,
        app_id: &identity,
        version: &header.bundle_version,
    };
    let policy = VerifyPolicy {
        mode: VerifyMode::Install,
        allow_expired_certificate_at_startup: false,
        held_revocation_generation: 0,
    };
    verify(
        &facts,
        &signature,
        &CertificateChain::new(certificate),
        &ctx.trust.anchor,
        &ctx.trust.revocations,
        ctx.now,
        &policy,
    )
    .map_err(|e| Refusal::new("signature_invalid", e.to_string()))?;

    let source = role_of(&header.source_kind)
        .ok_or_else(|| Refusal::new("direction", "the header names an unknown source kind"))?;
    let target = role_of(&header.target_kind)
        .ok_or_else(|| Refusal::new("direction", "the header names an unknown target kind"))?;
    if target != ctx.kind {
        return Err(Refusal::new(
            "wrong_target",
            format!(
                "the bundle is addressed to a {} environment; site `{}` is {}",
                target.name(),
                ctx.site,
                ctx.kind.name()
            ),
        ));
    }
    check_direction(source, ctx.kind)?;

    // Scripts and every other section are checked without a site: a script is re-transpiled.
    let problems = bundle.check(&registry);
    if let Some(problem) = problems.first() {
        return Err(Refusal::new(
            "check_failed",
            format!(
                "`{}` fails its check ({}); {} problem(s) in all",
                problem.item,
                problem.code,
                problems.len()
            ),
        ));
    }

    let failed =
        |what: &str, code: &str| Refusal::new("plan_refused", format!("{what} failed ({code})"));
    let plan = plan_apply(&bundle, ctx.image, &registry)
        .map_err(|errors| {
            let code = errors.first().map_or("unknown", |e| e.code());
            let mut refusal = failed("planning", code);
            if let Some(first) = errors.first() {
                // The item and the sentence name what to fix; neither carries a secret.
                refusal.message = format!("{} [{}: {}]", refusal.message, first.item(), first.message());
            }
            refusal
        })?;
    let mut after = ctx.image.clone();
    apply(&plan, &mut after, &registry).map_err(|e| failed("the in-memory apply", e.code()))?;

    let definitions = ctx.image.definitions();
    let options = |name: &str| {
        CaptureOptions::new(
            name,
            "0.0.0",
            "spot-bundle-import",
            ctx.created_at.to_owned(),
        )
    };
    let ours = capture(ctx.image, &options("target"))
        .map_err(|e| failed("capturing the target", e.code()))?;
    let mut empty = FakeSite::new().with_host(ctx.image.host_version());
    for (app, version) in ctx.image.installed_apps() {
        empty = empty.with_app(app, version);
    }
    for definition in &definitions {
        empty = empty.with_definition(definition.clone());
    }
    let base =
        capture(&empty, &options("base")).map_err(|e| failed("capturing the base", e.code()))?;
    let conflicts = merge3_with(&base, &ours, &bundle, &definitions)
        .map_err(|e| failed("the merge", e.code()))?
        .conflicts;
    let upgrade_conflicts = upgrade_check(&bundle, ctx.image, &definitions)?;
    Ok(ImportOutcome {
        header,
        bundle,
        plan,
        after,
        conflicts,
        upgrade_conflicts,
    })
}

/// Checks the bundle against the installed platform version when it was captured against other
/// app versions. Conservative: the release plan treats the installed metadata as newly added,
/// so a custom item that now collides with a standard one is reported.
pub fn upgrade_check(
    bundle: &Bundle,
    image: &FakeSite,
    definitions: &[Definition],
) -> Result<Vec<Conflict>, Refusal> {
    let installed: BTreeMap<String, String> = image
        .installed_apps()
        .into_iter()
        .map(|(app, version)| (app, version.to_string()))
        .collect();
    let same = bundle
        .header()
        .captured_against
        .iter()
        .all(|a| installed.get(&a.app) == Some(&a.version));
    if same {
        return Ok(Vec::new());
    }
    let new_meta = Registry::build(definitions.to_vec(), &Customisations::new())
        .map_err(|_| Refusal::new("plan_refused", "the installed metadata does not build"))?;
    let release = PlanInputs::new(
        SystemDescription::new(),
        describe(definitions),
        SchemaPlan::default(),
    )
    .release_plan();
    check_against_upgrade(bundle, &release, &new_meta).map_err(|e| {
        Refusal::new(
            "plan_refused",
            format!("the upgrade check failed ({})", e.code()),
        )
    })
}

pub fn describe(definitions: &[Definition]) -> SystemDescription {
    SystemDescription {
        doctypes: definitions
            .iter()
            .map(|definition| DocTypeDescription {
                definition: definition.clone(),
                permissions: Vec::new(),
                views: BTreeMap::new(),
            })
            .collect(),
        ..SystemDescription::new()
    }
}

/// The report an import prints.
#[must_use]
pub fn render_outcome(site: &str, kind: Role, outcome: &ImportOutcome) -> String {
    let mut out = String::new();
    let header = &outcome.header;
    out.push_str(&format!(
        "bundle `{}` {} from {} (site `{}`), digest {}\n  signed by a certificate this {} environment trusts\n",
        header.name,
        header.bundle_version,
        header.source_kind,
        header.source_site,
        header.digest,
        kind.name()
    ));
    out.push_str(&format!(
        "  plan {} for site `{site}`: {} change(s)\n",
        outcome.plan.hash(),
        outcome.plan.changes().len()
    ));
    out.push_str(&render_changes(outcome.plan.changes()));
    for warning in outcome.plan.warnings() {
        out.push_str(&format!(
            "  warning: {} ({})\n",
            warning.message, warning.item
        ));
    }
    out.push_str(&render_conflicts("target", &outcome.conflicts));
    out.push_str(&render_conflicts("platform", &outcome.upgrade_conflicts));
    if kind == Role::Prod {
        out.push_str("  production: this always goes through approval; nothing is applied here\n");
    }
    out
}

pub fn render_changes(changes: &[Change]) -> String {
    let mut out = String::new();
    for change in changes {
        let mark = match change.action {
            ChangeAction::Add => "+",
            ChangeAction::Update => "~",
            ChangeAction::Remove => "-",
        };
        out.push_str(&format!(
            "  {mark} {}/{} ({:?} risk)\n",
            change.section, change.key, change.risk
        ));
    }
    out
}

pub fn render_conflicts(label: &str, conflicts: &[Conflict]) -> String {
    let mut out = String::new();
    for conflict in conflicts {
        out.push_str(&format!(
            "  conflict[{label}] {} {}/{}: {}\n",
            conflict.kind.slug(),
            conflict.section(),
            conflict.key(),
            conflict.explanation
        ));
    }
    out
}
