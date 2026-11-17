//! Handover adoption wiring (ADR-063 section 5 and amendment 1; WP-RTB-02.T10).
//!
//! A `handover` record is owned by the node that created it until the hub **adopts** it, then by
//! the hub. What the hub does when such a record arrives depends on the preset's `adopt` setting:
//!
//! * `manual` (the default): the record is queued for a person at the hub to review; nothing is
//!   requested here;
//! * `auto`: the hub issues the adoption request as soon as the record arrives;
//! * `never`: the record stays owned by its creating node for ever; nothing is requested.
//!
//! The decision is pure ([`decide`]); issuing a request is the [`AdoptionPort`] the hub's record
//! module provides, so this crate does not depend on how a request is stored. An arrival that is
//! not a handover record (any other class) is never touched.

use spot_sync_policy::{AdoptMode, Class, PresetPolicy};

/// What to do with a record that just arrived at the hub.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrival {
    /// Not a handover record: nothing to adopt.
    NotHandover,
    /// Left for a person at the hub to review (`manual`).
    Review,
    /// The adoption request is to be issued now (`auto`).
    Adopt,
    /// Stays owned by the creating node (`never`).
    Stays,
}

/// The decision for a record of `class` under `preset` (`None`: the preset's default, `manual`).
///
/// Library note: `spot_sync_policy::Class` has no `handover`; its `Shared` class is the one whose
/// ownership moves, so a handover is read as `Shared` here (reported as an open question).
#[must_use]
pub fn decide(class: Class, preset: Option<&PresetPolicy>) -> Arrival {
    if class != Class::Shared {
        return Arrival::NotHandover;
    }
    match preset.map_or(AdoptMode::Manual, PresetPolicy::adopt_mode) {
        AdoptMode::Manual => Arrival::Review,
        AdoptMode::Auto => Arrival::Adopt,
        AdoptMode::Never => Arrival::Stays,
    }
}

/// Where an adoption request is issued: the hub's record module.
pub trait AdoptionPort: Send + Sync {
    /// Requests adoption of `entity`/`id`, created at node `origin`.
    ///
    /// # Errors
    /// A message without any value: the request was not stored.
    fn request(&self, entity: &str, id: &str, origin: &str) -> Result<(), String>;
}

/// Applies [`decide`] to an arrival and issues the request when it says so. Returns the decision;
/// a failed request is returned as `Err` so the caller keeps the record queued for review (an
/// `auto` adoption that cannot be requested must not be lost).
///
/// # Errors
/// The port's message.
pub fn on_arrival(
    class: Class,
    preset: Option<&PresetPolicy>,
    port: &dyn AdoptionPort,
    entity: &str,
    id: &str,
    origin: &str,
) -> Result<Arrival, String> {
    let decision = decide(class, preset);
    if decision == Arrival::Adopt {
        port.request(entity, id, origin)?;
    }
    Ok(decision)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Mutex;

    use spot_sync_policy::{AdoptMode, Class, Preset, PresetPolicy};

    use super::{AdoptionPort, Arrival, decide, on_arrival};

    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl AdoptionPort for Recorder {
        fn request(&self, entity: &str, id: &str, origin: &str) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("{entity}/{id}@{origin}"));
            Ok(())
        }
    }

    fn handover(adopt: Option<AdoptMode>) -> PresetPolicy {
        let mut policy = PresetPolicy::new(Preset::Handover);
        policy.adopt = adopt;
        policy
    }

    #[test]
    fn only_a_shared_record_is_a_handover_and_manual_is_the_default() {
        for class in [Class::Local, Class::Master, Class::Transactional] {
            assert_eq!(decide(class, None), Arrival::NotHandover);
        }
        assert_eq!(decide(Class::Shared, None), Arrival::Review);
        assert_eq!(
            decide(Class::Shared, Some(&handover(None))),
            Arrival::Review
        );
    }

    #[test]
    fn auto_requests_adoption_and_never_and_manual_do_not() {
        let port = Recorder::default();
        let auto = handover(Some(AdoptMode::Auto));
        let never = handover(Some(AdoptMode::Never));
        assert_eq!(
            on_arrival(Class::Shared, Some(&auto), &port, "Invoice", "i1", "edge-1").unwrap(),
            Arrival::Adopt
        );
        assert_eq!(
            on_arrival(
                Class::Shared,
                Some(&never),
                &port,
                "Invoice",
                "i2",
                "edge-1"
            )
            .unwrap(),
            Arrival::Stays
        );
        assert_eq!(
            on_arrival(Class::Shared, None, &port, "Invoice", "i3", "edge-1").unwrap(),
            Arrival::Review
        );
        assert_eq!(*port.0.lock().unwrap(), ["Invoice/i1@edge-1"]);
    }

    struct Failing;
    impl AdoptionPort for Failing {
        fn request(&self, _: &str, _: &str, _: &str) -> Result<(), String> {
            Err("not stored".to_owned())
        }
    }

    #[test]
    fn a_failed_auto_adoption_is_reported_not_lost() {
        let auto = handover(Some(AdoptMode::Auto));
        assert!(on_arrival(Class::Shared, Some(&auto), &Failing, "I", "1", "e").is_err());
    }
}
