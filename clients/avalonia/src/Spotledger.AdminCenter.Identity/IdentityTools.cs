namespace Spotledger.AdminCenter.Identity;

/// <summary>One screen the identity tool contributes to the Admin Center's navigation.</summary>
/// <param name="Id">Stable id.</param>
/// <param name="Title">What the navigation shows.</param>
/// <param name="RequiredDuty">The duty a user needs to see it; the server enforces the same.</param>
public sealed record IdentityToolEntry(string Id, string Title, string RequiredDuty);

/// <summary>
/// The screens this project registers. The Admin Center's tool registry (WP-ADM-09.T1) maps each
/// entry to its own navigation entry under the ERP administrator's tree; until it is merged this
/// list is the extension point, so the host can build against it.
/// </summary>
public static class IdentityTools
{
    /// <summary>The duty that administers users, roles and assignments.</summary>
    public const string IdentityAdminDuty = "identity-administration";

    /// <summary>The duty that runs access reviews and answers the four questions.</summary>
    public const string AccessGovernanceDuty = "access-governance";

    /// <summary>Every screen, in navigation order.</summary>
    public static IReadOnlyList<IdentityToolEntry> Entries { get; } =
    [
        new("identity.users", "Users", IdentityAdminDuty),
        new("identity.roles", "Roles and duties", IdentityAdminDuty),
        new("identity.assignments", "Assignments", IdentityAdminDuty),
        new("identity.sso", "Single sign-on", IdentityAdminDuty),
        new("identity.scim", "Provisioning (SCIM)", IdentityAdminDuty),
        new("identity.review", "Access review", AccessGovernanceDuty),
        new("identity.emergency", "Emergency access", "emergency-access"),
        new("identity.questions", "Who can do what", AccessGovernanceDuty),
    ];
}
