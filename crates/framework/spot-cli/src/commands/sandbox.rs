//! `spot sandbox create|list|destroy|reap` (WP-ALM-05.T3): masked clones of a site for testing.
//! Guide: `docs/running-spot/sandbox.md`.
//!
//! `create --from SITE --name N [--ttl 14d]` snapshots the source database (`pg_dump`, read only:
//! the source is never modified), restores it into a **new** database and site, strips the
//! secrets, masks every closed field of the clone in place (unclassified fields are masked:
//! `spot-sandbox`'s fail-closed rule), empties the tables that hold copies of values, scans the
//! clone for the original values of what it masked, puts the **effect hold** on the new site (no
//! outbound mail, webhook, bank call or edge sync, and it is never released by this command),
//! records a banner flag and the expiry, and only then says it is ready. Any failure on the way
//! destroys the clone: a half-masked sandbox is never left behind. The snapshot of unmasked data
//! is deleted in every case.
//!
//! Masking is deterministic per field type under a key drawn for this sandbox and kept nowhere,
//! so referential integrity holds and no mapping can be reversed afterwards.
//!
//! `destroy --name N` and `reap` (everything past its expiry, and clones whose creation never
//! finished) drop the database and the site folder. Both refuse a site that has no sandbox record:
//! a real site is never destroyed by this command.
//!
//! Exit codes: 0 done, 1 usage, 2 refused (nothing changed), 3 failed (the clone is gone).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Arg, ArgMatches, Command};
use serde::{Deserialize, Serialize};
use spot_auth::OpaqueToken;
use spot_core::SiteId;
use spot_postgres::PgConfig;
use spot_snapshot::{
    Destination, PgDumpProvider, Progress, ProgressSink, RestoreOptions, RestoreTarget,
    SnapshotOptions, SnapshotProvider, SnapshotTarget,
};

use super::{CommandCtx, Exit};
use db::{connection, drop_database, mask_clone, pg_bin, quote, verify_clone};

/// The PostgreSQL side: names, the maintenance connection, masking the clone in place and the
/// leak scan. Nothing here prints a value: errors name a table and a column.
pub mod db {
    use std::path::PathBuf;

    use serde_json::Value;
    use spot_meta::{Definition, FieldType};
    use spot_postgres::{PgBackend, PgConfig, Tls};
    use spot_sandbox::classify::{FieldClass, MetadataClasses};
    use spot_sandbox::mask::{MaskKind, Masker, SandboxKey, kind_for};
    use spot_sandbox::plan::{FieldPlan, MaskingPolicy};
    use spot_sandbox::scan::SourceIndex;

    /// Tables that hold copies of values and cannot be shown clean: emptied, never masked.
    pub const PURGE_PREFIXES: [&str; 4] = [
        "spot_search",
        "spot_embedding",
        "spot_cache",
        "spot_mail_sent_log",
    ];

    /// The most values of one column the leak scan reads.
    pub const SCAN_SAMPLE: i64 = 5_000;

    /// A short message for any pool or server failure: never a connection string.
    pub fn server_error(_: impl std::fmt::Display) -> String {
        "the database server could not be reached or refused the statement".to_owned()
    }

    /// `name` as a quoted identifier, or an error when it is not a plain storage name.
    pub fn quote(name: &str) -> Result<String, String> {
        let plain = !name.is_empty()
            && name.len() <= 63
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if plain {
            Ok(format!("\"{name}\""))
        } else {
            Err(format!("the name {name:?} is not a plain storage name"))
        }
    }

    /// A `postgres://` URL for `database`: it holds the password, so it goes to the snapshot provider
    /// only.
    pub fn connection(cfg: &PgConfig, database: &str) -> Option<String> {
        let mut url = url::Url::parse("postgres://placeholder/").ok()?;
        url.set_host(Some(&cfg.host)).ok()?;
        url.set_port(Some(cfg.port)).ok()?;
        url.set_username(&cfg.user).ok()?;
        if !cfg.password.is_empty() {
            url.set_password(Some(&cfg.password)).ok()?;
        }
        url.path_segments_mut().ok()?.clear().push(database);
        if matches!(cfg.tls, Tls::Require) {
            url.query_pairs_mut().append_pair("sslmode", "require");
        }
        Some(url.into())
    }

