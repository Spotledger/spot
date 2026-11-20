//! The pure reference model: what must happen for every call, before the engine is asked.
//!
//! Documents are named by **slot** (`te`, `task`), a small index chosen by the generator, so a
//! sequence stays meaningful when the shrinker drops calls. A call on a slot that was never
//! created is [`Expectation::Skip`] (the runner does not make it).
//!
//! # Rules (each with its source)
//!
//! - **R1 versions.** A document starts at version 1 and every committed write adds 1; a refusal
//!   changes nothing (ADR-42, optimistic versions; the engine's `stale` check).
//! - **R2 order of refusals.** A missing document is `NotFound`; then a stale version is `Stale`
//!   (the engine checks the version before the state and before the permission); then the state;
//!   then the permission (`engine/ops.rs` order, pinned by `harness/rust/l3-documents` cases).
//! - **R3 states.** A Time Entry is `Draft`, `Posted` or `Cancelled` (ADR-42). `post` only a
//!   draft, `cancel` only a posted one, `correct` only a posted one, `delete` only a draft.
//! - **R4 update.** A draft accepts a patch; a posted document refuses a field that is not
//!   flagged as allowed on posted (`FixedField`: `InvalidState` or `Validation`); a cancelled one
//!   is `InvalidState`. An update always commits a version, even with an unchanged value: the
//!   engine writes the document on every accepted update.
//! - **R5 correct.** Cancels the posted original (version +1) and creates one draft in the same
//!   step, whose `amended_from` is the original, version 1, same task and hours (ADR-42). Two
//!   lifecycle events.
//! - **R6 links.** A Time Entry's `task` must be a live Task (`InvalidLink`, a validation
//!   refusal); a Task that a Time Entry (in any state) still links cannot be deleted (`Linked`).
//! - **R7 rename.** A Task rename keeps the id and changes the title (`subject`), version +1.
//! - **R8 permission.** A principal without the right is refused when the call would otherwise
//!   have been accepted (`Denied`), and nothing changes.

use std::collections::BTreeMap;

use spot_app_api::ErrorClass;

/// Who makes the call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Who {
    /// A Project Manager: every right on the app DocTypes.
    Manager,
    /// A Viewer: read only.
    Viewer,
}

/// Which version the call sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ver {
    /// The version the document has.
    Current,
    /// A version it does not have.
    Stale,
}

/// One call of a sequence. Documents are slots, see the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Call {
    /// Creates Task `slot` in the seeded project.
    CreateTask {
        /// The slot the document takes.
        slot: u8,
    },
    /// Creates Time Entry `slot` linked to Task `task`.
    CreateTe {
        /// The slot the document takes.
        slot: u8,
        /// The Task slot.
        task: u8,
    },
    /// Sets the hours of a Time Entry.
    SetHours {
        /// The Time Entry slot.
        te: u8,
        /// The hours to set.
        hours: u8,
        /// The version the call carries.
        ver: Ver,
        /// The principal making the call.
        who: Who,
    },
    /// Posts a Time Entry.
    Post {
        /// The Time Entry slot.
        te: u8,
        /// The version the call carries.
        ver: Ver,
        /// The principal making the call.
        who: Who,
    },
    /// Cancels a Time Entry.
    Cancel {
        /// The Time Entry slot.
        te: u8,
        /// The version the call carries.
        ver: Ver,
        /// The principal making the call.
        who: Who,
    },
    /// Corrects a Time Entry into a new draft in slot `into`.
    Correct {
        /// The Time Entry slot.
        te: u8,
        /// The slot of the new draft.
        into: u8,
        /// The version the call carries.
        ver: Ver,
        /// The principal making the call.
        who: Who,
    },
    /// Deletes a Time Entry.
    DeleteTe {
        /// The Time Entry slot.
        te: u8,
        /// The version the call carries.
        ver: Ver,
    },
    /// Changes the subject of a Task.
    SetSubject {
        /// The Task slot.
        task: u8,
        /// A tag that makes the subject distinct.
        tag: u16,
        /// The version the call carries.
        ver: Ver,
        /// The principal making the call.
        who: Who,
    },
    /// Renames a Task.
    RenameTask {
        /// The Task slot.
        task: u8,
        /// A tag that makes the subject distinct.
        tag: u16,
        /// The version the call carries.
        ver: Ver,
        /// The principal making the call.
        who: Who,
    },
    /// Deletes a Task.
    DeleteTask {
        /// The Task slot.
        task: u8,
        /// The version the call carries.
        ver: Ver,
    },
}

