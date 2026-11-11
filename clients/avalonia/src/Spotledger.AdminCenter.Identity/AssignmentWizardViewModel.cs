using Spotledger.Client.Shell;

namespace Spotledger.AdminCenter.Identity;

/// <summary>The steps of the grant wizard, in order.</summary>
public enum GrantStep
{
    /// <summary>Pick the user and the role.</summary>
    Choose,

    /// <summary>Pick the scope and the validity dates.</summary>
    ScopeAndDates,

    /// <summary>The separation-of-duties result, before anything is granted.</summary>
    Check,

    /// <summary>The grant is done or requested.</summary>
    Done,
}

/// <summary>
/// The grant wizard: user and role, a scope picker (company, include descendants), validity
/// dates, then the separation-of-duties check shown BEFORE the grant is confirmed. A blocked grant
/// cannot be confirmed; one that needs approval is sent as a request; one that needs an override
/// requires a reason and a mitigating control. Dates are checked against a day passed in, never the clock.
/// </summary>
public sealed class AssignmentWizardViewModel : IdentityViewModel
{
    private GrantStep _step = GrantStep.Choose;
    private SodResult? _sod;
    private AssignmentRequest? _request;
    private string _overrideReason = string.Empty;
    private string _mitigatingControl = string.Empty;

    /// <summary>Creates the wizard.</summary>
    public AssignmentWizardViewModel(IIdentityGateway gateway, IIdentityShell shell)
        : base(gateway, shell)
    {
    }

    /// <summary>The user to grant to.</summary>
    public string? UserId { get; set; }

    /// <summary>
    /// The signed-in administrator, set by the host. A grant to oneself is refused here (no
    /// self-escalation); the server enforces it too.
    /// </summary>
    public string? CurrentUserId { get; set; }

    /// <summary>The role to grant.</summary>
    public string? RoleId { get; set; }

    /// <summary>The company the grant covers, or null for all.</summary>
    public string? CompanyId { get; set; }

    /// <summary>Whether child companies are included.</summary>
    public bool IncludeDescendants { get; set; }

    /// <summary>First day, or null for now.</summary>
    public DateOnly? ValidFrom { get; set; }

    /// <summary>Last day, or null for open-ended.</summary>
    public DateOnly? ValidTo { get; set; }

    /// <summary>The current step.</summary>
    public GrantStep Step
    {
        get => _step;
        private set => SetField(ref _step, value);
    }

    /// <summary>The check result, once the wizard reached the check step.</summary>
    public SodResult? Sod
    {
        get => _sod;
        private set
        {
            if (SetField(ref _sod, value))
            {
                OnPropertyChanged(nameof(CanConfirm));
                OnPropertyChanged(nameof(ConfirmLabel));
            }
        }
    }

    /// <summary>Why the grant is needed (override).</summary>
    public string OverrideReason
    {
        get => _overrideReason;
        set
        {
            if (SetField(ref _overrideReason, value))
            {
                OnPropertyChanged(nameof(CanConfirm));
            }
        }
    }

    /// <summary>The control that makes the override safe.</summary>
    public string MitigatingControl
    {
        get => _mitigatingControl;
        set
        {
            if (SetField(ref _mitigatingControl, value))
            {
                OnPropertyChanged(nameof(CanConfirm));
            }
        }
    }

    /// <summary>The outcome, after confirming.</summary>
    public GrantOutcome? Outcome { get; private set; }

    /// <summary>The scope the wizard describes.</summary>
    public AssignmentScope Scope => new(string.IsNullOrWhiteSpace(CompanyId) ? null : CompanyId, IncludeDescendants && !string.IsNullOrWhiteSpace(CompanyId));

    /// <summary>The label of the confirm button, which says what will happen.</summary>
    public string ConfirmLabel => Sod?.Outcome switch
    {
        SodOutcome.NeedsApproval => "Request approval",
        SodOutcome.NeedsOverride => "Grant with override",
        _ => "Grant",
    };

    /// <summary>True when the confirm button works now.</summary>
    public bool CanConfirm => Step == GrantStep.Check && Sod is { } sod && sod.Outcome switch
    {
        SodOutcome.Clear or SodOutcome.NeedsApproval => true,
        SodOutcome.NeedsOverride =>
            !string.IsNullOrWhiteSpace(OverrideReason) && !string.IsNullOrWhiteSpace(MitigatingControl),
        _ => false,
    };