    /// The directory of the PostgreSQL client tools: `SPOT_PG_BIN`, else the first `pg_dump` on
    /// `PATH`; absolute.
    pub fn pg_bin() -> Result<PathBuf, String> {
        let from_env = || std::env::var_os("SPOT_PG_BIN").map(PathBuf::from);
        let from_path = || {
            let path = std::env::var_os("PATH")?;
            std::env::split_paths(&path).find(|d| d.is_absolute() && d.join("pg_dump").is_file())
        };
        let dir = from_env().or_else(from_path).ok_or(
            "the PostgreSQL client tools were not found; put them on PATH or set SPOT_PG_BIN",
        )?;
        if dir.is_absolute() && dir.join("pg_restore").is_file() {
            Ok(dir)
        } else {
            Err(
                "the PostgreSQL client tools directory must be absolute and hold pg_restore"
                    .to_owned(),
            )
        }
    }

    async fn on_db(cfg: &PgConfig, database: &str) -> Result<PgBackend, String> {
        let mut config = cfg.clone().with_dbname(database);
        config.pool_max_size = 2;
        PgBackend::connect(config).await.map_err(server_error)
    }

    /// Runs `sql` on the maintenance database.
    pub async fn admin_exec(cfg: &PgConfig, sql: &str) -> Result<(), String> {
        let backend = on_db(cfg, "postgres").await?;
        let client = backend.pool().get().await.map_err(server_error)?;
        client.batch_execute(sql).await.map_err(server_error)
    }

    /// Drops a sandbox's database, ending its connections first. Used for expiry, a manual destroy
    /// and a failed run: the clone is never left behind.
    pub async fn drop_database(cfg: &PgConfig, database: &str) -> Result<(), String> {
        let name = quote(database)?;
        admin_exec(cfg, &format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")).await
    }

    /// What masking did, by count.
    #[derive(Debug, Default)]
    pub struct MaskStats {
        /// Fields rewritten.
        pub fields: usize,
        /// Rows rewritten.
        pub rows: u64,
        /// Tables emptied because they hold copies.
        pub purged: Vec<String>,
    }

    /// The constant a closed date, time or instant column is reset to.
    fn fixed_value(ty: &FieldType) -> Option<&'static str> {
        match ty {
            FieldType::Date => Some("DATE '1970-01-01'"),
            FieldType::Time => Some("TIME '00:00:00'"),
            FieldType::DateTime => Some("'1970-01-01 00:00:00+00'::timestamptz"),
            FieldType::Json => Some("'null'::jsonb"),
            _ => None,
        }
    }

    fn is_text(ty: &FieldType) -> bool {
        matches!(
            ty,
            FieldType::Text | FieldType::LongText | FieldType::Select
        )
    }