/// Why a call is refused, in the model's terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The document does not exist (`document.not_found`).
    NotFound,
    /// The version is not current (`document.stale_version`).
    Stale,
    /// The state does not allow it (`document.invalid_state`).
    InvalidState,
    /// A fixed field of a posted document (`InvalidState` or `Validation`).
    FixedField,
    /// Other documents link to this one (`document.linked`).
    Linked,
    /// A link points at a missing document (`document.invalid_link`, validation).
    InvalidLink,
    /// The principal may not (`permission.denied`).
    Denied,
}

impl Refusal {
    /// Whether the engine's error class is the one this refusal stands for.
    pub fn accepts(self, class: ErrorClass) -> bool {
        match self {
            Refusal::NotFound => class == ErrorClass::NotFound,
            Refusal::Stale => class == ErrorClass::StaleVersion,
            Refusal::InvalidState => class == ErrorClass::InvalidState,
            Refusal::FixedField => {
                matches!(class, ErrorClass::InvalidState | ErrorClass::Validation)
            }
            Refusal::Linked => class == ErrorClass::Linked,
            Refusal::InvalidLink => class == ErrorClass::Validation,
            Refusal::Denied => class == ErrorClass::PermissionDenied,
        }
    }
}

/// What the model says must happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expectation {
    /// A slot the sequence never created: the call is not made.
    Skip,
    /// The call commits and writes this many lifecycle events.
    Ok {
        /// How many lifecycle events the call writes.
        events: u32,
    },
    /// The call is refused and changes nothing.
    Refused(Refusal),
}

/// The state of a Time Entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum St {
    /// Editable.
    Draft,
    /// Submitted.
    Posted,
    /// Cancelled.
    Cancelled,
}

/// A Time Entry in the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Te {
    /// Whether it exists (a deleted one does not).
    pub exists: bool,
    /// Its state.
    pub state: St,
    /// Its version.
    pub version: u64,
    /// Its hours (whole).
    pub hours: u8,
    /// The Task slot it links.
    pub task: u8,
    /// The slot of the document it amends.
    pub amended_from: Option<u8>,
}

/// A Task in the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    /// Whether it exists.
    pub exists: bool,
    /// Its version.
    pub version: u64,
    /// Its subject, `None` until a call changes it (the creation subject is the runner's).
    pub subject: Option<String>,
}

/// A deliberately wrong rule, to prove the shrinker and the harness can see a wrong model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bug {
    /// No bug: the real rules.
    None,
    /// The model thinks a draft may be cancelled.
    CancelOfDraftAllowed,
}

/// The reference model.
#[derive(Clone, Debug)]
pub struct Model {
    /// The Time Entries by slot.
    pub tes: BTreeMap<u8, Te>,
    /// The Tasks by slot.
    pub tasks: BTreeMap<u8, Task>,
    bug: Bug,
}

impl Model {
    /// The model with the real rules.
    pub fn new() -> Model {
        Model::with_bug(Bug::None)
    }

    /// The model with one wrong rule.
    pub fn with_bug(bug: Bug) -> Model {
        Model {
            tes: BTreeMap::new(),
            tasks: BTreeMap::new(),
            bug,
        }
    }

    fn te(&self, slot: u8) -> Option<&Te> {
        self.tes.get(&slot)
    }

