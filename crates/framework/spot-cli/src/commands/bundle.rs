//! `spot bundle export|diff|import` and `spot bundle env link|list|unlink` (WP-ALM-02.T3): the
//! customisation bundle and the environment ladder as operations a user of the product can run.
//!
//! The libraries (`spot-custom-bundle`, `spot-bundle-signing`, `spot-bundle-merge`,
//! `spot-environments`) already capture, sign, merge and plan; this is the fail-closed host.
//!
//! * The **hand-over file** is one JSON header line (`spot-bundle-handover`) then the raw archive.
//!   The detached Ed25519 signature is bound to the two environment kinds and the bundle name, so
//!   a header edited to point elsewhere no longer verifies.
//! * A site's **environment** (`dev`, `test`, `prod`) and key policy live in
//!   `sites/<site>/environment.toml`. Transport goes one rung up only; production never goes down.
//! * **Import changes nothing.** It checks signature, digest, direction and target, type-checks
//!   scripts, computes the apply plan, the conflicts with the target's own customisations (three
//!   way, empty base) and with the installed platform (`check_against_upgrade`). With `--apply` it
//!   creates the migration plan through `spot_migration_ops::plan::plan`, keeps the file under the
//!   plan's hash and stops for approval.
//!
//! The site image is the installed apps' definitions plus the applied custom field and property
//! records; views, scripts, rules and fixtures have no live store yet, so a live export carries
//! `meta` only. Output names counts and digests, never a key or a value. Exit codes: 0 done,
//! 1 usage, 2 refused, 3 failed.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Arg, ArgAction, ArgMatches, Command};
use spot_bundle_merge::merge3_with;
use spot_bundle_signing::{RevocationList, SignedCertificate, generate_keypair};
use spot_custom_bundle::{Bundle, CaptureOptions, FakeSite, SectionRegistry, capture, diff};
use spot_environments::Role;
use spot_meta::{Customisations, Definition, Registry};
use spot_migration_ops::plan::{PlanInputs, plan};
use spot_migration_plan::{SchemaPlan, SystemDescription};
use spot_postgres::PgBackend;

use super::migrate_plan::{
    Opened, as_arg, build_lifecycle, exit_of, on_plain_thread, principal_of, role_arg, site_arg,
};
use super::{CommandCtx, Exit};

/// The subcommand name.
pub const NAME: &str = "bundle";

