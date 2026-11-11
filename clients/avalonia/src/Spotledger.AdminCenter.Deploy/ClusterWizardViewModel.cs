namespace Spotledger.AdminCenter.Deploy;

/// <summary>The steps of the Create cluster wizard, in order.</summary>
public enum ClusterStep
{
    /// <summary>Choose servers and give them roles; the engine validates as you go.</summary>
    Servers,

    /// <summary>Choose the profile.</summary>
    Profile,

    /// <summary>The plan preview with the estimated time.</summary>
    Plan,

    /// <summary>Collect the approvers' signatures.</summary>
    Sign,

    /// <summary>Execution: per-step status, logs, retry and resume.</summary>
    Run,

    /// <summary>Final verification: health and endpoints.</summary>
    Verify,
}

/// <summary>A profile with the words that explain it.</summary>
public sealed record ProfileChoice(DeployProfile Profile, string Label, string Explanation);

/// <summary>A role with the words that explain it.</summary>
public sealed record RoleChoice(NodeRole Role, string Label, string Explanation);

/// <summary>
/// The Create cluster wizard. Roles go onto servers by drag (the screen calls
/// <see cref="AssignRoleAsync"/> on drop) or by keyboard (<see cref="SelectedRole"/>,
/// <see cref="SelectedHost"/> and <see cref="AssignSelectedAsync"/>); both reach the engine's
/// validation, whose messages (problem and fix, all at once) are shown live. The plan is previewed
/// with its estimate, signed by the required approvers, run and verified. The place in the wizard
/// is saved so it survives closing the app.
/// </summary>
public sealed class ClusterWizardViewModel : DeployViewModel
{
    private readonly IWizardStateStore _store;
    private readonly List<ServerNode> _nodes = [];
    private IReadOnlyList<ServerFacts> _servers = [];
    private IReadOnlyList<Violation> _violations = [];
    private IReadOnlyList<PreflightRow> _preflight = [];
    private ClusterStep _step = ClusterStep.Servers;
    private DeployProfile _profile = DeployProfile.ClusterBasic;
    private string? _webAddress;
    private NodeRole? _selectedRole;
    private string? _selectedHost;
    private PlanView? _plan;
    private PlanSigning? _signing;
    private RunViewModel? _run;
    private VerificationReport? _verification;
    private int _validationRound;

    /// <summary>Creates the wizard.</summary>
    public ClusterWizardViewModel(IDeploymentEngine engine, IWizardStateStore store)
        : base(engine)
    {
        _store = store ?? throw new ArgumentNullException(nameof(store));
    }

    /// <summary>Every role with its plain explanation, in the order the palette shows them.</summary>
    public static IReadOnlyList<RoleChoice> Roles { get; } =
        [.. Enum.GetValues<NodeRole>().Select(role => new RoleChoice(role, Wording.Label(role), Wording.Explain(role)))];

    /// <summary>Every profile with its plain explanation.</summary>
    public static IReadOnlyList<ProfileChoice> Profiles { get; } =
        [.. Enum.GetValues<DeployProfile>().Select(profile => new ProfileChoice(profile, Wording.Label(profile), Wording.Explain(profile)))];

    /// <summary>The current step.</summary>
    public ClusterStep Step
    {
        get => _step;
        private set
        {
            if (SetField(ref _step, value))
            {
                OnPropertyChanged(nameof(FocusOrder));
            }
        }
    }

    /// <summary>Servers that have an agent and can join the cluster.</summary>
    public IReadOnlyList<ServerFacts> AvailableServers
    {
        get => _servers;
        private set => SetField(ref _servers, value);
    }

    /// <summary>The servers in the cluster with their roles.</summary>
    public IReadOnlyList<ServerNode> Nodes => _nodes;

    /// <summary>The chosen profile.</summary>
    public DeployProfile Profile => _profile;

    /// <summary>The address people will use.</summary>
    public string? WebAddress => _webAddress;

    /// <summary>The engine's current findings, all at once, each with its fix.</summary>
    public IReadOnlyList<Violation> Violations
    {
        get => _violations;
        private set
        {
            if (SetField(ref _violations, value))
            {
                OnPropertyChanged(nameof(IsValid));
                OnPropertyChanged(nameof(CanGoNext));
            }
        }
    }

    /// <summary>True when there is at least one server and the engine reports no violation.</summary>
    public bool IsValid => _nodes.Count > 0 && Violations.Count == 0;

    /// <summary>The role picked with the keyboard.</summary>
    public NodeRole? SelectedRole
    {
        get => _selectedRole;
        set => SetField(ref _selectedRole, value);
    }