    /// Applies `call` and says what must happen. A refused or skipped call changes nothing.
    pub fn step(&mut self, call: &Call) -> Expectation {
        match *call {
            Call::CreateTask { slot } => {
                if self.tasks.contains_key(&slot) {
                    return Expectation::Skip;
                }
                self.tasks.insert(
                    slot,
                    Task {
                        exists: true,
                        version: 1,
                        subject: None,
                    },
                );
                Expectation::Ok { events: 1 }
            }
            Call::CreateTe { slot, task } => {
                if self.tes.contains_key(&slot) || !self.tasks.contains_key(&task) {
                    return Expectation::Skip;
                }
                if !self.tasks.get(&task).is_some_and(|t| t.exists) {
                    return Expectation::Refused(Refusal::InvalidLink);
                }
                self.tes.insert(
                    slot,
                    Te {
                        exists: true,
                        state: St::Draft,
                        version: 1,
                        hours: 2,
                        task,
                        amended_from: None,
                    },
                );
                Expectation::Ok { events: 1 }
            }
            Call::SetHours {
                te,
                hours,
                ver,
                who,
            } => self.te_write(
                te,
                ver,
                who,
                |t| match t.state {
                    St::Draft => Ok(()),
                    St::Posted => Err(Refusal::FixedField),
                    St::Cancelled => Err(Refusal::InvalidState),
                },
                |t| {
                    t.hours = hours;
                    t.version += 1;
                },
            ),
            Call::Post { te, ver, who } => self.te_write(
                te,
                ver,
                who,
                |t| match t.state {
                    St::Draft => Ok(()),
                    _ => Err(Refusal::InvalidState),
                },
                |t| {
                    t.state = St::Posted;
                    t.version += 1;
                },
            ),
            Call::Cancel { te, ver, who } => {
                let bug = self.bug == Bug::CancelOfDraftAllowed;
                self.te_write(
                    te,
                    ver,
                    who,
                    move |t| match t.state {
                        St::Posted => Ok(()),
                        St::Draft if bug => Ok(()),
                        _ => Err(Refusal::InvalidState),
                    },
                    |t| {
                        t.state = St::Cancelled;
                        t.version += 1;
                    },
                )
            }
            Call::Correct { te, into, ver, who } => {
                if self.tes.contains_key(&into) {
                    return Expectation::Skip;
                }
                let original = self.te(te).cloned();
                let verdict = self.te_write(
                    te,
                    ver,
                    who,
                    |t| match t.state {
                        St::Posted => Ok(()),
                        _ => Err(Refusal::InvalidState),
                    },
                    |t| {
                        t.state = St::Cancelled;
                        t.version += 1;
                    },
                );
                if let (Expectation::Ok { .. }, Some(o)) = (verdict, original) {
                    self.tes.insert(
                        into,
                        Te {
                            exists: true,
                            state: St::Draft,
                            version: 1,
                            hours: o.hours,
                            task: o.task,
                            amended_from: Some(te),
                        },
                    );
                    return Expectation::Ok { events: 2 };
                }
                verdict
            }
            Call::DeleteTe { te, ver } => {
                let Some(t) = self.tes.get(&te) else {
                    return Expectation::Skip;
                };
                if !t.exists {
                    return Expectation::Refused(Refusal::NotFound);
                }
                if ver == Ver::Stale {
                    return Expectation::Refused(Refusal::Stale);
                }
                if t.state != St::Draft {
                    return Expectation::Refused(Refusal::InvalidState);
                }
                if let Some(t) = self.tes.get_mut(&te) {
                    t.exists = false;
                }
                Expectation::Ok { events: 1 }
            }
            Call::SetSubject {
                task,
                tag,
                ver,
                who,
            }
            | Call::RenameTask {
                task,
                tag,
                ver,
                who,
            } => {
                let Some(t) = self.tasks.get(&task) else {
                    return Expectation::Skip;
                };
                if !t.exists {
                    return Expectation::Refused(Refusal::NotFound);
                }
                if ver == Ver::Stale {
                    return Expectation::Refused(Refusal::Stale);
                }
                if who == Who::Viewer {
                    return Expectation::Refused(Refusal::Denied);
                }
                let kind = if matches!(call, Call::RenameTask { .. }) {
                    "renamed"
                } else {
                    "subject"
                };
                if let Some(t) = self.tasks.get_mut(&task) {
                    t.version += 1;
                    t.subject = Some(format!("{kind}-{tag}"));
                }
                Expectation::Ok { events: 1 }
            }
            Call::DeleteTask { task, ver } => {
                let Some(t) = self.tasks.get(&task) else {
                    return Expectation::Skip;
                };
                if !t.exists {
                    return Expectation::Refused(Refusal::NotFound);
                }
                if ver == Ver::Stale {
                    return Expectation::Refused(Refusal::Stale);
                }
                if self.tes.values().any(|e| e.exists && e.task == task) {
                    return Expectation::Refused(Refusal::Linked);
                }
                if let Some(t) = self.tasks.get_mut(&task) {
                    t.exists = false;
                }
                Expectation::Ok { events: 1 }
            }
        }
    }