/// Arguments and help.
pub fn command() -> Command {
    Command::new(NAME)
        .about(
            "Export, diff and import signed customisation bundles; link a site to an environment",
        )
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("export")
                .about("Write a signed hand-over file of the site's customisations")
                .arg(site_arg())
                .arg(
                    Arg::new("out")
                        .long("out")
                        .value_name("FILE")
                        .required(true)
                        .help("The hand-over file to create (never overwrites)"),
                )
                .arg(
                    Arg::new("signing_key")
                        .long("signing-key")
                        .value_name("FILE")
                        .required(true)
                        .help("A file holding the Ed25519 seed as 64 hex characters"),
                )
                .arg(
                    Arg::new("certificate")
                        .long("certificate")
                        .value_name("FILE")
                        .required(true)
                        .help("The signer's publisher certificate (JSON)"),
                )
                .arg(
                    Arg::new("to")
                        .long("to")
                        .value_name("KIND")
                        .help("The target kind (default: the next rung after the site's kind)"),
                )
                .arg(
                    Arg::new("name")
                        .long("name")
                        .value_name("NAME")
                        .help("The bundle name (default: the site name)"),
                )
                .arg(
                    Arg::new("bundle_version")
                        .long("bundle-version")
                        .value_name("SEMVER")
                        .default_value("1.0.0")
                        .help("The bundle's own version"),
                )
                .arg(as_arg())
                .arg(role_arg()),
        )
        .subcommand(
            Command::new("diff")
                .about("Show additions, changes, removals and conflicts between two bundles")
                .arg(Arg::new("a").value_name("A").required(true))
                .arg(Arg::new("b").value_name("B").required(true))
                .arg(
                    Arg::new("base")
                        .long("base")
                        .value_name("FILE")
                        .help("A common ancestor: conflicts are computed three-way against it"),
                ),
        )
        .subcommand(
            Command::new("import")
                .about("Validate a hand-over file against the site and plan it (changes nothing)")
                .arg(Arg::new("file").value_name("FILE").required(true))
                .arg(site_arg())
                .arg(
                    Arg::new("apply")
                        .long("apply")
                        .action(ArgAction::SetTrue)
                        .help("Create the migration plan and stop for approval"),
                )
                .arg(as_arg())
                .arg(role_arg()),
        )
        .subcommand(
            Command::new("env")
                .about("Record, list and remove a site's environment kind")
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(
                    Command::new("link")
                        .about("Record the site's kind (dev, test, prod) and its key policy")
                        .arg(site_arg())
                        .arg(
                            Arg::new("kind")
                                .long("kind")
                                .value_name("KIND")
                                .required(true)
                                .help("dev, test or prod"),
                        )
                        .arg(
                            Arg::new("trust_anchor")
                                .long("trust-anchor")
                                .value_name("FILE")
                                .help("The trust anchor JSON (roots that may sign bundles for this site)"),
                        )
                        .arg(
                            Arg::new("revocations")
                                .long("revocations")
                                .value_name("FILE")
                                .help("The signed revocation list JSON"),
                        ),
                )
                .subcommand(Command::new("list").about("List every site and its kind"))
                .subcommand(
                    Command::new("unlink")
                        .about("Remove the site's environment record and key policy")
                        .arg(site_arg()),
                ),
        )
}

#[path = "bundle/handover.rs"]
pub mod handover;
#[path = "bundle/import.rs"]
pub mod import;

pub use handover::*;
pub use import::*;

// --- running ------------------------------------------------------------------------------------

/// Runs the command.
pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    match matches.subcommand() {
        Some(("export", sub)) => export_cmd(sub, ctx).await,
        Some(("diff", sub)) => diff_cmd(sub, ctx),
        Some(("import", sub)) => import_cmd(sub, ctx).await,
        Some(("env", sub)) => match sub.subcommand() {
            Some(("link", s)) => env_link(s, ctx),
            Some(("list", _)) => env_list(ctx),
            Some(("unlink", s)) => env_unlink(s, ctx),
            _ => usage(ctx),
        },
        _ => usage(ctx),
    }
}

fn usage(ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    writeln!(ctx.err(), "usage: spot bundle <export|diff|import|env> ...")?;
    Ok(Exit::Usage)
}

fn arg(sub: &ArgMatches, name: &str) -> String {
    sub.get_one::<String>(name).cloned().unwrap_or_default()
}

fn now_of(ctx: &CommandCtx) -> spot_cli::Result<(i64, String)> {
    let now = ctx.clock().now_utc();
    let text = now
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|_| spot_cli::Error("the clock cannot be formatted".to_owned()))?;
    Ok((now.unix_timestamp(), text))
}

/// The base of the site image: installed apps, versions and definitions.
fn image_base(ctx: &mut CommandCtx, site: &str) -> spot_cli::Result<ImageBase> {
    let loaded = spot_cli::config::load(ctx.root(), false)?;
    let site_config = ctx.site(site)?.clone();
    let host = env!("CARGO_PKG_VERSION").to_owned();
    let catalogue = spot_cli::apps::catalogue(ctx.root(), &loaded)?;
    let names: Vec<String> = if site_config.apps.is_empty() {
        catalogue.iter().map(|a| a.name.clone()).collect()
    } else {
        site_config.apps.clone()
    };
    let apps = names
        .into_iter()
        .map(|name| {
            let version = catalogue
                .iter()
                .find(|a| a.name == name)
                .map_or_else(|| host.clone(), |a| a.version.clone());
            (name, version)
        })
        .collect();
    let definitions = spot_cli::apps::installed_definitions(ctx.root(), &loaded, &site_config)?;
    Ok(ImageBase {
        apps,
        host,
        definitions,
    })
}

