//! The harness's one error type, with stable, explained codes.

use std::path::PathBuf;

/// Something the harness could not do.
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    /// A corpus file or directory could not be read.
    #[error("support_eval.corpus_io: `{path}` could not be read: {detail}")]
    CorpusIo {
        /// The path.
        path: PathBuf,
        /// Why.
        detail: String,
    },
    /// A corpus file did not parse or broke a corpus rule.
    #[error("support_eval.corpus_invalid: {at}: {detail}")]
    CorpusInvalid {
        /// The file or incident the problem is in.
        at: String,
        /// What is wrong.
        detail: String,
    },
    /// A registered remedy or one of its parameters is not usable for a corpus row.
    #[error("support_eval.resolution_invalid: {at}: {detail}")]
    ResolutionInvalid {
        /// The incident id.
        at: String,
        /// What is wrong.
        detail: String,
    },
    /// The simulator refused to build an incident's world.
    #[error("support_eval.simulation: {detail}")]
    Simulation {
        /// What went wrong.
        detail: String,
    },
    /// The fix pipeline could not run.
    #[error("support_eval.pipeline: {detail}")]
    Pipeline {
        /// What went wrong.
        detail: String,
    },
    /// The optional model driver failed.
    #[error("support_eval.model: {detail}")]
    Model {
        /// What went wrong.
        detail: String,
    },
    /// A result or report could not be written.
    #[error("support_eval.output: `{path}` could not be written: {detail}")]
    Output {
        /// The destination.
        path: PathBuf,
        /// Why.
        detail: String,
    },
}

/// The harness's result type.
pub type Result<T> = std::result::Result<T, EvalError>;
