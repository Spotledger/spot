namespace Spotledger.AdminCenter.Identity;

/// <summary>Whether a user is a person or a service identity.</summary>
public enum UserKind
{
    /// <summary>A human user.</summary>
    Person,

    /// <summary>A service user (API keys, no password).</summary>
    Service,
}

/// <summary>Where a user is in their life.</summary>
public enum UserState
{
    /// <summary>Invited, not yet signed in.</summary>
    Invited,

    /// <summary>Can sign in.</summary>
    Active,

    /// <summary>Disabled; sessions and keys are gone, the record stays.</summary>
    Disabled,

    /// <summary>Personal data erased; the record stays for audit.</summary>
    Anonymised,
}

/// <summary>One row of the user list.</summary>
/// <param name="Id">The user's id.</param>
/// <param name="Name">Display name.</param>
/// <param name="Email">Email, or a service user's owner contact.</param>
/// <param name="Kind">Person or service.</param>
/// <param name="State">Life-cycle state.</param>
/// <param name="LastSignIn">When they last signed in, if ever.</param>
/// <param name="Roles">Names of the roles they hold now.</param>
public sealed record UserSummary(
    string Id,
    string Name,
    string Email,
    UserKind Kind,
    UserState State,
    DateTimeOffset? LastSignIn,
    IReadOnlyList<string> Roles);

/// <summary>The list filter. Null members do not filter.</summary>
/// <param name="Text">Matches name or email, case-insensitively.</param>
/// <param name="State">Only this state.</param>
/// <param name="Kind">Only this kind.</param>
public sealed record UserFilter(string? Text = null, UserState? State = null, UserKind? Kind = null);

/// <summary>An invitation to create.</summary>
/// <param name="Email">Where the invitation is for.</param>
/// <param name="Name">Their display name.</param>
/// <param name="InitialRoleIds">Roles to grant when they accept.</param>
public sealed record InviteRequest(string Email, string Name, IReadOnlyList<string> InitialRoleIds);

/// <summary>A single-use secret the server returned once (an invitation link, an API key, a SCIM token).</summary>
/// <param name="Label">What it is, for the screen.</param>
/// <param name="Value">The secret. Shown once; the view-models never keep it past dismissal.</param>
public sealed record OneTimeSecret(string Label, string Value)
{
    /// <summary>Never prints the value, so a log line or a debugger view cannot leak it.</summary>
    public override string ToString() => $"{Label}: [hidden]";
}

/// <summary>What disabling a user would take with it.</summary>
/// <param name="UserId">The user.</param>
/// <param name="Sessions">Sessions that would end.</param>
/// <param name="ApiKeys">API keys that would be revoked.</param>
/// <param name="Invitations">Pending invitations that would be withdrawn.</param>
/// <param name="Tasks">Open tasks and who would take them.</param>
public sealed record LeaverPreview(
    string UserId,
    int Sessions,
    int ApiKeys,
    int Invitations,
    IReadOnlyList<TaskReassignment> Tasks);

/// <summary>An open task of a leaver and its proposed new owner.</summary>
/// <param name="Task">The task's title.</param>
/// <param name="NewOwner">Who takes it, or null when it has no owner yet.</param>
public sealed record TaskReassignment(string Task, string? NewOwner);

/// <summary>An account recovery action. Each needs a step-up.</summary>
public enum RecoveryKind
{
    /// <summary>Send a password reset.</summary>
    ResetPassword,

    /// <summary>Clear second-factor enrolment.</summary>
    ResetSecondFactor,

    /// <summary>End every session.</summary>
    EndSessions,

    /// <summary>Revoke every API key.</summary>
    RevokeKeys,
}

/// <summary>Whether a role is a shipped reference role or a site-owned copy.</summary>
public enum RoleKind
{
    /// <summary>A reference pack role: read-only, upgraded by pack releases.</summary>
    Reference,

    /// <summary>A custom copy a site owns and edits.</summary>
    Custom,
}

/// <summary>One role in the role list.</summary>
/// <param name="Id">The role's id.</param>
/// <param name="Name">Display name.</param>
/// <param name="Kind">Reference or custom.</param>
/// <param name="BasedOn">For a custom copy, the reference role it was copied from.</param>
/// <param name="PackVersion">The pack version the role is at.</param>
/// <param name="LatestPackVersion">The newest pack version available, when it is newer.</param>
/// <param name="Duties">The duties the role carries.</param>
public sealed record RoleSummary(
    string Id,
    string Name,
    RoleKind Kind,
    string? BasedOn,
    string? PackVersion,
    string? LatestPackVersion,
    IReadOnlyList<string> Duties)
{
    /// <summary>True when a newer pack version exists for this role.</summary>
    public bool UpgradeAvailable => LatestPackVersion is not null && LatestPackVersion != PackVersion;
}

/// <summary>A duty and what it allows, for the duty list.</summary>
/// <param name="Name">The duty.</param>
/// <param name="Description">What it is for.</param>
/// <param name="Rights">The rights it carries, as text (<c>read invoice</c>).</param>
public sealed record DutyInfo(string Name, string Description, IReadOnlyList<string> Rights);

