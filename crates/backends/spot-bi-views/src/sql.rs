//! SQL building blocks. Identifiers and literals are quoted here and nowhere else; PostgreSQL
//! types come from one table; names are checked against PostgreSQL's identifier limit.

use spot_storage_contract::FieldType;

/// The schema that holds the views. Nothing else is granted to the BI role.
pub const BI_SCHEMA: &str = "bi";
/// The `NOLOGIN` role every BI login is granted.
pub const READER_ROLE: &str = "spot_bi_reader";
/// The field holding a document's posting state. Only posted rows are shown by default.
pub const POSTED_FIELD: &str = "docstatus";
/// The `docstatus` value of a posted (submitted) document.
pub const POSTED: i64 = 1;
/// PostgreSQL's identifier limit in bytes.
pub const MAX_IDENT_BYTES: usize = 63;
/// The prefix of the platform's own tables, which no BI view may expose.
pub const RESERVED_PREFIX: &str = "spot_";
/// Default `statement_timeout` of the BI role.
pub const STATEMENT_TIMEOUT: &str = "60s";
/// Default `idle_in_transaction_session_timeout` of the BI role.
pub const IDLE_TRANSACTION_TIMEOUT: &str = "60s";
/// Default `CONNECTION LIMIT` of the BI role.
pub const CONNECTION_LIMIT: u32 = 20;
/// How long apply and retire wait for any one lock before failing.
pub const LOCK_TIMEOUT: &str = "5s";

/// A SQL identifier, quoted. Embedded double quotes are doubled.
#[must_use]
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A SQL string literal. The `E` form makes backslash an escape whatever the server's
/// `standard_conforming_strings` says, so the escaping is the same on every server.
#[must_use]
pub fn quote_literal(text: &str) -> String {
    format!("E'{}'", text.replace('\\', "\\\\").replace('\'', "''"))
}

/// The PostgreSQL type of a field in a BI view, spelled as `format_type` reports it. JSON,
/// bytes, geo and range have no BI representation; their error names the storage type.
pub fn pg_type(ty: &FieldType) -> Result<String, &'static str> {
    match ty {
        FieldType::Bool => Ok("boolean".to_owned()),
        FieldType::Int => Ok("bigint".to_owned()),
        FieldType::Decimal { precision, scale } => Ok(format!("numeric({precision},{scale})")),
        FieldType::Text { .. } => Ok("text".to_owned()),
        FieldType::Date => Ok("date".to_owned()),
        FieldType::Time => Ok("time without time zone".to_owned()),
        FieldType::DateTime => Ok("timestamp with time zone".to_owned()),
        FieldType::Link { .. } => Ok("uuid".to_owned()),
        FieldType::Json => Err("json"),
        FieldType::Bytes => Err("bytes"),
        FieldType::Geo { .. } => Err("geo"),
        FieldType::Range { .. } => Err("range"),
    }
}

/// The view name of a document: `v<version>_<entity>`.
#[must_use]
pub fn view_name(version: u32, entity: &str) -> String {
    format!("v{version}_{entity}")
}

/// The view name of a child table: `v<version>_<parent>__<child>`.
#[must_use]
pub fn child_view_name(version: u32, parent: &str, child: &str) -> String {
    format!("v{version}_{parent}__{child}")
}

/// FNV-1a over the bytes, as 16 lowercase hex digits. Used to tell whether a view was made from
/// the same SQL, not for security.
#[must_use]
pub fn fingerprint(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_and_literals_are_escaped() {
        assert_eq!(quote_ident("order"), "\"order\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_literal("O'Brien"), "E'O''Brien'");
        assert_eq!(quote_literal("a\\b"), "E'a\\\\b'");
    }

    #[test]
    fn json_and_bytes_have_no_bi_type() {
        assert_eq!(pg_type(&FieldType::Json), Err("json"));
        assert_eq!(pg_type(&FieldType::Bytes), Err("bytes"));
        assert_eq!(
            pg_type(&FieldType::Decimal {
                precision: 18,
                scale: 2
            }),
            Ok("numeric(18,2)".to_owned())
        );
    }
}
