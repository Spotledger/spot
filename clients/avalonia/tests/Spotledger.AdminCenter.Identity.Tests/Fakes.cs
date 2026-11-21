using Spotledger.AdminCenter.Identity;

namespace Spotledger.AdminCenter.Identity.Tests;

/// <summary>A shell whose answers the test sets, recording what it was asked.</summary>
internal sealed class FakeShell : IIdentityShell
{
    public bool ConfirmAnswer { get; set; } = true;

    public string? TextAnswer { get; set; }

    public StepUpProof? StepUp { get; set; } = new("proof-1");

    public List<string> Confirms { get; } = [];

    public List<string> Prompts { get; } = [];

    public int StepUpPrompts { get; private set; }

    public Task<bool> ConfirmAsync(string title, string message, string confirmLabel)
    {
        Confirms.Add($"{title}: {message}");
        return Task.FromResult(ConfirmAnswer);
    }

    public Task<string?> AskTextAsync(string title, string prompt)
    {
        Prompts.Add(prompt);
        return Task.FromResult(TextAnswer);
    }

    public Task<StepUpProof?> PromptStepUpAsync(string reason)
    {
        StepUpPrompts++;
        return Task.FromResult(StepUp);
    }
}

/// <summary>An in-memory gateway: canned data, recorded calls, injectable failures.</summary>
internal sealed class FakeGateway : IIdentityGateway
{
    public List<string> Calls { get; } = [];

    /// <summary>Method name to error: the next call of that method fails once with it.</summary>
    public Dictionary<string, GatewayError> Failures { get; } = [];

    /// <summary>Methods that require a step-up proof the first time.</summary>
    public HashSet<string> RequireStepUp { get; } = [];

    public List<StepUpProof?> ProofsSeen { get; } = [];

    public List<UserSummary> Users { get; } = [];
    public LeaverPreview? Leaver { get; set; }
    public List<RoleSummary> Roles { get; } = [];
    public List<DutyInfo> Duties { get; } = [];
    public UpgradeDiff? Diff { get; set; }
    public List<AssignmentInfo> Assignments { get; } = [];
    public SodResult Sod { get; set; } = new(SodOutcome.Clear, []);
    public GrantOutcome Grant { get; set; } = GrantOutcome.Granted;
    public (AssignmentRequest Request, SodOverride? Override)? LastGrant { get; private set; }
    public List<SsoProvider> Providers { get; } = [];
    public SsoProviderDraft? LastDraft { get; private set; }
    public SsoTestResult TestResult { get; set; } = new(true, "Signed in.");
    public TenantAuthState Tenant { get; set; } = new(SsoPolicy.Local, 2);
    public List<ScimTokenInfo> ScimTokens { get; } = [];
    public List<GroupMapping> Mappings { get; } = [];
    public List<string> UnmappedGroups { get; } = [];
    public List<CampaignInfo> Campaigns { get; } = [];
    public List<ReviewItem> Worklist { get; } = [];
    public IReadOnlyDictionary<string, ReviewDecision>? LastDecisions { get; private set; }
    public List<EmergencyGrantInfo> Mine { get; } = [];
    public List<EmergencyGrantInfo> ToApprove { get; } = [];
    public List<EmergencyReviewTask> Reviews { get; } = [];
    public EmergencyPolicyInfo Policy { get; set; } = new(60);
    public AnswerPage Answer { get; set; } = new([], null);
    public QuestionRequest? LastQuestion { get; private set; }
    public UserFilter? LastFilter { get; private set; }

    private GatewayResult<T>? Gate<T>(string name, StepUpProof? proof = null)
    {
        Calls.Add(name);
        if (Failures.Remove(name, out var failure))
        {
            return GatewayResult.Fail<T>(failure.Code, failure.Message);
        }

        if (RequireStepUp.Contains(name))
        {
            ProofsSeen.Add(proof);
            if (proof is null)
            {
                return GatewayResult.Fail<T>(GatewayError.StepUpRequired, "Step up first.");
            }
        }

        return null;
    }

    private static Task<GatewayResult<T>> T<T>(GatewayResult<T> r) => Task.FromResult(r);

    public Task<GatewayResult<IReadOnlyList<UserSummary>>> ListUsersAsync(UserFilter filter)
    {
        LastFilter = filter;
        return T(Gate<IReadOnlyList<UserSummary>>("ListUsers") ?? GatewayResult.Ok<IReadOnlyList<UserSummary>>(
            Users.Where(u => (filter.State is null || u.State == filter.State)
                && (filter.Kind is null || u.Kind == filter.Kind)
                && (string.IsNullOrEmpty(filter.Text)
                    || u.Name.Contains(filter.Text, StringComparison.OrdinalIgnoreCase)
                    || u.Email.Contains(filter.Text, StringComparison.OrdinalIgnoreCase))).ToList()));
    }

