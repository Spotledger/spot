//! Statement cancellation against real PostgreSQL: a [`StatementCanceller`] that sends the
//! protocol's cancel request for the connection a statement runs on.
//!
//! `StatementCanceller::cancel` is synchronous, so the request is handed to a task that owns
//! the network call. The caller then waits for the statement to finish (it ends with SQLSTATE
//! 57014) before it gives the connection back to the pool, so a late cancel can never hit the
//! next statement on a reused connection.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use spot_ai_isolation::{StatementCanceller, StatementId};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_postgres::CancelToken;
use tokio_postgres::NoTls;

/// Cancels statements by id through the cancel token of the connection they run on.
pub struct PgCanceller {
    tokens: Mutex<HashMap<StatementId, CancelToken>>,
    sender: mpsc::UnboundedSender<CancelToken>,
    requested: AtomicU64,
}

impl PgCanceller {
    /// A canceller and the task that sends its requests. The task ends, returning how many
    /// requests it sent, when every `Arc` of the canceller is dropped.
    #[must_use]
    pub fn spawn() -> (Arc<Self>, JoinHandle<u64>) {
        let (sender, mut receiver) = mpsc::unbounded_channel::<CancelToken>();
        let task = tokio::spawn(async move {
            let mut sent = 0_u64;
            while let Some(token) = receiver.recv().await {
                if token.cancel_query(NoTls).await.is_ok() {
                    sent += 1;
                }
            }
            sent
        });
        let canceller = Arc::new(Self {
            tokens: Mutex::new(HashMap::new()),
            sender,
            requested: AtomicU64::new(0),
        });
        (canceller, task)
    }

    /// Remembers the connection `statement` runs on.
    pub fn register(&self, statement: StatementId, token: CancelToken) {
        self.lock().insert(statement, token);
    }

    /// Forgets `statement` once it has ended.
    pub fn forget(&self, statement: StatementId) {
        self.lock().remove(&statement);
    }

    /// How many cancellations were asked for.
    #[must_use]
    pub fn requested(&self) -> u64 {
        self.requested.load(Ordering::Relaxed)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<StatementId, CancelToken>> {
        self.tokens.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl StatementCanceller for PgCanceller {
    fn cancel(&self, statement: StatementId) {
        let token = self.lock().get(&statement).cloned();
        if let Some(token) = token {
            self.requested.fetch_add(1, Ordering::Relaxed);
            let _ = self.sender.send(token);
        }
    }
}
