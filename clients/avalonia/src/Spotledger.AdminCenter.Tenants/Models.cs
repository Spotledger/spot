namespace Spotledger.AdminCenter.Tenants;

/// <summary>The three-colour health model: worst first, so ordering by it puts red on top.</summary>
public enum HealthColor
{
    /// <summary>A person is needed.</summary>
    Red,

    /// <summary>The system is handling it.</summary>
    Amber,

    /// <summary>Healthy.</summary>
    Green,
}

/// <summary>A tenant's lifecycle state.</summary>
public enum TenantState
{
    /// <summary>Being provisioned.</summary>
    Provisioning,

    /// <summary>In service.</summary>
    Active,

    /// <summary>Suspended: data kept, nobody signs in.</summary>
    Suspended,
}

/// <summary>One metered quantity against its limit. A limit of 0 means unlimited.</summary>
public sealed record QuotaUsage(string Metric, ulong Used, ulong Limit)
{
    /// <summary>Whole percent used, or null when unlimited.</summary>
    public int? PercentUsed => Limit == 0 ? null : (int)Math.Min(1000, Used * 100 / Limit);

    /// <summary>True from 90 percent.</summary>
    public bool NearLimit => PercentUsed is >= 90;

    /// <summary>True when the limit is passed.</summary>
    public bool OverLimit => Limit != 0 && Used > Limit;
}

/// <summary>A tenant as the management API returns it.</summary>
public sealed record TenantInfo(
    string Id,
    string Name,
    string Group,
    IReadOnlyList<string> Tags,
    TenantState State,
    string Edition,
    string LicenceState,
    string Server,
    IReadOnlyList<QuotaUsage> Usage);

/// <summary>A company inside a tenant.</summary>
public sealed record CompanyInfo(string Id, string Name, string? ParentId);

/// <summary>An app installed in a tenant.</summary>
public sealed record InstalledApp(string AppId, string Name, string Version, string? AvailableVersion);

/// <summary>What kind of change to an app.</summary>
public enum AppChangeKind
{
    /// <summary>Install.</summary>
    Install,

    /// <summary>Upgrade.</summary>
    Upgrade,

    /// <summary>Uninstall.</summary>
    Uninstall,
}

/// <summary>A capability an app asks for. <see cref="Widened"/> marks one the app already had but now wants more of.</summary>
public sealed record CapabilityRequest(string Name, string Description, bool Widened);

/// <summary>How risky a migration change is. Ordered, the maximum is the plan's risk.</summary>
public enum MigrationRisk
{
    /// <summary>Safe.</summary>
    Additive,

    /// <summary>A person must look.</summary>
    NeedsReview,

    /// <summary>Destructive or irreversible.</summary>
    Destructive,
}

/// <summary>One planned change.</summary>
public sealed record PlannedChange(string Rule, string Description, MigrationRisk Risk);

/// <summary>A conflict between the release and what is installed.</summary>
public sealed record MigrationConflict(string Code, string Description, bool Blocking);

/// <summary>A migration plan as the API renders it: risk classes, conflicts and the plan's own text.</summary>
public sealed record MigrationPlanView(
    string PlanId,
    string Digest,
    MigrationRisk Risk,
    IReadOnlyList<PlannedChange> Changes,
    IReadOnlyList<MigrationConflict> Conflicts,
    string RenderedText,
    ulong EstimatedSeconds)
{
    /// <summary>True when any conflict blocks the plan.</summary>
    public bool Blocked => Conflicts.Any(c => c.Blocking);
}

/// <summary>What changing an app will do: the capabilities to approve and the migration plan.</summary>
public sealed record AppChangePlan(
    string AppId,
    AppChangeKind Kind,
    string Summary,
    IReadOnlyList<CapabilityRequest> Capabilities,
    MigrationPlanView? Migration);

/// <summary>The stages of a migration, in order.</summary>
public enum MigrationStage
{
    /// <summary>A plan exists.</summary>
    Plan,

    /// <summary>The plan is approved.</summary>
    Approve,

    /// <summary>The plan was rehearsed on a copy.</summary>
    Rehearse,

    /// <summary>The plan is being or was applied.</summary>
    Apply,

    /// <summary>The result was verified.</summary>
    Verify,
}

/// <summary>What the policy needs for a plan: how many approvals and whether a rehearsal must pass first.</summary>
public sealed record ApprovalPolicy(int RequiredApprovals, bool RehearsalRequired, string Explanation);

/// <summary>One table's fingerprint before and after a rehearsal.</summary>
public sealed record FingerprintDiff(string Table, string Before, string After, bool Expected);

/// <summary>The rehearsal's report.</summary>
public sealed record RehearsalReport(bool Passed, IReadOnlyList<string> Problems, IReadOnlyList<FingerprintDiff> Diffs);

/// <summary>How an apply ended; carries rollback evidence when it was rolled back.</summary>
public sealed record ApplyOutcome(bool Succeeded, string Detail, string? RollbackEvidence);

/// <summary>One update on the <c>migration.progress</c> channel. The last one carries the outcome.</summary>
public sealed record MigrationProgress(string Step, int PercentBasisPoints, ApplyOutcome? Final = null);

/// <summary>The verify result after an apply.</summary>
public sealed record VerifyResult(bool Ok, IReadOnlyList<string> Checks);

/// <summary>One migration audit entry.</summary>
public sealed record AuditEntry(long AtUnixSeconds, string Actor, string Event, string PlanId);

/// <summary>One drifted setting: a server or tenant whose observed value is not the desired one.</summary>
public sealed record DriftItem(string Id, string Subject, string Setting, string Desired, string Observed, bool Exempt);

/// <summary>How a wave takes its nodes.</summary>
public sealed record WaveConfig(string Name, int MaxNodes, ulong SoakSeconds, bool Canary);

/// <summary>Why a rollout paused, and where.</summary>
public sealed record PauseInfo(string Reason, string? Node, string? Wave, string Detail);

/// <summary>A rollout as the API reports it.</summary>
public sealed record RolloutStatus(
    string RolloutId,
    IReadOnlyList<WaveConfig> Waves,
    int CompletedWaves,
    PauseInfo? Paused);

/// <summary>One row of the fleet health grid.</summary>
public sealed record HealthRow(string Id, string Name, bool IsTenant, HealthColor Color, string Reason, long ObservedUnixSeconds);

/// <summary>A refusal from the management API in its own words.</summary>
public sealed class TenantApiException : Exception
{
    /// <summary>Creates the exception.</summary>
    public TenantApiException(string message)
        : base(message)
    {
    }

    /// <summary>Creates the exception.</summary>
    public TenantApiException()
        : base("The management API refused the request.")
    {
    }

    /// <summary>Creates the exception.</summary>
    public TenantApiException(string message, Exception innerException)
        : base(message, innerException)
    {
    }
}
