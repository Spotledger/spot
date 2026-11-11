namespace Spotledger.AdminCenter.Tenants;

/// <summary>Operation ids the tenant and fleet screens use.</summary>
public static class TenantOps
{
    /// <summary>Provisions a tenant.</summary>
    public const string TenantCreate = "tenant.create";

    /// <summary>Suspends a tenant.</summary>
    public const string TenantSuspend = "tenant.suspend";

    /// <summary>Resumes a tenant.</summary>
    public const string TenantResume = "tenant.resume";

    /// <summary>Moves a tenant to another server.</summary>
    public const string TenantMove = "tenant.move";

    /// <summary>Installs an app.</summary>
    public const string AppInstall = "app.install";

    /// <summary>Upgrades an app.</summary>
    public const string AppUpgrade = "app.upgrade";

    /// <summary>Uninstalls an app.</summary>
    public const string AppUninstall = "app.uninstall";

    /// <summary>Approves a migration plan.</summary>
    public const string MigrationApprove = "migration.approve";

    /// <summary>Remediates drift.</summary>
    public const string DriftRemediate = "desired.remediate";

    /// <summary>Starts the next rollout wave.</summary>
    public const string RolloutNextWave = "rollout.next_wave";

    /// <summary>Resumes a halted rollout.</summary>
    public const string RolloutResume = "rollout.resume";

    /// <summary>Saves a rollout's wave configuration.</summary>
    public const string RolloutConfigure = "rollout.configure";
}

/// <summary>Tenants and companies: list and search by group and tag, create, suspend, move, edition and usage.</summary>
public sealed class TenantsScreen : TenantScreen
{
    private IReadOnlyList<TenantInfo> _tenants = [];
    private IReadOnlyList<CompanyInfo> _companies = [];
    private string _search = string.Empty;
    private string? _group;
    private string? _tag;
    private TenantInfo? _selected;

    /// <summary>Creates the screen.</summary>
    public TenantsScreen(TenantContext context)
        : base(context)
    {
    }

    /// <inheritdoc />
    public override string Id => TenantTools.TenantsId;

    /// <inheritdoc />
    public override string Title => "Tenants and companies";

    /// <inheritdoc />
    public override IReadOnlyList<string> FocusOrder => ["Search", "Group", "Tag", "Tenants", "Create", "Suspend", "Move"];

    /// <summary>Text matched in names and ids.</summary>
    public string Search
    {
        get => _search;
        set
        {
            if (SetField(ref _search, value))
            {
                OnPropertyChanged(nameof(Visible));
            }
        }
    }

    /// <summary>Only this group, or null for all.</summary>
    public string? Group
    {
        get => _group;
        set
        {
            if (SetField(ref _group, value))
            {
                OnPropertyChanged(nameof(Visible));
            }
        }
    }

    /// <summary>Only tenants with this tag, or null for all.</summary>
    public string? Tag
    {
        get => _tag;
        set
        {
            if (SetField(ref _tag, value))
            {
                OnPropertyChanged(nameof(Visible));
            }
        }
    }

    /// <summary>The groups present, for the filter.</summary>
    public IReadOnlyList<string> Groups => [.. _tenants.Select(t => t.Group).Distinct(StringComparer.Ordinal).Order(StringComparer.Ordinal)];

    /// <summary>The tags present, for the filter.</summary>
    public IReadOnlyList<string> Tags => [.. _tenants.SelectMany(t => t.Tags).Distinct(StringComparer.Ordinal).Order(StringComparer.Ordinal)];

    /// <summary>The tenants that pass the filters.</summary>
    public IReadOnlyList<TenantInfo> Visible =>
    [
        .. _tenants.Where(t =>
            (Group is null || string.Equals(t.Group, Group, StringComparison.Ordinal))
            && (Tag is null || t.Tags.Contains(Tag, StringComparer.Ordinal))
            && (string.IsNullOrWhiteSpace(Search)
                || t.Name.Contains(Search, StringComparison.OrdinalIgnoreCase)
                || t.Id.Contains(Search, StringComparison.OrdinalIgnoreCase))),
    ];

    /// <summary>The tenant whose details show.</summary>
    public TenantInfo? Selected => _selected;

    /// <summary>The companies of the selected tenant.</summary>
    public IReadOnlyList<CompanyInfo> Companies => _companies;

