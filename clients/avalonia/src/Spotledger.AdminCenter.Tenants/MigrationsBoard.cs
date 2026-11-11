namespace Spotledger.AdminCenter.Tenants;

/// <summary>
/// The migration lifecycle board for one tenant: Plan, Approve, Rehearse, Apply, Verify. Each stage
/// is reachable only from the one before; the plan's risk classes and conflicts are shown first, the
/// approval buttons follow the policy, the rehearsal report shows the fingerprint diff, the apply
/// shows live progress from <c>migration.progress</c>, the verify result closes the loop and a
/// rollback leaves its evidence on screen. History comes from the migration audit.
/// </summary>
public sealed class MigrationsBoard : TenantScreen
{
    private readonly string _tenantId;
    private IReadOnlyList<MigrationSummary> _migrations = [];
    private IReadOnlyList<AuditEntry> _history = [];
    private MigrationSummary? _selected;
    private MigrationPlanView? _plan;
    private ApprovalPolicy? _policy;
    private MigrationStage _stage;
    private RehearsalReport? _rehearsal;
    private ApplyOutcome? _outcome;
    private VerifyResult? _verify;
    private string _currentStep = string.Empty;
    private int _percentBasisPoints;
    private bool _isApplying;
    private readonly List<string> _progress = [];

    /// <summary>Creates the board for one tenant.</summary>
    public MigrationsBoard(TenantContext context, string tenantId)
        : base(context)
    {
        _tenantId = tenantId ?? throw new ArgumentNullException(nameof(tenantId));
    }

    /// <inheritdoc />
    public override string Id => TenantTools.MigrationsId;

    /// <inheritdoc />
    public override string Title => "Migrations";

    /// <inheritdoc />
    public override IReadOnlyList<string> FocusOrder => ["Migrations", "Approve", "Rehearse", "Apply", "Verify", "History"];

    /// <summary>The tenant's migrations, newest first.</summary>
    public IReadOnlyList<MigrationSummary> Migrations => _migrations;

    /// <summary>The audit history.</summary>
    public IReadOnlyList<AuditEntry> History => _history;

    /// <summary>The selected migration.</summary>
    public MigrationSummary? Selected => _selected;

    /// <summary>The plan view of the selected migration.</summary>
    public MigrationPlanView? Plan => _plan;

    /// <summary>The policy for the selected plan.</summary>
    public ApprovalPolicy? Policy => _policy;

    /// <summary>The stage reached.</summary>
    public MigrationStage Stage
    {
        get => _stage;
        private set
        {
            if (SetField(ref _stage, value))
            {
                RaiseCan();
            }
        }
    }

    /// <summary>The rehearsal report.</summary>
    public RehearsalReport? Rehearsal
    {
        get => _rehearsal;
        private set
        {
            if (SetField(ref _rehearsal, value))
            {
                RaiseCan();
            }
        }
    }

    /// <summary>How the apply ended.</summary>
    public ApplyOutcome? Outcome
    {
        get => _outcome;
        private set => SetField(ref _outcome, value);
    }

    /// <summary>The verify result.</summary>
    public VerifyResult? Verify
    {
        get => _verify;
        private set => SetField(ref _verify, value);
    }

    /// <summary>The step the apply is on.</summary>
    public string CurrentStep
    {
        get => _currentStep;
        private set => SetField(ref _currentStep, value);
    }

    /// <summary>Apply progress in hundredths of a percent.</summary>
    public int PercentBasisPoints
    {
        get => _percentBasisPoints;
        private set => SetField(ref _percentBasisPoints, value);
    }

    /// <summary>The steps seen on the progress channel, in order.</summary>
    public IReadOnlyList<string> Progress => _progress;

    /// <summary>True while an apply streams.</summary>
    public bool IsApplying
    {
        get => _isApplying;
        private set
        {
            if (SetField(ref _isApplying, value))
            {
                RaiseCan();
            }
        }
    }

    /// <summary>True when the plan's risk or the policy needs a rehearsal before applying.</summary>
    public bool RehearsalRequired => _plan is { } plan && (plan.Risk == MigrationRisk.Destructive || _policy?.RehearsalRequired == true);

    /// <summary>True when Approve works: planned, not blocked.</summary>
    public bool CanApprove => _selected is not null && _plan is { Blocked: false } && Stage == MigrationStage.Plan && !IsApplying;

    /// <summary>True when Rehearse works: approved and not yet rehearsed.</summary>
    public bool CanRehearse => _selected is not null && Stage == MigrationStage.Approve && !IsApplying;

    /// <summary>True when Apply works: approved, and rehearsed where a rehearsal is required.</summary>
    public bool CanApply =>
        _selected is not null && !IsApplying
        && Rehearsal is not { Passed: false }
        && (Stage == MigrationStage.Rehearse || (Stage == MigrationStage.Approve && !RehearsalRequired));

    /// <summary>True when Verify works: applied successfully.</summary>
    public bool CanVerify => Stage == MigrationStage.Apply && Outcome is { Succeeded: true } && !IsApplying;

    /// <summary>The approval requirement in words ("2 approvals needed...").</summary>
    public string? ApprovalText => _policy is null
        ? null
        : $"{_policy.RequiredApprovals} approval{(_policy.RequiredApprovals == 1 ? string.Empty : "s")} needed. {_policy.Explanation}";

