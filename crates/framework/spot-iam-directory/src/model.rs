//! The directory's records: users, linked identities and invitations (ADR-070 section 1).
//!
//! Every type is `serde` with `deny_unknown_fields`, so a persisted or imported record with a
//! key this version does not know is refused, not silently dropped. Validation is separate
//! ([`User::validate`] and friends) and reports every problem at once.

use serde::{Deserialize, Serialize};
use spot_storage_contract::RecordId;
use time::OffsetDateTime;

use crate::error::{self as e, IamError, IamErrors, Result};
use crate::wire;

/// The longest sign-in name: an address, as RFC 5321 allows.
pub const MAX_SIGN_IN_NAME: usize = 254;
/// The longest display name, in characters.
pub const MAX_DISPLAY_NAME: usize = 200;
/// The longest issuer or subject of a linked identity (an OIDC `sub` is at most 255 bytes).
pub const MAX_IDENTITY_PART: usize = 2048;

/// What kind of principal a user is (ADR-070 section 1.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserType {
    /// Staff with desk access.
    Internal,
    /// An outside party reaching only its own records through party scoping.
    Portal,
    /// A non-human account that owns API keys and integration credentials.
    Service,
    /// An AI agent identity.
    Agent,
}

impl UserType {
    /// Whether the user is a person who may be invited by e-mail.
    pub fn is_invitable(self) -> bool {
        matches!(self, Self::Internal | Self::Portal)
    }

    /// Whether the user signs in at all with a secret they know (a password): service and
    /// agent users never do.
    pub fn may_use_password(self) -> bool {
        matches!(self, Self::Internal | Self::Portal)
    }
}

/// A user. The id is stable for ever: it is the `Actor::User { id }` of events and audit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct User {
    /// The stable record id.
    #[serde(with = "wire::record_id")]
    pub id: RecordId,
    /// The kind of principal.
    pub user_type: UserType,
    /// Whether the user may sign in and act. `false` deactivates; the id stays in audit.
    pub enabled: bool,
    /// An e-mail address or an explicit username (not the IdP subject; see [`LinkedIdentity`]).
    pub sign_in_name: String,
    /// The name shown to other people.
    pub display_name: String,
    /// The customer or supplier a portal user belongs to (portal only).
    #[serde(default, with = "wire::opt_record_id")]
    pub party_link: Option<RecordId>,
    /// Who open tasks go to when this user leaves.
    #[serde(default, with = "wire::opt_record_id")]
    pub manager: Option<RecordId>,
    /// When the user was created.
    #[serde(with = "wire::utc")]
    pub created_at: OffsetDateTime,
    /// When the user was last deactivated; `None` while enabled.
    #[serde(default, with = "wire::opt_utc")]
    pub deactivated_at: Option<OffsetDateTime>,
}

impl User {
    /// A new enabled user. Not validated; call [`User::validate`].
    pub fn new(
        id: RecordId,
        user_type: UserType,
        sign_in_name: impl Into<String>,
        display_name: impl Into<String>,
        created_at: OffsetDateTime,
    ) -> Self {
        Self {
            id,
            user_type,
            enabled: true,
            sign_in_name: sign_in_name.into(),
            display_name: display_name.into(),
            party_link: None,
            manager: None,
            created_at,
            deactivated_at: None,
        }
    }

