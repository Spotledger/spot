//! Running a blocking call from any thread, including one that belongs to an async runtime.
//!
//! The shared bridge (ADR-082 addendum 1) refuses a call from a thread inside an async runtime
//! (`pg_bridge.wrong_thread`), but the two seams this crate fills are synchronous by contract
//! ([`HighWaterStore`](spot_partition::HighWaterStore) is called by the naming path inside a
//! request, and the snapshot store's persistence by the carrier). The call therefore runs on a
//! short-lived plain thread that the caller waits for. The wait is the cost of one database
//! round trip; the range store pays it once per reservation stride, not per number
//! ([`crate::store_pg`]).

/// Runs `job` on a plain thread and returns its result, or `None` when the job panicked.
pub(crate) fn off_runtime<R: Send>(job: impl FnOnce() -> R + Send) -> Option<R> {
    std::thread::scope(|scope| scope.spawn(job).join().ok())
}

/// The fixed text of a store failure; a driver message can quote a key or a value, so it is
/// never shown.
pub(crate) fn failure_text<E>(error: &spot_pg_bridge::BridgeError<E>) -> String {
    match error.code() {
        Some(code) => format!("the database is not available ({code})"),
        None => "a database statement failed".to_owned(),
    }
}

/// What went wrong inside one bridged call.
#[derive(Debug)]
pub(crate) enum Op {
    /// The driver failed.
    Db,
    /// A stored value did not decode.
    Decode,
    /// The statement ran and refused (a guard did not match).
    Refused,
}

impl From<tokio_postgres::Error> for Op {
    fn from(_error: tokio_postgres::Error) -> Self {
        Self::Db
    }
}