    /// <summary>The server picked with the keyboard.</summary>
    public string? SelectedHost
    {
        get => _selectedHost;
        set => SetField(ref _selectedHost, value);
    }

    /// <summary>Pre-flight rows for the chosen servers, failures first.</summary>
    public IReadOnlyList<PreflightRow> Preflight
    {
        get => _preflight;
        private set => SetField(ref _preflight, value);
    }

    /// <summary>The plan being previewed or run.</summary>
    public PlanView? Plan
    {
        get => _plan;
        private set => SetField(ref _plan, value);
    }

    /// <summary>The signature collection, in the Sign step.</summary>
    public PlanSigning? Signing
    {
        get => _signing;
        private set => SetField(ref _signing, value);
    }

    /// <summary>The execution view, in the Run and Verify steps.</summary>
    public RunViewModel? Run
    {
        get => _run;
        private set => SetField(ref _run, value);
    }

    /// <summary>The final report.</summary>
    public VerificationReport? Verification
    {
        get => _verification;
        private set => SetField(ref _verification, value);
    }

    /// <summary>True when Next works in the current step.</summary>
    public bool CanGoNext => Step switch
    {
        ClusterStep.Servers => _nodes.Count > 0,
        ClusterStep.Profile => IsValid,
        ClusterStep.Plan => Plan is not null,
        ClusterStep.Sign => Signing?.IsComplete == true,
        _ => false,
    };

    /// <summary>Loads the servers that have an agent.</summary>
    public async Task<bool> LoadServersAsync(CancellationToken cancellationToken = default)
    {
        IReadOnlyList<ServerFacts>? servers = null;
        if (!await TryAsync(async () => servers = await Engine.ListServersAsync(cancellationToken)))
        {
            return false;
        }

        AvailableServers = servers!;
        return true;
    }

    /// <summary>Puts a server into the cluster (no roles yet).</summary>
    public async Task<bool> AddServerAsync(string host, CancellationToken cancellationToken = default)
    {
        if (Step != ClusterStep.Servers || _nodes.Exists(node => Same(node.Host, host)))
        {
            return false;
        }

        if (!_servers.Any(server => Same(server.Host, host)))
        {
            Error = $"{host} has no agent yet. Add it with Add server first.";
            return false;
        }

        _nodes.Add(new ServerNode(host, new HashSet<NodeRole>()));
        return await ChangedAsync(cancellationToken);
    }

    /// <summary>Takes a server out of the cluster with its roles.</summary>
    public async Task<bool> RemoveServerAsync(string host, CancellationToken cancellationToken = default)
    {
        if (Step != ClusterStep.Servers || _nodes.RemoveAll(node => Same(node.Host, host)) == 0)
        {
            return false;
        }

        return await ChangedAsync(cancellationToken);
    }

    /// <summary>Gives a role to a server. This is what a drop does; the keyboard path ends here too.</summary>
    public async Task<bool> AssignRoleAsync(NodeRole role, string host, CancellationToken cancellationToken = default)
    {
        if (!Replace(host, node => node.Roles.Contains(role) ? node : node with { Roles = new HashSet<NodeRole>(node.Roles) { role } }))
        {
            return false;
        }

        return await ChangedAsync(cancellationToken);
    }

    /// <summary>Takes a role away from a server.</summary>
    public async Task<bool> UnassignRoleAsync(NodeRole role, string host, CancellationToken cancellationToken = default)
    {
        if (!Replace(host, node => node with { Roles = new HashSet<NodeRole>(node.Roles.Where(r => r != role)) }))
        {
            return false;
        }

        return await ChangedAsync(cancellationToken);
    }

    /// <summary>Moves a role from one server to another (a drag between servers).</summary>
    public async Task<bool> MoveRoleAsync(NodeRole role, string from, string to, CancellationToken cancellationToken = default)
    {
        if (Same(from, to) || !Replace(from, node => node with { Roles = new HashSet<NodeRole>(node.Roles.Where(r => r != role)) }))
        {
            return false;
        }

        Replace(to, node => node with { Roles = new HashSet<NodeRole>(node.Roles) { role } });
        return await ChangedAsync(cancellationToken);
    }

    /// <summary>The keyboard way to drop: assigns <see cref="SelectedRole"/> to <see cref="SelectedHost"/>.</summary>
    public async Task<bool> AssignSelectedAsync(CancellationToken cancellationToken = default)
    {
        if (SelectedRole is not { } role || SelectedHost is not { } host)
        {
            Error = "Pick a role and a server first.";
            return false;
        }

        return await AssignRoleAsync(role, host, cancellationToken);
    }

