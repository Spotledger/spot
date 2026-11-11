//! The effect classes a platform host knows (WP-EXT-02.T5).
//!
//! One list, shared by every host that builds a strict registry (`spot-cli`,
//! `spot-app-compose`), so the copies cannot drift apart. The names are the keys of the hold
//! policy table (`spot-effect-hold`) and the sandbox port list (`spot-sandbox`).

use spot_app_api::EffectClassName;

/// The platform effect classes, without duplicates, in the order the hold policy documents them
/// (the order the pinning tests in `spot-cli` and `spot-app-compose` already assert). An app that adds a class widens this
/// list in the same change that adds the class.
pub const PLATFORM_EFFECT_CLASSES: &[&str] = &[
    "mail",
    "bank_api",
    "webhook",
    "connector",
    "sms",
    "push",
    "edge_sync",
    "fiscal_transmission",
];

/// [`PLATFORM_EFFECT_CLASSES`] as validated names, ready for
/// [`RegistryBuilder::known_effect_classes`](crate::RegistryBuilder::known_effect_classes).
///
/// Fails closed: a name the contract refuses is left out, so nothing declaring it is accepted
/// (a test checks that none is refused).
#[must_use]
pub fn platform_effect_classes() -> Vec<EffectClassName> {
    PLATFORM_EFFECT_CLASSES
        .iter()
        .filter_map(|class| EffectClassName::new(class).ok())
        .collect()
}