    public Task<GatewayResult<OneTimeSecret>> InviteUserAsync(InviteRequest request) =>
        T(Gate<OneTimeSecret>("Invite") ?? GatewayResult.Ok(new OneTimeSecret("Invitation link", "https://x.test/invite/SECRET-LINK")));

    public Task<GatewayResult<OneTimeSecret>> CreateServiceUserAsync(string name, string ownerUserId) =>
        T(Gate<OneTimeSecret>("CreateServiceUser") ?? GatewayResult.Ok(new OneTimeSecret("API key", "spk_KEY_SECRET")));

    public Task<GatewayResult<LeaverPreview>> PreviewLeaverAsync(string userId) =>
        T(Gate<LeaverPreview>("PreviewLeaver") ?? GatewayResult.Ok(Leaver ?? new LeaverPreview(userId, 0, 0, 0, [])));

    public Task<GatewayResult<Unit>> DisableUserAsync(string userId, IReadOnlyList<TaskReassignment> tasks) =>
        T(Gate<Unit>("Disable") ?? GatewayResult.Done());

    public Task<GatewayResult<Unit>> EnableUserAsync(string userId) => T(Gate<Unit>("Enable") ?? GatewayResult.Done());

    public Task<GatewayResult<Unit>> RecoverUserAsync(string userId, RecoveryKind kind, StepUpProof? proof) =>
        T(Gate<Unit>("Recover", proof) ?? GatewayResult.Done());

    public Task<GatewayResult<Unit>> AnonymiseUserAsync(string userId, StepUpProof? proof) =>
        T(Gate<Unit>("Anonymise", proof) ?? GatewayResult.Done());

    public Task<GatewayResult<IReadOnlyList<RoleSummary>>> ListRolesAsync() =>
        T(Gate<IReadOnlyList<RoleSummary>>("ListRoles") ?? GatewayResult.Ok<IReadOnlyList<RoleSummary>>([.. Roles]));

    public Task<GatewayResult<IReadOnlyList<DutyInfo>>> ListDutiesAsync() =>
        T(Gate<IReadOnlyList<DutyInfo>>("ListDuties") ?? GatewayResult.Ok<IReadOnlyList<DutyInfo>>([.. Duties]));

    public Task<GatewayResult<RoleSummary>> CopyRoleAsync(string roleId, string newName)
    {
        var copy = new RoleSummary("copy-" + roleId, newName, RoleKind.Custom, roleId, null, null, []);
        Roles.Add(copy);
        return T(Gate<RoleSummary>("CopyRole") ?? GatewayResult.Ok(copy));
    }

    public Task<GatewayResult<UpgradeDiff>> GetUpgradeDiffAsync(string roleId) =>
        T(Gate<UpgradeDiff>("UpgradeDiff") ?? GatewayResult.Ok(Diff!));

    public Task<GatewayResult<RoleSummary>> ApplyUpgradeAsync(string roleId, string toVersion)
    {
        var index = Roles.FindIndex(r => r.Id == roleId);
        Roles[index] = Roles[index] with { PackVersion = toVersion, LatestPackVersion = null };
        return T(Gate<RoleSummary>("ApplyUpgrade") ?? GatewayResult.Ok(Roles[index]));
    }

    public Task<GatewayResult<IReadOnlyList<AssignmentInfo>>> ListAssignmentsAsync(string userId) =>
        T(Gate<IReadOnlyList<AssignmentInfo>>("ListAssignments") ?? GatewayResult.Ok<IReadOnlyList<AssignmentInfo>>(
            Assignments.Where(a => a.UserId == userId).ToList()));

    public Task<GatewayResult<SodResult>> CheckSodAsync(AssignmentRequest request) =>
        T(Gate<SodResult>("CheckSod") ?? GatewayResult.Ok(Sod));

    public Task<GatewayResult<GrantOutcome>> GrantAssignmentAsync(AssignmentRequest request, SodOverride? sodOverride)
    {
        LastGrant = (request, sodOverride);
        return T(Gate<GrantOutcome>("Grant") ?? GatewayResult.Ok(Grant));
    }

    public Task<GatewayResult<Unit>> RevokeAssignmentAsync(string assignmentId)
    {
        Assignments.RemoveAll(a => a.Id == assignmentId);
        return T(Gate<Unit>("Revoke") ?? GatewayResult.Done());
    }

