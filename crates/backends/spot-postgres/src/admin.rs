//! Additive database provisioning for the product host; never modifies an existing database.

use spot_storage_contract::{ErrorKind, StorageError, StorageResult};
use tokio_postgres::error::SqlState;

use crate::PgConfig;
use crate::conn::{TlsMode, connect_raw};
use crate::error::map_pg_error;

fn identifier(name: &str) -> StorageResult<String> {
    if name.is_empty()
        || name.len() > 63
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(StorageError::new(
            ErrorKind::InvalidValue,
            "database name must contain 1 to 63 lowercase ASCII letters, digits or underscores",
        ));
    }
    Ok(format!("\"{name}\""))
}

/// Whether the validated database name already exists on the admin server.
///
/// Invalid names fail before opening a connection. Names are bound as values in the lookup.
pub async fn database_exists(admin: &PgConfig, name: &str) -> StorageResult<bool> {
    identifier(name)?;
    let client = connect_raw(admin, &TlsMode::new(admin.tls)?).await?;
    let row = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)",
            &[&name],
        )
        .await
        .map_err(|e| map_pg_error(&e, None))?;
    row.try_get(0).map_err(|e| map_pg_error(&e, None))
}

/// The `LC_CTYPE` of a new database unless the caller names another: a UTF-8 locale, so
/// `pg_trgm` classifies Arabic-script letters as word characters and builds trigrams for Urdu.
///
/// Under a `C` ctype it builds none, and the trigram fallback of search (typos, Urdu
/// morphology) silently returns nothing (WP-STO-15.T3 lab, ADR-087 section 11.3).
pub const DEFAULT_CTYPE: &str = "C.UTF-8";

fn valid_token(token: &str, allow_dot: bool) -> bool {
    !token.is_empty()
        && token.bytes().all(|b| {
            b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || (allow_dot && b == b'.')
        })
}

/// The `CREATE DATABASE` statement for a validated quoted name, ICU collation and ctype.
///
/// The collation stays ICU (ordering is the collation the caller passes); `LC_COLLATE` is the
/// libc `C` and is not used for ordering; `LC_CTYPE` is the character classification libc gives
/// `pg_trgm` and friends. Callers validate every token with [`valid_token`] first.
pub(crate) fn create_database_sql(quoted_name: &str, collation: &str, ctype: &str) -> String {
    format!(
        "CREATE DATABASE {quoted_name} TEMPLATE template0 ENCODING 'UTF8' \
         LC_COLLATE 'C' LC_CTYPE '{ctype}' LOCALE_PROVIDER icu ICU_LOCALE '{collation}'"
    )
}

/// Creates a UTF-8, ICU database from template0 with the [`DEFAULT_CTYPE`] ctype, refusing
/// duplicates without alteration. See [`create_database_with_ctype`].
pub async fn create_database(admin: &PgConfig, name: &str, collation: &str) -> StorageResult<()> {
    create_database_with_ctype(admin, name, collation, DEFAULT_CTYPE).await
}

/// Creates a UTF-8, ICU database from template0 whose `LC_CTYPE` is `ctype`, refusing duplicates
/// without alteration.
///
/// PostgreSQL does not accept bind parameters for this utility statement. All three tokens are
/// validated before quoting (ASCII alphanumerics, `_` and `-`; a ctype may also contain `.`);
/// no caller-controlled quote, escape or statement delimiter is legal. A ctype the server does
/// not have fails with `ErrorKind::Unsupported` naming it; there is no fallback to a `C` ctype.
pub async fn create_database_with_ctype(
    admin: &PgConfig,
    name: &str,
    collation: &str,
    ctype: &str,
) -> StorageResult<()> {
    let name = identifier(name)?;
    if !valid_token(collation, false) {
        return Err(StorageError::new(
            ErrorKind::InvalidValue,
            "invalid ICU collation",
        ));
    }
    if !valid_token(ctype, true) {
        return Err(StorageError::new(
            ErrorKind::InvalidValue,
            "invalid ctype: use ASCII letters, digits, `_`, `-` and `.`, for example C.UTF-8",
        ));
    }
    let client = connect_raw(admin, &TlsMode::new(admin.tls)?).await?;
    client
        .batch_execute(&create_database_sql(&name, collation, ctype))
        .await
        .map_err(|e| {
            if is_locale_refusal(&e) {
                StorageError::new(
                    ErrorKind::Unsupported,
                    format!(
                        "the server refused the locale `{ctype}` or the collation `{collation}` \
                         (is it installed? check `locale -a`); the database was not created"
                    ),
                )
            } else {
                map_pg_error(&e, None)
            }
        })
}

