//! The channel exclusion rules (ADR-070 section 12 rule 6 and rule 12.13 h).
//!
//! *The control is applied in one channel and the data leaves through another* is the failure
//! every surveyed product has. Every channel that emits a field asks [`ChannelGuard`] first
//! and gets one of three answers:
//!
//! - [`Emission::Emit`]: the value may leave as itself (the channel's own rights still apply).
//! - [`Emission::Mask`]: only the masked form may leave; the real value never does.
//! - [`Emission::Drop`]: the field is omitted entirely.
//!
//! The decision is a pure function of the group's [`SyncClass`] and the channel's
//! [`Destination`]. It is deliberately small and total: a new destination or class breaks
//! every `match`, so it cannot ship without a decision.
//!
//! # Registration
//!
//! The built-in list ([`builtin_registry`]) carries every channel ADR-070 names, and a test
//! fails if one is missing. Another crate adds a channel by implementing [`GuardedChannel`]
//! and calling [`ChannelRegistry::register`]; its channel is then covered by the same rule and
//! the same leak test.

use std::collections::BTreeMap;

use crate::class::SyncClass;
use crate::error::SealedFieldsError;

/// Where a channel sends data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Destination {
    /// Head office itself: the place the value legitimately lives and is revealed.
    Hub,
    /// A branch or offline node, through sync or a bootstrap snapshot.
    Branch,
    /// Outside the platform: an export, a print, an API client, a search index, an embedding
    /// store, a notification or a shared cache.
    External,
    /// A backup or archive that leaves the live site.
    Backup,
}

impl Destination {
    /// Every destination, in declaration order.
    pub const ALL: [Destination; 4] = [
        Destination::Hub,
        Destination::Branch,
        Destination::External,
        Destination::Backup,
    ];

    /// The stable lower-case name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hub => "hub",
            Self::Branch => "branch",
            Self::External => "external",
            Self::Backup => "backup",
        }
    }
}

impl std::fmt::Display for Destination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a channel may do with a protected field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Emission {
    /// The value may leave as itself.
    Emit,
    /// Only the masked form may leave.
    Mask,
    /// The field is omitted.
    Drop,
}

impl Emission {
    /// The stable lower-case name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Emit => "emit",
            Self::Mask => "mask",
            Self::Drop => "drop",
        }
    }

    /// Whether the value itself may leave.
    #[must_use]
    pub fn is_emit(self) -> bool {
        matches!(self, Self::Emit)
    }

    /// Whether the field is omitted.
    #[must_use]
    pub fn is_drop(self) -> bool {
        matches!(self, Self::Drop)
    }
}

impl std::fmt::Display for Emission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The guard every channel calls before emitting a protected field.
#[derive(Clone, Copy, Debug, Default)]
pub struct ChannelGuard;

impl ChannelGuard {
    /// The one decision, as a pure function of the class and the destination.
    ///
    /// - `everywhere` emits everywhere; the node enforces the same grants offline.
    /// - `encrypted` emits at the hub and to a branch (the node holds it encrypted under a
    ///   leased key), masks on an external channel and leaves as ciphertext in a backup.
    /// - `sealed` emits at the hub (which opens it only through the audited reveal), is
    ///   **dropped** for a branch (a node writes and never reads), masks externally and leaves
    ///   as its sealed blob in a backup.
    /// - `home_only` emits at the hub only: a branch, an external channel and a backup all
    ///   drop it, because no ciphertext exists and the plaintext must not leave.
    #[must_use]
    pub fn decide(class: SyncClass, destination: Destination) -> Emission {
        match (class, destination) {
            (SyncClass::Everywhere, _) => Emission::Emit,
            (
                SyncClass::Encrypted,
                Destination::Hub | Destination::Branch | Destination::Backup,
            ) => Emission::Emit,
            (SyncClass::Encrypted, Destination::External) => Emission::Mask,
            (SyncClass::Sealed, Destination::Hub | Destination::Backup) => Emission::Emit,
            (SyncClass::Sealed, Destination::Branch) => Emission::Drop,
            (SyncClass::Sealed, Destination::External) => Emission::Mask,
            (SyncClass::HomeOnly, Destination::Hub) => Emission::Emit,
            (
                SyncClass::HomeOnly,
                Destination::Branch | Destination::External | Destination::Backup,
            ) => Emission::Drop,
        }
    }
}

/// One registered channel: its stable name, where it sends data, and which part of ADR-070
/// names it.
///
/// A crate with a channel implements this trait and registers it; the default [`Self::emission`]
/// is the shared rule, so a channel never re-implements the class table.
pub trait GuardedChannel: Send + Sync {
    /// The stable lower-case name used in logs, the registry test and error messages.
    fn name(&self) -> &'static str;

