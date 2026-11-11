//! The runner: load a suite, drive the scripted agent through every incident, and build the
//! result the gate reads.
//!
//! `run_scripted` is the CI path. It is deterministic: the corpus order, the injected worlds, the
//! support-case matcher and the pipeline's hashes are all stable, and the caller supplies the
//! only instant in the run.

use std::path::{Path, PathBuf};

use spot_remedies::{Corpus as SupportCases, Registry};

use crate::DEFAULT_MIN_FIX_RATE_BP;
use crate::agent::{Agent, ScriptedAgent};
use crate::corpus::Corpus;
use crate::error::{EvalError, Result};
use crate::metrics::{EvalResult, collect};
use crate::pipeline::{build_registry, support_agent};
use crate::simulator::Simulator;

/// Which corpus suite to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Suite {
    /// The 30 synthetic incidents.
    Incidents,
    /// The prompt-injection suite.
    Injections,
    /// Both suites.
    All,
}

impl Suite {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Incidents => "incidents",
            Self::Injections => "injections",
            Self::All => "all",
        }
    }

    /// Parses the command line's suite name.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "incidents" => Some(Self::Incidents),
            "injections" => Some(Self::Injections),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    /// The corpus directories this suite loads, relative to the corpus root.
    #[must_use]
    pub fn directories(self) -> &'static [&'static str] {
        match self {
            Self::Incidents => &["incidents"],
            Self::Injections => &["injections"],
            Self::All => &["incidents", "injections"],
        }
    }
}

/// One run's configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunConfig {
    /// The suite to run.
    pub suite: Suite,
    /// The caller's instant (unix seconds); the harness reads no clock.
    pub now: i64,
    /// The fix-rate floor, in basis points.
    pub min_fix_rate_bp: u32,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            suite: Suite::All,
            now: 1_760_000_000,
            min_fix_rate_bp: DEFAULT_MIN_FIX_RATE_BP,
        }
    }
}

/// The default corpus root.
#[must_use]
pub fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus")
}

/// Loads the support-case corpus the scripted agent matches against.
///
/// # Errors
///
/// [`EvalError::CorpusIo`] or [`EvalError::CorpusInvalid`] when the file is missing or invalid.
pub fn load_support_cases(root: &Path) -> Result<SupportCases> {
    SupportCases::from_file(&root.join("cases.toml")).map_err(|error| EvalError::CorpusInvalid {
        at: "corpus/cases.toml".to_owned(),
        detail: error.sentence(),
    })
}

/// Loads one suite.
///
/// # Errors
///
/// [`EvalError::CorpusIo`] or [`EvalError::CorpusInvalid`] when a directory or file is missing
/// or invalid.
pub fn load(root: &Path, suite: Suite) -> Result<Corpus> {
    let mut corpus = Corpus::new();
    for directory in suite.directories() {
        let loaded = Corpus::from_dir(&root.join(directory))?;
        corpus = corpus.merged(loaded);
    }
    corpus.check_shape(&root.display().to_string())?;
    Ok(corpus)
}

/// Runs the scripted evaluation over one suite.
///
/// # Errors
///
/// [`EvalError`] when the corpus, the registry or a simulator cannot be built. A failed incident
/// is a result, not an error.
pub fn run_scripted(root: &Path, config: &RunConfig) -> Result<EvalResult> {
    let corpus = load(root, config.suite)?;
    let registry = build_registry()?;
    corpus.validate(&registry)?;
    let support = load_support_cases(root)?;
    support
        .validate(&registry)
        .map_err(|error| EvalError::CorpusInvalid {
            at: "corpus/cases.toml".to_owned(),
            detail: error.sentence(),
        })?;
    let results = evaluate(&corpus, &support, config.now)?;
    Ok(EvalResult::from_incidents(
        config.suite.as_str(),
        config.now,
        config.min_fix_rate_bp,
        results,
    ))
}

/// Drives the scripted agent over an already loaded corpus.
///
/// # Errors
///
/// [`EvalError::Simulation`] when an incident's world cannot be built or the agent cannot call
/// its surface.
pub fn evaluate(
    corpus: &Corpus,
    support: &SupportCases,
    now: i64,
) -> Result<Vec<crate::metrics::IncidentResult>> {
    let agent = support_agent("support-eval", spot_agent_tiers::Tier::T2);
    let mut results = Vec::with_capacity(corpus.len());
    for incident in corpus.incidents() {
        let mut simulator = Simulator::new(incident, support.clone(), &agent, now)?;
        let mut driver = ScriptedAgent::new(&agent.id);
        driver.run(&mut simulator)?;
        results.push(collect(incident, &simulator));
    }
    Ok(results)
}

/// Validates a corpus against the live registry, for tests and the command line.
///
/// # Errors
///
/// [`EvalError`] when the registry or the corpus is invalid.
pub fn validate(root: &Path, suite: Suite) -> Result<(Corpus, Registry)> {
    let corpus = load(root, suite)?;
    let registry = build_registry()?;
    corpus.validate(&registry)?;
    Ok((corpus, registry))
}
