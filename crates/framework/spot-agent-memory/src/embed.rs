//! The embedding seam: `spot-retrieval`'s `EmbeddingProvider` under this crate's name, and the
//! one place this crate touches a float.
//!
//! An entry's vector is produced when the entry is recorded and stored beside it; recall embeds
//! the query with the same provider. No model is chosen, called or retried here — the host
//! supplies the provider, exactly as `spot-retrieval` does for document indexing.

use crate::error::MemoryError;

/// The embedding port. It is `spot-retrieval`'s trait, re-exported under the name memory code
/// reads naturally with: there is no second embedding trait on this platform.
pub use spot_retrieval::EmbeddingProvider as Embedder;
/// The cosine similarity `spot-retrieval` implements, reused rather than re-derived.
pub use spot_retrieval::cosine;

/// The width of every vector in the memory namespace. A pgvector column is fixed-width, so the
/// crate fixes it too: a provider that answers with any other length is refused when an entry is
/// recorded or stored and when a query is embedded, and `similarity_bp` scores two vectors of
/// different lengths as `0` rather than comparing a prefix.
pub const EMBEDDING_DIM: usize = 64;

/// Embeds exactly one text and checks the provider answered with a usable vector.
///
/// # Errors
///
/// [`MemoryError::Embed`] when the provider fails, answers with no vector, or answers with an
/// empty or non-finite one.
pub(crate) fn embed_one(embedder: &dyn Embedder, text: &str) -> Result<Vec<f32>, MemoryError> {
    let texts = [text.to_owned()];
    let mut vectors = embedder.embed(&texts).map_err(|error| MemoryError::Embed {
        detail: error.to_string(),
    })?;
    if vectors.len() != 1 {
        return Err(MemoryError::Embed {
            detail: format!(
                "the provider answered with {} vectors for one text",
                vectors.len()
            ),
        });
    }
    let vector = vectors.remove(0);
    if !vector_is_usable(&vector) {
        return Err(MemoryError::Embed {
            detail: format!(
                "the provider answered with a vector that is empty, non-finite or not {EMBEDDING_DIM} wide"
            ),
        });
    }
    Ok(vector)
}

/// Whether a vector can be stored and scored: exactly [`EMBEDDING_DIM`] wide and every element
/// finite.
#[must_use]
pub fn vector_is_usable(vector: &[f32]) -> bool {
    vector.len() == EMBEDDING_DIM && vector.iter().all(|value| value.is_finite())
}

/// The cosine similarity of two vectors as integer basis points, `-10_000` to `10_000`.
///
/// Scores are integers so a recall order is exact and comparable across runs: the float lives
/// inside [`cosine`] and in this one rounding, nowhere else (ADR-12's no-float rule, with the
/// vector exception `spot-retrieval` documents).
#[must_use]
#[allow(clippy::float_arithmetic)] // the vector exception: the cosine and its one rounding
pub fn similarity_bp(a: &[f32], b: &[f32]) -> i32 {
    if !vector_is_usable(a) || !vector_is_usable(b) {
        return 0;
    }
    let score = cosine(a, b);
    if !score.is_finite() {
        return 0;
    }
    let scaled = (score * 10_000.0).round();
    if scaled > 10_000.0 {
        return 10_000;
    }
    if scaled < -10_000.0 {
        return -10_000;
    }
    #[allow(clippy::cast_possible_truncation)]
    let basis_points = scaled as i32;
    basis_points
}

#[cfg(test)]
mod tests {
    use super::{EMBEDDING_DIM, similarity_bp, vector_is_usable};

    /// A full-width vector with `values` at the front and zeros after.
    fn wide(values: &[f32]) -> Vec<f32> {
        let mut vector = vec![0.0; EMBEDDING_DIM];
        for (slot, value) in vector.iter_mut().zip(values) {
            *slot = *value;
        }
        vector
    }

    #[test]
    fn a_score_is_exact_integer_basis_points() {
        assert_eq!(
            similarity_bp(&wide(&[1.0, 0.0]), &wide(&[1.0, 0.0])),
            10_000
        );
        assert_eq!(similarity_bp(&wide(&[1.0, 0.0]), &wide(&[0.0, 1.0])), 0);
        assert_eq!(
            similarity_bp(&wide(&[1.0, 0.0]), &wide(&[-1.0, 0.0])),
            -10_000
        );
        assert_eq!(similarity_bp(&[], &wide(&[1.0])), 0);
        assert_eq!(similarity_bp(&wide(&[f32::NAN]), &wide(&[1.0])), 0);
    }

    #[test]
    fn a_vector_of_the_wrong_width_is_neither_usable_nor_scored() {
        assert!(vector_is_usable(&wide(&[1.0])));
        assert!(!vector_is_usable(&[1.0, 0.0]));
        assert!(!vector_is_usable(&vec![1.0; EMBEDDING_DIM + 1]));
        assert_eq!(similarity_bp(&[1.0, 0.0], &wide(&[1.0, 0.0])), 0);
    }
}