    /// <summary>Selects a migration and loads its plan, policy and stage.</summary>
    public async Task<bool> SelectAsync(string planId, CancellationToken cancellationToken = default)
    {
        var summary = _migrations.FirstOrDefault(m => string.Equals(m.PlanId, planId, StringComparison.Ordinal));
        if (summary is null)
        {
            return false;
        }

        try
        {
            _plan = await Api.GetMigrationPlanAsync(planId, cancellationToken);
            _policy = await Api.GetPolicyAsync(planId, cancellationToken);
        }
        catch (TenantApiException refused)
        {
            Error = refused.Message;
            return false;
        }

        _selected = summary;
        Rehearsal = null;
        Outcome = null;
        Verify = null;
        _progress.Clear();
        PercentBasisPoints = 0;
        CurrentStep = string.Empty;
        Stage = summary.Stage;
        OnPropertyChanged(nameof(Selected));
        OnPropertyChanged(nameof(Plan));
        OnPropertyChanged(nameof(Policy));
        OnPropertyChanged(nameof(ApprovalText));
        OnPropertyChanged(nameof(RehearsalRequired));
        OnPropertyChanged(nameof(Progress));
        RaiseCan();
        return true;
    }

    /// <summary>Starts approving the plan: the signatures the policy needs are collected through <see cref="TenantScreen.Gate"/>.</summary>
    public bool BeginApprove()
    {
        if (!CanApprove || _plan is null)
        {
            Error = _plan is { Blocked: true } ? "A blocking conflict must be resolved before approval." : "This plan cannot be approved now.";
            return false;
        }

        Begin(AdminAction.Create(TenantOps.MigrationApprove, ("plan", _plan.PlanId), ("digest", _plan.Digest)), $"Approve the migration ({_plan.Risk}).");
        return true;
    }

    /// <summary>Sends the approval once its signatures are in; the board moves to Approve.</summary>
    public async Task<bool> ApproveAsync(long nowUnixSeconds, CancellationToken cancellationToken = default)
    {
        if (Gate is null || !Gate.Action.OperationId.Equals(TenantOps.MigrationApprove, StringComparison.Ordinal))
        {
            return false;
        }

        if (!await ConfirmAsync(nowUnixSeconds, cancellationToken))
        {
            return false;
        }

        Stage = MigrationStage.Approve;
        return true;
    }

    /// <summary>Rehearses the plan. A failed rehearsal keeps the board at Approve; the report shows why.</summary>
    public async Task<bool> RehearseAsync(CancellationToken cancellationToken = default)
    {
        if (!CanRehearse || _plan is null)
        {
            Error = "Approve the plan first.";
            return false;
        }

        try
        {
            Rehearsal = await Api.RehearseAsync(_plan.PlanId, cancellationToken);
        }
        catch (TenantApiException refused)
        {
            Error = refused.Message;
            return false;
        }

        Error = null;
        if (Rehearsal.Passed)
        {
            Stage = MigrationStage.Rehearse;
        }

        return Rehearsal.Passed;
    }

    /// <summary>Applies the plan and follows <c>migration.progress</c>. Refused until approved (and rehearsed where required).</summary>
    public async Task<bool> ApplyAsync(CancellationToken cancellationToken = default)
    {
        if (!CanApply || _plan is null)
        {
            Error = Stage < MigrationStage.Approve
                ? "Approve the plan first."
                : RehearsalRequired ? "This plan must pass a rehearsal before it is applied." : "This plan cannot be applied now.";
            return false;
        }

        Error = null;
        IsApplying = true;
        try
        {
            await foreach (var update in Api.ApplyAsync(_plan.PlanId, cancellationToken))
            {
                CurrentStep = update.Step;
                PercentBasisPoints = update.PercentBasisPoints;
                _progress.Add(update.Step);
                OnPropertyChanged(nameof(Progress));
                if (update.Final is { } final)
                {
                    Outcome = final;
                }
            }
        }
        catch (TenantApiException refused)
        {
            Error = refused.Message;
            return false;
        }
        finally
        {
            IsApplying = false;
        }

        if (Outcome is not { Succeeded: true })
        {
            Error = Outcome?.RollbackEvidence is not null
                ? $"The migration failed and was rolled back. {Outcome.Detail}"
                : Outcome?.Detail ?? "The migration ended without a result.";
            return false;
        }

        Stage = MigrationStage.Apply;
        return true;
    }

    /// <summary>Verifies the applied migration.</summary>
    public async Task<bool> VerifyAsync(CancellationToken cancellationToken = default)
    {
        if (!CanVerify || _plan is null)
        {
            Error = "Apply the plan successfully first.";
            return false;
        }

        try
        {
            Verify = await Api.VerifyAsync(_plan.PlanId, cancellationToken);
        }
        catch (TenantApiException refused)
        {
            Error = refused.Message;
            return false;
        }

        if (!Verify.Ok)
        {
            Error = "The verify checks did not all pass.";
            return false;
        }

        Error = null;
        Stage = MigrationStage.Verify;
        return true;
    }

    private void RaiseCan()
    {
        OnPropertyChanged(nameof(CanApprove));
        OnPropertyChanged(nameof(CanRehearse));
        OnPropertyChanged(nameof(CanApply));
        OnPropertyChanged(nameof(CanVerify));
    }

    /// <inheritdoc />
    protected override async Task FetchAsync(long nowUnixSeconds, CancellationToken cancellationToken)
    {
        _migrations = await Api.GetMigrationsAsync(_tenantId, cancellationToken);
        _history = await Api.GetHistoryAsync(_tenantId, cancellationToken);
        OnPropertyChanged(nameof(Migrations));
        OnPropertyChanged(nameof(History));
        if (_selected is { } current && _migrations.FirstOrDefault(m => string.Equals(m.PlanId, current.PlanId, StringComparison.Ordinal)) is { } fresh && fresh.Stage > Stage)
        {
            Stage = fresh.Stage;
        }
    }
}
