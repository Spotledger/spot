//! Anonymisation and deletion (ADR-070 section 1.2): a user is never deleted once they have
//! acted; a privacy request overwrites the personal fields in place and keeps the id.

use spot_storage_contract::RecordId;

use crate::error::{self as e, IamError, IamErrors, Result};
use crate::model::User;

/// The display name every anonymised user gets.
pub const ANONYMISED_DISPLAY_NAME: &str = "Anonymised user";

/// The prefix of an anonymised user's sign-in name, followed by their id. The id keeps the name
/// unique per tenant without keeping anything personal.
pub const ANONYMISED_PREFIX: &str = "anonymised-";

fn placeholder_sign_in_name(id: RecordId) -> String {
    format!("{ANONYMISED_PREFIX}{id}")
}

/// Whether a user has been anonymised.
pub fn is_anonymised(user: &User) -> bool {
    user.sign_in_name == placeholder_sign_in_name(user.id)
        && user.display_name == ANONYMISED_DISPLAY_NAME
}

/// A user whose personal fields were overwritten. The wrapper makes the state visible in the
/// type: only [`anonymise`] makes one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnonymisedUser(User);

impl AnonymisedUser {
    /// The anonymised record, ready to store.
    pub fn user(&self) -> &User {
        &self.0
    }

    /// The anonymised record, by value.
    pub fn into_user(self) -> User {
        self.0
    }
}

/// Overwrites the personal fields: the sign-in name and the display name become placeholders,
/// the party link and the manager are dropped, and the user is disabled. The id, the user type
/// and the timestamps are kept, so audit and events still point at the same record.
///
/// Idempotent, and it never changes the id. Linked identities and invitations hold personal
/// data too; the caller removes them (the leaver actions already cancel the invitations).
pub fn anonymise(user: &User) -> AnonymisedUser {
    AnonymisedUser(User {
        id: user.id,
        user_type: user.user_type,
        enabled: false,
        sign_in_name: placeholder_sign_in_name(user.id),
        display_name: ANONYMISED_DISPLAY_NAME.to_owned(),
        party_link: None,
        manager: None,
        created_at: user.created_at,
        deactivated_at: user.deactivated_at,
    })
}

/// What the directory knows about the user's past.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserHistory {
    /// Whether the user ever acted: signed in, created or changed a document, appeared in an
    /// event. The audit trail answers it; this crate never looks.
    pub has_acted: bool,
}

/// Whether the user may be deleted: only one that never acted. Everyone else is deactivated or
/// anonymised, and keeps their id.
pub fn check_delete(user: &User, history: UserHistory) -> Result<()> {
    let _ = user;
    if history.has_acted {
        return Err(IamErrors::one(IamError::new(
            e::HAS_ACTED,
            "a user that has acted is never deleted; deactivate or anonymise them instead",
        )));
    }
    Ok(())
}
