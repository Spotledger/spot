namespace Spotledger.AdminCenter.Tenants;

/// <summary>
/// The typed management API behind the tenant and fleet tools. Responses are typed records, never
/// scraped text; instants are arguments of the callers. A tool shows only what the API returns:
/// it never reaches into another tenant's data.
/// </summary>
public interface ITenantsApi
{
    /// <summary>Runs a catalogue action (create, suspend, move, install, remediate, resume...) with the approvals collected.</summary>
    Task ExecuteAsync(AdminAction action, IReadOnlyList<Approval> approvals, CancellationToken cancellationToken);

    /// <summary>Tenants the caller may administer.</summary>
    Task<IReadOnlyList<TenantInfo>> GetTenantsAsync(CancellationToken cancellationToken);

    /// <summary>Companies in a tenant.</summary>
    Task<IReadOnlyList<CompanyInfo>> GetCompaniesAsync(string tenantId, CancellationToken cancellationToken);

    /// <summary>Apps installed in a tenant.</summary>
    Task<IReadOnlyList<InstalledApp>> GetAppsAsync(string tenantId, CancellationToken cancellationToken);

    /// <summary>The plan for an app change: capabilities to approve and the migration plan.</summary>
    Task<AppChangePlan> PlanAppChangeAsync(string tenantId, string appId, AppChangeKind kind, CancellationToken cancellationToken);

    /// <summary>Migrations of a tenant, newest first, with their stage.</summary>
    Task<IReadOnlyList<MigrationSummary>> GetMigrationsAsync(string tenantId, CancellationToken cancellationToken);

    /// <summary>A migration's plan view: risk classes, conflicts and the plan's rendered text.</summary>
    Task<MigrationPlanView> GetMigrationPlanAsync(string planId, CancellationToken cancellationToken);

    /// <summary>The policy that applies to a plan.</summary>
    Task<ApprovalPolicy> GetPolicyAsync(string planId, CancellationToken cancellationToken);

    /// <summary>Rehearses the plan on a copy and reports.</summary>
    Task<RehearsalReport> RehearseAsync(string planId, CancellationToken cancellationToken);

    /// <summary>Applies the approved plan and streams <c>migration.progress</c>; the last item carries the outcome.</summary>
    IAsyncEnumerable<MigrationProgress> ApplyAsync(string planId, CancellationToken cancellationToken);

    /// <summary>Verifies an applied plan.</summary>
    Task<VerifyResult> VerifyAsync(string planId, CancellationToken cancellationToken);

    /// <summary>The migration audit for a tenant.</summary>
    Task<IReadOnlyList<AuditEntry>> GetHistoryAsync(string tenantId, CancellationToken cancellationToken);

    /// <summary>The settings schema and a group's desired values.</summary>
    Task<IReadOnlyList<SettingDescriptor>> GetSettingsSchemaAsync(CancellationToken cancellationToken);

    /// <summary>A group's desired values.</summary>
    Task<IReadOnlyDictionary<string, SettingValue>> GetDesiredAsync(string group, CancellationToken cancellationToken);

    /// <summary>Saves desired values for a group. Returns per-key errors (empty when saved).</summary>
    Task<IReadOnlyDictionary<string, string>> SetDesiredAsync(string group, IReadOnlyDictionary<string, string> changes, CancellationToken cancellationToken);

    /// <summary>Drift of a group against its desired state.</summary>
    Task<IReadOnlyList<DriftItem>> GetDriftAsync(string group, CancellationToken cancellationToken);

    /// <summary>The group's rollout, or null when none is configured.</summary>
    Task<RolloutStatus?> GetRolloutAsync(string group, CancellationToken cancellationToken);

    /// <summary>The fleet health grid from the telemetry feed.</summary>
    Task<IReadOnlyList<HealthRow>> GetHealthAsync(CancellationToken cancellationToken);

    /// <summary>The health poll interval, in seconds, from settings.</summary>
    Task<uint> GetPollIntervalSecondsAsync(CancellationToken cancellationToken);
}

/// <summary>A migration in the lifecycle board's list.</summary>
public sealed record MigrationSummary(string PlanId, string Title, MigrationStage Stage, MigrationRisk Risk);
