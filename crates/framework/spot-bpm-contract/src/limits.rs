//! Size bounds of a contract.
//!
//! A contract is a document a partner supplies, so it is untrusted input: the
//! bounds below keep every check linear or small-polynomial in a size the
//! reader chose to accept, and keep a hostile document from costing memory or
//! time before any other problem is reported. They are generous for a real
//! conversation (the shipped contracts use a handful of each) and part of the
//! contract format: changing one is a decision, not a tweak.
//!
//! [`exceeded`] lists every bound a contract goes past. [`check`](crate::check)
//! and [`conforms`](crate::conforms) refuse an oversize contract with a
//! `bpm_contract.too_large` problem and do no further work; [`from_text`]
//! refuses oversize text before parsing it and an oversize result after.
//!
//! [`from_text`]: crate::from_text

use crate::check::{Problem, codes};
use crate::model::ProcessContract;

/// The most bytes of contract text [`from_text`](crate::from_text) reads.
pub const MAX_TEXT_BYTES: usize = 256 * 1024;
/// The most roles a contract names.
pub const MAX_ROLES: usize = 32;
/// The most messages a contract defines.
pub const MAX_MESSAGES: usize = 256;
/// The most fields one message carries.
pub const MAX_FIELDS_PER_MESSAGE: usize = 64;
/// The most order rules a contract has.
pub const MAX_ORDER_RULES: usize = 1_024;
/// The most outcomes a contract declares.
pub const MAX_OUTCOMES: usize = 64;
/// The most wait limits a contract sets.
pub const MAX_TIMEOUTS: usize = 256;
/// The most signatures a contract version carries.
pub const MAX_SIGNATURES: usize = 64;
/// The most bytes of any one text in a contract: an id, a name, a topic, a
/// title, a key id, a signature.
pub const MAX_STRING_BYTES: usize = 1_024;

/// Every bound the contract goes past, as `bpm_contract.too_large` problems,
/// sorted and de-duplicated. Empty when the contract is within every bound.
#[must_use]
pub fn exceeded(contract: &ProcessContract) -> Vec<Problem> {
    let mut problems = Vec::new();
    let mut count = |what: &str, len: usize, max: usize| {
        if len > max {
            problems.push(Problem::new(
                codes::TOO_LARGE,
                what,
                format!("the contract has {len} {what}, more than the {max} allowed"),
            ));
        }
    };
    count("roles", contract.participants.len(), MAX_ROLES);
    count("messages", contract.messages.len(), MAX_MESSAGES);
    count("order rules", contract.order.len(), MAX_ORDER_RULES);
    count("outcomes", contract.outcomes.len(), MAX_OUTCOMES);
    count("wait limits", contract.timeouts.len(), MAX_TIMEOUTS);
    count("signatures", contract.signatures.len(), MAX_SIGNATURES);
    for message in &contract.messages {
        count(
            "fields in a message",
            message.fields.len(),
            MAX_FIELDS_PER_MESSAGE,
        );
    }
    if longest_text(contract) > MAX_STRING_BYTES {
        problems.push(Problem::new(
            codes::TOO_LARGE,
            "text",
            format!("a name, topic, title or key in the contract is longer than {MAX_STRING_BYTES} bytes"),
        ));
    }
    problems.sort();
    problems.dedup();
    problems
}

/// The length in bytes of the longest text in the contract.
fn longest_text(contract: &ProcessContract) -> usize {
    let mut longest = contract.id.len();
    let mut take = |text: &str| longest = longest.max(text.len());
    if let Some(title) = &contract.title {
        take(title);
    }
    for participant in &contract.participants {
        take(&participant.role);
    }
    for message in &contract.messages {
        take(&message.name);
        take(&message.topic);
        take(&message.from);
        take(&message.to);
        for field in &message.fields {
            take(&field.name);
        }
    }
    for rule in &contract.order {
        take(&rule.by);
        if let Some(after) = &rule.after {
            take(after);
        }
        if let Some(next) = rule.next_message().or_else(|| rule.next_outcome()) {
            take(next);
        }
    }
    for outcome in &contract.outcomes {
        take(&outcome.name);
    }
    for timeout in &contract.timeouts {
        take(&timeout.message);
        take(&timeout.by);
        if let Some(outcome) = &timeout.outcome {
            take(outcome);
        }
    }
    for signature in &contract.signatures {
        take(&signature.role);
        take(&signature.algorithm);
        take(&signature.key_id);
        take(&signature.signed_hash);
        take(&signature.signature);
    }
    longest
}