    /// Checks the record, reporting every problem at once.
    pub fn validate(&self) -> Result<()> {
        let mut problems = Vec::new();
        if !is_valid_sign_in_name(&self.sign_in_name) {
            problems.push(IamError::in_field(
                e::INVALID_SIGN_IN_NAME,
                "sign_in_name",
                format!(
                    "the sign-in name must be an e-mail address or a username of lower-case \
                     letters, digits, `.`, `_` and `-` (2 to 64 characters), at most {MAX_SIGN_IN_NAME} characters"
                ),
            ));
        }
        if !is_valid_display_name(&self.display_name) {
            problems.push(IamError::in_field(
                e::INVALID_DISPLAY_NAME,
                "display_name",
                format!(
                    "the display name must have text, no control characters and at most \
                     {MAX_DISPLAY_NAME} characters"
                ),
            ));
        }
        if self.party_link.is_some() && self.user_type != UserType::Portal {
            problems.push(IamError::in_field(
                e::PARTY_LINK,
                "party_link",
                "only a portal user has a party link",
            ));
        }
        if self.manager == Some(self.id) {
            problems.push(IamError::in_field(
                e::MANAGER_SELF,
                "manager",
                "a user cannot be their own manager",
            ));
        }
        if self.enabled && self.deactivated_at.is_some() {
            problems.push(IamError::in_field(
                e::INVALID_STATE,
                "deactivated_at",
                "an enabled user has no deactivation time",
            ));
        }
        if let Some(at) = self.deactivated_at
            && at < self.created_at
        {
            problems.push(IamError::in_field(
                e::INVALID_STATE,
                "deactivated_at",
                "a user cannot be deactivated before they were created",
            ));
        }
        match IamErrors::from_list(problems) {
            Some(errors) => Err(errors),
            None => Ok(()),
        }
    }
}

/// An address-or-username check shared by users and invitations.
fn is_valid_sign_in_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_SIGN_IN_NAME || name != name.trim() {
        return false;
    }
    if name.contains('@') {
        return is_valid_email(name);
    }
    let len = name.chars().count();
    (2..=64).contains(&len)
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

/// A deliberately plain address check: one `@`, text on both sides, a dot in the domain, no
/// spaces or control characters. Whether the mailbox exists is the invitation's business.
pub(crate) fn is_valid_email(address: &str) -> bool {
    if address.is_empty() || address.len() > MAX_SIGN_IN_NAME {
        return false;
    }
    if address.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return false;
    }
    let mut parts = address.split('@');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(local), Some(domain), None) => {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && !domain.contains("..")
        }
        _ => false,
    }
}

fn is_valid_display_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name.chars().count() <= MAX_DISPLAY_NAME
        && !name.chars().any(char::is_control)
}

/// A link between a user and an identity at an external provider (ADR-070 section 1.4).
///
/// The pair `(issuer, subject)` is the key, unique per tenant; a user may have several. A
/// changed work e-mail does not touch it, so the account is never orphaned.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkedIdentity {
    /// The user the identity signs in as.
    #[serde(with = "wire::record_id")]
    pub user: RecordId,
    /// The provider: the OIDC `iss` or the SAML entity id.
    pub issuer: String,
    /// The provider's stable subject for the person: the OIDC `sub` or the SAML name id.
    pub subject: String,
}

/// The key of a linked identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IdentityKey {
    /// The provider.
    pub issuer: String,
    /// The provider's subject.
    pub subject: String,
}

impl LinkedIdentity {
    /// The `(issuer, subject)` key.
    pub fn key(&self) -> IdentityKey {
        IdentityKey {
            issuer: self.issuer.clone(),
            subject: self.subject.clone(),
        }
    }

    /// Checks the record, reporting every problem at once.
    pub fn validate(&self) -> Result<()> {
        let mut problems = Vec::new();
        for (field, value) in [("issuer", &self.issuer), ("subject", &self.subject)] {
            if value.is_empty()
                || value.len() > MAX_IDENTITY_PART
                || value.chars().any(char::is_control)
            {
                problems.push(IamError::in_field(
                    e::INVALID_IDENTITY,
                    field,
                    format!(
                        "must have text, no control characters and at most \
                         {MAX_IDENTITY_PART} bytes"
                    ),
                ));
            }
        }
        match IamErrors::from_list(problems) {
            Some(errors) => Err(errors),
            None => Ok(()),
        }
    }
}

/// Checks that linking `new` keeps `(issuer, subject)` unique: it may already belong to the
/// same user (linking again is harmless), never to another one.
pub fn check_identity_unique(existing: &[LinkedIdentity], new: &LinkedIdentity) -> Result<()> {
    new.validate()?;
    let key = new.key();
    if existing
        .iter()
        .any(|l| l.key() == key && l.user != new.user)
    {
        return Err(IamErrors::one(IamError::new(
            e::IDENTITY_TAKEN,
            "this identity is already linked to another user",
        )));
    }
    Ok(())
}