/// The applied custom field and property records of the site's database.
async fn applied_meta(
    ctx: &mut CommandCtx,
    site: &str,
    backend: &Arc<PgBackend>,
) -> spot_cli::Result<AppliedMeta> {
    use spot_customise::{RecordState, RecordStore, StorageRecordStore};
    let loaded = spot_cli::config::load(ctx.root(), false)?;
    let site_config = ctx.site(site)?.clone();
    let meta = spot_cli::apps::serving(ctx.root(), &loaded, &site_config)?;
    let catalog = Arc::new(spot_cli::schema::catalog(&meta)?);
    let store = StorageRecordStore::new(Arc::new((**backend).clone()), catalog);
    let records = store
        .load()
        .await
        .map_err(|_| spot_cli::Error("the customisation records could not be read".to_owned()))?;
    Ok(AppliedMeta {
        fields: records
            .fields
            .iter()
            .filter(|r| r.state == RecordState::Applied)
            .map(|r| (r.doctype.clone(), r.spec.clone(), r.insert_after.clone()))
            .collect(),
        properties: records
            .properties
            .iter()
            .filter(|r| r.state == RecordState::Applied)
            .map(|r| {
                (
                    r.doctype.clone(),
                    r.field.clone(),
                    r.property.clone(),
                    r.value.clone(),
                )
            })
            .collect(),
    })
}

async fn load_image(
    ctx: &mut CommandCtx,
    site: &str,
) -> spot_cli::Result<(FakeSite, ImageBase, Arc<PgBackend>)> {
    let base = image_base(ctx, site)?;
    let backend = ctx.backend(site).await?;
    let meta = applied_meta(ctx, site, &backend).await?;
    let image = build_image(&base, &meta).map_err(spot_cli::Error)?;
    Ok((image, base, backend))
}

fn refuse(ctx: &mut CommandCtx, refusal: &Refusal) -> spot_cli::Result<Exit> {
    writeln!(ctx.err(), "{refusal}")?;
    Ok(Exit::Refused)
}

fn linked(ctx: &mut CommandCtx, site: &str) -> spot_cli::Result<Result<(EnvRecord, Role), Exit>> {
    ctx.site(site)?;
    let dir = site_dir(ctx.root(), site);
    let Some(record) = read_env(&dir)? else {
        writeln!(
            ctx.err(),
            "refused (not_linked): site `{site}` has no environment; run `spot bundle env link`"
        )?;
        return Ok(Err(Exit::Refused));
    };
    match role_of(&record.kind) {
        Some(role) => Ok(Ok((record, role))),
        None => {
            writeln!(
                ctx.err(),
                "refused (not_linked): the environment record names an unknown kind"
            )?;
            Ok(Err(Exit::Refused))
        }
    }
}

