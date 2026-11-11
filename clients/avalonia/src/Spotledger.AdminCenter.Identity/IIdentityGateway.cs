namespace Spotledger.AdminCenter.Identity;

/// <summary>Nothing: the value of an operation that returns no data.</summary>
public readonly record struct Unit;

/// <summary>A server failure in the server's own words.</summary>
/// <param name="Code">The stable code (<c>iam.last_administrator</c>).</param>
/// <param name="Message">What the server says. Shown as is.</param>
public sealed record GatewayError(string Code, string Message)
{
    /// <summary>The code the server uses when an action needs a fresh step-up.</summary>
    public const string StepUpRequired = "auth.step_up_required";
}

/// <summary>The result of one gateway call: a value or the server's error.</summary>
/// <typeparam name="T">The value type.</typeparam>
public sealed class GatewayResult<T>
{
    internal GatewayResult(T? value, GatewayError? error)
    {
        Value = value;
        Error = error;
    }

    /// <summary>The value, when the call succeeded.</summary>
    public T? Value { get; }

    /// <summary>The error, when it failed.</summary>
    public GatewayError? Error { get; }

    /// <summary>True when the call succeeded.</summary>
    public bool IsOk => Error is null;

}

/// <summary>Builds <see cref="GatewayResult{T}"/> values.</summary>
public static class GatewayResult
{
    /// <summary>A success.</summary>
    public static GatewayResult<T> Ok<T>(T value) => new(value, null);

    /// <summary>A success with no data.</summary>
    public static GatewayResult<Unit> Done() => new(default, null);

    /// <summary>A failure.</summary>
    public static GatewayResult<T> Fail<T>(string code, string message) => new(default, new GatewayError(code, message));
}

/// <summary>Proof of a recent step-up (a fresh second-factor check). Never printed.</summary>
public sealed record StepUpProof(string Token)
{
    /// <inheritdoc />
    public override string ToString() => "StepUpProof [hidden]";
}

/// <summary>
/// The typed native API of the identity DocTypes (ADR-070 section 10.1), in the shapes the screens
/// need. The host adapts the generated client onto it; the tests use a fake. Every sensitive call
/// takes the step-up proof (null on the first try). Time is never read here: dates come in as arguments.
/// </summary>
public interface IIdentityGateway
{
    // Users

    /// <summary>Lists users matching the filter.</summary>
    Task<GatewayResult<IReadOnlyList<UserSummary>>> ListUsersAsync(UserFilter filter);

    /// <summary>Creates an invitation. The single-use link comes back once.</summary>
    Task<GatewayResult<OneTimeSecret>> InviteUserAsync(InviteRequest request);

    /// <summary>Creates a service user. Its first API key comes back once.</summary>
    Task<GatewayResult<OneTimeSecret>> CreateServiceUserAsync(string name, string ownerUserId);

    /// <summary>What disabling the user would take with it.</summary>
    Task<GatewayResult<LeaverPreview>> PreviewLeaverAsync(string userId);

    /// <summary>Disables the user, reassigning tasks as previewed. The server refuses the last administrator.</summary>
    Task<GatewayResult<Unit>> DisableUserAsync(string userId, IReadOnlyList<TaskReassignment> tasks);

    /// <summary>Enables a disabled user.</summary>
    Task<GatewayResult<Unit>> EnableUserAsync(string userId);

    /// <summary>Runs a recovery action. Needs a step-up.</summary>
    Task<GatewayResult<Unit>> RecoverUserAsync(string userId, RecoveryKind kind, StepUpProof? proof);

    /// <summary>Anonymises the user. Needs a step-up.</summary>
    Task<GatewayResult<Unit>> AnonymiseUserAsync(string userId, StepUpProof? proof);

    // Roles and assignments

    /// <summary>Lists roles.</summary>
    Task<GatewayResult<IReadOnlyList<RoleSummary>>> ListRolesAsync();

    /// <summary>Lists duties.</summary>
    Task<GatewayResult<IReadOnlyList<DutyInfo>>> ListDutiesAsync();

    /// <summary>Copies a reference role to a custom role.</summary>
    Task<GatewayResult<RoleSummary>> CopyRoleAsync(string roleId, string newName);

    /// <summary>The diff between the role's pack version and the newest.</summary>
    Task<GatewayResult<UpgradeDiff>> GetUpgradeDiffAsync(string roleId);

    /// <summary>Applies the upgrade to a role.</summary>
    Task<GatewayResult<RoleSummary>> ApplyUpgradeAsync(string roleId, string toVersion);

    /// <summary>Lists a user's assignments with their sources.</summary>
    Task<GatewayResult<IReadOnlyList<AssignmentInfo>>> ListAssignmentsAsync(string userId);

    /// <summary>The separation-of-duties check for a grant, before it is confirmed.</summary>
    Task<GatewayResult<SodResult>> CheckSodAsync(AssignmentRequest request);

    /// <summary>Grants the assignment. A conflict needs its override (or goes to approval).</summary>
    Task<GatewayResult<GrantOutcome>> GrantAssignmentAsync(AssignmentRequest request, SodOverride? sodOverride);

    /// <summary>Revokes an assignment.</summary>
    Task<GatewayResult<Unit>> RevokeAssignmentAsync(string assignmentId);