/// Where an invitation stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvitationState {
    /// Sent, not yet used.
    Pending,
    /// Used: the user exists.
    Accepted,
    /// Its time ran out before anyone used it.
    Expired,
    /// Withdrawn by an administrator (or by the leaver actions).
    Cancelled,
}

/// A single-use, time-limited invitation to become a user (ADR-070 section 6.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Invitation {
    /// The invitation's id.
    #[serde(with = "wire::record_id")]
    pub id: RecordId,
    /// Where the link is sent.
    pub email: String,
    /// The kind of user the invitation creates.
    pub user_type: UserType,
    /// After this instant (inclusive) the invitation can no longer be accepted.
    #[serde(with = "wire::utc")]
    pub expires_at: OffsetDateTime,
    /// Where it stands.
    pub state: InvitationState,
}

impl Invitation {
    /// A new pending invitation. Not validated; call [`Invitation::validate`].
    pub fn new(
        id: RecordId,
        email: impl Into<String>,
        user_type: UserType,
        expires_at: OffsetDateTime,
    ) -> Self {
        Self {
            id,
            email: email.into(),
            user_type,
            expires_at,
            state: InvitationState::Pending,
        }
    }

    /// Checks the record, reporting every problem at once.
    pub fn validate(&self) -> Result<()> {
        let mut problems = Vec::new();
        if !is_valid_email(&self.email) {
            problems.push(IamError::in_field(
                e::INVALID_EMAIL,
                "email",
                "the address is not an e-mail address",
            ));
        }
        if !self.user_type.is_invitable() {
            problems.push(IamError::in_field(
                e::INVITATION_TYPE,
                "user_type",
                "only internal and portal users are invited; service and agent users are created by an administrator",
            ));
        }
        match IamErrors::from_list(problems) {
            Some(errors) => Err(errors),
            None => Ok(()),
        }
    }

    /// The state at `now`: a pending invitation past its time reads as expired, whether or not
    /// anyone stored that yet.
    pub fn state_at(&self, now: OffsetDateTime) -> InvitationState {
        if self.state == InvitationState::Pending && now >= self.expires_at {
            InvitationState::Expired
        } else {
            self.state
        }
    }

    /// Accepts the invitation: single-use, and only before it expires.
    pub fn accept(&self, now: OffsetDateTime) -> Result<Invitation> {
        match self.state_at(now) {
            InvitationState::Pending => Ok(Invitation {
                state: InvitationState::Accepted,
                ..self.clone()
            }),
            InvitationState::Accepted => Err(IamErrors::one(IamError::new(
                e::INVITATION_USED,
                "this invitation was already used",
            ))),
            InvitationState::Expired => Err(IamErrors::one(IamError::new(
                e::INVITATION_EXPIRED,
                "this invitation has expired",
            ))),
            InvitationState::Cancelled => Err(IamErrors::one(IamError::new(
                e::INVITATION_CANCELLED,
                "this invitation was cancelled",
            ))),
        }
    }

    /// Cancels a pending invitation. Cancelling a cancelled or expired one changes nothing; an
    /// accepted one cannot be cancelled.
    pub fn cancel(&self, now: OffsetDateTime) -> Result<Invitation> {
        match self.state_at(now) {
            InvitationState::Accepted => Err(IamErrors::one(IamError::new(
                e::INVITATION_USED,
                "this invitation was already used",
            ))),
            InvitationState::Pending => Ok(Invitation {
                state: InvitationState::Cancelled,
                ..self.clone()
            }),
            InvitationState::Expired | InvitationState::Cancelled => Ok(Invitation {
                state: if self.state == InvitationState::Pending {
                    InvitationState::Expired
                } else {
                    self.state
                },
                ..self.clone()
            }),
        }
    }

    /// Stores the expiry: a pending invitation past its time becomes `Expired`.
    pub fn expire(&self, now: OffsetDateTime) -> Invitation {
        Invitation {
            state: self.state_at(now),
            ..self.clone()
        }
    }
}
