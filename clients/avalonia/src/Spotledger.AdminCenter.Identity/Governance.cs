namespace Spotledger.AdminCenter.Identity;

/// <summary>The protocol of an SSO provider.</summary>
public enum SsoProtocol
{
    /// <summary>OpenID Connect.</summary>
    Oidc,

    /// <summary>SAML 2.0.</summary>
    Saml,
}

/// <summary>An SSO provider as saved. It holds no secret: the client secret is write-only.</summary>
/// <param name="Id">The provider's id.</param>
/// <param name="Name">Display name on the sign-in page.</param>
/// <param name="Protocol">OIDC or SAML.</param>
/// <param name="Issuer">The OIDC issuer URL, or the SAML IdP entity id.</param>
/// <param name="ClientIdOrEntity">The OIDC client id, or the SAML service-provider entity id.</param>
/// <param name="MetadataUrl">The SAML metadata URL, if used.</param>
/// <param name="HasSecret">Whether a client secret is stored (the value never comes back).</param>
public sealed record SsoProvider(
    string Id,
    string Name,
    SsoProtocol Protocol,
    string Issuer,
    string ClientIdOrEntity,
    string? MetadataUrl,
    bool HasSecret);

/// <summary>A provider form to save. <see cref="ClientSecret"/> is sent once and cleared.</summary>
public sealed record SsoProviderDraft(
    string? Id,
    string Name,
    SsoProtocol Protocol,
    string Issuer,
    string ClientIdOrEntity,
    string? MetadataUrl,
    string? ClientSecret)
{
    /// <summary>Never prints the secret.</summary>
    public override string ToString() => $"{Protocol} provider '{Name}'";
}

/// <summary>The outcome of a test sign-in.</summary>
/// <param name="Succeeded">Whether the provider signed the test user in.</param>
/// <param name="Message">What happened, in words (no token or claim values).</param>
public sealed record SsoTestResult(bool Succeeded, string Message);

/// <summary>How single sign-on is used in the tenant.</summary>
public enum SsoPolicy
{
    /// <summary>Only local passwords.</summary>
    Local,

    /// <summary>Either local or SSO.</summary>
    Optional,

    /// <summary>SSO only; local sign-in is closed to everyone except break-glass administrators.</summary>
    Required,
}

/// <summary>The tenant's sign-in state.</summary>
/// <param name="Policy">The current policy.</param>
/// <param name="BreakGlassAdministrators">How many local break-glass administrators exist.</param>
public sealed record TenantAuthState(SsoPolicy Policy, int BreakGlassAdministrators);

/// <summary>The SCIM endpoint.</summary>
/// <param name="BaseUrl">The endpoint the identity provider calls.</param>
/// <param name="Enabled">Whether provisioning is on.</param>
public sealed record ScimEndpoint(string BaseUrl, bool Enabled);

/// <summary>A SCIM token's record. Never the token.</summary>
/// <param name="Id">The token's id.</param>
/// <param name="CreatedAt">When it was made.</param>
/// <param name="LastUsedAt">When it was last used, if ever.</param>
public sealed record ScimTokenInfo(string Id, DateTimeOffset CreatedAt, DateTimeOffset? LastUsedAt);

/// <summary>An identity-provider group mapped to a role and scope.</summary>
/// <param name="Group">The group name as the IdP sends it.</param>
/// <param name="RoleId">The role it grants.</param>
/// <param name="Scope">Where.</param>
public sealed record GroupMapping(string Group, string RoleId, AssignmentScope Scope);

/// <summary>A decision on one assignment in a review.</summary>
public enum ReviewDecision
{
    /// <summary>Not decided yet.</summary>
    Undecided,

    /// <summary>The reviewer keeps the assignment.</summary>
    Keep,

    /// <summary>The reviewer removes it.</summary>
    Remove,
}

/// <summary>A campaign to create.</summary>
/// <param name="Name">The campaign's name.</param>
/// <param name="Scope">Which assignments are reviewed.</param>
/// <param name="ReviewerIds">Who reviews.</param>
/// <param name="DueDate">When reviewers must be done.</param>
public sealed record CampaignRequest(string Name, AssignmentScope Scope, IReadOnlyList<string> ReviewerIds, DateOnly DueDate);

/// <summary>A campaign in the list.</summary>
/// <param name="Id">The campaign's id.</param>
/// <param name="Name">Name.</param>
/// <param name="DueDate">Due date.</param>
/// <param name="Total">Assignments under review.</param>
/// <param name="Decided">Assignments decided.</param>
/// <param name="Escalation">Escalation status in words, or null.</param>
/// <param name="Completed">Whether it is closed.</param>
public sealed record CampaignInfo(string Id, string Name, DateOnly DueDate, int Total, int Decided, string? Escalation, bool Completed);