    public Task<GatewayResult<IReadOnlyList<SsoProvider>>> ListSsoProvidersAsync() =>
        T(Gate<IReadOnlyList<SsoProvider>>("ListProviders") ?? GatewayResult.Ok<IReadOnlyList<SsoProvider>>([.. Providers]));

    public Task<GatewayResult<SsoProvider>> SaveSsoProviderAsync(SsoProviderDraft draft)
    {
        LastDraft = draft;
        var saved = new SsoProvider(draft.Id ?? "p1", draft.Name, draft.Protocol, draft.Issuer, draft.ClientIdOrEntity, draft.MetadataUrl, draft.ClientSecret is not null);
        Providers.RemoveAll(p => p.Id == saved.Id);
        Providers.Add(saved);
        return T(Gate<SsoProvider>("SaveProvider") ?? GatewayResult.Ok(saved));
    }

    public Task<GatewayResult<SsoTestResult>> TestSsoProviderAsync(string providerId) =>
        T(Gate<SsoTestResult>("TestProvider") ?? GatewayResult.Ok(TestResult));

    public Task<GatewayResult<TenantAuthState>> GetTenantAuthAsync() =>
        T(Gate<TenantAuthState>("GetTenant") ?? GatewayResult.Ok(Tenant));

    public Task<GatewayResult<TenantAuthState>> SetSsoPolicyAsync(SsoPolicy policy)
    {
        Tenant = Tenant with { Policy = policy };
        return T(Gate<TenantAuthState>("SetPolicy") ?? GatewayResult.Ok(Tenant));
    }

    public Task<GatewayResult<(ScimEndpoint Endpoint, IReadOnlyList<ScimTokenInfo> Tokens)>> GetScimAsync() =>
        T(Gate<(ScimEndpoint, IReadOnlyList<ScimTokenInfo>)>("GetScim")
            ?? GatewayResult.Ok((new ScimEndpoint("https://x.test/scim/v2", true), (IReadOnlyList<ScimTokenInfo>)[.. ScimTokens])));

    public Task<GatewayResult<OneTimeSecret>> CreateScimTokenAsync(StepUpProof? proof)
    {
        var gate = Gate<OneTimeSecret>("CreateScimToken", proof);
        if (gate is null)
        {
            ScimTokens.Add(new ScimTokenInfo("t" + ScimTokens.Count, new DateTimeOffset(2026, 10, 1, 0, 0, 0, TimeSpan.Zero), null));
        }

        return T(gate ?? GatewayResult.Ok(new OneTimeSecret("SCIM token", "scim_TOKEN_SECRET")));
    }

    public Task<GatewayResult<Unit>> RevokeScimTokenAsync(string tokenId, StepUpProof? proof)
    {
        var gate = Gate<Unit>("RevokeScimToken", proof);
        if (gate is null)
        {
            ScimTokens.RemoveAll(t => t.Id == tokenId);
        }

        return T(gate ?? GatewayResult.Done());
    }

    public Task<GatewayResult<(IReadOnlyList<GroupMapping> Mappings, IReadOnlyList<string> Unmapped)>> GetGroupMappingsAsync() =>
        T(Gate<(IReadOnlyList<GroupMapping>, IReadOnlyList<string>)>("GetMappings")
            ?? GatewayResult.Ok(((IReadOnlyList<GroupMapping>)[.. Mappings], (IReadOnlyList<string>)[.. UnmappedGroups])));

    public Task<GatewayResult<Unit>> SaveGroupMappingsAsync(IReadOnlyList<GroupMapping> mappings)
    {
        var gate = Gate<Unit>("SaveMappings");
        if (gate is null)
        {
            Mappings.Clear();
            Mappings.AddRange(mappings);
            UnmappedGroups.RemoveAll(g => mappings.Any(m => m.Group == g));
        }

        return T(gate ?? GatewayResult.Done());
    }

    public Task<GatewayResult<IReadOnlyList<CampaignInfo>>> ListCampaignsAsync() =>
        T(Gate<IReadOnlyList<CampaignInfo>>("ListCampaigns") ?? GatewayResult.Ok<IReadOnlyList<CampaignInfo>>([.. Campaigns]));

    public Task<GatewayResult<CampaignInfo>> CreateCampaignAsync(CampaignRequest request)
    {
        var info = new CampaignInfo("c" + Campaigns.Count, request.Name, request.DueDate, 0, 0, null, false);
        Campaigns.Add(info);
        return T(Gate<CampaignInfo>("CreateCampaign") ?? GatewayResult.Ok(info));
    }