    /// Masks every closed field of the clone in place, deterministically under `key`, empties the
    /// copy-holding tables, and returns the digests of every original value it replaced so the scan
    /// can look for them. A closed field that cannot be rewritten in place fails the run (fail
    /// closed): the caller destroys the clone.
    ///
    /// # Errors
    /// A message naming a table and column, never a value.
    pub async fn mask_clone(
        cfg: &PgConfig,
        database: &str,
        definitions: &[Definition],
        key: [u8; 32],
    ) -> Result<(MaskStats, SourceIndex), String> {
        let policy = MaskingPolicy::from_toml("").map_err(|e| e.to_string())?;
        let classes = MetadataClasses::from_definitions(definitions);
        FieldPlan::build(&classes, &policy).map_err(|e| e.to_string())?;
        let masker = Masker::new(SandboxKey::new(key));
        let mut index = SourceIndex::new(policy.scan_min_chars);
        let mut stats = MaskStats::default();
        let backend = on_db(cfg, database).await?;
        let client = backend.pool().get().await.map_err(server_error)?;
        client.batch_execute("BEGIN").await.map_err(server_error)?;
        for definition in definitions {
            let table = quote(&definition.name)?;
            for spec in &definition.fields {
                let class = classes
                    .get(&definition.name, &spec.name)
                    .cloned()
                    .unwrap_or_else(|| {
                        FieldClass::plain(&definition.name, &spec.name, spec.ty)
                    });
                let kind = kind_for(&class, &policy);
                if !kind.masks() {
                    continue;
                }
                let column = quote(&spec.name)?;
                let at = format!("{}.{}", definition.name, spec.name);
                stats.fields += 1;
                if kind == MaskKind::Secret {
                    // A secret-typed field lives in two engine-managed columns, never in one named
                    // after the field: the sealed value and, for a lookup field, its blind index.
                    let columns: Vec<String> = if spec.ty == FieldType::Secret {
                        let mut names = vec![format!("{}__sealed", spec.name)];
                        let blind = format!("{}__bidx", spec.name);
                        let present = client
                            .query_opt(
                                "SELECT 1 FROM information_schema.columns \
                                 WHERE table_schema = 'public' AND table_name = $1 \
                                 AND column_name = $2",
                                &[&definition.name, &blind],
                            )
                            .await
                            .map_err(|_| format!("{at}: a secret could not be removed"))?;
                        if present.is_some() {
                            names.push(blind);
                        }
                        names
                    } else {
                        vec![spec.name.clone()]
                    };
                    for name in columns {
                        let secret_column = quote(&name)?;
                        client
                            .batch_execute(&format!("UPDATE {table} SET {secret_column} = NULL"))
                            .await
                            .map_err(|_| format!("{at}: a secret could not be removed"))?;
                    }
                    continue;
                }
                if let Some(constant) = fixed_value(&spec.ty) {
                    // A date or instant carries no pseudonym: every row gets the one constant, so
                    // no original survives (and a column that is NOT NULL stays valid).
                    let reset = format!("UPDATE {table} SET {column} = {constant} WHERE {column} IS NOT NULL");
                    let touched = client
                        .execute(&reset, &[])
                        .await
                        .map_err(|_| format!("{at}: a masked value was refused by the column"))?;
                    stats.rows += touched;
                    continue;
                }
                if matches!(spec.ty, FieldType::Bool) {
                    // A flag keeps its value under every mask: nothing to rewrite.
                    continue;
                }
                if matches!(spec.ty, FieldType::Int | FieldType::Decimal) {
                    let select = format!(
                        "SELECT ctid::text, {column}::text FROM {table} WHERE {column} IS NOT NULL"
                    );
                    let update = format!(
                        "UPDATE {table} SET {column} = CAST($1::text AS numeric) WHERE ctid = $2::text::tid"
                    );
                    let rows = client
                        .query(&select, &[])
                        .await
                        .map_err(|_| format!("{at}: unreadable"))?;
                    for row in rows {
                        let ctid: String = row.get(0);
                        let original: String = row.get(1);
                        let Ok(number) = original.parse::<serde_json::Number>()
                        else {
                            return Err(format!("{at}: a number could not be read"));
                        };
                        let masked = match masker.mask_value(kind, &Value::Number(number), "") {
                            Value::Number(masked) => masked.to_string(),
                            Value::String(masked) => masked,
                            _ => return Err(format!("{at}: a number could not be masked")),
                        };
                        client
                            .execute(&update, &[&masked, &ctid])
                            .await
                            .map_err(|_| format!("{at}: a masked value was refused by the column"))?;
                        stats.rows += 1;
                    }
                    continue;
                }
                if matches!(spec.ty, FieldType::Bytes | FieldType::Geo | FieldType::Range) {
                    client
                        .batch_execute(&format!("UPDATE {table} SET {column} = NULL"))
                        .await
                        .map_err(|_| format!("{at}: a masked value was refused by the column"))?;
                    continue;
                }
                if !is_text(&spec.ty) {
                    return Err(format!(
                        "{at}: a closed field of this type cannot be masked in place"
                    ));
                }
                let select =
                    format!("SELECT ctid::text, {column} FROM {table} WHERE {column} IS NOT NULL");
                let update = format!("UPDATE {table} SET {column} = $1 WHERE ctid = $2::text::tid");
                let rows = client
                    .query(&select, &[])
                    .await
                    .map_err(|_| format!("{at}: unreadable"))?;
                for row in rows {
                    let ctid: String = row.get(0);
                    let original: String = row.get(1);
                    index.add(&original);
                    let masked = match masker.mask_value(kind, &Value::String(original.clone()), "")
                    {
                        Value::String(text) => text,
                        _ => String::new(),
                    };
                    // Never longer than the original: the column's own length check still holds.
                    let masked: String = masked
                        .chars()
                        .take(original.chars().count().max(1))
                        .collect();
                    client
                        .execute(&update, &[&masked, &ctid])
                        .await
                        .map_err(|_| format!("{at}: a masked value was refused by the column"))?;
                    stats.rows += 1;
                }
            }
        }
        let tables = client
            .query(
                "SELECT table_name FROM information_schema.tables \
                 WHERE table_schema = 'public' AND table_type = 'BASE TABLE'",
                &[],
            )
            .await
            .map_err(server_error)?;
        for row in tables {
            let name: String = row.get(0);
            if PURGE_PREFIXES.iter().any(|p| name.starts_with(p)) {
                client
                    .batch_execute(&format!("TRUNCATE {}", quote(&name)?))
                    .await
                    .map_err(|_| format!("{name}: could not be emptied"))?;
                stats.purged.push(name);
            }
        }
        client.batch_execute("COMMIT").await.map_err(server_error)?;
        Ok((stats, index))
    }

