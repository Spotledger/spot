//! The extra chains of the audit watchman (WP-SEC-06.T9): the AI trail (`ai:<site>`) and the
//! quarantine trail (`quarantine:<site>`), verified on the same schedule as the document lanes.
//!
//! Each chain is named (`ai`, `quarantine`) and its scope comes from the module that owns it
//! ([`crate::services::ai_audit::scope_of`], [`crate::services::quarantine::live::scope_of`]), so
//! the scope string is spelled in one place. `[services.audit_verify] extra_chains` selects them.
//!
//! - **Incremental.** Each chain is verified from its saved verified head (`Saved::extra_verified`);
//!   the head advances only when the chain is clean and the service is not degraded.
//! - **Full.** Each full slice also verifies one slice of every extra chain from its own saved
//!   cursor (`Saved::extra_cursor`), and a chain's walk that ends clean is the only thing that
//!   clears a degraded state that chain caused.
//! - **A mismatch** degrades the service with the chain's name, the first bad entry index and the
//!   kind (the chain's own code: `audit.hash_mismatch`, `audit.link_broken`, `audit.seq_gap`...),
//!   never entry content, and raises the same `spot::audit_verify` error event as a lane.
//! - **Internal consistency only.** These chains have no signer: no anchor and no signed head is
//!   written for them. Each entry's hash, its link to the entry before it and the sequence are
//!   checked; a wholesale rewrite of the tail (or a rollback to an earlier head that agrees with
//!   itself) is not detectable here.
//!
//! A chain with no entries is clean: a site that never used the AI service or the quarantine has
//! nothing to verify. The saved state gains the extra chains additively, so a state file written
//! before them reads as "not started".

use spot_audit_chain::{AuditStore, Verifier};

use super::{AuditVerifyService, Mismatch, UNAVAILABLE};
use crate::services::{ai_audit, quarantine};
use crate::{Error, Result};

/// One extra chain the watchman verifies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExtraChain {
    /// The AI trail, scope `ai:<site>` (`services/ai_audit.rs`).
    Ai,
    /// The quarantine trail, scope `quarantine:<site>` (`services/quarantine/live.rs`).
    Quarantine,
}

impl ExtraChain {
    /// Every extra chain, in the order the passes visit them.
    pub const ALL: [ExtraChain; 2] = [ExtraChain::Ai, ExtraChain::Quarantine];

    /// The name used in the settings, the saved state, the readiness text and the alert.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            ExtraChain::Ai => "ai",
            ExtraChain::Quarantine => "quarantine",
        }
    }

    /// The chain named `name`, if it is one of ours.
    #[must_use]
    pub fn from_name(name: &str) -> Option<ExtraChain> {
        ExtraChain::ALL
            .into_iter()
            .find(|chain| chain.name() == name)
    }

    /// The chain scope of `site`, from the module that owns the chain.
    #[must_use]
    pub fn scope(self, site: &str) -> String {
        match self {
            ExtraChain::Ai => ai_audit::scope_of(site),
            ExtraChain::Quarantine => quarantine::live::scope_of(site),
        }
    }
}

/// The names the settings carry by default: every extra chain.
#[must_use]
pub fn default_names() -> Vec<String> {
    ExtraChain::ALL
        .into_iter()
        .map(|chain| chain.name().to_owned())
        .collect()
}

/// The chains `names` select, in the order given, each once.
///
/// # Errors
///
/// [`Error`] naming the key and the unknown chain (the name is the operator's own setting text).
pub fn parse(names: &[String]) -> Result<Vec<ExtraChain>> {
    let mut chains = Vec::new();
    for name in names {
        let Some(chain) = ExtraChain::from_name(name) else {
            return Err(Error(format!(
                "services.audit_verify.extra_chains names an unknown chain `{name}` (known: ai, quarantine)"
            )));
        };
        if !chains.contains(&chain) {
            chains.push(chain);
        }
    }
    Ok(chains)
}

/// The chains `names` select, skipping a name that is not one of ours. The settings are validated
/// ([`parse`]) before a site starts, so a service never meets an unknown name; this keeps `new`
/// total.
pub(super) fn chains(names: &[String]) -> Vec<ExtraChain> {
    parse(names).unwrap_or_else(|_| {
        names
            .iter()
            .filter_map(|name| ExtraChain::from_name(name))
            .collect()
    })
}