    public Task<GatewayResult<IReadOnlyList<ReviewItem>>> GetWorklistAsync(string campaignId) =>
        T(Gate<IReadOnlyList<ReviewItem>>("Worklist") ?? GatewayResult.Ok<IReadOnlyList<ReviewItem>>([.. Worklist]));

    public Task<GatewayResult<Unit>> SubmitDecisionsAsync(string campaignId, IReadOnlyDictionary<string, ReviewDecision> decisions)
    {
        LastDecisions = decisions;
        var gate = Gate<Unit>("SubmitDecisions");
        if (gate is null)
        {
            foreach (var (id, decision) in decisions)
            {
                var index = Worklist.FindIndex(i => i.Id == id);
                Worklist[index] = Worklist[index] with { Decision = decision };
            }
        }

        return T(gate ?? GatewayResult.Done());
    }

    public Task<GatewayResult<ReviewReport>> CompleteCampaignAsync(string campaignId) =>
        T(Gate<ReviewReport>("CompleteCampaign") ?? GatewayResult.Ok(new ReviewReport(
            campaignId,
            Worklist.Count(i => i.Decision == ReviewDecision.Keep),
            Worklist.Count(i => i.Decision == ReviewDecision.Remove),
            Worklist.Count(i => i.Decision == ReviewDecision.Undecided),
            Worklist.Count(i => i.Decision == ReviewDecision.Keep && i.SodOverridden))));

    public Task<GatewayResult<EmergencyPolicyInfo>> GetEmergencyPolicyAsync() =>
        T(Gate<EmergencyPolicyInfo>("EmergencyPolicy") ?? GatewayResult.Ok(Policy));

    public Task<GatewayResult<(IReadOnlyList<EmergencyGrantInfo> Mine, IReadOnlyList<EmergencyGrantInfo> ToApprove)>> ListEmergencyAsync() =>
        T(Gate<(IReadOnlyList<EmergencyGrantInfo>, IReadOnlyList<EmergencyGrantInfo>)>("ListEmergency")
            ?? GatewayResult.Ok(((IReadOnlyList<EmergencyGrantInfo>)[.. Mine], (IReadOnlyList<EmergencyGrantInfo>)[.. ToApprove])));

    public Task<GatewayResult<EmergencyGrantInfo>> RequestEmergencyAsync(EmergencyRequestDraft draft)
    {
        var grant = new EmergencyGrantInfo("g" + Mine.Count, "me", draft.RoleId, draft.Reason, EmergencyPhase.AwaitingApproval, null);
        Mine.Add(grant);
        return T(Gate<EmergencyGrantInfo>("RequestEmergency") ?? GatewayResult.Ok(grant));
    }

    public Task<GatewayResult<Unit>> DecideEmergencyAsync(string grantId, bool approve, string? reason)
    {
        ToApprove.RemoveAll(g => g.Id == grantId);
        return T(Gate<Unit>("DecideEmergency") ?? GatewayResult.Done());
    }

    public Task<GatewayResult<Unit>> EndEmergencyAsync(string grantId)
    {
        var index = Mine.FindIndex(g => g.Id == grantId);
        Mine[index] = Mine[index] with { Phase = EmergencyPhase.Ended };
        return T(Gate<Unit>("EndEmergency") ?? GatewayResult.Done());
    }

    public Task<GatewayResult<IReadOnlyList<EmergencyReviewTask>>> ListEmergencyReviewsAsync() =>
        T(Gate<IReadOnlyList<EmergencyReviewTask>>("ListReviews") ?? GatewayResult.Ok<IReadOnlyList<EmergencyReviewTask>>([.. Reviews]));

    public Task<GatewayResult<Unit>> ConfirmEmergencyReviewAsync(string reviewId)
    {
        Reviews.RemoveAll(r => r.Id == reviewId);
        return T(Gate<Unit>("ConfirmReview") ?? GatewayResult.Done());
    }

    public Task<GatewayResult<Unit>> FlagEmergencyReviewAsync(string reviewId, string reason)
    {
        Reviews.RemoveAll(r => r.Id == reviewId);
        return T(Gate<Unit>("FlagReview") ?? GatewayResult.Done());
    }

    public Task<GatewayResult<AnswerPage>> AskAsync(QuestionRequest request)
    {
        LastQuestion = request;
        return T(Gate<AnswerPage>("Ask") ?? GatewayResult.Ok(Answer));
    }

    public Task<GatewayResult<string>> ExportAnswerAsync(QuestionRequest request) =>
        T(Gate<string>("Export") ?? GatewayResult.Ok("subject,summary\n"));
}