    fn leaves(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(text) => out.push(text.clone()),
            Value::Array(items) => items.iter().for_each(|v| leaves(v, out)),
            Value::Object(map) => map.values().for_each(|v| leaves(v, out)),
            _ => {}
        }
    }

    /// Reads a sample of every text and JSON column of the clone and fails when any original value
    /// of a masked field is found. Returns the number of columns scanned.
    ///
    /// # Errors
    /// A message naming each table and column that still holds an original value, never the value.
    pub async fn verify_clone(
        cfg: &PgConfig,
        database: &str,
        index: &SourceIndex,
    ) -> Result<usize, String> {
        let backend = on_db(cfg, database).await?;
        let client = backend.pool().get().await.map_err(server_error)?;
        let columns = client
            .query(
                "SELECT c.table_name, c.column_name, c.data_type FROM information_schema.columns c \
                 JOIN information_schema.tables t USING (table_schema, table_name) \
                 WHERE c.table_schema = 'public' AND t.table_type = 'BASE TABLE' \
                 AND c.data_type IN ('text', 'character varying', 'json', 'jsonb')",
                &[],
            )
            .await
            .map_err(server_error)?;
        let (mut scanned, mut leaked) = (0_usize, Vec::new());
        for row in columns {
            let (table, column, kind): (String, String, String) =
                (row.get(0), row.get(1), row.get(2));
            let sql = format!(
                "SELECT {col}::text FROM {tbl} WHERE {col} IS NOT NULL LIMIT {SCAN_SAMPLE}",
                col = ["\"", &column.replace('"', "\"\""), "\""].concat(),
                tbl = ["\"", &table.replace('"', "\"\""), "\""].concat(),
            );
            let values = client.query(&sql, &[]).await.map_err(server_error)?;
            scanned += 1;
            let json = kind.starts_with("json");
            let found = values.iter().any(|v| {
                let text: String = v.get(0);
                if json {
                    let mut found = Vec::new();
                    if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
                        leaves(&parsed, &mut found);
                    }
                    found.iter().any(|s| index.contains(s).is_some())
                } else {
                    index.contains(&text).is_some()
                }
            });
            if found {
                leaked.push(format!("{table}.{column}"));
            }
        }
        if leaked.is_empty() {
            Ok(scanned)
        } else {
            Err(format!(
                "an original value survives in: {}",
                leaked.join(", ")
            ))
        }
    }
}

/// The subcommand name.
pub const NAME: &str = "sandbox";

/// The file that marks a site folder as a sandbox, beside `site.toml`.
pub const RECORD_FILE: &str = "sandbox.toml";

/// The default time to live, in seconds (14 days).
pub const DEFAULT_TTL: i64 = 14 * 86_400;

/// The longest time to live (90 days).
pub const MAX_TTL: i64 = 90 * 86_400;

/// How long an unfinished creation is left before `reap` destroys it.
pub const STALE_CREATING: i64 = 3_600;

/// The hold reason every sandbox carries.
pub const HOLD_REASON: &str = "sandbox: no outbound effect may leave a masked clone";

/// A sandbox's record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub source: String,
    pub database: String,
    pub created_at: i64,
    pub expires_at: i64,
    /// `creating` or `active`.
    pub state: String,
    /// The banner the interface shows on every page of a sandbox.
    pub banner: bool,
}