impl<S: AuditStore + 'static> AuditVerifyService<S> {
    /// The extra chains this service verifies.
    #[must_use]
    pub fn extra_chains(&self) -> &[ExtraChain] {
        &self.extras
    }

    /// The extra chain the degraded state comes from, if it comes from one.
    #[must_use]
    pub fn degraded_chain(&self) -> Option<ExtraChain> {
        self.lock().degraded_chain
    }

    /// The last verified head of `chain` (0 when its verification has not started).
    #[must_use]
    pub fn extra_verified_head(&self, chain: ExtraChain) -> u64 {
        self.lock()
            .saved
            .extra_verified
            .get(chain.name())
            .copied()
            .unwrap_or(0)
    }

    /// Records the first mismatch of an extra chain: degrades the service (naming the chain) and
    /// raises the high-priority alert, whose fields are the chain, the entry index and the kind.
    fn flag_extra(&self, chain: ExtraChain, seq: u64, kind: &'static str) {
        let first = {
            let mut inner = self.lock();
            inner
                .saved
                .extra_cycle_failed
                .insert(chain.name().to_owned(), true);
            let first = inner.degraded.is_none();
            if first {
                inner.degraded = Some(Mismatch {
                    lane: None,
                    seq,
                    kind,
                });
                inner.degraded_chain = Some(chain);
            }
            first
        };
        if first {
            tracing::error!(target: "spot::audit_verify", site = %self.site, kind, chain = chain.name(), seq, "AUDIT CHAIN MISMATCH");
        }
    }

    /// Verifies `from..=to` of one extra chain. Returns the entries checked and whether the range
    /// was clean, or flags the first failure.
    async fn verify_extra(
        &self,
        chain: ExtraChain,
        from: u64,
        to: u64,
    ) -> std::result::Result<(u64, bool), &'static str> {
        let keys = self.keys.keys();
        let report = Verifier::new(&keys)
            .verify_range(&*self.store, &chain.scope(&self.site), from, to)
            .await
            .map_err(|_| UNAVAILABLE)?;
        if let Some(first) = report.failures.first() {
            self.flag_extra(chain, first.seq, first.kind.code());
            return Ok((report.checked, false));
        }
        Ok((report.checked, true))
    }

    /// The incremental pass over the extra chains: each from its verified head. Returns the number
    /// of entries checked.
    ///
    /// # Errors
    ///
    /// [`UNAVAILABLE`] when the store cannot be read.
    pub(super) async fn run_extra_incremental(&self) -> std::result::Result<u64, &'static str> {
        let mut checked = 0u64;
        for &chain in &self.extras {
            let from = self.extra_verified_head(chain).saturating_add(1);
            let (count, clean) = self.verify_extra(chain, from, u64::MAX).await?;
            checked = checked.saturating_add(count);
            if clean && self.degraded().is_none() {
                let head = self
                    .store
                    .head(&chain.scope(&self.site))
                    .await
                    .map_err(|_| UNAVAILABLE)?;
                // Never backwards: a head lower than the verified one is a truncation for the
                // full walk (and an anchor, which these chains do not have) to judge.
                if let Some(head) = head.filter(|h| h.seq > self.extra_verified_head(chain)) {
                    self.lock()
                        .saved
                        .extra_verified
                        .insert(chain.name().to_owned(), head.seq);
                }
            }
        }
        Ok(checked)
    }

    /// One slice of every extra chain's full walk, each from its own saved cursor. Returns the
    /// entries checked and whether any slice found a mismatch. The cursors are saved by the
    /// caller's `persist`.
    ///
    /// # Errors
    ///
    /// [`UNAVAILABLE`] when the store cannot be read.
    pub(super) async fn run_extra_full_slice(
        &self,
    ) -> std::result::Result<(u64, bool), &'static str> {
        let mut checked = 0u64;
        let mut found = false;
        for &chain in &self.extras {
            let name = chain.name();
            let from = self
                .lock()
                .saved
                .extra_cursor
                .get(name)
                .copied()
                .unwrap_or(0)
                .max(1);
            let to = from.saturating_add(self.settings.full_slice_entries - 1);
            let (count, clean) = self.verify_extra(chain, from, to).await?;
            checked = checked.saturating_add(count);
            found |= !clean;
            let head = self
                .store
                .head(&chain.scope(&self.site))
                .await
                .map_err(|_| UNAVAILABLE)?;
            let done = head.is_none_or(|h| h.seq <= to);
            let mut inner = self.lock();
            if done {
                inner.saved.extra_cursor.insert(name.to_owned(), 1);
                let failed = inner
                    .saved
                    .extra_cycle_failed
                    .insert(name.to_owned(), false)
                    .unwrap_or(false);
                // Only the chain's own whole clean walk clears a state the chain caused.
                if !failed && inner.degraded_chain == Some(chain) {
                    inner.degraded = None;
                    inner.degraded_chain = None;
                }
            } else {
                inner
                    .saved
                    .extra_cursor
                    .insert(name.to_owned(), to.saturating_add(1));
            }
        }
        Ok((checked, found))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use spot_audit_chain::{
        AppendOutcome, AuditChain, AuditRecord, ChainEntry, Checkpoint, CheckpointPolicy,
        CheckpointSigner, InMemoryStore, KeyRing, LocalSigner, PayloadValue, StoreFuture,
    };
    use spot_core::ManualClock;

    use super::super::{
        AccessLog, AnchorSink, AuditAccess, AuditKeys, Saved, Settings, StateStore, TracingLog,
        Wiring,
    };
    use super::*;

    const SITE: &str = "site-a";

    #[test]
    fn the_names_scopes_and_defaults_are_the_owning_modules() {
        assert_eq!(ExtraChain::Ai.name(), "ai");
        assert_eq!(ExtraChain::Quarantine.name(), "quarantine");
        assert_eq!(ExtraChain::from_name("ai"), Some(ExtraChain::Ai));
        assert_eq!(
            ExtraChain::from_name("quarantine"),
            Some(ExtraChain::Quarantine)
        );
        assert_eq!(ExtraChain::from_name("AI"), None);
        assert_eq!(ExtraChain::Ai.scope(SITE), ai_audit::scope_of(SITE));
        assert_eq!(
            ExtraChain::Quarantine.scope(SITE),
            quarantine::live::scope_of(SITE)
        );
        assert_eq!(ExtraChain::Ai.scope(SITE), "ai:site-a");
        assert_eq!(ExtraChain::Quarantine.scope(SITE), "quarantine:site-a");
        assert_eq!(
            default_names(),
            vec!["ai".to_owned(), "quarantine".to_owned()]
        );
        assert_eq!(Settings::default().extra_chains, default_names());
    }

    #[test]
    fn an_unknown_name_is_refused_by_name_and_an_empty_list_turns_it_off() {
        let error = parse(&["ai".to_owned(), "journal".to_owned()]).unwrap_err();
        assert!(
            error.0.contains("extra_chains") && error.0.contains("`journal`"),
            "{}",
            error.0
        );
        let bad = Settings {
            extra_chains: vec!["nope".to_owned()],
            ..Settings::default()
        };
        assert!(bad.validate().unwrap_err().0.contains("`nope`"));
        let off = Settings {
            extra_chains: Vec::new(),
            ..Settings::default()
        };
        assert!(off.validate().is_ok());
        assert_eq!(parse(&off.extra_chains).unwrap(), Vec::new());
        // A name given twice is one chain.
        assert_eq!(
            parse(&["quarantine".to_owned(), "quarantine".to_owned()]).unwrap(),
            vec![ExtraChain::Quarantine]
        );
    }

    #[test]
    fn the_settings_table_reads_extra_chains_and_still_refuses_unknown_keys() {
        let ok: Settings = toml::from_str("extra_chains = [\"ai\"]").unwrap();
        assert_eq!(ok.extra_chains, vec!["ai".to_owned()]);
        let none: Settings = toml::from_str("extra_chains = []").unwrap();
        assert!(none.extra_chains.is_empty());
        assert!(toml::from_str::<Settings>("extra_chain = [\"ai\"]").is_err());
    }

    #[test]
    fn a_state_file_written_before_the_extra_chains_reads_as_not_started() {
        let old = r#"{"verified":{"0":7},"cursor_lane":0,"cursor_seq":3,"cycle_failed":false}"#;
        let saved: Saved = serde_json::from_str(old).unwrap();
        assert_eq!(saved.verified.get(&0), Some(&7));
        assert_eq!(saved.cursor_seq, 3);
        assert!(saved.extra_verified.is_empty());
        assert!(saved.extra_cursor.is_empty());
        assert!(saved.extra_cycle_failed.is_empty());
        // And the new fields round trip.
        let mut next = saved.clone();
        next.extra_verified.insert("ai".to_owned(), 9);
        next.extra_cursor.insert("quarantine".to_owned(), 4);
        next.extra_cycle_failed.insert("ai".to_owned(), true);
        let again: Saved = serde_json::from_str(&serde_json::to_string(&next).unwrap()).unwrap();
        assert_eq!(again, next);
    }

    // ---- the passes, over an in-memory store that can show a damaged view ---------------

    /// A chain store that can show a damaged view of one scope, as a rewritten database would.
    struct Damaged {
        inner: Arc<InMemoryStore>,
        #[allow(clippy::type_complexity)]
        edit: Mutex<
            Option<(
                String,
                Box<dyn Fn(Vec<ChainEntry>) -> Vec<ChainEntry> + Send + Sync>,
            )>,
        >,
    }

    impl Damaged {
        fn apply(&self, site: &str, entries: Vec<ChainEntry>) -> Vec<ChainEntry> {
            match &*self.edit.lock().unwrap() {
                Some((scope, edit)) if scope == site => edit(entries),
                _ => entries,
            }
        }
    }

    impl AuditStore for Damaged {
        fn head<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<ChainEntry>> {
            Box::pin(async move {
                let all = self.inner.range(site, 1, u64::MAX).await?;
                Ok(self.apply(site, all).pop())
            })
        }
        fn append_if_head<'a>(
            &'a self,
            site: &'a str,
            expected_seq: u64,
            entry: ChainEntry,
        ) -> StoreFuture<'a, AppendOutcome> {
            self.inner.append_if_head(site, expected_seq, entry)
        }
        fn range<'a>(
            &'a self,
            site: &'a str,
            from: u64,
            to: u64,
        ) -> StoreFuture<'a, Vec<ChainEntry>> {
            Box::pin(async move {
                let all = self.inner.range(site, 1, u64::MAX).await?;
                Ok(self
                    .apply(site, all)
                    .into_iter()
                    .filter(|e| e.seq >= from && e.seq <= to)
                    .collect())
            })
        }
        fn put_checkpoint<'a>(&'a self, checkpoint: Checkpoint) -> StoreFuture<'a, ()> {
            self.inner.put_checkpoint(checkpoint)
        }
        fn latest_checkpoint<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<Checkpoint>> {
            self.inner.latest_checkpoint(site)
        }
    }

    struct Keys(Arc<LocalSigner>);
    impl AuditKeys for Keys {
        fn signer(&self) -> Arc<dyn CheckpointSigner> {
            self.0.clone()
        }
        fn keys(&self) -> KeyRing {
            let mut ring = KeyRing::new();
            ring.add(&self.0.public_key()).unwrap();
            ring
        }
    }

    struct NoAnchors;
    impl AnchorSink for NoAnchors {
        fn append(&self, _line: &str) -> std::result::Result<(), String> {
            Ok(())
        }
        fn last(&self) -> std::result::Result<Option<String>, String> {
            Ok(None)
        }
    }

    #[derive(Default)]
    struct MemState(Mutex<Option<Saved>>);
    impl StateStore for MemState {
        fn load(&self) -> std::result::Result<Option<Saved>, String> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn save(&self, saved: &Saved) -> std::result::Result<(), String> {
            *self.0.lock().unwrap() = Some(saved.clone());
            Ok(())
        }
    }

    struct Nobody;
    impl AuditAccess for Nobody {
        fn may(&self, _caller: &str, _export: bool) -> std::result::Result<bool, String> {
            Ok(false)
        }
    }

    fn record(kind: &str, n: u64) -> AuditRecord {
        let mut payload = BTreeMap::new();
        payload.insert("n".to_owned(), PayloadValue::Int(i64::try_from(n).unwrap()));
        AuditRecord {
            kind: kind.to_owned(),
            actor: "agent:a1".to_owned(),
            on_behalf_of: Some("user:u1".to_owned()),
            subject: None,
            at: 1_000 + i64::try_from(n).unwrap(),
            payload,
            masked: true,
        }
    }

    struct Rig {
        store: Arc<Damaged>,
        state: Arc<MemState>,
        signer: Arc<LocalSigner>,
    }

    /// `n` entries on each of the two extra chains.
    async fn rig(n: u64) -> Rig {
        let raw = Arc::new(InMemoryStore::new());
        let signer = Arc::new(LocalSigner::from_seed([6u8; 32]));
        for (chain, kind) in [
            (ExtraChain::Ai, "ai.tool_call"),
            (ExtraChain::Quarantine, "quarantine_retry"),
        ] {
            let writer = AuditChain::new(
                spot_doc_audit::SharedStore::new(Arc::clone(&raw)),
                chain.scope(SITE),
                signer.clone(),
                CheckpointPolicy { every: 0 },
            );
            for i in 1..=n {
                writer.append(record(kind, i)).await.unwrap();
            }
        }
        Rig {
            store: Arc::new(Damaged {
                inner: raw,
                edit: Mutex::new(None),
            }),
            state: Arc::new(MemState::default()),
            signer,
        }
    }

    impl Rig {
        fn service(&self, extra: &[&str], slice: u64) -> Arc<AuditVerifyService<Damaged>> {
            let log: Arc<dyn AccessLog> = Arc::new(TracingLog);
            AuditVerifyService::new(Wiring {
                site: SITE.to_owned(),
                store: Arc::clone(&self.store),
                keys: Arc::new(Keys(Arc::clone(&self.signer))),
                anchors: Arc::new(NoAnchors),
                state: self.state.clone(),
                appender: None,
                access: Arc::new(Nobody),
                log,
                clock: Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH)),
                settings: Settings {
                    full_slice_entries: slice,
                    extra_chains: extra.iter().map(|name| (*name).to_owned()).collect(),
                    ..Settings::default()
                },
            })
        }

        fn damage(
            &self,
            chain: ExtraChain,
            edit: impl Fn(Vec<ChainEntry>) -> Vec<ChainEntry> + Send + Sync + 'static,
        ) {
            *self.store.edit.lock().unwrap() = Some((chain.scope(SITE), Box::new(edit)));
        }

        fn repair(&self) {
            *self.store.edit.lock().unwrap() = None;
        }
    }

    #[tokio::test]
    async fn two_clean_chains_verify_and_their_heads_advance() {
        let r = rig(4).await;
        let service = r.service(&["ai", "quarantine"], 100);
        let report = service.run_incremental().await.unwrap();
        assert_eq!(report.checked, 8);
        assert!(!report.mismatch);
        assert_eq!(service.extra_verified_head(ExtraChain::Ai), 4);
        assert_eq!(service.extra_verified_head(ExtraChain::Quarantine), 4);
        assert_eq!(service.readiness_text(), "ok");
        assert_eq!(service.degraded_chain(), None);
        // A second pass has nothing new.
        assert_eq!(service.run_incremental().await.unwrap().checked, 0);
    }

    #[tokio::test]
    async fn a_chain_with_no_entries_is_clean() {
        let r = rig(0).await;
        let service = r.service(&["ai", "quarantine"], 100);
        let report = service.run_incremental().await.unwrap();
        assert_eq!(report.checked, 0);
        assert!(!report.mismatch);
        assert_eq!(service.extra_verified_head(ExtraChain::Ai), 0);
        assert!(!service.run_full_slice().await.unwrap().mismatch);
        assert_eq!(service.readiness_text(), "ok");
    }

    #[tokio::test]
    async fn an_altered_entry_names_the_chain_and_the_index_and_stops_that_head() {
        for chain in ExtraChain::ALL {
            let r = rig(5).await;
            r.damage(chain, |mut entries| {
                entries[2].record.actor = "user:mallory".to_owned();
                entries
            });
            let service = r.service(&["ai", "quarantine"], 100);
            let report = service.run_incremental().await.unwrap();
            assert!(report.mismatch, "{}", chain.name());
            assert_eq!(
                service.degraded(),
                Some(Mismatch {
                    lane: None,
                    seq: 3,
                    kind: "audit.hash_mismatch"
                })
            );
            assert_eq!(service.degraded_chain(), Some(chain));
            assert_eq!(
                service.readiness_text(),
                format!(
                    "degraded: audit.hash_mismatch in the {} chain entry 3",
                    chain.name()
                )
            );
            assert_eq!(
                service.extra_verified_head(chain),
                0,
                "the bad chain's head stays"
            );
            // The text names no entry content.
            assert!(!service.readiness_text().contains("mallory"));
        }
    }

    #[tokio::test]
    async fn a_deleted_entry_and_a_reorder_are_found_in_each_chain() {
        for chain in ExtraChain::ALL {
            let r = rig(5).await;
            r.damage(chain, |mut entries| {
                entries.remove(2);
                entries
            });
            let service = r.service(&["ai", "quarantine"], 100);
            service.run_incremental().await.unwrap();
            assert_eq!(service.degraded().map(|m| m.kind), Some("audit.seq_gap"));
            assert_eq!(service.degraded_chain(), Some(chain));

            let r = rig(5).await;
            r.damage(chain, |mut entries| {
                entries.swap(1, 2);
                entries
            });
            let service = r.service(&["ai", "quarantine"], 100);
            service.run_incremental().await.unwrap();
            assert!(service.degraded().is_some(), "a reorder breaks the links");
            assert_eq!(service.degraded_chain(), Some(chain));
        }
    }

    #[tokio::test]
    async fn the_full_walk_finds_an_old_entry_resumes_after_a_restart_and_clears_when_clean() {
        let r = rig(5).await;
        // The incremental pass has already trusted the chain; the entry is altered afterwards.
        let first = r.service(&["ai"], 2);
        first.run_incremental().await.unwrap();
        r.damage(ExtraChain::Ai, |mut entries| {
            entries[3].record.actor = "user:mallory".to_owned();
            entries
        });
        // Slice 1 (entries 1-2) is clean and its cursor is saved.
        first.run_full_slice().await.unwrap();
        assert_eq!(
            r.state
                .0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .extra_cursor
                .get("ai"),
            Some(&3)
        );
        // The service restarts from the saved state; slice 2 (3-4) finds entry 4.
        let restarted = r.service(&["ai"], 2);
        let slice = restarted.run_full_slice().await.unwrap();
        assert!(slice.mismatch);
        assert_eq!(restarted.degraded().map(|m| m.seq), Some(4));
        assert_eq!(restarted.degraded_chain(), Some(ExtraChain::Ai));
        // The rest of the walk completes; a walk that found something does not clear the alert.
        restarted.run_full_slice().await.unwrap();
        assert!(restarted.degraded().is_some());
        // A document-lane cycle ending clean never clears a state an extra chain caused.
        assert_eq!(restarted.degraded_chain(), Some(ExtraChain::Ai));
        // Repaired: the next whole clean walk (three slices of two entries) clears it.
        r.repair();
        for _ in 0..3 {
            restarted.run_full_slice().await.unwrap();
        }
        assert_eq!(restarted.degraded(), None);
        assert_eq!(restarted.degraded_chain(), None);
        assert_eq!(restarted.readiness_text(), "ok");
    }

    #[tokio::test]
    async fn with_the_list_empty_nothing_extra_is_read() {
        let r = rig(5).await;
        r.damage(ExtraChain::Ai, |mut entries| {
            entries[1].record.actor = "user:mallory".to_owned();
            entries
        });
        let service = r.service(&[], 100);
        assert!(service.extra_chains().is_empty());
        let report = service.run_incremental().await.unwrap();
        assert_eq!(report.checked, 0);
        assert!(!report.mismatch);
        assert!(!service.run_full_slice().await.unwrap().mismatch);
        assert_eq!(service.readiness_text(), "ok");
        assert_eq!(service.extra_verified_head(ExtraChain::Ai), 0);
    }
}