    /// <summary>Marks the database primary.</summary>
    public async Task<bool> SetPrimaryAsync(string host, CancellationToken cancellationToken = default)
    {
        if (!_nodes.Exists(node => Same(node.Host, host)))
        {
            return false;
        }

        for (var i = 0; i < _nodes.Count; i++)
        {
            _nodes[i] = _nodes[i] with { Primary = Same(_nodes[i].Host, host) };
        }

        return await ChangedAsync(cancellationToken);
    }

    /// <summary>Sets the address people will use.</summary>
    public async Task<bool> SetWebAddressAsync(string? address, CancellationToken cancellationToken = default)
    {
        _webAddress = string.IsNullOrWhiteSpace(address) ? null : address.Trim();
        OnPropertyChanged(nameof(WebAddress));
        return await ChangedAsync(cancellationToken);
    }

    /// <summary>Chooses the profile.</summary>
    public async Task<bool> SetProfileAsync(DeployProfile profile, CancellationToken cancellationToken = default)
    {
        _profile = profile;
        OnPropertyChanged(nameof(Profile));
        return await ChangedAsync(cancellationToken);
    }

    /// <summary>Moves to the next step: validates, runs pre-flight and plans as needed.</summary>
    public async Task<bool> NextAsync(CancellationToken cancellationToken = default)
    {
        switch (Step)
        {
            case ClusterStep.Servers when _nodes.Count > 0:
                Step = ClusterStep.Profile;
                await SaveAsync(cancellationToken);
                return true;
            case ClusterStep.Profile:
                return await PreviewPlanAsync(cancellationToken);
            case ClusterStep.Plan when Plan is { } plan:
                Signing = new PlanSigning(plan);
                Step = ClusterStep.Sign;
                await SaveAsync(cancellationToken);
                return true;
            case ClusterStep.Sign when Signing is { IsComplete: true } signing && Plan is { } plan:
                Run = new RunViewModel(Engine, plan, signing.Signatures);
                Step = ClusterStep.Run;
                await SaveAsync(cancellationToken);
                return true;
            default:
                return false;
        }
    }

    /// <summary>Goes back one step before anything runs. A run in progress cannot be left this way.</summary>
    public void Back()
    {
        Error = null;
        switch (Step)
        {
            case ClusterStep.Profile:
                Step = ClusterStep.Servers;
                break;
            case ClusterStep.Plan:
                Plan = null;
                Step = ClusterStep.Profile;
                break;
            case ClusterStep.Sign:
                Signing = null;
                Step = ClusterStep.Plan;
                break;
        }
    }

    /// <summary>Starts the run (Run step).</summary>
    public async Task<bool> StartAsync(CancellationToken cancellationToken = default)
    {
        if (Step != ClusterStep.Run || Run is not { } run)
        {
            return false;
        }

        var ok = await run.StartAsync(cancellationToken);
        return ok && await VerifyAsync(cancellationToken);
    }

    /// <summary>Retries the failed step (Run step) and, when the run completes, verifies.</summary>
    public async Task<bool> RetryAsync(CancellationToken cancellationToken = default)
    {
        if (Step != ClusterStep.Run || Run is not { } run)
        {
            return false;
        }

        var ok = await run.RetryAsync(cancellationToken);
        return ok && await VerifyAsync(cancellationToken);
    }

    /// <summary>
    /// After the app was closed: loads the saved place and, when a plan was running, rebuilds the
    /// execution view from the engine's journal. A finished deployment goes straight to its
    /// verification. Returns false when nothing was saved.
    /// </summary>
    public async Task<bool> RestoreAsync(CancellationToken cancellationToken = default)
    {
        WizardState? saved = null;
        if (!await TryAsync(async () => saved = await _store.LoadAsync(cancellationToken)) || saved is null)
        {
            return false;
        }

        _nodes.Clear();
        _nodes.AddRange(saved.Topology.Nodes);
        _profile = saved.Topology.Profile;
        _webAddress = saved.Topology.WebAddress;
        OnPropertyChanged(nameof(Nodes));
        OnPropertyChanged(nameof(Profile));
        OnPropertyChanged(nameof(WebAddress));
        await LoadServersAsync(cancellationToken);
        await RefreshValidationAsync(cancellationToken);

        if (saved.PlanId is not { } planId || saved.Step < ClusterStep.Run)
        {
            // A plan that was only previewed or being signed is made again: nothing ran.
            Step = saved.Step >= ClusterStep.Plan && saved.Step < ClusterStep.Run ? ClusterStep.Profile : saved.Step;
            return true;
        }

        PlanView? plan = null;
        RunSnapshot? journal = null;
        if (!await TryAsync(async () =>
            {
                plan = await Engine.GetPlanAsync(planId, cancellationToken);
                journal = await Engine.GetJournalAsync(planId, cancellationToken);
            }) || plan is null)
        {
            Error ??= "The saved deployment could not be found on the server.";
            return false;
        }

        Plan = plan;
        Run = new RunViewModel(Engine, plan, []);
        if (journal is not null)
        {
            Run.Seed(journal);
        }

        Step = ClusterStep.Run;
        if (Run.State == RunState.Complete)
        {
            return await VerifyAsync(cancellationToken);
        }

        Status = "Your deployment was left part-way. Continue to finish it.";
        return true;
    }

