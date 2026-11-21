//! The engine-backed [`DocumentReader`] (WP-AI-03.T8): the real read behind the LLM prompt
//! builder, so a document can enter a prompt as the invoking user and only as far as that user
//! may read it. Operator guide: `docs/running-spot/llm-reader.md`.
//!
//! The LLM service (`services/llm.rs`) takes `dyn DocumentReader` when one is provided and falls
//! back to a reader that denies everything. This module provides it.
//!
//! # What a read does
//!
//! 1. **Who.** The caller is `user:<id>` (the typed principal reference of a signed-in user or an
//!    agent acting as one). Anything else, a `consumer:` principal, the platform, a component name
//!    or a bare word, is refused: the caller's own identity is the rule.
//! 2. **The load is the user's load.** The document engine loads the record **as that user**
//!    (role grants, user permissions and shares of the live decision core), exactly as the API
//!    does. A missing document, an unreadable one and a hidden DocType are indistinguishable:
//!    [`ReadRefusal::Denied`], with no existence oracle. A permission revoked a moment ago applies
//!    to the very next read: nothing is cached between calls.
//! 3. **Field-level permissions.** The loaded document goes through the site's field guard on the
//!    `Search` channel (the one retrieval uses) for the principal. A field the guard hides is
//!    absent; a field it only masks is dropped, never passed as its masked form.
//! 4. **Honest classes.** Every field that remains is labelled for the prompt builder from the
//!    served metadata: `Protected` when it is in a protection group, `Sealed` for a secret
//!    (classification `secret`, or a value in the sealed or encrypted envelope), `Personal` for
//!    classification `pii`, else `Plain`. The builder then redacts and drops the protected
//!    classes; this reader does not do that again, it only tells the truth.
//! 5. **Outages are not denials.** A document engine or principal directory that cannot answer is
//!    [`ReadRefusal::Unavailable`], and the module counts it and says so in readiness
//!    (`degraded: llm_reader (...)`) until the next check finds none.
//!
//! Only the record's own fields are read (child-table rows are not part of the view). Binary,
//! geographic and range values have no text form and are left out.
//!
//! # Settings (`[services.llm_reader]`)
//!
//! `enabled` (true) and `max_value_chars` (4000: a longer value is cut there, with an ellipsis).
//!
//! # Level and order
//!
//! **Optional.** [`ORDER`] is 1255: after the secrets host (1000) and the egress service (950),
//! below the LLM service (1260) whose `ctx.port::<dyn DocumentReader>()` must find it. It needs no
//! other module's port: the document engine, the permission decision core and the principal
//! directory are read from the site's live hub on every call, so a module that starts later
//! (they are attached at runtime start) is seen. A host with no live hub degrades and provides
//! nothing, so the LLM service keeps its deny-all reader.
//!
//! # Ports
//!
//! Provides `dyn DocumentReader`. Takes nothing.

use std::sync::Arc;

use serde::Deserialize;
use spot_core::SiteId;
use tokio::sync::watch;

use crate::services::llm::prompt::DocumentReader;
use crate::services::{CheckFuture, Idle, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

#[path = "llm_reader/live.rs"]
pub mod live;
#[path = "llm_reader/reader.rs"]
pub mod reader;

use reader::EngineReader;

/// Start order: below the LLM service (1260), which takes the port.
pub const ORDER: u32 = 1255;

const SETTINGS_INVALID: &str = "llm_reader.settings_invalid";
const SITE_UNAVAILABLE: &str = "llm_reader.site_unavailable";

fn default_true() -> bool {
    true
}

fn default_max_chars() -> usize {
    4_000
}

/// `[services.llm_reader]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Host the reader. Default true.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// The longest value a field carries into a view, in characters.
    #[serde(default = "default_max_chars")]
    pub max_value_chars: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_value_chars: default_max_chars(),
        }
    }
}

impl Settings {
    /// Checks the numbers.
    ///
    /// # Errors
    ///
    /// A refusal naming the key; it never echoes a value.
    pub fn check(&self) -> Result<()> {
        if !(1..=100_000).contains(&self.max_value_chars) {
            return Err(Error(format!(
                "{SETTINGS_INVALID}: max_value_chars must be 1 to 100000"
            )));
        }
        Ok(())
    }
}

/// The service: readiness only (the reads are driven by the LLM service's calls).
struct ReaderService {
    reader: Arc<EngineReader>,
}

impl Service for ReaderService {
    fn name(&self) -> &'static str {
        "llm_reader"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move { self.reader.health() })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let _ = stop.changed().await;
            Ok(())
        })
    }

    fn shutdown(&self) -> RunFuture<'_> {
        Box::pin(async { Ok(()) })
    }
}

/// A degraded entry: the module is present but provides nothing, for `reason`.
fn degraded(ctx: &mut SiteCtx<'_>, reason: impl Into<String>) {
    let reason: String = reason.into();
    ctx.add(Idle::new("llm_reader", Level::Optional, move || {
        let reason = reason.clone();
        Box::pin(async move { Err(reason) })
    }));
}

/// Registers the reader and provides it as `dyn DocumentReader`.
///
/// # Errors
///
/// A refused `[services.llm_reader]` table, or the port provided twice.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("llm_reader")?;
    if !settings.enabled || !ctx.assembling() {
        return Ok(());
    }
    settings.check()?;
    let env = match ctx.env() {
        Ok(env) => *env,
        Err(_) => {
            degraded(
                ctx,
                format!(
                    "{SITE_UNAVAILABLE}: this host has no site runtime to read documents through"
                ),
            );
            return Ok(());
        }
    };
    let site = SiteId::new(&env.site.site).map_err(crate::error)?;
    let seams = Arc::new(live::LiveSeams::new(
        Arc::clone(env.live),
        site,
        Arc::clone(ctx.clock()),
    ));
    let reader = Arc::new(EngineReader::new(seams, settings.max_value_chars));
    ctx.add(Arc::new(ReaderService {
        reader: Arc::clone(&reader),
    }));
    ctx.provide::<dyn DocumentReader>(reader as Arc<dyn DocumentReader>)
        .map_err(|e| Error(format!("services.llm_reader: {e}")))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_on_and_valid() {
        let s = Settings::default();
        assert!(s.enabled && s.check().is_ok());
        let parsed: Settings = toml::from_str("").unwrap();
        assert_eq!(parsed.max_value_chars, 4_000);
        assert!(toml::from_str::<Settings>("nope = 1").is_err());
    }

    #[test]
    fn an_out_of_range_width_is_refused_without_a_value() {
        for bad in [0, 100_001] {
            let s = Settings {
                max_value_chars: bad,
                ..Settings::default()
            };
            let error = s.check().unwrap_err();
            assert!(error.0.starts_with(SETTINGS_INVALID), "{}", error.0);
        }
    }
}
