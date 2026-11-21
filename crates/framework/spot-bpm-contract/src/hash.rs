//! The content hash of a contract version (ADR-080 section 5.2).
//!
//! Two contract versions with the same content hash the same whatever their
//! definition order, title or signatures. The hash covers everything a
//! participant must agree to — the id, version, participants, messages with
//! their fields, order rules, outcomes and time limits — and nothing else:
//!
//! - the **title** is descriptive, like a process version's title;
//! - the **signatures** are excluded because a signature covers this hash; a
//!   signature that were part of the hashed content could never be produced.
//!
//! A signature's `signed_hash` is this value, so changing anything a
//! participant agreed to invalidates the signature rather than silently
//! travelling with it.

use spot_bpm_model::ContentHash;

use crate::model::{Field, Next, Participant, ProcessContract, Transition};

impl ProcessContract {
    /// The contract's stable content hash.
    #[must_use]
    pub fn content_hash(&self) -> ContentHash {
        ContentHash::of_canonical(&canonical_bytes(self))
    }
}

/// The canonical byte form of a contract's content: a fixed preamble, then one
/// tab-separated record per line, every set sorted by its natural key.
#[must_use]
pub fn canonical_bytes(contract: &ProcessContract) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"bpm-contract-v1\n");
    put(&mut out, &contract.id);
    out.push(b'\n');
    put(&mut out, &contract.version.to_string());
    out.push(b'\n');

    let mut participants: Vec<&Participant> = contract.participants.iter().collect();
    participants.sort_by(|a, b| (&a.role, a.kind.as_str()).cmp(&(&b.role, b.kind.as_str())));
    for participant in participants {
        out.extend_from_slice(b"participant\t");
        put(&mut out, &participant.role);
        out.push(b'\t');
        put(&mut out, participant.kind.as_str());
        out.push(b'\n');
    }

    let mut messages: Vec<&crate::model::MessageDef> = contract.messages.iter().collect();
    messages.sort_by(|a, b| a.name.cmp(&b.name));
    for message in messages {
        out.extend_from_slice(b"message\t");
        put(&mut out, &message.name);
        out.push(b'\t');
        put(&mut out, &message.topic);
        out.push(b'\t');
        put(&mut out, &message.schema_version.to_string());
        out.push(b'\t');
        put(&mut out, &message.from);
        out.push(b'\t');
        put(&mut out, &message.to);
        out.push(b'\n');
        let mut fields: Vec<&Field> = message.fields.iter().collect();
        fields.sort_by(|a, b| a.name.cmp(&b.name));
        for field in fields {
            out.extend_from_slice(b"field\t");
            put(&mut out, &message.name);
            out.push(b'\t');
            put(&mut out, &field.name);
            out.push(b'\t');
            put(&mut out, field.kind.as_str());
            out.push(b'\t');
            out.push(if field.required { b't' } else { b'f' });
            out.push(b'\t');
            put_u64(&mut out, field.max_len);
            out.push(b'\n');
        }
    }

    let mut order: Vec<&Transition> = contract.order.iter().collect();
    order.sort_by_key(|transition| transition_key(transition));
    for transition in order {
        out.extend_from_slice(b"order\t");
        put(&mut out, transition.after.as_deref().unwrap_or("-"));
        out.push(b'\t');
        match &transition.next {
            Next::Message(name) => {
                out.push(b'm');
                out.push(b'\t');
                put(&mut out, name);
            }
            Next::Outcome(name) => {
                out.push(b'o');
                out.push(b'\t');
                put(&mut out, name);
            }
        }
        out.push(b'\t');
        put(&mut out, &transition.by);
        out.push(b'\t');
        put_u64(&mut out, transition.bound.map(u64::from));
        out.push(b'\n');
    }

    let mut outcomes: Vec<&crate::model::Outcome> = contract.outcomes.iter().collect();
    outcomes.sort_by(|a, b| (&a.name, a.kind.as_str()).cmp(&(&b.name, b.kind.as_str())));
    for outcome in outcomes {
        out.extend_from_slice(b"outcome\t");
        put(&mut out, &outcome.name);
        out.push(b'\t');
        put(&mut out, outcome.kind.as_str());
        out.push(b'\n');
    }

    let mut timeouts: Vec<&crate::model::TimeoutRule> = contract.timeouts.iter().collect();
    timeouts.sort_by(|a, b| (&a.message, &a.by).cmp(&(&b.message, &b.by)));
    for timeout in timeouts {
        out.extend_from_slice(b"timeout\t");
        put(&mut out, &timeout.message);
        out.push(b'\t');
        put(&mut out, &timeout.by);
        out.push(b'\t');
        put(&mut out, &timeout.seconds.to_string());
        out.push(b'\t');
        put(&mut out, timeout.outcome.as_deref().unwrap_or("-"));
        out.push(b'\n');
    }

    out
}

/// The sort key of a transition: what it follows, what it leads to, and who may
/// take it.
fn transition_key(transition: &Transition) -> (String, String, String, String) {
    let (kind, name) = match &transition.next {
        Next::Message(name) => ("m".to_owned(), name.clone()),
        Next::Outcome(name) => ("o".to_owned(), name.clone()),
    };
    (
        transition.after.clone().unwrap_or_default(),
        kind,
        name,
        transition.by.clone(),
    )
}

/// Appends bytes.
fn put(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(text.as_bytes());
}

/// Writes an optional integer or `-`.
fn put_u64(out: &mut Vec<u8>, value: Option<u64>) {
    match value {
        Some(value) => out.extend_from_slice(value.to_string().as_bytes()),
        None => out.push(b'-'),
    }
}