    private async Task<bool> PreviewPlanAsync(CancellationToken cancellationToken)
    {
        if (!IsValid)
        {
            Error = "Fix the problems listed first.";
            return false;
        }

        var hosts = _nodes.Select(node => node.Host).ToList();
        IReadOnlyList<PreflightRow>? rows = null;
        if (!await TryAsync(async () => rows = await Engine.RunPreflightAsync(hosts, cancellationToken)))
        {
            return false;
        }

        Preflight = [.. rows!.OrderByDescending(row => row.Severity)];
        if (Preflight.Any(row => row.Severity == Severity.Fail))
        {
            Error = "Pre-flight found problems that block the deployment. Fix them and try again.";
            return false;
        }

        PlanView? plan = null;
        if (!await TryAsync(async () => plan = await Engine.PlanAsync(Draft(), cancellationToken)))
        {
            return false;
        }

        Plan = plan;
        Step = ClusterStep.Plan;
        await SaveAsync(cancellationToken);
        return true;
    }

    private async Task<bool> VerifyAsync(CancellationToken cancellationToken)
    {
        if (Run is not { State: RunState.Complete } run)
        {
            return false;
        }

        VerificationReport? report = null;
        if (!await TryAsync(async () => report = await Engine.VerifyAsync(run.Plan.PlanId, cancellationToken)))
        {
            return false;
        }

        Verification = report;
        Step = ClusterStep.Verify;
        try
        {
            await _store.ClearAsync(cancellationToken);
        }
        catch (DeployEngineException)
        {
            // The deployment is finished; a stale saved place only offers a harmless restore.
        }

        Status = report!.Health.All(row => row.Healthy)
            ? "The cluster is deployed and healthy."
            : "The cluster is deployed, but some services are not healthy.";
        return true;
    }

    private TopologyDraft Draft() => new([.. _nodes], _profile, _webAddress);

    private bool Replace(string host, Func<ServerNode, ServerNode> change)
    {
        if (Step != ClusterStep.Servers)
        {
            return false;
        }

        var index = _nodes.FindIndex(node => Same(node.Host, host));
        if (index < 0)
        {
            Error = $"{host} is not in the cluster yet.";
            return false;
        }

        _nodes[index] = change(_nodes[index]);
        return true;
    }

    private async Task<bool> ChangedAsync(CancellationToken cancellationToken)
    {
        Error = null;
        OnPropertyChanged(nameof(Nodes));
        OnPropertyChanged(nameof(CanGoNext));
        await RefreshValidationAsync(cancellationToken);
        await SaveAsync(cancellationToken);
        return true;
    }

    /// <summary>Asks the engine for every violation. An answer to an older edit is dropped.</summary>
    private async Task RefreshValidationAsync(CancellationToken cancellationToken)
    {
        var round = ++_validationRound;
        IReadOnlyList<Violation>? violations = null;
        if (!await TryAsync(async () => violations = await Engine.ValidateAsync(Draft(), cancellationToken)))
        {
            return;
        }

        if (round == _validationRound)
        {
            Violations = violations!;
        }
    }

    private async Task SaveAsync(CancellationToken cancellationToken)
    {
        try
        {
            await _store.SaveAsync(new WizardState(Draft(), Plan?.PlanId, Step), cancellationToken);
        }
        catch (DeployEngineException failure)
        {
            Error = $"Your place could not be saved: {failure.Message}";
        }
    }

    private static bool Same(string a, string b) => string.Equals(a, b, StringComparison.OrdinalIgnoreCase);

    /// <summary>The order the keyboard visits the controls of the current step, for focus handling and tests.</summary>
    public IReadOnlyList<string> FocusOrder => Step switch
    {
        ClusterStep.Servers => ["Servers", "Roles", "AssignSelected", "WebAddress", "Next"],
        ClusterStep.Profile => ["Profile", "Back", "Next"],
        ClusterStep.Plan => ["Back", "Next"],
        ClusterStep.Sign => ["Sign", "Back", "Next"],
        ClusterStep.Run => Run?.CanRetry == true ? ["Retry", "Log"] : ["Start", "Log"],
        _ => ["Close"],
    };
}