    /// Where the channel sends data.
    fn destination(&self) -> Destination;

    /// The ADR-070 section that requires the channel to be guarded.
    fn adr_section(&self) -> &'static str;

    /// What this channel may do with a field of `class`.
    fn emission(&self, class: SyncClass) -> Emission {
        ChannelGuard::decide(class, self.destination())
    }
}

/// The registered channels, by name.
#[derive(Default)]
pub struct ChannelRegistry {
    channels: BTreeMap<&'static str, Box<dyn GuardedChannel>>,
}

impl ChannelRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `channel`. A name may be registered once; registering it again replaces the
    /// earlier entry (the later, more specific channel wins).
    pub fn register(&mut self, channel: Box<dyn GuardedChannel>) {
        self.channels.insert(channel.name(), channel);
    }

    /// The channel named `name`, if registered.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&dyn GuardedChannel> {
        self.channels.get(name).map(Box::as_ref)
    }

    /// The emission for `name` and `class`.
    ///
    /// # Errors
    ///
    /// [`SealedFieldsError::ChannelUnregistered`] when the channel has no registration: a
    /// channel that was never guarded emits nothing.
    pub fn emission(&self, name: &str, class: SyncClass) -> Result<Emission, SealedFieldsError> {
        self.get(name)
            .map(|channel| channel.emission(class))
            .ok_or_else(|| SealedFieldsError::ChannelUnregistered {
                channel: name.to_owned(),
            })
    }

    /// Every registered name, in order.
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.channels.keys().copied().collect()
    }

    /// How many channels are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.channels.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }
}

/// Declares one built-in channel.
macro_rules! channel {
    ($ty:ident, $name:literal, $destination:ident, $section:literal, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, Debug, Default)]
        pub struct $ty;

        impl GuardedChannel for $ty {
            fn name(&self) -> &'static str {
                $name
            }

            fn destination(&self) -> Destination {
                Destination::$destination
            }

            fn adr_section(&self) -> &'static str {
                $section
            }
        }
    };
}

channel!(
    DocumentRead,
    "document_read",
    Hub,
    "ADR-070 12.6",
    "A document read or list row: the value's home, where grants decide it."
);
channel!(
    List,
    "list",
    Hub,
    "ADR-070 12.6",
    "A list view: the same rights as a document read."
);
channel!(
    Report,
    "report",
    Hub,
    "ADR-070 12.6",
    "A report result, grouped or aggregated: needs the `report` right."
);
channel!(
    Export,
    "export",
    External,
    "ADR-070 12.6",
    "A file export (CSV, spreadsheet, JSON) that leaves the platform."
);
channel!(
    Print,
    "print",
    External,
    "ADR-070 12.6",
    "A printed or rendered document: it leaves as paper or PDF."
);
channel!(
    RealtimeEvent,
    "realtime_event",
    External,
    "ADR-070 12.6",
    "A realtime event pushed to a client."
);
channel!(
    JournalStream,
    "journal_stream",
    Branch,
    "ADR-061, ADR-070 12.6",
    "The event journal stream replicated to a node."
);
channel!(
    LinkLookup,
    "link_lookup",
    Hub,
    "ADR-070 12.6",
    "A link lookup and a link's display title."
);
channel!(
    SearchIndex,
    "search_index",
    External,
    "ADR-070 12.6, WP-AI-10",
    "The search index and its result snippets."
);
channel!(
    Embedding,
    "embedding",
    External,
    "ADR-070 12.6, WP-AI-10",
    "An embedding vector feeder: it leaves the platform for a model."
);
channel!(
    VersionHistory,
    "version_history",
    Hub,
    "ADR-070 12.6",
    "Version history built from before and after images."
);
channel!(
    AuditDiff,
    "audit_diff",
    Hub,
    "ADR-070 12.6",
    "An audit diff: a secured field's change is written masked."
);
channel!(
    ApiOutput,
    "api_output",
    External,
    "ADR-070 12.6",
    "The native API's response body."
);
channel!(
    ScimOutput,
    "scim_output",
    External,
    "ADR-070 12.6",
    "A SCIM or provisioning response that mirrors a record out."
);
channel!(
    Notification,
    "notification",
    External,
    "ADR-070 12.6",
    "A notification payload (mail, push, realtime notice)."
);
channel!(
    BootstrapSnapshot,
    "bootstrap_snapshot",
    Branch,
    "ADR-066, ADR-070 12.13 h",
    "A bootstrap snapshot a new node loads."
);
channel!(
    Cache,
    "cache",
    External,
    "ADR-070 12.13 h",
    "A shared or on-disk cache entry."
);
channel!(
    Backup,
    "backup",
    Backup,
    "ADR-070 12.13 h",
    "A backup or archive exporter."
);

