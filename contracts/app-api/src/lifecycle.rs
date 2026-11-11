//! Lifecycle operations, phases, hook points, their order, and what each phase may do (ADR-51).
//!
//! Every operation runs four phases. The first three run inside the business transaction; the
//! fourth runs after commit, delivered from the outbox (ADR-11, ADR-056):
//!
//! | Phase | Hook point | Role | May |
//! | --- | --- | --- | --- |
//! | 1 | `before_<op>` | Derive | change its own document |
//! | 2 | `validate_<op>` | Validate | read anything, raise errors; never write |
//! | 3 | `after_<op>` | Act | change other documents through their lifecycle, enqueue jobs and events |
//! | 4 | `<op>_committed` | Notify | external effects; never rolls the document back; must be idempotent |
//!
//! `before_save`, `validate_save`, `after_save` and `save_committed` fire on both create and
//! update. `load` and `field_changed` are read-only events outside any transaction.
//! [`sequence`] gives the exact order, including where the write and the commit happen.
//! Hosts enforce [`PhaseRules`] at runtime for every controller kind (native, WASM, script).

pub use spot_events_contract::lifecycle::Operation;

/// A phase of an operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Phase {
    /// Phase 1, derive: in the transaction, before the write.
    Before,
    /// Phase 2, validate: in the transaction, before the write.
    Validate,
    /// Phase 3, act: in the transaction, after the write.
    After,
    /// Phase 4, notify: after commit, from the outbox.
    Committed,
    /// Read-only form events (`load`, `field_changed`), outside a transaction.
    Read,
}

impl Phase {
    /// The phase's name, as used in messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Before => "before",
            Phase::Validate => "validate",
            Phase::After => "after",
            Phase::Committed => "committed",
            Phase::Read => "read",
        }
    }

    /// What code running in this phase may do.
    pub fn rules(self) -> PhaseRules {
        match self {
            Phase::Before => PhaseRules {
                modify_own: true,
                read: true,
                write_others: false,
                enqueue: false,
                external: false,
            },
            Phase::Validate => PhaseRules {
                modify_own: false,
                read: true,
                write_others: false,
                enqueue: false,
                external: false,
            },
            Phase::After => PhaseRules {
                modify_own: false,
                read: true,
                write_others: true,
                enqueue: true,
                external: false,
            },
            Phase::Committed => PhaseRules {
                modify_own: false,
                read: true,
                write_others: true,
                enqueue: true,
                external: true,
            },
            Phase::Read => PhaseRules {
                modify_own: false,
                read: true,
                write_others: false,
                enqueue: false,
                external: false,
            },
        }
    }
}

/// What a phase allows. A host checks these on every [`HookContext`](crate::HookContext) call
/// and fails a disallowed one with `platform.phase_violation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhaseRules {
    /// Change the document the hook runs for.
    pub modify_own: bool,
    /// Load documents and run queries.
    pub read: bool,
    /// Create, change or delete *other* documents, always through their own lifecycle. In the
    /// committed phase each such write is its own new transaction.
    pub write_others: bool,
    /// Enqueue jobs, app events and realtime notices; in phase 3 they are delivered only if the
    /// transaction commits.
    pub enqueue: bool,
    /// Call systems outside the platform (e-mail, webhooks, HTTP).
    pub external: bool,
}

/// What a hook point applies to: one operation, or saving (create and update).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    /// One operation.
    Op(Operation),
    /// Both create and update.
    Save,
}

/// A point where controller steps run.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum HookPoint {
    /// A phase of an operation, or of saving.
    Lifecycle {
        /// Which phase (never `Read`).
        phase: Phase,
        /// Which operation.
        target: Target,
    },
    /// The document is opened (read-only).
    Load,
    /// A field was changed in a form; may adjust the unsaved draft, never storage.
    FieldChanged(String),
}

impl HookPoint {
    /// A lifecycle hook point.
    pub fn of(phase: Phase, target: Target) -> Self {
        HookPoint::Lifecycle { phase, target }
    }

    /// The phase code at this point runs in.
    pub fn phase(&self) -> Phase {
        match self {
            HookPoint::Lifecycle { phase, .. } => *phase,
            HookPoint::Load | HookPoint::FieldChanged(_) => Phase::Read,
        }
    }

