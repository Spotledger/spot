//! Conditions for stuck exchange (ADR-058 rule 13, ADR-067): plain-language entries in the
//! shape `spot-fleet-health` consumes.
//!
//! One condition per link and kind, with a count, so a long outage raises a handful of lines
//! rather than one per message.

use spot_quarantine::{Condition, Severity};
use spot_tenant_link::{Link, LinkState};

use crate::error::Result;
use crate::state::MessageState;
use crate::store::{InboxStore, OutboxStore};

/// How long is too long.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Thresholds {
    /// A message queued longer than this is stuck.
    pub queued_ms: u64,
    /// A message sent but unconfirmed longer than this is stuck.
    pub sent_ms: u64,
    /// A message the peer holds but has not applied, longer than this, is stuck.
    pub unapplied_ms: u64,
    /// A gap in the inbox older than this is stuck.
    pub parked_ms: u64,
    /// A link expiring within this window is nearly expired.
    pub link_expiry_ms: u64,
}

impl Default for Thresholds {
    fn default() -> Self {
        const HOUR: u64 = 3_600_000;
        Self {
            queued_ms: HOUR,
            sent_ms: HOUR,
            unapplied_ms: 24 * HOUR,
            parked_ms: HOUR,
            link_expiry_ms: 7 * 24 * HOUR,
        }
    }
}

/// The conditions of `links` at `now`.
///
/// # Errors
/// A store failure.
pub fn stuck_messages(
    outbox: &dyn OutboxStore,
    inbox: &dyn InboxStore,
    links: &[Link],
    now: u64,
    thresholds: &Thresholds,
) -> Result<Vec<Condition>> {
    let parked = inbox.parked()?;
    let mut out = Vec::new();
    for link in links {
        let peer = link
            .peer()
            .map_or_else(|| "the peer".to_owned(), ToString::to_string);
        let subject = format!("link {}", link.id);
        let state = link.state_at(now);
        let blocked = matches!(state, LinkState::Revoked | LinkState::Expired);
        let (mut queued, mut sent, mut unapplied) =
            (Aged::default(), Aged::default(), Aged::default());
        for entry in outbox.live(Some(&link.id))? {
            let age = now.saturating_sub(entry.state_at);
            match entry.state {
                MessageState::Queued if age > thresholds.queued_ms => queued.add(age),
                MessageState::Sent if age > thresholds.sent_ms => sent.add(age),
                MessageState::Acknowledged if age > thresholds.unapplied_ms => unapplied.add(age),
                MessageState::Expired | MessageState::Rejected(_)
                    if entry.filler.as_ref().is_some_and(|f| !f.acknowledged)
                        && age > thresholds.sent_ms =>
                {
                    sent.add(age)
                }
                _ => {}
            }
        }
        let severity = if blocked {
            Severity::Red
        } else {
            Severity::Amber
        };
        if queued.count > 0 {
            let why = if blocked {
                "the link is no longer active, so they will not be sent"
            } else {
                "the peer may be unreachable or the topic is no longer granted"
            };
            out.push(Condition {
                code: "LINK_MESSAGES_QUEUED".to_owned(),
                severity,
                subject: subject.clone(),
                sentence: format!(
                    "{} message(s) for {peer} have waited more than {} to be sent; {why}.",
                    queued.count,
                    plain(queued.oldest),
                ),
                action: "Check that the peer is reachable and that the link is active and still grants the topic.".to_owned(),
            });
        }
        if sent.count > 0 {
            out.push(Condition {
                code: "LINK_MESSAGES_UNCONFIRMED".to_owned(),
                severity,
                subject: subject.clone(),
                sentence: format!(
                    "{} message(s) sent to {peer} have gone unconfirmed for more than {}.",
                    sent.count,
                    plain(sent.oldest),
                ),
                action: "Check the connection to the peer; unconfirmed messages are sent again and expire at the end of their retention.".to_owned(),
            });
        }
        if unapplied.count > 0 {
            out.push(Condition {
                code: "LINK_MESSAGES_UNAPPLIED".to_owned(),
                severity: Severity::Amber,
                subject: subject.clone(),
                sentence: format!(
                    "{peer} holds {} message(s) from us that it has not applied for more than {}.",
                    unapplied.count,
                    plain(unapplied.oldest),
                ),
                action: "Ask the peer to check its inbox and quarantine.".to_owned(),
            });
        }
        let mut gap = Aged::default();
        for (parked_link, entry) in &parked {
            let age = now.saturating_sub(entry.parked_at);
            if parked_link == &link.id && age > thresholds.parked_ms {
                gap.add(age);
            }
        }
        if gap.count > 0 {
            out.push(Condition {
                code: "LINK_INBOX_GAP".to_owned(),
                severity: Severity::Amber,
                subject: subject.clone(),
                sentence: format!(
                    "{} message(s) from {peer} have waited more than {} for an earlier message in their conversation.",
                    gap.count,
                    plain(gap.oldest),
                ),
                action: "Ask the peer to send again; if the earlier message expired, it fills the gap by itself when the peer's carrier reconnects.".to_owned(),
            });
        }
        if let Some(expires) = link_end(link)
            && state == LinkState::Active
            && expires > now
            && expires - now <= thresholds.link_expiry_ms
        {
            out.push(Condition {
                code: "LINK_NEARLY_EXPIRED".to_owned(),
                severity: Severity::Amber,
                subject,
                sentence: format!(
                    "The link with {peer} expires in {}; messages cannot cross it after that.",
                    plain(expires - now),
                ),
                action: "Renew the link or let it lapse on purpose.".to_owned(),
            });
        }
    }
    Ok(out)
}

/// When the link stops being usable: its own expiry, or the last grant's.
fn link_end(link: &Link) -> Option<u64> {
    let grants = link.grants.iter().map(|g| g.expires).max();
    match (link.expires, grants) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, b) => b,
    }
}

#[derive(Default)]
struct Aged {
    count: u64,
    oldest: u64,
}

impl Aged {
    fn add(&mut self, age: u64) {
        self.count += 1;
        self.oldest = self.oldest.max(age);
    }
}

/// A duration in the largest whole unit, for a sentence.
fn plain(ms: u64) -> String {
    const MINUTE: u64 = 60_000;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    let (n, unit) = if ms >= DAY {
        (ms / DAY, "day")
    } else if ms >= HOUR {
        (ms / HOUR, "hour")
    } else {
        ((ms / MINUTE).max(1), "minute")
    };
    if n == 1 {
        format!("1 {unit}")
    } else {
        format!("{n} {unit}s")
    }
}