struct Quiet;
impl ProgressSink for Quiet {
    fn on(&self, _event: Progress) {}
}

/// Arguments and help.
pub fn command() -> Command {
    let name = || {
        Arg::new("name")
            .long("name")
            .value_name("NAME")
            .required(true)
    };
    Command::new(NAME)
        .about("Create, list and destroy masked clones of a site for testing")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("create")
                .about("Clone a site into a masked, outbound-locked sandbox with an expiry")
                .arg(
                    Arg::new("from")
                        .long("from")
                        .value_name("SITE")
                        .required(true),
                )
                .arg(name().help("The new sandbox site name"))
                .arg(
                    Arg::new("ttl")
                        .long("ttl")
                        .value_name("AGE")
                        .default_value("14d")
                        .help("Time to live: 30m, 12h or 14d (at most 90d)"),
                ),
        )
        .subcommand(Command::new("list").about("List sandboxes, their source and expiry"))
        .subcommand(
            Command::new("destroy")
                .about("Drop a sandbox's database and site folder")
                .arg(name().help("The sandbox site name")),
        )
        .subcommand(Command::new("reap").about("Destroy every sandbox past its expiry"))
}

/// Parses `30m`, `12h` or `14d` into seconds, at most [`MAX_TTL`].
#[must_use]
pub fn parse_ttl(text: &str) -> Option<i64> {
    if !text.is_ascii() || text.len() < 2 {
        return None;
    }
    let (number, unit) = text.split_at(text.len() - 1);
    let n: i64 = number.parse().ok().filter(|n| *n > 0)?;
    let seconds = n.checked_mul(match unit {
        "d" => 86_400,
        "h" => 3_600,
        "m" => 60,
        _ => return None,
    })?;
    (seconds <= MAX_TTL).then_some(seconds)
}

/// The database a sandbox named `name` lives in: lowercase letters, digits and underscores.
#[must_use]
pub fn database_name(name: &str) -> Option<String> {
    let database = format!("sandbox_{}", name.replace('-', "_").to_ascii_lowercase());
    quote(&database).ok().map(|_| database)
}

