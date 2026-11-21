//! The outbound side: [`ExternalSender`] hands what the outbox releases to the peer's channel
//! and turns the peer's answer into the receipts the outbox already understands, so an external
//! message goes through the same `Queued -> Sent -> Acknowledged -> Applied | Rejected(code) |
//! Expired` states, the same audit and the same stuck-message conditions as a tenant message.
//!
//! A peer that is not a tenant cannot sign a receipt. The receipt is signed by the **bridge**
//! identity: a key this platform holds for the external party, whose public half is the key the
//! link pins for that party. It attests "the peer answered this way", which is all an external
//! answer can mean; nothing the peer sends is ever trusted for more.
//!
//! | The peer answers | The message becomes |
//! | --- | --- |
//! | 2xx | `Acknowledged`; `Applied` when the body says `{"status": "applied"}` |
//! | 400, 422 and other 4xx | `Rejected(payload_invalid)` |
//! | 401, 403 | `Rejected(not_granted)` |
//! | 406, 415, 426 | `Rejected(version_unsupported)` |
//! | 410 | `Rejected(out_of_order_expired)` |
//! | 408, 425, 429, 5xx, no answer | unchanged; handed over again after the resend interval, until the retention ends and it becomes `Expired` |

use spot_link_exchange::{
    Error as ExchangeError, Message, MessageState, Outbox, OutboxStore, Receipt, ReceiptEffect,
    ReceiptOutcome, RejectCode, TransportParty,
};
use spot_tenant_link::{
    AuditSink, IdentitySigner, Link, LinkEvent, LinkEventKind, RegionOracle, record_event,
};

use crate::channel::{Delivery, PeerChannel};
use crate::error::{Error, Result, codes};
use crate::peer::ExternalPeer;
use crate::wire::{InboundAck, encode_outbound};

/// What one [`ExternalSender::pump`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PumpReport {
    /// Messages handed to the channel.
    pub attempted: usize,
    /// Messages the peer took (`Acknowledged`).
    pub acknowledged: usize,
    /// Messages the peer confirmed as applied.
    pub applied: usize,
    /// Messages the peer refused.
    pub rejected: usize,
    /// Messages that got no usable answer and will be handed over again.
    pub retried: usize,
    /// Slots of ended messages confirmed locally (a peer has no sequence to fill).
    pub fillers_confirmed: usize,
    /// Messages already acknowledged and not sent again.
    pub skipped: usize,
}

/// What an answer means for the message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The message moves on with this receipt outcome.
    Receipt(ReceiptOutcome),
    /// No usable answer: the message stays and is tried again. The code says why.
    Retry(&'static str),
}

/// The rejection code a 4xx status becomes.
#[must_use]
pub fn reject_code_for(status: u16) -> RejectCode {
    match status {
        401 | 403 => RejectCode::NotGranted,
        406 | 415 | 426 => RejectCode::VersionUnsupported,
        410 => RejectCode::OutOfOrderExpired,
        _ => RejectCode::PayloadInvalid,
    }
}

/// Reads a peer's answer. See the table in the module documentation.
#[must_use]
pub fn classify(status: u16, body: &str) -> Verdict {
    match status {
        200..=299 => {
            let applied = serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|json| {
                    json.get("status")
                        .and_then(serde_json::Value::as_str)
                        .map(|text| text == "applied")
                })
                .unwrap_or(false);
            Verdict::Receipt(if applied {
                ReceiptOutcome::Applied
            } else {
                ReceiptOutcome::Acknowledged
            })
        }
        408 | 425 | 429 | 500..=599 => Verdict::Retry(codes::PEER_UNREACHABLE),
        400..=499 => Verdict::Receipt(ReceiptOutcome::Rejected(reject_code_for(status))),
        _ => Verdict::Retry(codes::UNEXPECTED_STATUS),
    }
}

/// The outbound carrier of one external link.
pub struct ExternalSender<'a, 'o> {
    outbox: &'a Outbox<'o>,
    peer: &'a ExternalPeer,
    channel: &'a dyn PeerChannel,
    bridge: &'a dyn IdentitySigner,
    audit: &'a dyn AuditSink,
}

impl<'a, 'o> ExternalSender<'a, 'o> {
    /// A sender over `outbox` for `peer`, delivering through `channel` and attesting answers as
    /// `bridge`.
    #[must_use]
    pub fn new(
        outbox: &'a Outbox<'o>,
        peer: &'a ExternalPeer,
        channel: &'a dyn PeerChannel,
        bridge: &'a dyn IdentitySigner,
        audit: &'a dyn AuditSink,
    ) -> Self {
        Self {
            outbox,
            peer,
            channel,
            bridge,
            audit,
        }
    }

