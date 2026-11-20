//! The classifier a host builds for tenants whose journal holds application topics.
//!
//! The rebuild classifies every entry through an [`EventClassifier`]; the default is the events
//! contract registry, which knows the platform topics only. An app registers its own `<app>_*`
//! topics, so a tenant that runs one needs a classifier that adds the app's declared topics with
//! their class. [`DomainTopicsClassifier`] is that classifier: the base classifier (usually
//! [`spot_journal_rebuild::RegistryClassifier`]) plus a table of declared topics. Reading app
//! manifests to fill the table is the host's job, not this crate's.
//!
//! An extra entry may never shadow a topic the base classifier already knows, and may never be a
//! platform topic (`doc_`, `sys_`, `identity_`): a platform topic that has no registry entry is
//! refused by design, and an app cannot give it a class.

use std::collections::BTreeMap;

use spot_events_contract::names::Topic;
use spot_journal_rebuild::{EventClass, EventClassifier};

/// Why a [`DomainTopicsClassifier`] could not be built. No variant echoes an unchecked input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClassifierError {
    /// An extra topic is already known to the base classifier (a registered platform topic).
    #[error("the topic {topic} is already classified by the platform registry")]
    ShadowsRegistered {
        /// The shadowed topic.
        topic: String,
    },
    /// An extra topic is not a valid app topic (the grammar, or a reserved platform prefix).
    #[error("an extra topic is not a valid app topic name")]
    InvalidTopic,
    /// The same extra topic was declared with two classes.
    #[error("the topic {topic} is declared with two different classes")]
    Conflicting {
        /// The topic.
        topic: String,
    },
}

impl ClassifierError {
    /// The stable catalogue code of this refusal.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ShadowsRegistered { .. } => "journal_audit_run.classifier_shadows_registered",
            Self::InvalidTopic => "journal_audit_run.classifier_topic_invalid",
            Self::Conflicting { .. } => "journal_audit_run.classifier_topic_conflicting",
        }
    }
}

/// A base classifier plus an app's declared topics.
#[derive(Debug, Clone)]
pub struct DomainTopicsClassifier<C> {
    base: C,
    extra: BTreeMap<String, EventClass>,
}

impl<C: EventClassifier> DomainTopicsClassifier<C> {
    /// Wraps `base` and adds `extra`, each topic with its class.
    ///
    /// # Errors
    /// [`ClassifierError`] when an extra topic is not a valid app topic, shadows a topic `base`
    /// already knows, or is declared twice with different classes.
    pub fn new(base: C, extra: &[(&str, EventClass)]) -> Result<Self, ClassifierError> {
        let mut table: BTreeMap<String, EventClass> = BTreeMap::new();
        for (name, class) in extra {
            if Topic::any(name).is_err() {
                return Err(ClassifierError::InvalidTopic);
            }
            if base.class_of(name).is_some() {
                return Err(ClassifierError::ShadowsRegistered {
                    topic: (*name).to_owned(),
                });
            }
            if Topic::app(name).is_err() {
                return Err(ClassifierError::InvalidTopic);
            }
            if let Some(previous) = table.insert((*name).to_owned(), *class)
                && previous != *class
            {
                return Err(ClassifierError::Conflicting {
                    topic: (*name).to_owned(),
                });
            }
        }
        Ok(Self { base, extra: table })
    }
}

impl<C: EventClassifier> EventClassifier for DomainTopicsClassifier<C> {
    fn class_of(&self, topic: &str) -> Option<EventClass> {
        self.base
            .class_of(topic)
            .or_else(|| self.extra.get(topic).copied())
    }
}