    /// The ADR-51 name: `before_create`, `validate_save`, `post_committed`, `load`,
    /// `field_changed:customer`.
    pub fn name(&self) -> String {
        match self {
            HookPoint::Lifecycle { phase, target } => {
                let t = match target {
                    Target::Save => "save",
                    Target::Op(op) => op_name(*op),
                };
                match phase {
                    Phase::Committed => format!("{t}_committed"),
                    p => format!("{}_{t}", p.as_str()),
                }
            }
            HookPoint::Load => "load".to_owned(),
            HookPoint::FieldChanged(f) => format!("field_changed:{f}"),
        }
    }
}

/// The ADR-51 name of an operation.
pub fn op_name(op: Operation) -> &'static str {
    match op {
        Operation::Create => "create",
        Operation::Update => "update",
        Operation::Post => "post",
        Operation::Cancel => "cancel",
        Operation::Delete => "delete",
        Operation::Rename => "rename",
        Operation::UpdatePosted => "update_posted",
    }
}

/// One step of an operation's run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Controller steps at this hook point run.
    Hook(HookPoint),
    /// The document is written (for post: its state becomes posted and its ledger entries are
    /// written; for delete: it is removed).
    Write,
    /// The transaction commits.
    Commit,
}

/// The exact order of one operation (ADR-51):
///
/// ```text
/// create:  before_create → before_save → validate_create → validate_save → [write]
///          → after_create → after_save → [commit] → create_committed → save_committed
/// update:  the same with update
/// others:  before_<op> → validate_<op> → [write] → after_<op> → [commit] → <op>_committed
/// ```
pub fn sequence(op: Operation) -> Vec<Stage> {
    use Phase::*;
    let hook = |p, t| Stage::Hook(HookPoint::of(p, t));
    let o = Target::Op(op);
    if matches!(op, Operation::Create | Operation::Update) {
        vec![
            hook(Before, o),
            hook(Before, Target::Save),
            hook(Validate, o),
            hook(Validate, Target::Save),
            Stage::Write,
            hook(After, o),
            hook(After, Target::Save),
            Stage::Commit,
            hook(Committed, o),
            hook(Committed, Target::Save),
        ]
    } else {
        vec![
            hook(Before, o),
            hook(Validate, o),
            Stage::Write,
            hook(After, o),
            Stage::Commit,
            hook(Committed, o),
        ]
    }
}

/// The order of *correct* (ADR-42: cancel and amend as one step). The original's cancel and the
/// amended draft's create run phases 1–3 in **one** transaction; then both committed phases run.
/// A failure anywhere before the commit leaves the original posted and creates nothing.
pub fn correct_sequence() -> Vec<(CorrectPart, Stage)> {
    let (in_tx_cancel, post_cancel) = split_at_commit(sequence(Operation::Cancel));
    let (in_tx_create, post_create) = split_at_commit(sequence(Operation::Create));
    let mut out: Vec<(CorrectPart, Stage)> = Vec::new();
    out.extend(in_tx_cancel.into_iter().map(|s| (CorrectPart::Original, s)));
    out.extend(
        in_tx_create
            .into_iter()
            .map(|s| (CorrectPart::Amendment, s)),
    );
    out.push((CorrectPart::Both, Stage::Commit));
    out.extend(post_cancel.into_iter().map(|s| (CorrectPart::Original, s)));
    out.extend(post_create.into_iter().map(|s| (CorrectPart::Amendment, s)));
    out
}

/// Which document a stage of [`correct_sequence`] is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CorrectPart {
    /// The posted document being cancelled.
    Original,
    /// The amended draft being created.
    Amendment,
    /// The shared commit.
    Both,
}

fn split_at_commit(stages: Vec<Stage>) -> (Vec<Stage>, Vec<Stage>) {
    let at = stages
        .iter()
        .position(|s| *s == Stage::Commit)
        .unwrap_or(stages.len());
    let mut before = stages;
    let after = before.split_off(at);
    (before, after.into_iter().skip(1).collect())
}