    // SSO and SCIM

    /// <summary>Lists SSO providers.</summary>
    Task<GatewayResult<IReadOnlyList<SsoProvider>>> ListSsoProvidersAsync();

    /// <summary>Saves a provider (create when the id is null).</summary>
    Task<GatewayResult<SsoProvider>> SaveSsoProviderAsync(SsoProviderDraft draft);

    /// <summary>Runs a test sign-in against a saved provider.</summary>
    Task<GatewayResult<SsoTestResult>> TestSsoProviderAsync(string providerId);

    /// <summary>The tenant policy and the break-glass administrator count.</summary>
    Task<GatewayResult<TenantAuthState>> GetTenantAuthAsync();

    /// <summary>Sets the tenant policy. The server re-checks the break-glass rule.</summary>
    Task<GatewayResult<TenantAuthState>> SetSsoPolicyAsync(SsoPolicy policy);

    /// <summary>The SCIM endpoint and its tokens.</summary>
    Task<GatewayResult<(ScimEndpoint Endpoint, IReadOnlyList<ScimTokenInfo> Tokens)>> GetScimAsync();

    /// <summary>Creates a SCIM token. The value comes back once.</summary>
    Task<GatewayResult<OneTimeSecret>> CreateScimTokenAsync(StepUpProof? proof);

    /// <summary>Revokes a SCIM token.</summary>
    Task<GatewayResult<Unit>> RevokeScimTokenAsync(string tokenId, StepUpProof? proof);

    /// <summary>The group mappings and the groups the IdP sent that have no mapping.</summary>
    Task<GatewayResult<(IReadOnlyList<GroupMapping> Mappings, IReadOnlyList<string> Unmapped)>> GetGroupMappingsAsync();

    /// <summary>Replaces the group mappings.</summary>
    Task<GatewayResult<Unit>> SaveGroupMappingsAsync(IReadOnlyList<GroupMapping> mappings);

    // Access review

    /// <summary>Lists campaigns.</summary>
    Task<GatewayResult<IReadOnlyList<CampaignInfo>>> ListCampaignsAsync();

    /// <summary>Creates a campaign.</summary>
    Task<GatewayResult<CampaignInfo>> CreateCampaignAsync(CampaignRequest request);

    /// <summary>The reviewer's worklist for a campaign.</summary>
    Task<GatewayResult<IReadOnlyList<ReviewItem>>> GetWorklistAsync(string campaignId);

    /// <summary>Records keep or remove decisions (item id to decision).</summary>
    Task<GatewayResult<Unit>> SubmitDecisionsAsync(string campaignId, IReadOnlyDictionary<string, ReviewDecision> decisions);

    /// <summary>Closes the campaign and returns its report.</summary>
    Task<GatewayResult<ReviewReport>> CompleteCampaignAsync(string campaignId);

    // Emergency access

    /// <summary>The emergency policy limits.</summary>
    Task<GatewayResult<EmergencyPolicyInfo>> GetEmergencyPolicyAsync();

    /// <summary>The caller's own grants and, for an approver, the requests waiting for them.</summary>
    Task<GatewayResult<(IReadOnlyList<EmergencyGrantInfo> Mine, IReadOnlyList<EmergencyGrantInfo> ToApprove)>> ListEmergencyAsync();

    /// <summary>Requests emergency access.</summary>
    Task<GatewayResult<EmergencyGrantInfo>> RequestEmergencyAsync(EmergencyRequestDraft draft);

    /// <summary>Approves or refuses a request.</summary>
    Task<GatewayResult<Unit>> DecideEmergencyAsync(string grantId, bool approve, string? reason);

    /// <summary>Ends an active session early.</summary>
    Task<GatewayResult<Unit>> EndEmergencyAsync(string grantId);

    /// <summary>The controller's review queue.</summary>
    Task<GatewayResult<IReadOnlyList<EmergencyReviewTask>>> ListEmergencyReviewsAsync();

    /// <summary>Confirms a review.</summary>
    Task<GatewayResult<Unit>> ConfirmEmergencyReviewAsync(string reviewId);

    /// <summary>Flags a session for a person to look at.</summary>
    Task<GatewayResult<Unit>> FlagEmergencyReviewAsync(string reviewId, string reason);

    // The four questions

    /// <summary>Asks one of the four questions. Permission-checked and audited by the server.</summary>
    Task<GatewayResult<AnswerPage>> AskAsync(QuestionRequest request);

    /// <summary>Exports the answer (CSV text). The server checks the export permission.</summary>
    Task<GatewayResult<string>> ExportAnswerAsync(QuestionRequest request);
}

/// <summary>The dialogs the screens need from their host. A view-model never opens a window itself.</summary>
public interface IIdentityShell
{
    /// <summary>Asks a yes/no question. False declines.</summary>
    Task<bool> ConfirmAsync(string title, string message, string confirmLabel);

    /// <summary>Asks for a line of text (a typed confirmation). Null cancels.</summary>
    Task<string?> AskTextAsync(string title, string prompt);

    /// <summary>Asks for a fresh second-factor check. Null cancels.</summary>
    Task<StepUpProof?> PromptStepUpAsync(string reason);
}