    /// <summary>The selected tenant's quotas worth attention (near or over the limit).</summary>
    public IReadOnlyList<QuotaUsage> QuotaWarnings => [.. _selected?.Usage.Where(u => u.NearLimit) ?? []];

    /// <summary>Selects a tenant and loads its companies.</summary>
    public async Task<bool> SelectAsync(string tenantId, CancellationToken cancellationToken = default)
    {
        var tenant = _tenants.FirstOrDefault(t => string.Equals(t.Id, tenantId, StringComparison.Ordinal));
        if (tenant is null)
        {
            return false;
        }

        try
        {
            _companies = await Api.GetCompaniesAsync(tenantId, cancellationToken);
        }
        catch (TenantApiException refused)
        {
            Error = refused.Message;
            return false;
        }

        _selected = tenant;
        OnPropertyChanged(nameof(Selected));
        OnPropertyChanged(nameof(Companies));
        OnPropertyChanged(nameof(QuotaWarnings));
        return true;
    }

    /// <summary>Starts creating a tenant (the control plane provisions it). A name is required and unique.</summary>
    public bool Create(string name, string group, string edition)
    {
        if (string.IsNullOrWhiteSpace(name))
        {
            Error = "Enter a name for the tenant.";
            return false;
        }

        if (_tenants.Any(t => string.Equals(t.Name, name.Trim(), StringComparison.OrdinalIgnoreCase)))
        {
            Error = $"A tenant named {name.Trim()} already exists.";
            return false;
        }

        Begin(AdminAction.Create(TenantOps.TenantCreate, ("name", name.Trim()), ("group", group), ("edition", edition)), $"Create tenant {name.Trim()} on the {edition} edition.");
        return true;
    }

    /// <summary>Starts suspending a tenant. Already-suspended tenants are refused.</summary>
    public bool Suspend(string tenantId)
    {
        var tenant = _tenants.FirstOrDefault(t => string.Equals(t.Id, tenantId, StringComparison.Ordinal));
        if (tenant is null || tenant.State != TenantState.Active)
        {
            Error = "Only an active tenant can be suspended.";
            return false;
        }

        Begin(AdminAction.Create(TenantOps.TenantSuspend, ("tenant", tenantId)), $"Suspend {tenant.Name}: nobody can sign in until it is resumed. Data is kept.");
        return true;
    }

    /// <summary>Starts resuming a suspended tenant.</summary>
    public bool Resume(string tenantId)
    {
        var tenant = _tenants.FirstOrDefault(t => string.Equals(t.Id, tenantId, StringComparison.Ordinal));
        if (tenant is null || tenant.State != TenantState.Suspended)
        {
            Error = "Only a suspended tenant can be resumed.";
            return false;
        }

        Begin(AdminAction.Create(TenantOps.TenantResume, ("tenant", tenantId)), $"Resume {tenant.Name}.");
        return true;
    }

    /// <summary>Starts moving a tenant to another server.</summary>
    public bool Move(string tenantId, string targetServer)
    {
        var tenant = _tenants.FirstOrDefault(t => string.Equals(t.Id, tenantId, StringComparison.Ordinal));
        if (tenant is null || string.IsNullOrWhiteSpace(targetServer) || string.Equals(tenant.Server, targetServer, StringComparison.Ordinal))
        {
            Error = "Choose a different server to move to.";
            return false;
        }

        Begin(AdminAction.Create(TenantOps.TenantMove, ("tenant", tenantId), ("target", targetServer)), $"Move {tenant.Name} from {tenant.Server} to {targetServer}.");
        return true;
    }

    /// <inheritdoc />
    protected override async Task FetchAsync(long nowUnixSeconds, CancellationToken cancellationToken)
    {
        _tenants = await Api.GetTenantsAsync(cancellationToken);
        OnPropertyChanged(nameof(Visible));
        OnPropertyChanged(nameof(Groups));
        OnPropertyChanged(nameof(Tags));
    }
}

/// <summary>
/// Apps of one tenant: install, upgrade, uninstall. Each shows the capability-approval dialog (every
/// requested capability must be approved) and the migration plan before the signatures are asked for.
/// </summary>
public sealed class AppsScreen : TenantScreen
{
    private readonly string _tenantId;
    private IReadOnlyList<InstalledApp> _apps = [];
    private AppChangePlan? _plan;
    private readonly HashSet<string> _approved = new(StringComparer.Ordinal);