async fn export_cmd(sub: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let site = arg(sub, "site");
    let (_, source) = match linked(ctx, &site)? {
        Ok(found) => found,
        Err(exit) => return Ok(exit),
    };
    let target = match sub.get_one::<String>("to") {
        Some(name) => match role_of(name) {
            Some(role) => role,
            None => {
                writeln!(ctx.err(), "`--to` is dev, test or prod")?;
                return Ok(Exit::Usage);
            }
        },
        None => match next_role(source) {
            Some(role) => role,
            None => {
                writeln!(
                    ctx.err(),
                    "refused (direction): production is the end of the ladder; there is nowhere to hand a bundle on to"
                )?;
                return Ok(Exit::Refused);
            }
        },
    };
    if let Err(refusal) = check_direction(source, target) {
        return refuse(ctx, &refusal);
    }
    let seed = match read_seed(Path::new(&arg(sub, "signing_key"))) {
        Ok(seed) => seed,
        Err(message) => {
            writeln!(ctx.err(), "{message}")?;
            return Ok(Exit::Usage);
        }
    };
    let certificate = match std::fs::read(arg(sub, "certificate"))
        .map_err(|e| e.to_string())
        .and_then(|b| SignedCertificate::from_json(&b).map_err(|e| e.to_string()))
    {
        Ok(certificate) => certificate,
        Err(_) => {
            writeln!(
                ctx.err(),
                "the certificate cannot be read or does not parse"
            )?;
            return Ok(Exit::Usage);
        }
    };
    let out_path = PathBuf::from(arg(sub, "out"));
    if out_path.exists() {
        writeln!(
            ctx.err(),
            "refused (exists): the output file already exists"
        )?;
        return Ok(Exit::Refused);
    }
    let (image, _, _) = load_image(ctx, &site).await?;
    let (signed_at, created_at) = now_of(ctx)?;
    let created_by = principal_name(sub);
    let name = sub
        .get_one::<String>("name")
        .cloned()
        .unwrap_or_else(|| site.clone());
    let options = CaptureOptions::new(
        name,
        arg(sub, "bundle_version"),
        created_by.clone(),
        created_at,
    );
    let bundle = match capture(&image, &options) {
        Ok(bundle) => bundle,
        Err(error) => {
            writeln!(ctx.err(), "the site cannot be captured ({})", error.code())?;
            return Ok(Exit::Failed);
        }
    };
    let key = generate_keypair(&seed);
    let bytes = match make_handover(
        &bundle,
        &site,
        source,
        target,
        &created_by,
        &key,
        &certificate,
        signed_at,
    ) {
        Ok(bytes) => bytes,
        Err(message) => {
            writeln!(ctx.err(), "refused (signing): {message}")?;
            return Ok(Exit::Refused);
        }
    };
    write_private(&out_path, &bytes, false)?;
    writeln!(
        ctx.out(),
        "exported {} -> {}: digest {}, {} bytes, signed for {}",
        source.name(),
        target.name(),
        bundle.digest().to_hex(),
        bytes.len(),
        target.name()
    )?;
    Ok(Exit::Ok)
}

fn principal_name(sub: &ArgMatches) -> String {
    sub.get_one::<String>("as")
        .cloned()
        .unwrap_or_else(|| "system".to_owned())
}

/// A bundle from a hand-over file or a raw archive.
fn read_any(path: &str) -> Result<Bundle, Refusal> {
    let bytes = std::fs::read(path)
        .map_err(|_| Refusal::new("unreadable", "a bundle file cannot be read"))?;
    let registry = SectionRegistry::new();
    if bytes.first() == Some(&b'{') {
        let (_, archive) = decode_handover(&bytes)?;
        return open_bundle(archive, &registry);
    }
    open_bundle(&bytes, &registry)
}

fn diff_cmd(sub: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let (a, b) = match (read_any(&arg(sub, "a")), read_any(&arg(sub, "b"))) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(r), _) | (_, Err(r)) => return refuse(ctx, &r),
    };
    let registry = SectionRegistry::new();
    let changes = diff(&a, &b, &registry);
    writeln!(
        ctx.out(),
        "A {}\nB {}\n{} difference(s) from A to B",
        a.digest().to_hex(),
        b.digest().to_hex(),
        changes.len()
    )?;
    write!(ctx.out(), "{}", render_changes(&changes))?;
    match sub.get_one::<String>("base") {
        None => writeln!(
            ctx.out(),
            "conflicts: pass --base to compute them three-way"
        )?,
        Some(path) => {
            let base = match read_any(path) {
                Ok(base) => base,
                Err(r) => return refuse(ctx, &r),
            };
            match merge3_with(&base, &a, &b, &[]) {
                Ok(result) => {
                    writeln!(ctx.out(), "{} conflict(s)", result.conflicts.len())?;
                    write!(
                        ctx.out(),
                        "{}",
                        render_conflicts("merge", &result.conflicts)
                    )?;
                }
                Err(error) => {
                    writeln!(ctx.err(), "the merge failed ({})", error.code())?;
                    return Ok(Exit::Failed);
                }
            }
        }
    }
    Ok(Exit::Ok)
}