/// Reads a sandbox record; `None` when the folder holds none.
///
/// # Errors
/// [`spot_cli::Error`] when the file exists and does not parse.
pub fn read_record(dir: &Path) -> spot_cli::Result<Option<Record>> {
    match std::fs::read_to_string(dir.join(RECORD_FILE)) {
        Ok(text) => toml::from_str(&text)
            .map(Some)
            .map_err(|_| spot_cli::Error("a sandbox record does not parse".to_owned())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn write_record(dir: &Path, record: &Record) -> spot_cli::Result<()> {
    let text = toml::to_string(record).map_err(|e| spot_cli::Error(e.to_string()))?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(dir.join(RECORD_FILE))?
        .write_all(text.as_bytes())?;
    Ok(())
}

fn now_secs(ctx: &CommandCtx) -> i64 {
    ctx.clock().now_utc().unix_timestamp()
}

/// Runs the command.
pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    match matches.subcommand() {
        Some(("create", sub)) => create(sub, ctx).await,
        Some(("list", _)) => list(ctx),
        Some(("destroy", sub)) => {
            let name = sub.get_one::<String>("name").cloned().unwrap_or_default();
            destroy(ctx, &name).await
        }
        Some(("reap", _)) => reap(ctx).await,
        _ => {
            writeln!(ctx.err(), "usage: spot sandbox <create|list|destroy|reap>")?;
            Ok(Exit::Usage)
        }
    }
}

async fn create(sub: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let from = sub.get_one::<String>("from").cloned().unwrap_or_default();
    let name = sub.get_one::<String>("name").cloned().unwrap_or_default();
    let ttl_text = sub.get_one::<String>("ttl").cloned().unwrap_or_default();
    let (Some(ttl), Some(database)) = (parse_ttl(&ttl_text), database_name(&name)) else {
        writeln!(
            ctx.err(),
            "--ttl is 30m, 12h or 14d (at most 90d) and the name must make a database name"
        )?;
        return Ok(Exit::Usage);
    };
    if SiteId::new(&name).is_err() || name == from {
        writeln!(
            ctx.err(),
            "the sandbox name is not a valid site name or equals the source"
        )?;
        return Ok(Exit::Usage);
    }
    let loaded = spot_cli::config::load(ctx.root(), false)?;
    let Some(source) = loaded.sites.iter().find(|s| s.site == from).cloned() else {
        writeln!(ctx.err(), "there is no site `{from}`")?;
        return Ok(Exit::Usage);
    };
    let dir = ctx.root().join("sites").join(&name);
    if loaded.sites.iter().any(|s| s.site == name) || dir.exists() {
        writeln!(
            ctx.err(),
            "refused (exists): there is already a site `{name}`"
        )?;
        return Ok(Exit::Refused);
    }
    if read_record(&ctx.root().join("sites").join(&from))?.is_some() {
        writeln!(ctx.err(), "refused (nested): a sandbox is not cloned again")?;
        return Ok(Exit::Refused);
    }
    let bin = match pg_bin() {
        Ok(bin) => bin,
        Err(message) => {
            writeln!(ctx.err(), "{message}")?;
            return Ok(Exit::Failed);
        }
    };
    let now = now_secs(ctx);
    std::fs::create_dir(&dir)?;
    let record = Record {
        source: from.clone(),
        database: database.clone(),
        created_at: now,
        expires_at: now.saturating_add(ttl),
        state: "creating".to_owned(),
        banner: true,
    };
    write_record(&dir, &record)?;
    let cfg = loaded.admin.clone();
    let outcome = build(ctx, &loaded, &source, &name, &dir, &cfg, &bin, &record).await;
    // The snapshot holds unmasked data: it goes in every case.
    let _ = std::fs::remove_dir_all(dir.join("snapshot-tmp"));
    match outcome {
        Ok(summary) => {
            write_record(
                &dir,
                &Record {
                    state: "active".to_owned(),
                    ..record
                },
            )?;
            writeln!(
                ctx.out(),
                "sandbox `{name}` is ready (database {database}, expires in {ttl_text}): {summary}"
            )?;
            Ok(Exit::Ok)
        }
        Err(message) => {
            let left = drop_database(&cfg, &database).await.is_err();
            let _ = std::fs::remove_dir_all(&dir);
            writeln!(ctx.err(), "sandbox failed and was destroyed: {message}")?;
            if left {
                writeln!(
                    ctx.err(),
                    "the database {database} could not be dropped; it may hold data: drop it"
                )?;
            }
            Ok(Exit::Failed)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn build(
    ctx: &mut CommandCtx,
    loaded: &spot_cli::config::Loaded,
    source: &spot_cli::config::Site,
    name: &str,
    dir: &Path,
    cfg: &PgConfig,
    bin: &Path,
    record: &Record,
) -> Result<String, String> {
    let url =
        |database: &str| connection(cfg, database).ok_or("the connection cannot be described");
    let provider = PgDumpProvider::new(bin.to_path_buf(), Arc::clone(ctx.clock()));
    let taken = provider
        .take(
            &SnapshotTarget::Database {
                connection: url(&source.database)?,
                tenant: source.site.clone(),
                database: source.database.clone(),
            },
            &Destination {
                dir: dir.join("snapshot-tmp"),
            },
            &SnapshotOptions::default(),
            Arc::new(Quiet),
        )
        .await
        .map_err(|e| format!("the snapshot failed ({})", e.code()))?;
    provider
        .restore(
            &taken.snapshot,
            &RestoreTarget::Database {
                connection: url(&record.database)?,
                database: record.database.clone(),
            },
            &RestoreOptions::default(),
            Arc::new(Quiet),
        )
        .await
        .map_err(|e| format!("the restore failed ({})", e.code()))?;

    // The new site: the source's apps and locale, none of its credentials, keys or aliases.
    let mut site = source.clone();
    site.site = name.to_owned();
    site.database = record.database.clone();
    site.aliases.clear();
    site.api_keys.clear();
    site.credentials.clear();
    site.partition = None;
    spot_cli::sitefile::create(&dir.join("site.toml"), &site).map_err(|e| e.to_string())?;

    let definitions = spot_cli::apps::installed_definitions(ctx.root(), loaded, source)
        .map_err(|e| e.to_string())?;
    let key = *OpaqueToken::generate().digest().as_bytes();
    let (stats, index) = mask_clone(cfg, &record.database, &definitions, key).await?;
    let scanned = verify_clone(cfg, &record.database, &index).await?;

    hold(ctx, name).await?;
    Ok(format!(
        "{} field(s) masked in {} row(s), {} table(s) emptied, {scanned} column(s) scanned clean, outbound effects held",
        stats.fields,
        stats.rows,
        stats.purged.len()
    ))
}

/// Puts the effect hold on the new site. A fresh context: the new site did not exist when this
/// command's configuration was read.
async fn hold(ctx: &mut CommandCtx, name: &str) -> Result<(), String> {
    /// Keeps what the hold command says, so a refusal names its reason.
    #[derive(Clone, Default)]
    struct Said(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Said {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if let Ok(mut held) = self.0.lock() {
                held.extend_from_slice(bytes);
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let said = Said::default();
    let mut fresh = CommandCtx::new(
        ctx.root().to_path_buf(),
        Arc::clone(ctx.clock()),
        Box::new(std::io::sink()),
        Box::new(said.clone()),
    );
    // The clone is a copy of a database whose site may never have been served, so the hold's
    // ledger tables may not exist in it yet: create them before the hold is placed.
    let backend = fresh
        .backend(name)
        .await
        .map_err(|_| "the new site has no usable PostgreSQL backend".to_owned())?;
    spot_cli::services::effect_hold::pg_ledger(&backend)
        .prepare()
        .await
        .map_err(|_| "the hold ledger of the new site could not be prepared".to_owned())?;
    let matches = super::migrate_hold::command()
        .try_get_matches_from([
            "migrate-hold",
            "hold",
            "--site",
            name,
            "--reason",
            HOLD_REASON,
        ])
        .map_err(|_| "the hold command did not parse".to_owned())?;
    match super::migrate_hold::run(&matches, &mut fresh).await {
        Ok(Exit::Ok) => Ok(()),
        other => {
            let reason = said
                .0
                .lock()
                .map(|held| String::from_utf8_lossy(&held).trim().to_owned())
                .unwrap_or_default();
            let how = match other {
                Ok(exit) => format!("{exit:?}"),
                Err(_) => "failed".to_owned(),
            };
            Err(format!(
                "the effect hold could not be placed, so no clone is released ({how}): {reason}"
            ))
        }
    }
}

fn records(ctx: &mut CommandCtx) -> spot_cli::Result<Vec<(String, PathBuf, Record)>> {
    let sites = ctx.root().join("sites");
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(&sites) else {
        return Ok(found);
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if let (Some(name), Some(record)) = (
            entry.file_name().to_str().map(str::to_owned),
            read_record(&dir)?,
        ) {
            found.push((name, dir, record));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

fn list(ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    for (name, _, record) in records(ctx)? {
        writeln!(
            ctx.out(),
            "{name}\tsource {}\t{}\texpires {}",
            record.source,
            record.state,
            record.expires_at
        )?;
    }
    Ok(Exit::Ok)
}

async fn destroy(ctx: &mut CommandCtx, name: &str) -> spot_cli::Result<Exit> {
    let dir = ctx.root().join("sites").join(name);
    let Some(record) = (if SiteId::new(name).is_ok() {
        read_record(&dir)?
    } else {
        None
    }) else {
        writeln!(
            ctx.err(),
            "refused (not_a_sandbox): `{name}` has no sandbox record; nothing was changed"
        )?;
        return Ok(Exit::Refused);
    };
    let cfg = spot_cli::config::load(ctx.root(), false)?.admin;
    if drop_database(&cfg, &record.database).await.is_err() {
        writeln!(
            ctx.err(),
            "the database {} could not be dropped",
            record.database
        )?;
        return Ok(Exit::Failed);
    }
    std::fs::remove_dir_all(&dir)?;
    writeln!(ctx.out(), "sandbox `{name}` destroyed")?;
    Ok(Exit::Ok)
}

async fn reap(ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let now = now_secs(ctx);
    let mut worst = Exit::Ok;
    for (name, _, record) in records(ctx)? {
        let expired = record.expires_at <= now;
        let stale = record.state == "creating" && record.created_at + STALE_CREATING <= now;
        if expired || stale {
            let exit = destroy(ctx, &name).await?;
            if exit != Exit::Ok {
                worst = exit;
            }
        }
    }
    Ok(worst)
}
