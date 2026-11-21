//! The wire forms: a vector as pgvector's text literal, and the integer widths of a chunk key.
//!
//! No pgvector driver crate is used. A vector crosses the wire as the text `[v1,v2,...]` and the
//! statements cast it (`$1::text::vector`, `embedding::text`), exactly as `spot-agent-memory-pg`
//! does. Rendering refuses a non-finite value (pgvector refuses it too, with a less helpful
//! message), and parsing accepts only what pgvector prints.

use spot_retrieval::RetrievalError;
use spot_storage_contract::RecordId;

use crate::error::refused;

/// Renders `vector` as a pgvector literal of exactly `dim` finite values.
///
/// # Errors
///
/// [`RetrievalError::Store`] when the width is not `dim` or a value is NaN or infinite.
pub(crate) fn render_vector(vector: &[f32], dim: usize) -> Result<String, RetrievalError> {
    if vector.len() != dim {
        return Err(refused("a vector does not have the width of the index"));
    }
    let mut out = String::with_capacity(vector.len() * 12 + 2);
    out.push('[');
    for (i, value) in vector.iter().enumerate() {
        if !value.is_finite() {
            return Err(refused("a vector holds a value that is not finite"));
        }
        if i > 0 {
            out.push(',');
        }
        out.push_str(&value.to_string());
    }
    out.push(']');
    Ok(out)
}

/// Parses pgvector's text output (`[1,2.5,-3]`) back into `f32` values.
///
/// # Errors
///
/// [`RetrievalError::Store`] when the text is not a bracketed, comma-separated list of numbers.
pub(crate) fn parse_vector(text: &str) -> Result<Vec<f32>, RetrievalError> {
    let inner = text
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .ok_or_else(|| refused("a stored vector could not be decoded"))?;
    if inner.is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .map(|part| {
            part.trim()
                .parse::<f32>()
                .map_err(|_| refused("a stored vector could not be decoded"))
        })
        .collect()
}

/// Whether every component is zero. A zero vector has no direction, so its cosine distance is
/// undefined in pgvector (NaN); the store ranks such a query by key instead.
pub(crate) fn is_zero(vector: &[f32]) -> bool {
    vector.iter().all(|value| *value == 0.0)
}

/// A `u64` as the `bigint` it is stored in.
pub(crate) fn big(value: u64) -> Result<i64, RetrievalError> {
    i64::try_from(value).map_err(|_| refused("a version does not fit the database column"))
}

/// A `u32` chunk number as the `bigint` it is stored in.
pub(crate) fn big_u32(value: u32) -> i64 {
    i64::from(value)
}

/// A record id read from a `bytea` column, re-validated as the UUIDv7 it must be.
///
/// # Errors
///
/// [`RetrievalError::Store`] when the bytes are not 16 bytes of a UUIDv7.
pub(crate) fn record_id(bytes: &[u8]) -> Result<RecordId, RetrievalError> {
    let array: [u8; 16] = bytes
        .try_into()
        .map_err(|_| refused("a stored record id could not be decoded"))?;
    RecordId::from_bytes(array).map_err(|_| refused("a stored record id could not be decoded"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_vector_round_trips_through_its_literal() {
        let vector = vec![1.0_f32, -2.5, 0.000_1, 3.0e10];
        let text = render_vector(&vector, 4).unwrap();
        assert!(text.starts_with('[') && text.ends_with(']'));
        assert_eq!(parse_vector(&text).unwrap(), vector);
    }

    #[test]
    fn a_wrong_width_or_a_non_finite_value_is_refused() {
        assert!(render_vector(&[1.0, 2.0], 3).is_err());
        assert!(render_vector(&[1.0, f32::NAN], 2).is_err());
        assert!(render_vector(&[f32::INFINITY, 1.0], 2).is_err());
    }

    #[test]
    fn garbage_does_not_parse() {
        assert!(parse_vector("1,2,3").is_err());
        assert!(parse_vector("[1,two]").is_err());
        assert_eq!(parse_vector("[]").unwrap(), Vec::<f32>::new());
    }

    #[test]
    fn zero_detection_and_integer_widths() {
        assert!(is_zero(&[0.0, 0.0]));
        assert!(!is_zero(&[0.0, 0.5]));
        assert_eq!(big(5).unwrap(), 5);
        assert!(big(u64::MAX).is_err());
        assert_eq!(big_u32(u32::MAX), 4_294_967_295);
    }

    #[test]
    fn a_record_id_must_be_sixteen_bytes_of_a_uuid_v7() {
        let id = RecordId::new_v7();
        assert_eq!(record_id(&id.as_bytes()[..]).unwrap(), id);
        assert!(record_id(&[1, 2, 3]).is_err());
        assert!(record_id(&[0u8; 16]).is_err());
    }
}