async fn import_cmd(sub: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let site = arg(sub, "site");
    let (record, kind) = match linked(ctx, &site)? {
        Ok(found) => found,
        Err(exit) => return Ok(exit),
    };
    let dir = site_dir(ctx.root(), &site);
    let trust = match load_trust(&dir, &record) {
        Ok(trust) => trust,
        Err(refusal) => return refuse(ctx, &refusal),
    };
    let bytes = match std::fs::read(arg(sub, "file")) {
        Ok(bytes) => bytes,
        Err(_) => {
            writeln!(ctx.err(), "the hand-over file cannot be read")?;
            return Ok(Exit::Usage);
        }
    };
    let (image, base, backend) = load_image(ctx, &site).await?;
    let (now, created_at) = now_of(ctx)?;
    let outcome = match evaluate(
        &bytes,
        &ImportContext {
            site: &site,
            kind,
            trust: &trust,
            image: &image,
            now,
            created_at: &created_at,
        },
    ) {
        Ok(outcome) => outcome,
        Err(refusal) => return refuse(ctx, &refusal),
    };
    write!(ctx.out(), "{}", render_outcome(&site, kind, &outcome))?;
    if !sub.get_flag("apply") {
        writeln!(
            ctx.out(),
            "  nothing was changed; rerun with --apply to create the migration plan"
        )?;
        return Ok(Exit::Ok);
    }
    lifecycle_plan(
        sub, ctx, &site, &image, &base, &backend, &outcome, &bytes, &dir,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn lifecycle_plan(
    sub: &ArgMatches,
    ctx: &mut CommandCtx,
    site: &str,
    image: &FakeSite,
    base: &ImageBase,
    backend: &Arc<PgBackend>,
    outcome: &ImportOutcome,
    handover: &[u8],
    dir: &Path,
) -> spot_cli::Result<Exit> {
    let before = describe_effective(&base.definitions, image.customisations())?;
    let after_meta = effective(&base.definitions, outcome.after.customisations())?;
    let catalog = spot_cli::schema::catalog(&after_meta)?;
    let migration = spot_cli::schema::plan_with_platform_tables(backend, &catalog).await?;
    let to = describe(after_meta.definitions());
    let inputs = PlanInputs::new(before, to, SchemaPlan::from_migration(&migration));
    let clock = Arc::clone(ctx.clock());
    let Opened { lifecycle, store } = build_lifecycle(backend, site, clock, principal_of(sub))?;
    let outcome_plan = on_plain_thread(move || {
        store.ensure_present().map_err(|e| (e.to_string(), None))?;
        plan(&lifecycle, &inputs).map_err(|r| (r.error.to_string(), Some(exit_of(&r.error))))
    })?;
    match outcome_plan {
        Ok(report) => {
            let pending = dir.join(PENDING_DIR);
            std::fs::create_dir_all(&pending)?;
            let file = pending.join(format!("{}.handover", report.hash.as_str()));
            if !file.exists() {
                write_private(&file, handover, false)?;
            }
            write!(ctx.out(), "{}", report.render(site))?;
            writeln!(
                ctx.out(),
                "  the hand-over file is kept as {PENDING_DIR}/{}.handover; approval and apply are the lifecycle's",
                report.hash.as_str()
            )?;
            Ok(Exit::Ok)
        }
        Err((message, exit)) => {
            writeln!(ctx.err(), "{message}")?;
            Ok(exit.unwrap_or(Exit::Failed))
        }
    }
}

fn effective(
    definitions: &[Definition],
    customisations: &Customisations,
) -> spot_cli::Result<spot_meta::EffectiveMeta> {
    Registry::build(definitions.to_vec(), customisations).map_err(|errors| {
        spot_cli::Error(format!(
            "the metadata does not build ({} error(s))",
            errors.len()
        ))
    })
}

fn describe_effective(
    definitions: &[Definition],
    customisations: &Customisations,
) -> spot_cli::Result<SystemDescription> {
    Ok(describe(
        effective(definitions, customisations)?.definitions(),
    ))
}

// --- environment commands ----------------------------------------------------------------------

fn env_link(sub: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let site = arg(sub, "site");
    ctx.site(&site)?;
    let Some(role) = role_of(&arg(sub, "kind")) else {
        writeln!(ctx.err(), "`--kind` is dev, test or prod")?;
        return Ok(Exit::Usage);
    };
    let dir = site_dir(ctx.root(), &site);
    if let Some(existing) = read_env(&dir)?
        && existing.kind != role.name() {
            writeln!(
                ctx.err(),
                "refused (relink): the site is already {}; unlink it first, a kind never changes quietly",
                existing.kind
            )?;
            return Ok(Exit::Refused);
        }
    let mut record = EnvRecord {
        kind: role.name().to_owned(),
        trust_anchor: None,
        revocations: None,
    };
    let policy_dir = dir.join(ENV_DIR);
    if let Some(path) = sub.get_one::<String>("trust_anchor") {
        let bytes = std::fs::read(path)?;
        if parse_anchor(&bytes).is_err() {
            writeln!(ctx.err(), "the trust anchor does not parse")?;
            return Ok(Exit::Usage);
        }
        std::fs::create_dir_all(&policy_dir)?;
        write_private(&policy_dir.join("anchor.json"), &bytes, true)?;
        record.trust_anchor = Some(format!("{ENV_DIR}/anchor.json"));
    }
    if let Some(path) = sub.get_one::<String>("revocations") {
        let bytes = std::fs::read(path)?;
        if RevocationList::from_json(&bytes).is_err() {
            writeln!(ctx.err(), "the revocation list does not parse")?;
            return Ok(Exit::Usage);
        }
        std::fs::create_dir_all(&policy_dir)?;
        write_private(&policy_dir.join("revocations.json"), &bytes, true)?;
        record.revocations = Some(format!("{ENV_DIR}/revocations.json"));
    }
    let text = toml::to_string(&record).map_err(|e| spot_cli::Error(e.to_string()))?;
    write_private(&dir.join(ENV_FILE), text.as_bytes(), true)?;
    writeln!(ctx.out(), "site `{site}` is linked as {}", role.name())?;
    Ok(Exit::Ok)
}

fn env_list(ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let names: Vec<String> = ctx.sites()?.iter().map(|s| s.site.clone()).collect();
    for name in names {
        let dir = site_dir(ctx.root(), &name);
        let kind = read_env(&dir)?.map_or_else(|| "-".to_owned(), |r| r.kind);
        writeln!(ctx.out(), "{name}\t{kind}")?;
    }
    Ok(Exit::Ok)
}

fn env_unlink(sub: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let site = arg(sub, "site");
    ctx.site(&site)?;
    let dir = site_dir(ctx.root(), &site);
    if read_env(&dir)?.is_none() {
        writeln!(ctx.err(), "site `{site}` is not linked")?;
        return Ok(Exit::Usage);
    }
    std::fs::remove_file(dir.join(ENV_FILE))?;
    let policy = dir.join(ENV_DIR);
    if policy.exists() {
        std::fs::remove_dir_all(policy)?;
    }
    writeln!(ctx.out(), "site `{site}` is no longer linked")?;
    Ok(Exit::Ok)
}
