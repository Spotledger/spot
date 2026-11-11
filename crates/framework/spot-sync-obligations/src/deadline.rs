//! Deadlines and the escalation ladder (ADR-065 sections 3 and 6).

use crate::error::ObligationError;
use crate::model::{LadderOutcome, Level, Notify, Obligation, ObligationSettings, Recipient};

/// The deadline: `published_at + window`; with an effective date, the earlier of that and
/// `effective_from - safety_margin`. Arithmetic saturates, so an extreme input never panics.
#[must_use]
pub fn due_by(obligation: &Obligation, settings: &ObligationSettings) -> i64 {
    let by_window = obligation
        .event
        .published_at_millis
        .saturating_add(settings.delivery_window_millis);
    match obligation.event.effective_from_millis {
        Some(effective) => by_window.min(effective.saturating_sub(settings.safety_margin_millis)),
        None => by_window,
    }
}

/// The level of one obligation at `now_millis`.
///
/// Amber starts at the midpoint between publication and the deadline (50% of the window when no
/// effective date shortens it), overdue at the deadline, red once the grace has elapsed. An
/// effective date within `effective_red_lead_millis` makes it red at once (section 6).
#[must_use]
pub fn level_of(obligation: &Obligation, now_millis: i64, settings: &ObligationSettings) -> Level {
    let due = due_by(obligation, settings);
    if let Some(effective) = obligation.event.effective_from_millis
        && now_millis >= effective.saturating_sub(settings.effective_red_lead_millis)
    {
        return Level::Red;
    }
    if now_millis >= due.saturating_add(settings.grace_millis()) {
        return Level::Red;
    }
    if now_millis >= due {
        return Level::Overdue;
    }
    let published = obligation.event.published_at_millis;
    let half = (due.saturating_sub(published)).max(0) / 2;
    if now_millis >= published.saturating_add(half) {
        Level::Amber
    } else {
        Level::Ok
    }
}

/// Evaluate a node's obligations and return its level plus the notifications to send.
///
/// The node level is the worst obligation level. Notifications are produced only when that level
/// rises above `last_notified`: reaching overdue notifies the node contact, reaching red notifies
/// `sync_admin` (and the contact too when both thresholds are crossed at once). Calling again at
/// the same level, or at a lower one, returns no notifications, so the caller can retry safely.
///
/// # Errors
/// [`ObligationError::SettingsInvalid`] when the settings are out of range.
pub fn ladder(
    node: &str,
    obligations: &[Obligation],
    now_millis: i64,
    settings: &ObligationSettings,
    last_notified: Level,
) -> Result<LadderOutcome, ObligationError> {
    settings.validate()?;
    let levels: Vec<(u64, Level)> = obligations
        .iter()
        .map(|o| (o.event.position, level_of(o, now_millis, settings)))
        .collect();
    let level = levels.iter().map(|(_, l)| *l).max().unwrap_or(Level::Ok);
    let mut notifications = Vec::new();
    if level > last_notified {
        for (who, at) in [
            (Recipient::NodeContact, Level::Overdue),
            (Recipient::SyncAdmin, Level::Red),
        ] {
            if level >= at && last_notified < at {
                let mut positions: Vec<u64> = levels
                    .iter()
                    .filter(|(_, l)| *l >= at)
                    .map(|(p, _)| *p)
                    .collect();
                positions.sort_unstable();
                notifications.push(Notify {
                    who,
                    level: at,
                    node: node.to_owned(),
                    obligations: positions,
                });
            }
        }
    }
    Ok(LadderOutcome {
        level,
        notifications,
    })
}
