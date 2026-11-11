//! The PostgreSQL source: deadpool connections, and the cancel request.

use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use deadpool_postgres::{ClientWrapper, Object, Pool, PoolError};
use tokio_postgres::tls::{MakeTlsConnect, TlsConnect};
use tokio_postgres::{CancelToken, NoTls, Socket};

use crate::core::{Abandon, BoxFuture, Checked, Failure, Source};

/// A pooled connection handed to the closure of a call.
///
/// It dereferences to deadpool's `ClientWrapper`, which dereferences to the driver's `Client`,
/// so `query`, `execute`, `transaction` and `prepare_cached` work as on any pooled client. When
/// the call is cancelled (timeout or shutdown), or the closure panics, the connection is closed
/// on drop instead of returning to the pool: the pool then opens a fresh one, so a cancel can
/// never land on the next user's statement.
pub struct PooledConn {
    inner: Option<Object>,
    discard: Arc<AtomicBool>,
}

impl Deref for PooledConn {
    type Target = ClientWrapper;

    fn deref(&self) -> &ClientWrapper {
        match self.inner.as_ref() {
            Some(object) => object,
            None => released(),
        }
    }
}

impl DerefMut for PooledConn {
    fn deref_mut(&mut self) -> &mut ClientWrapper {
        match self.inner.as_mut() {
            Some(object) => object,
            None => released(),
        }
    }
}

/// The `None` arm of [`PooledConn::deref`] and [`PooledConn::deref_mut`], which the type cannot
/// reach: `inner` is emptied only inside [`PooledConn::drop`], and a value being dropped is never
/// dereferenced again.
///
/// It panics rather than aborting. A connection is never a reason to end the process: the panic
/// unwinds the closure's task on the bridge's runtime, the caller's channel closes and the call
/// returns `BridgeError::Closed`, exactly as for a closure that panics. Cold and never inlined,
/// so the hot path keeps a plain `match`.
#[cold]
#[inline(never)]
fn released() -> ! {
    panic!(
        "spot-pg-bridge: PooledConn was dereferenced after its connection was released; the \
         connection is emptied only in PooledConn::drop"
    )
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        // A connection that is not discarded returns to the pool when `object` drops here.
        if let Some(object) = self.inner.take()
            && (self.discard.load(Ordering::SeqCst) || std::thread::panicking())
        {
            drop(Object::take(object));
        }
    }
}

/// How a cancel request reaches the server (which TLS connector it uses).
pub(crate) trait CancelWith: Send + Sync {
    fn cancel(&self, token: CancelToken) -> BoxFuture<'static, ()>;

    /// Test hook: whether this cancel request goes out over TLS. Only the tests ask; a caller
    /// never needs it, because the choice is made once, at construction.
    #[cfg(test)]
    fn uses_tls(&self) -> bool;
}

/// The cancel of a plain pool: the key travels in clear, exactly as the pool's own connections do.
pub(crate) struct NoTlsCancel;

impl CancelWith for NoTlsCancel {
    fn cancel(&self, token: CancelToken) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            // Failure is not fatal: the connection is closed right after, which ends the
            // session on the server as soon as it reads the socket.
            let _ = token.cancel_query(NoTls).await;
        })
    }

    #[cfg(test)]
    fn uses_tls(&self) -> bool {
        false
    }
}

pub(crate) struct TlsCancel<T>(pub(crate) T);

impl<T> CancelWith for TlsCancel<T>
where
    T: MakeTlsConnect<Socket> + Clone + Send + Sync + 'static,
    T::Stream: Send,
    T::TlsConnect: Send,
    <T::TlsConnect as TlsConnect<Socket>>::Future: Send,
{
    fn cancel(&self, token: CancelToken) -> BoxFuture<'static, ()> {
        let tls = self.0.clone();
        Box::pin(async move {
            // Failure is not fatal: the connection is closed right after, which ends the
            // session on the server as soon as it reads the socket.
            let _ = token.cancel_query(tls).await;
        })
    }

    #[cfg(test)]
    fn uses_tls(&self) -> bool {
        true
    }
}

struct PgAbandon {
    token: CancelToken,
    discard: Arc<AtomicBool>,
    cancel: Arc<dyn CancelWith>,
}

impl Abandon for PgAbandon {
    fn abandon(self: Box<Self>) -> BoxFuture<'static, ()> {
        self.discard.store(true, Ordering::SeqCst);
        self.cancel.cancel(self.token.clone())
    }
}

pub(crate) struct PgSource {
    pub(crate) pool: Pool,
    pub(crate) cancel: Arc<dyn CancelWith>,
}

impl PgSource {
    /// Test hook: whether the cancel requests of this source go out over TLS.
    #[cfg(test)]
    pub(crate) fn cancel_uses_tls(&self) -> bool {
        self.cancel.uses_tls()
    }
}

impl Source for PgSource {
    type Conn = PooledConn;

    fn checkout(&self) -> BoxFuture<'static, Result<Checked<PooledConn>, Failure>> {
        let pool = self.pool.clone();
        let cancel = Arc::clone(&self.cancel);
        Box::pin(async move {
            match pool.get().await {
                Ok(object) => {
                    let token = object.cancel_token();
                    let discard = Arc::new(AtomicBool::new(false));
                    Ok(Checked {
                        conn: PooledConn {
                            inner: Some(object),
                            discard: Arc::clone(&discard),
                        },
                        abandon: Box::new(PgAbandon {
                            token,
                            discard,
                            cancel,
                        }),
                    })
                }
                Err(PoolError::Timeout(_)) => Err(Failure::Timeout),
                Err(PoolError::Closed) => Err(Failure::Closed),
                Err(_) => Err(Failure::Connect),
            }
        })
    }
}