/// <summary>How a duty changes between two pack versions.</summary>
public enum DutyChangeKind
{
    /// <summary>The new version adds the duty.</summary>
    Added,

    /// <summary>The new version removes it.</summary>
    Removed,

    /// <summary>Its rights changed.</summary>
    Changed,
}

/// <summary>One line of the upgrade diff.</summary>
/// <param name="Duty">The duty.</param>
/// <param name="Kind">What happens to it.</param>
/// <param name="Detail">Rights added or removed, in words.</param>
public sealed record DutyChange(string Duty, DutyChangeKind Kind, string? Detail);

/// <summary>The diff between a role's pack version and the newest.</summary>
/// <param name="RoleId">The role.</param>
/// <param name="FromVersion">Where it is.</param>
/// <param name="ToVersion">Where it would go.</param>
/// <param name="Changes">The duty changes.</param>
public sealed record UpgradeDiff(string RoleId, string FromVersion, string ToVersion, IReadOnlyList<DutyChange> Changes);

/// <summary>Where an assignment came from.</summary>
public enum AssignmentSource
{
    /// <summary>An administrator granted it.</summary>
    Manual,

    /// <summary>An assignment rule produced it.</summary>
    Rule,

    /// <summary>The identity provider (SSO group, SCIM) manages it.</summary>
    Idp,

    /// <summary>An emergency access session.</summary>
    Emergency,
}

/// <summary>Where an assignment applies.</summary>
/// <param name="CompanyId">The company, or null for everywhere.</param>
/// <param name="IncludeDescendants">Whether child companies are included.</param>
public sealed record AssignmentScope(string? CompanyId, bool IncludeDescendants)
{
    /// <summary>The scope as the screen words it.</summary>
    public string Describe() => CompanyId is null
        ? "All companies"
        : IncludeDescendants ? $"{CompanyId} and its descendants" : CompanyId;
}

/// <summary>A role assignment.</summary>
/// <param name="Id">The assignment's id.</param>
/// <param name="UserId">Who holds it.</param>
/// <param name="RoleId">The role.</param>
/// <param name="Scope">Where it applies.</param>
/// <param name="ValidFrom">First day, or null for now.</param>
/// <param name="ValidTo">Last day, or null for open-ended.</param>
/// <param name="Source">Where it came from.</param>
public sealed record AssignmentInfo(
    string Id,
    string UserId,
    string RoleId,
    AssignmentScope Scope,
    DateOnly? ValidFrom,
    DateOnly? ValidTo,
    AssignmentSource Source)
{
    /// <summary>True when the identity provider manages it (the screen locks it).</summary>
    public bool ManagedByIdp => Source == AssignmentSource.Idp;

    /// <summary>Why the assignment cannot be changed here, or null when it can.</summary>
    public string? LockReason => Source switch
    {
        AssignmentSource.Idp => "Managed by your identity provider. Change it there.",
        AssignmentSource.Emergency => "Ends with its emergency access session.",
        _ => null,
    };
}

/// <summary>A new assignment to check or create.</summary>
/// <param name="UserId">Who receives it.</param>
/// <param name="RoleId">The role.</param>
/// <param name="Scope">Where it applies.</param>
/// <param name="ValidFrom">First day, or null.</param>
/// <param name="ValidTo">Last day, or null.</param>
public sealed record AssignmentRequest(
    string UserId,
    string RoleId,
    AssignmentScope Scope,
    DateOnly? ValidFrom,
    DateOnly? ValidTo);

/// <summary>What the separation-of-duties check decided.</summary>
public enum SodOutcome
{
    /// <summary>No conflict.</summary>
    Clear,

    /// <summary>The grant is forbidden and cannot be confirmed.</summary>
    Blocked,

    /// <summary>It conflicts; an approver must approve the request.</summary>
    NeedsApproval,

    /// <summary>It conflicts; an administrator may override with a reason and a mitigating control.</summary>
    NeedsOverride,
}

/// <summary>One conflict the check found.</summary>
/// <param name="Rule">The separation-of-duties rule.</param>
/// <param name="Description">In words.</param>
/// <param name="ConflictsWith">The duty or role it conflicts with.</param>
public sealed record SodFinding(string Rule, string Description, string ConflictsWith);

/// <summary>The result of the check, shown before the grant is confirmed.</summary>
/// <param name="Outcome">The decision.</param>
/// <param name="Findings">The conflicts, empty when clear.</param>
public sealed record SodResult(SodOutcome Outcome, IReadOnlyList<SodFinding> Findings);

/// <summary>The override an administrator gives for a conflict.</summary>
/// <param name="Reason">Why the grant is needed.</param>
/// <param name="MitigatingControl">The control that makes it safe.</param>
public sealed record SodOverride(string Reason, string MitigatingControl);

/// <summary>What creating an assignment led to.</summary>
public enum GrantOutcome
{
    /// <summary>The assignment exists.</summary>
    Granted,

    /// <summary>An approval request was opened; nothing is granted yet.</summary>
    ApprovalRequested,
}