    /// The shared shape of a Time Entry write: missing, stale, state, permission, then apply.
    fn te_write(
        &mut self,
        slot: u8,
        ver: Ver,
        who: Who,
        state_ok: impl Fn(&Te) -> Result<(), Refusal>,
        apply: impl Fn(&mut Te),
    ) -> Expectation {
        let Some(t) = self.tes.get(&slot) else {
            return Expectation::Skip;
        };
        if !t.exists {
            return Expectation::Refused(Refusal::NotFound);
        }
        if ver == Ver::Stale {
            return Expectation::Refused(Refusal::Stale);
        }
        if let Err(r) = state_ok(t) {
            return Expectation::Refused(r);
        }
        if who == Who::Viewer {
            return Expectation::Refused(Refusal::Denied);
        }
        if let Some(t) = self.tes.get_mut(&slot) {
            apply(t);
        }
        Expectation::Ok { events: 1 }
    }
}

impl Default for Model {
    fn default() -> Self {
        Model::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn cur() -> Ver {
        Ver::Current
    }

    fn base() -> Model {
        let mut m = Model::new();
        m.step(&Call::CreateTask { slot: 0 });
        m.step(&Call::CreateTe { slot: 0, task: 0 });
        m
    }

    #[test]
    fn a_new_time_entry_is_a_draft_at_version_one() {
        let m = base();
        let t = m.tes.get(&0).unwrap();
        assert_eq!((t.state, t.version), (St::Draft, 1));
    }

    #[test]
    fn post_then_cancel_then_nothing_more() {
        let mut m = base();
        assert_eq!(
            m.step(&Call::Post {
                te: 0,
                ver: cur(),
                who: Who::Manager
            }),
            Expectation::Ok { events: 1 }
        );
        assert_eq!(
            m.step(&Call::Post {
                te: 0,
                ver: cur(),
                who: Who::Manager
            }),
            Expectation::Refused(Refusal::InvalidState)
        );
        assert_eq!(
            m.step(&Call::Cancel {
                te: 0,
                ver: cur(),
                who: Who::Manager
            }),
            Expectation::Ok { events: 1 }
        );
        assert_eq!(
            m.step(&Call::Cancel {
                te: 0,
                ver: cur(),
                who: Who::Manager
            }),
            Expectation::Refused(Refusal::InvalidState)
        );
        assert_eq!(m.tes.get(&0).unwrap().version, 3);
    }

    #[test]
    fn a_refusal_changes_nothing() {
        let mut m = base();
        let before = m.tes.get(&0).cloned();
        m.step(&Call::Cancel {
            te: 0,
            ver: cur(),
            who: Who::Manager,
        });
        m.step(&Call::Post {
            te: 0,
            ver: Ver::Stale,
            who: Who::Manager,
        });
        m.step(&Call::Post {
            te: 0,
            ver: cur(),
            who: Who::Viewer,
        });
        assert_eq!(m.tes.get(&0).cloned(), before);
    }

    #[test]
    fn stale_comes_before_state_and_state_before_permission() {
        let mut m = base();
        assert_eq!(
            m.step(&Call::Cancel {
                te: 0,
                ver: Ver::Stale,
                who: Who::Manager
            }),
            Expectation::Refused(Refusal::Stale)
        );
        assert_eq!(
            m.step(&Call::Cancel {
                te: 0,
                ver: cur(),
                who: Who::Viewer
            }),
            Expectation::Refused(Refusal::InvalidState)
        );
        assert_eq!(
            m.step(&Call::Post {
                te: 0,
                ver: cur(),
                who: Who::Viewer
            }),
            Expectation::Refused(Refusal::Denied)
        );
    }

    #[test]
    fn correct_cancels_the_original_and_makes_one_draft() {
        let mut m = base();
        m.step(&Call::Post {
            te: 0,
            ver: cur(),
            who: Who::Manager,
        });
        assert_eq!(
            m.step(&Call::Correct {
                te: 0,
                into: 1,
                ver: cur(),
                who: Who::Manager
            }),
            Expectation::Ok { events: 2 }
        );
        assert_eq!(m.tes.get(&0).unwrap().state, St::Cancelled);
        let d = m.tes.get(&1).unwrap();
        assert_eq!(
            (d.state, d.version, d.amended_from),
            (St::Draft, 1, Some(0))
        );
        // A correct of a draft, and of a cancelled document, is refused.
        assert_eq!(
            m.step(&Call::Correct {
                te: 1,
                into: 2,
                ver: cur(),
                who: Who::Manager
            }),
            Expectation::Refused(Refusal::InvalidState)
        );
        assert_eq!(
            m.step(&Call::Correct {
                te: 0,
                into: 3,
                ver: cur(),
                who: Who::Manager
            }),
            Expectation::Refused(Refusal::InvalidState)
        );
    }

    #[test]
    fn delete_only_a_draft_and_never_a_linked_task() {
        let mut m = base();
        assert_eq!(
            m.step(&Call::DeleteTask {
                task: 0,
                ver: cur()
            }),
            Expectation::Refused(Refusal::Linked)
        );
        m.step(&Call::Post {
            te: 0,
            ver: cur(),
            who: Who::Manager,
        });
        assert_eq!(
            m.step(&Call::DeleteTe { te: 0, ver: cur() }),
            Expectation::Refused(Refusal::InvalidState)
        );
        m.step(&Call::Cancel {
            te: 0,
            ver: cur(),
            who: Who::Manager,
        });
        // A cancelled entry still links its task.
        assert_eq!(
            m.step(&Call::DeleteTask {
                task: 0,
                ver: cur()
            }),
            Expectation::Refused(Refusal::Linked)
        );
        m.step(&Call::CreateTe { slot: 1, task: 0 });
        assert_eq!(
            m.step(&Call::DeleteTe { te: 1, ver: cur() }),
            Expectation::Ok { events: 1 }
        );
        assert_eq!(
            m.step(&Call::DeleteTe { te: 1, ver: cur() }),
            Expectation::Refused(Refusal::NotFound)
        );
    }

    #[test]
    fn a_link_to_a_deleted_task_is_refused_and_a_posted_update_is_a_fixed_field() {
        let mut m = Model::new();
        m.step(&Call::CreateTask { slot: 0 });
        assert_eq!(
            m.step(&Call::DeleteTask {
                task: 0,
                ver: cur()
            }),
            Expectation::Ok { events: 1 }
        );
        assert_eq!(
            m.step(&Call::CreateTe { slot: 0, task: 0 }),
            Expectation::Refused(Refusal::InvalidLink)
        );
        let mut m = base();
        m.step(&Call::Post {
            te: 0,
            ver: cur(),
            who: Who::Manager,
        });
        assert_eq!(
            m.step(&Call::SetHours {
                te: 0,
                hours: 3,
                ver: cur(),
                who: Who::Manager
            }),
            Expectation::Refused(Refusal::FixedField)
        );
    }

    #[test]
    fn rename_and_subject_bump_the_version_and_a_call_on_a_missing_slot_is_skipped() {
        let mut m = base();
        assert_eq!(
            m.step(&Call::RenameTask {
                task: 0,
                tag: 7,
                ver: cur(),
                who: Who::Manager
            }),
            Expectation::Ok { events: 1 }
        );
        assert_eq!(
            m.tasks.get(&0).unwrap().subject.as_deref(),
            Some("renamed-7")
        );
        assert_eq!(m.tasks.get(&0).unwrap().version, 2);
        assert_eq!(
            m.step(&Call::Post {
                te: 9,
                ver: cur(),
                who: Who::Manager
            }),
            Expectation::Skip
        );
    }

    #[test]
    fn the_wrong_model_allows_cancelling_a_draft() {
        let mut m = Model::with_bug(Bug::CancelOfDraftAllowed);
        m.step(&Call::CreateTask { slot: 0 });
        m.step(&Call::CreateTe { slot: 0, task: 0 });
        assert_eq!(
            m.step(&Call::Cancel {
                te: 0,
                ver: cur(),
                who: Who::Manager
            }),
            Expectation::Ok { events: 1 }
        );
    }
}