    /// Delivers what the outbox releases now: at most `limit` messages, oldest sequence first.
    ///
    /// The outbox applies the link's grant, direction, expiry and revocation first, so a
    /// revoked link sends nothing. Run it from the host's periodic job; the outbox's resend
    /// interval paces the retries.
    ///
    /// # Errors
    /// A link that does not fit the peer ([`ExternalPeer::validate_for`]), a bridge identity
    /// that is not the peer, or an exchange failure (audit, store). A failed delivery is not an
    /// error: it is audited and counted in [`PumpReport::retried`].
    pub async fn pump(
        &self,
        link: &Link,
        now: u64,
        limit: usize,
        actor: &str,
        oracle: Option<&dyn RegionOracle>,
    ) -> Result<PumpReport> {
        self.peer.validate_for(link)?;
        if self.bridge.party() != self.peer.party() {
            return Err(Error::PeerMismatch);
        }
        let mut report = PumpReport::default();
        for message in self.outbox.next_batch(link, now, limit, actor, oracle)? {
            if message.tombstone {
                self.receive(link, &message, ReceiptOutcome::Acknowledged, now)?;
                report.fillers_confirmed += 1;
                continue;
            }
            if self.outbox.state(&message.id)? == MessageState::Acknowledged {
                report.skipped += 1;
                continue;
            }
            report.attempted += 1;
            let Ok(body) = encode_outbound(self.peer, &message, now) else {
                self.refuse(link, actor, now, codes::MAPPING_FAILED)?;
                report.retried += 1;
                continue;
            };
            let delivery = Delivery {
                message: message.id,
                topic: &message.topic,
                body: &body,
                now_ms: now,
            };
            let verdict = match self.channel.deliver(self.peer, &delivery).await {
                Ok(answer) => classify(answer.status, &answer.body),
                Err(failure) => Verdict::Retry(failure.code()),
            };
            match verdict {
                Verdict::Retry(code) => {
                    self.refuse(link, actor, now, code)?;
                    report.retried += 1;
                }
                Verdict::Receipt(outcome) => {
                    self.receive(link, &message, outcome, now)?;
                    match outcome {
                        ReceiptOutcome::Acknowledged => report.acknowledged += 1,
                        ReceiptOutcome::Applied => report.applied += 1,
                        ReceiptOutcome::Rejected(_) => report.rejected += 1,
                    }
                }
            }
        }
        Ok(report)
    }

    fn receive(
        &self,
        link: &Link,
        message: &Message,
        outcome: ReceiptOutcome,
        now: u64,
    ) -> Result<ReceiptEffect> {
        attest(
            self.outbox,
            self.bridge,
            link,
            // We dialled the peer's configured endpoint: it is the party of this connection.
            &TransportParty::from_verified_connection(self.peer.party()),
            message,
            outcome,
            now,
        )
    }

    fn refuse(&self, link: &Link, actor: &str, now: u64, code: &'static str) -> Result<()> {
        let event = LinkEvent {
            link: link.id,
            kind: LinkEventKind::Refused,
            actor: actor.to_owned(),
            at: now,
            revision: link.version,
            detail: Some(code),
        };
        record_event(self.audit, &event, "record a failed delivery").map_err(|_| {
            Error::Exchange(ExchangeError::AuditFailed {
                action: "record a failed delivery",
            })
        })
    }
}

fn attest(
    outbox: &Outbox<'_>,
    bridge: &dyn IdentitySigner,
    link: &Link,
    peer: &TransportParty,
    message: &Message,
    outcome: ReceiptOutcome,
    now: u64,
) -> Result<ReceiptEffect> {
    let receipt = Receipt::sign(
        bridge,
        link.id,
        message.id,
        message.conversation.clone(),
        message.seq,
        outcome,
        now,
    );
    Ok(outbox.on_receipt_from(link, peer, &receipt, now)?)
}

/// Applies a business acknowledgement the peer sent for one of our messages (an inbound
/// `ack`), through the same receipt path as a synchronous answer.
///
/// # Errors
/// [`Error::PeerMismatch`] when `bridge` is not the peer, [`ExchangeError::NotFound`] (as an
/// exchange error) for a message this link never sent, or an exchange failure.
pub fn apply_ack(
    outbox: &Outbox<'_>,
    store: &dyn OutboxStore,
    bridge: &dyn IdentitySigner,
    peer: &ExternalPeer,
    link: &Link,
    ack: &InboundAck,
    now: u64,
) -> Result<ReceiptEffect> {
    if bridge.party() != peer.party() {
        return Err(Error::PeerMismatch);
    }
    let entry = store
        .find(&ack.message)?
        .filter(|entry| entry.message.link == link.id)
        .ok_or(Error::Exchange(ExchangeError::NotFound))?;
    attest(
        outbox,
        bridge,
        link,
        // The caller verified the inbound signature with this peer's credential.
        &TransportParty::from_verified_connection(peer.party()),
        &entry.message,
        ack.outcome,
        now,
    )
}
