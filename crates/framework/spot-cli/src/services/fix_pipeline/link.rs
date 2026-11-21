//! The link between the blocking fix engine and the asynchronous site.
//!
//! The engine and its journal are synchronous, and the PostgreSQL journal reaches the database
//! through the shared blocking bridge, which refuses any thread that belongs to a tokio runtime
//! (ADR-082 addendum 1; the blocking pool included). So one run of the engine happens on a **plain
//! thread** it owns, and everything the engine needs from the asynchronous side (apply the remedy
//! on the rehearsal snapshot or the live site, undo it, engage the effect hold, append to the audit
//! chain, take a snapshot) is a request the thread sends down a channel and answers on a reply
//! channel. [`serve`] runs on the caller's task, answers the requests as they arrive and returns
//! the thread's result when it ends. Nothing is shared by reference with the thread, so it needs no
//! borrowed data and runs on a current-thread runtime as well as a multi-thread one.

use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};

use spot_audit_chain::AuditRecord;
use spot_fix_pipeline::{FixId, SnapshotRef};
use spot_mcp::AuditSink;
use spot_remedies::{Applied, RemedyId, UndoToken};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use super::host::{EffectHold, Snapshots};
use crate::services::remedies::host::{DirectApply, RemedyHost};
use crate::services::remedies::world::WorldKind;

/// One reply channel.
pub type Reply<T> = Sender<T>;

/// What the engine thread asks the asynchronous side to do. Errors are fixed sentences.
pub enum Req {
    /// Apply the remedy on the rehearsal snapshot.
    Rehearse(Reply<Result<(), String>>),
    /// Apply the remedy on the live site and answer with what it changed.
    ApplyLive(Reply<Result<Applied, String>>),
    /// Undo the live apply named by `token`.
    Undo(UndoToken, Reply<Result<(), String>>),
    /// Engage the effect hold before the first live change.
    Hold(String, Reply<Result<(), String>>),
    /// Append one record to the audit chain.
    Audit(Box<AuditRecord>, Reply<Result<(), String>>),
    /// Take a site snapshot.
    SnapshotTake(FixId, Reply<Result<SnapshotRef, String>>),
    /// Restore a site snapshot.
    SnapshotRestore(SnapshotRef, Reply<Result<(), String>>),
}

/// The engine thread's end of the link.
#[derive(Clone)]
pub struct Link {
    tx: UnboundedSender<Req>,
}

impl Link {
    /// Sends a request built around a fresh reply channel and waits for the answer.
    ///
    /// # Errors
    ///
    /// A fixed sentence when the asynchronous side has gone away.
    pub fn ask<T>(&self, make: impl FnOnce(Reply<T>) -> Req) -> Result<T, String> {
        let (reply, answer) = channel();
        self.tx
            .send(make(reply))
            .map_err(|_| "the site side of the fix link is closed".to_owned())?;
        answer
            .recv()
            .map_err(|_| "the site side of the fix link did not answer".to_owned())
    }
}

/// What the asynchronous side answers the requests with.
pub struct Side<'a> {
    /// The pipeline's handle on the plan being applied (absent for a later undo or a recovery).
    pub direct: Option<&'a DirectApply<'a>>,
    /// The remedy host, for an undo by token when no handle is held.
    pub remedies: &'a RemedyHost,
    /// The tenant.
    pub tenant: &'a str,
    /// The acting agent's id.
    pub agent_id: &'a str,
    /// The remedy the run is about.
    pub remedy: Option<&'a RemedyId>,
    /// The audit chain.
    pub sink: &'a Arc<dyn AuditSink>,
    /// The effect hold, when installed.
    pub hold: Option<&'a Arc<dyn EffectHold>>,
    /// Whether a run must not apply without a hold.
    pub require_hold: bool,
    /// The snapshot provider.
    pub snapshots: &'a Arc<dyn Snapshots>,
    /// The migration id the hold is taken under (`fix-<id>`), when one was.
    pub held_as: Option<String>,
    /// The fix's id, text.
    pub fix: String,
    /// The instant of the run, unix seconds.
    pub now: i64,
}

impl Side<'_> {
    async fn handle(&mut self, request: Req) {
        match request {
            Req::Rehearse(reply) => {
                let answer = match self.direct {
                    Some(direct) => direct
                        .apply(WorldKind::Rehearsal)
                        .await
                        .map(|_| ())
                        .map_err(|e| e.code().to_owned()),
                    None => Err("no remedy handle for a rehearsal".to_owned()),
                };
                let _ = reply.send(answer);
            }
            Req::ApplyLive(reply) => {
                let answer = match self.direct {
                    Some(direct) => direct
                        .apply(WorldKind::Live)
                        .await
                        .map_err(|e| e.code().to_owned()),
                    None => Err("no remedy handle for a live apply".to_owned()),
                };
                let _ = reply.send(answer);
            }
            Req::Undo(token, reply) => {
                let answer = self.undo(&token).await;
                let _ = reply.send(answer);
            }
            Req::Hold(reason, reply) => {
                let answer = self.engage(&reason).await;
                let _ = reply.send(answer);
            }
            Req::Audit(record, reply) => {
                let answer = self
                    .sink
                    .record(*record)
                    .await
                    .map_err(|_| "the audit chain did not take the record".to_owned());
                let _ = reply.send(answer);
            }
            Req::SnapshotTake(fix, reply) => {
                let answer = self.snapshots.take(fix.as_str()).await;
                let _ = reply.send(answer);
            }
            Req::SnapshotRestore(snapshot, reply) => {
                let answer = self.snapshots.restore(&snapshot).await;
                let _ = reply.send(answer);
            }
        }
    }

    async fn undo(&self, token: &UndoToken) -> Result<(), String> {
        if let Some(direct) = self.direct {
            return direct
                .undo(WorldKind::Live, token)
                .await
                .map(|_| ())
                .map_err(|e| e.code().to_owned());
        }
        let Some(remedy) = self.remedy else {
            return Err("no remedy to undo".to_owned());
        };
        self.remedies
            .undo(self.tenant, self.agent_id, remedy, token, None)
            .await
            .map(|_| ())
            .map_err(|e| e.code().to_owned())
    }

    async fn engage(&mut self, reason: &str) -> Result<(), String> {
        let Some(hold) = self.hold else {
            return if self.require_hold {
                Err("the effect hold is required and not installed".to_owned())
            } else {
                Ok(())
            };
        };
        let id = format!("fix-{}", self.fix);
        hold.begin(&id, reason, self.now).await?;
        self.held_as = Some(id);
        Ok(())
    }
}

/// Runs `work` on a plain thread, answers its requests on this task and returns its result.
///
/// # Errors
///
/// A fixed sentence when the thread cannot be started or ended without a result (a panic).
pub async fn serve<T: Send + 'static>(
    side: &mut Side<'_>,
    work: impl FnOnce(Link) -> T + Send + 'static,
) -> Result<T, &'static str> {
    let (tx, mut requests) = unbounded_channel::<Req>();
    let (done, result) = tokio::sync::oneshot::channel::<T>();
    std::thread::Builder::new()
        .name("fix-pipeline".to_owned())
        .spawn(move || {
            let out = work(Link { tx });
            let _ = done.send(out);
        })
        .map_err(|_| "the fix pipeline thread could not be started")?;
    while let Some(request) = requests.recv().await {
        side.handle(request).await;
    }
    result
        .await
        .map_err(|_| "the fix pipeline thread ended without a result")
}