    /// <summary>Creates the screen for one tenant.</summary>
    public AppsScreen(TenantContext context, string tenantId)
        : base(context)
    {
        _tenantId = tenantId ?? throw new ArgumentNullException(nameof(tenantId));
    }

    /// <inheritdoc />
    public override string Id => TenantTools.AppsId;

    /// <inheritdoc />
    public override string Title => "Apps";

    /// <inheritdoc />
    public override IReadOnlyList<string> FocusOrder => ["Apps", "Install", "Upgrade", "Uninstall"];

    /// <summary>The installed apps.</summary>
    public IReadOnlyList<InstalledApp> Apps => _apps;

    /// <summary>The change being reviewed: capabilities and migration plan; null when none.</summary>
    public AppChangePlan? Plan => _plan;

    /// <summary>Capabilities the person has approved so far.</summary>
    public IReadOnlyCollection<string> ApprovedCapabilities => _approved;

    /// <summary>True when every requested capability is approved (trivially true with none).</summary>
    public bool CapabilitiesApproved => _plan is not null && _plan.Capabilities.All(c => _approved.Contains(c.Name));

    /// <summary>True when the migration plan, if any, has no blocking conflict.</summary>
    public bool CanProceed => _plan is { } plan && CapabilitiesApproved && plan.Migration is not { Blocked: true };

    /// <summary>Reviews installing an app.</summary>
    public Task<bool> InstallAsync(string appId, CancellationToken cancellationToken = default) => ReviewAsync(appId, AppChangeKind.Install, cancellationToken);

    /// <summary>Reviews upgrading an app.</summary>
    public Task<bool> UpgradeAsync(string appId, CancellationToken cancellationToken = default) => ReviewAsync(appId, AppChangeKind.Upgrade, cancellationToken);

    /// <summary>Reviews uninstalling an app.</summary>
    public Task<bool> UninstallAsync(string appId, CancellationToken cancellationToken = default) => ReviewAsync(appId, AppChangeKind.Uninstall, cancellationToken);

    /// <summary>Approves one requested capability in the dialog.</summary>
    public bool ApproveCapability(string name)
    {
        if (_plan is null || !_plan.Capabilities.Any(c => string.Equals(c.Name, name, StringComparison.Ordinal)))
        {
            return false;
        }

        _approved.Add(name);
        OnPropertyChanged(nameof(CapabilitiesApproved));
        OnPropertyChanged(nameof(CanProceed));
        return true;
    }

    /// <summary>
    /// Moves on to the signatures once every capability is approved and nothing blocks the migration.
    /// The action is sent only by <see cref="TenantScreen.ConfirmAsync"/> after the signatures.
    /// </summary>
    public bool Proceed()
    {
        if (_plan is not { } plan)
        {
            return false;
        }

        if (!CapabilitiesApproved)
        {
            Error = "Approve every capability the app asks for first.";
            return false;
        }

        if (plan.Migration is { Blocked: true })
        {
            Error = "The migration plan has a blocking conflict. Resolve it first.";
            return false;
        }

        var operation = plan.Kind switch
        {
            AppChangeKind.Install => TenantOps.AppInstall,
            AppChangeKind.Upgrade => TenantOps.AppUpgrade,
            _ => TenantOps.AppUninstall,
        };
        var args = new List<(string, string)> { ("tenant", _tenantId), ("app", plan.AppId) };
        if (plan.Migration is { } migration)
        {
            args.Add(("plan", migration.Digest));
        }

        Begin(AdminAction.Create(operation, [.. args]), plan.Summary);
        return true;
    }

    private async Task<bool> ReviewAsync(string appId, AppChangeKind kind, CancellationToken cancellationToken)
    {
        try
        {
            _plan = await Api.PlanAppChangeAsync(_tenantId, appId, kind, cancellationToken);
        }
        catch (TenantApiException refused)
        {
            Error = refused.Message;
            return false;
        }

        _approved.Clear();
        Error = null;
        OnPropertyChanged(nameof(Plan));
        OnPropertyChanged(nameof(ApprovedCapabilities));
        OnPropertyChanged(nameof(CapabilitiesApproved));
        OnPropertyChanged(nameof(CanProceed));
        return true;
    }

    /// <inheritdoc />
    protected override async Task FetchAsync(long nowUnixSeconds, CancellationToken cancellationToken)
    {
        _apps = await Api.GetAppsAsync(_tenantId, cancellationToken);
        OnPropertyChanged(nameof(Apps));
    }
}
