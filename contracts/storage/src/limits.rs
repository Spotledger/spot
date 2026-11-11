//! Limits added by ADR-087, as configurable defaults beside ADR-069's query limits.
//!
//! These are approved defaults, not measured values (ADR-087 section 14, questions 8; the
//! measuring task is WP-STO-02.T11). A tenant or backend cannot raise them; raising one is a
//! contract amendment. The five ADR-069 limits are unchanged and are not defined here.

/// The widest radius a `WithinDistance` test may ask for, in metres: above any consumer
/// "near me" use, below a whole-planet scan. A larger radius is `InvalidQuery`.
pub const MAX_GEO_RADIUS_M: u32 = 500_000;

/// The widest span, in days, a date or instant range test may cover (100 years): a range that
/// covers all of history defeats the index. A larger span is `InvalidQuery`.
pub const MAX_RANGE_SPAN_DAYS: u32 = 36_600;

/// The longest search term, in characters. Beyond that a term is a payload, not a query.
pub const MAX_SEARCH_TERM_CHARS: usize = 512;

/// The most fields one search term, or one search declaration, may name.
pub const MAX_SEARCH_FIELDS: usize = 16;

/// Decimal places a coordinate may carry (about one centimetre at the equator).
pub const GEO_SCALE: u32 = 7;

/// The only spatial reference system accepted in v1: WGS-84 longitude/latitude.
pub const GEO_SRID: u32 = 4326;