/// Every channel ADR-070 requires to be guarded, in the order the ADR names them.
#[must_use]
pub fn builtin_registry() -> ChannelRegistry {
    let mut registry = ChannelRegistry::new();
    registry.register(Box::new(DocumentRead));
    registry.register(Box::new(List));
    registry.register(Box::new(Report));
    registry.register(Box::new(Export));
    registry.register(Box::new(Print));
    registry.register(Box::new(RealtimeEvent));
    registry.register(Box::new(JournalStream));
    registry.register(Box::new(LinkLookup));
    registry.register(Box::new(SearchIndex));
    registry.register(Box::new(Embedding));
    registry.register(Box::new(VersionHistory));
    registry.register(Box::new(AuditDiff));
    registry.register(Box::new(ApiOutput));
    registry.register(Box::new(ScimOutput));
    registry.register(Box::new(Notification));
    registry.register(Box::new(BootstrapSnapshot));
    registry.register(Box::new(Cache));
    registry.register(Box::new(Backup));
    registry
}

/// The channel names ADR-070 section 12.6 and rule 12.13 h require. The registry test
/// compares this list with [`builtin_registry`]; adding a channel to the ADR without a
/// registration fails that test.
pub const ADR_CHANNELS: [&str; 18] = [
    "document_read",
    "list",
    "report",
    "export",
    "print",
    "realtime_event",
    "journal_stream",
    "link_lookup",
    "search_index",
    "embedding",
    "version_history",
    "audit_diff",
    "api_output",
    "scim_output",
    "notification",
    "bootstrap_snapshot",
    "cache",
    "backup",
];

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn every_adr_channel_has_a_guard_registration() {
        let registry = builtin_registry();
        for name in ADR_CHANNELS {
            assert!(
                registry.get(name).is_some(),
                "channel `{name}` is named by ADR-070 and has no guard registration"
            );
        }
        assert_eq!(
            registry.len(),
            ADR_CHANNELS.len(),
            "a channel is not listed in the ADR"
        );
    }

    #[test]
    fn an_unregistered_channel_emits_nothing() {
        let registry = ChannelRegistry::new();
        let error = registry
            .emission("journal_stream", SyncClass::Everywhere)
            .expect_err("unregistered");
        assert_eq!(error.code(), crate::error::CHANNEL_UNREGISTERED);
    }

    #[test]
    fn home_only_never_reaches_a_branch_or_an_external_channel() {
        let registry = builtin_registry();
        for name in registry.names() {
            let destination = registry.get(name).expect("registered").destination();
            let emission = registry
                .emission(name, SyncClass::HomeOnly)
                .expect("registered");
            if destination == Destination::Hub {
                assert_eq!(emission, Emission::Emit, "{name}");
            } else {
                assert_eq!(emission, Emission::Drop, "{name}");
            }
        }
    }

    #[test]
    fn sealed_is_never_emitted_to_a_branch() {
        let registry = builtin_registry();
        for name in registry.names() {
            if registry.get(name).expect("registered").destination() == Destination::Branch {
                assert_eq!(
                    registry
                        .emission(name, SyncClass::Sealed)
                        .expect("registered"),
                    Emission::Drop,
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn the_rule_is_total_over_classes_and_destinations() {
        for class in SyncClass::ALL {
            for destination in Destination::ALL {
                let emission = ChannelGuard::decide(class, destination);
                assert!(
                    [Emission::Emit, Emission::Mask, Emission::Drop].contains(&emission),
                    "{class} -> {destination}"
                );
            }
        }
        // `everywhere` is the only class that emits on every destination.
        for destination in Destination::ALL {
            assert_eq!(
                ChannelGuard::decide(SyncClass::Everywhere, destination),
                Emission::Emit
            );
        }
    }

    #[test]
    fn another_crate_can_append_a_channel() {
        struct Webhook;
        impl GuardedChannel for Webhook {
            fn name(&self) -> &'static str {
                "webhook"
            }
            fn destination(&self) -> Destination {
                Destination::External
            }
            fn adr_section(&self) -> &'static str {
                "ADR-070 12.6"
            }
        }
        let mut registry = builtin_registry();
        registry.register(Box::new(Webhook));
        assert_eq!(
            registry
                .emission("webhook", SyncClass::HomeOnly)
                .expect("registered"),
            Emission::Drop
        );
        assert_eq!(
            registry
                .emission("webhook", SyncClass::Sealed)
                .expect("registered"),
            Emission::Mask
        );
    }
}
