//! An in-process [`MailTransport`] for tests of other crates (WP-PLT-11.T1): records the MIME
//! bytes of every send and can be scripted to fail the next send with a given [`MailError`]. This
//! lets account and queue layers test their mail integration without a socket server.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::error::MailError;
use crate::message::OutgoingMessage;
use crate::transport::MailTransport;

#[derive(Debug, Default)]
struct CaptureState {
    messages: Vec<Vec<u8>>,
    failures: std::collections::VecDeque<MailError>,
}

/// Records sends instead of touching the network. Thread-safe; cheap to hold in tests.
#[derive(Debug, Default)]
pub struct CaptureTransport {
    state: Mutex<CaptureState>,
}

impl CaptureTransport {
    /// Locks the state, recovering from poisoning (test recorder, never returns an error here).
    fn lock(&self) -> MutexGuard<'_, CaptureState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Builds an empty capture transport.
    pub fn new() -> Self {
        Self::default()
    }

    /// Scripts the next `send` to fail with `error`, and every send after it to fail once each
    /// time this is called again.
    pub fn fail_next(&self, error: MailError) {
        self.lock().failures.push_back(error);
    }

    /// Clones the MIME bytes of every recorded send, in send order.
    pub fn messages(&self) -> Vec<Vec<u8>> {
        self.lock().messages.clone()
    }

    async fn send_once(
        &self,
        message: &OutgoingMessage,
    ) -> Result<crate::transport::SendReceipt, MailError> {
        message.validate()?;
        let mut state = self.lock();
        match state.failures.pop_front() {
            Some(error) => Err(error),
            None => {
                state.messages.push(crate::mime::build_mime(message));
                let message_id = message.resolved_message_id();
                let accepted = message
                    .recipients()
                    .map(|mailbox| mailbox.address.clone())
                    .collect();
                Ok(crate::transport::SendReceipt {
                    message_id,
                    accepted,
                    rejected: Vec::new(),
                })
            }
        }
    }
}

impl MailTransport for CaptureTransport {
    fn send<'a>(
        &'a self,
        message: &'a OutgoingMessage,
    ) -> Pin<Box<dyn Future<Output = Result<crate::transport::SendReceipt, MailError>> + Send + 'a>>
    {
        Box::pin(self.send_once(message))
    }
}