    private bool IsSelfGrant =>
        !string.IsNullOrEmpty(CurrentUserId) && string.Equals(UserId, CurrentUserId, StringComparison.Ordinal);

    /// <summary>Moves from choosing to scope and dates. A user and a role are required.</summary>
    public bool Next()
    {
        if (Step == GrantStep.Choose)
        {
            if (string.IsNullOrWhiteSpace(UserId) || string.IsNullOrWhiteSpace(RoleId))
            {
                Error = "Choose a user and a role.";
                return false;
            }

            if (IsSelfGrant)
            {
                Error = "You cannot grant access to yourself. Ask another administrator.";
                return false;
            }

            Error = null;
            Step = GrantStep.ScopeAndDates;
            return true;
        }

        return false;
    }

    /// <summary>Goes back one step. Leaving the check forgets its result.</summary>
    public void Back()
    {
        Error = null;
        if (Step == GrantStep.Check)
        {
            Sod = null;
            Step = GrantStep.ScopeAndDates;
        }
        else if (Step == GrantStep.ScopeAndDates)
        {
            Step = GrantStep.Choose;
        }
    }

    /// <summary>
    /// Validates the dates against <paramref name="today"/> and runs the separation-of-duties check.
    /// The result is shown before anything is granted.
    /// </summary>
    public async Task<bool> RunCheckAsync(DateOnly today)
    {
        if (Step != GrantStep.ScopeAndDates || UserId is null || RoleId is null)
        {
            return false;
        }

        if (IsSelfGrant)
        {
            Error = "You cannot grant access to yourself. Ask another administrator.";
            return false;
        }

        if (ValidTo is { } to && to < today)
        {
            Error = "The end date is already past.";
            return false;
        }

        if (ValidFrom is { } from && ValidTo is { } end && end < from)
        {
            Error = "The end date is before the start date.";
            return false;
        }

        _request = new AssignmentRequest(UserId, RoleId, Scope, ValidFrom, ValidTo);
        var sod = await CallAsync(() => Gateway.CheckSodAsync(_request));
        if (sod is null)
        {
            return false;
        }

        Sod = sod;
        Step = GrantStep.Check;
        return true;
    }

    /// <summary>Confirms the grant (or requests approval). Refused while <see cref="CanConfirm"/> is false.</summary>
    public async Task<bool> ConfirmAsync()
    {
        var sod = Sod;
        if (!CanConfirm || _request is null || sod is null)
        {
            Error = sod?.Outcome == SodOutcome.Blocked
                ? "This grant is blocked by a separation-of-duties rule and cannot be confirmed."
                : "Complete the override reason and the mitigating control first.";
            return false;
        }

        SodOverride? sodOverride = sod.Outcome == SodOutcome.NeedsOverride
            ? new SodOverride(OverrideReason.Trim(), MitigatingControl.Trim())
            : null;
        var request = _request;
        GrantOutcome outcome = default;
        var granted = await TryAsync(async () =>
        {
            var result = await Gateway.GrantAssignmentAsync(request, sodOverride);
            outcome = result.Value;
            return result;
        });
        if (!granted)
        {
            return false;
        }

        Outcome = outcome;
        Status = outcome == GrantOutcome.Granted ? "Assignment granted." : "Approval requested. Nothing is granted yet.";
        Step = GrantStep.Done;
        return true;
    }

    /// <summary>The order the keyboard visits the controls of the current step (Tab order), for focus handling and tests.</summary>
    public IReadOnlyList<string> FocusOrder => Step switch
    {
        GrantStep.Choose => ["User", "Role", "Next"],
        GrantStep.ScopeAndDates => ["Company", "IncludeDescendants", "ValidFrom", "ValidTo", "Back", "Check"],
        GrantStep.Check => Sod?.Outcome == SodOutcome.NeedsOverride
            ? ["OverrideReason", "MitigatingControl", "Back", "Confirm"]
            : ["Back", "Confirm"],
        _ => ["Close"],
    };
}