/// Whether the server refused the locale or collation of `CREATE DATABASE`. The SQLSTATE differs
/// by PostgreSQL version and by what is wrong, so the server's message is consulted too (to
/// classify only; it is never shown).
fn is_locale_refusal(error: &tokio_postgres::Error) -> bool {
    error.as_db_error().is_some_and(|db| {
        matches!(
            *db.code(),
            SqlState::INVALID_PARAMETER_VALUE
                | SqlState::WRONG_OBJECT_TYPE
                | SqlState::FEATURE_NOT_SUPPORTED
        ) || db.message().to_ascii_lowercase().contains("locale")
    })
}

/// The Urdu word the conformance check measures: it has four letters, so it has trigrams under a
/// Unicode ctype and none under a `C` ctype.
const URDU_PROBE: &str = "\u{627}\u{631}\u{62f}\u{648}";

/// Checks that a tenant database can serve the search fallback (ADR-087 sections 7.3 and 11.3):
/// `show_trgm` of an Urdu word is not empty, `to_tsvector('simple', ...)` keeps the word and
/// `gin_fuzzy_search_limit` is `0`.
///
/// The `pg_trgm` extension must already be installed (this function never installs one); when it
/// is absent the error is `ErrorKind::InvalidSchema` naming `extension.install`, as `prepare`
/// reports a missing extension. A failed check is `ErrorKind::Unsupported`, names which of the
/// three failed and the database's `datctype`, and carries no connection string.
pub async fn check_text_search_ctype(config: &PgConfig) -> StorageResult<()> {
    let client = connect_raw(config, &TlsMode::new(config.tls)?).await?;
    let installed = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pg_trgm')",
            &[],
        )
        .await
        .map_err(|e| map_pg_error(&e, None))?
        .try_get::<_, bool>(0)
        .map_err(|e| map_pg_error(&e, None))?;
    if !installed {
        return Err(StorageError::new(
            ErrorKind::InvalidSchema,
            "the `pg_trgm` extension is not installed in this database, so the search fallback \
             cannot be checked: install it with the audited `extension.install` operation",
        ));
    }
    let row = client
        .query_one(
            "SELECT cardinality(show_trgm($1::text)), \
                    to_tsvector('simple', $1::text) @@ plainto_tsquery('simple', $1::text), \
                    current_setting('gin_fuzzy_search_limit'), \
                    (SELECT datctype::text FROM pg_database WHERE datname = current_database())",
            &[&URDU_PROBE],
        )
        .await
        .map_err(|e| map_pg_error(&e, None))?;
    let trigrams: i32 = row.try_get(0).map_err(|e| map_pg_error(&e, None))?;
    let word_kept: bool = row.try_get(1).map_err(|e| map_pg_error(&e, None))?;
    let fuzzy: String = row.try_get(2).map_err(|e| map_pg_error(&e, None))?;
    let datctype: String = row.try_get(3).map_err(|e| map_pg_error(&e, None))?;
    let mut failed = Vec::new();
    if trigrams == 0 {
        failed.push(
            "`show_trgm` of an Urdu word is empty (the ctype is not a Unicode one, so `pg_trgm` \
             builds no trigrams and typo tolerance returns nothing)",
        );
    }
    if !word_kept {
        failed.push("`to_tsvector('simple', ...)` does not keep an Urdu word");
    }
    if fuzzy.trim() != "0" {
        failed.push("`gin_fuzzy_search_limit` is not 0, so GIN returns a random subset");
    }
    if failed.is_empty() {
        return Ok(());
    }
    Err(StorageError::new(
        ErrorKind::Unsupported,
        format!(
            "this database cannot serve the search fallback (datctype `{datctype}`): {}; create \
             tenant databases with `create_database`, which uses a UTF-8 ctype",
            failed.join("; ")
        ),
    ))
}