/// <summary>One assignment in a reviewer's worklist.</summary>
/// <param name="Id">The item's id.</param>
/// <param name="Assignment">The assignment under review.</param>
/// <param name="UserName">Who holds it.</param>
/// <param name="RoleName">The role's name.</param>
/// <param name="LastUsed">When the access was last used, if ever.</param>
/// <param name="SodOverridden">Whether it exists only through a separation-of-duties override.</param>
/// <param name="Decision">The reviewer's decision so far.</param>
public sealed record ReviewItem(
    string Id,
    AssignmentInfo Assignment,
    string UserName,
    string RoleName,
    DateOnly? LastUsed,
    bool SodOverridden,
    ReviewDecision Decision);

/// <summary>The completion report of a campaign.</summary>
/// <param name="CampaignId">The campaign.</param>
/// <param name="Kept">How many assignments were kept.</param>
/// <param name="Removed">How many were removed.</param>
/// <param name="NotReviewed">How many nobody decided.</param>
/// <param name="OverridesKept">Separation-of-duties overrides that were kept.</param>
public sealed record ReviewReport(string CampaignId, int Kept, int Removed, int NotReviewed, int OverridesKept);

/// <summary>Where an emergency grant is, as the screen needs it.</summary>
public enum EmergencyPhase
{
    /// <summary>Waiting for the role owner.</summary>
    AwaitingApproval,

    /// <summary>The session runs.</summary>
    Active,

    /// <summary>Refused or expired; nothing granted.</summary>
    Refused,

    /// <summary>Ended and waiting for the controller's review.</summary>
    Ended,
}

/// <summary>An emergency access grant.</summary>
/// <param name="Id">The grant's id.</param>
/// <param name="UserId">Who it is for.</param>
/// <param name="RoleId">The role they asked for.</param>
/// <param name="Reason">Why.</param>
/// <param name="Phase">Where it is.</param>
/// <param name="EndsAt">When an active session ends.</param>
public sealed record EmergencyGrantInfo(
    string Id,
    string UserId,
    string RoleId,
    string Reason,
    EmergencyPhase Phase,
    DateTimeOffset? EndsAt);

/// <summary>An emergency request.</summary>
/// <param name="RoleId">The role needed.</param>
/// <param name="Reason">Why, in words.</param>
/// <param name="Minutes">How long the session may run.</param>
public sealed record EmergencyRequestDraft(string RoleId, string Reason, int Minutes);

/// <summary>Limits the policy sets on emergency requests.</summary>
/// <param name="MaxMinutes">The longest session.</param>
public sealed record EmergencyPolicyInfo(int MaxMinutes);

/// <summary>A controller's review of an ended emergency session.</summary>
/// <param name="Id">The review's id.</param>
/// <param name="GrantId">The session.</param>
/// <param name="SessionUserId">Who had the access.</param>
/// <param name="Actions">How many actions were recorded.</param>
/// <param name="Escalated">Whether it fell due unconfirmed.</param>
public sealed record EmergencyReviewTask(string Id, string GrantId, string SessionUserId, int Actions, bool Escalated);

/// <summary>The four answerable questions.</summary>
public enum QuestionKind
{
    /// <summary>What can this user do, and why.</summary>
    WhatCanUserDo,

    /// <summary>Who can do this, and why.</summary>
    WhoCanDo,

    /// <summary>Who can see this document.</summary>
    WhoCanSee,

    /// <summary>What changed between two dates.</summary>
    WhatChanged,
}

/// <summary>One row of an answer, with its why-chain.</summary>
/// <param name="Subject">The permission, user or change the row is about.</param>
/// <param name="Summary">The row in words.</param>
/// <param name="Why">The chain: assignment, role path, duty, source, scope, rule. Empty for a change row.</param>
public sealed record AnswerRow(string Subject, string Summary, IReadOnlyList<string> Why);

/// <summary>One page of an answer.</summary>
/// <param name="Rows">The rows.</param>
/// <param name="NextCursor">The next page, or null.</param>
public sealed record AnswerPage(IReadOnlyList<AnswerRow> Rows, string? NextCursor);

/// <summary>A question to ask.</summary>
/// <param name="Kind">Which of the four.</param>
/// <param name="UserId">The user (what can this user do).</param>
/// <param name="Permission">The permission, as <c>doctype:right</c> (who can do this).</param>
/// <param name="Doctype">The document's DocType (who can see this document).</param>
/// <param name="DocumentId">The document.</param>
/// <param name="From">Start of the period (what changed).</param>
/// <param name="To">End of the period.</param>
/// <param name="Cursor">The page cursor.</param>
public sealed record QuestionRequest(
    QuestionKind Kind,
    string? UserId = null,
    string? Permission = null,
    string? Doctype = null,
    string? DocumentId = null,
    DateOnly? From = null,
    DateOnly? To = null,
    string? Cursor = null);
