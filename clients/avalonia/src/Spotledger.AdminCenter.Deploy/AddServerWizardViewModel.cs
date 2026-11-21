namespace Spotledger.AdminCenter.Deploy;

/// <summary>The steps of the Add server wizard, in order.</summary>
public enum AddServerStep
{
    /// <summary>Name the server and create the enrolment token.</summary>
    Token,

    /// <summary>The token is handed out; waiting for the agent on the new server to appear.</summary>
    WaitingForAgent,

    /// <summary>The agent appeared; its facts are shown.</summary>
    Facts,

    /// <summary>Pre-flight results: pass, warn and fail rows with fix hints.</summary>
    Preflight,

    /// <summary>The server is ready to be given roles.</summary>
    Done,
}

/// <summary>
/// What the administrator takes to the new server: the one-time token, the command line and the QR
/// text. It exists only while the token is unused; <see cref="AddServerWizardViewModel"/> drops it
/// the moment the engine reports the token used or expired.
/// </summary>
public sealed class TokenHandout
{
    internal TokenHandout(EnrolmentOffer offer)
    {
        OfferId = offer.OfferId;
        Token = offer.Token;
        CommandLine = offer.CommandLine;
        QrPayload = offer.QrPayload;
        ExpiresAtUnixSeconds = offer.ExpiresAtUnixSeconds;
    }

    /// <summary>The offer's id (not a secret).</summary>
    public string OfferId { get; }

    /// <summary>The token itself.</summary>
    public string Token { get; }

    /// <summary>The command to run on the new server.</summary>
    public string CommandLine { get; }

    /// <summary>The text to turn into a QR code.</summary>
    public string QrPayload { get; }

    /// <summary>When the token stops working.</summary>
    public long ExpiresAtUnixSeconds { get; }

    /// <summary>Never prints the secret.</summary>
    public override string ToString() => $"TokenHandout {{ {OfferId}, secret: hidden }}";
}

/// <summary>
/// The Add server wizard: generate a one-time enrolment token (copy it, scan it or paste the
/// command), wait for the agent to appear, show the server's facts, then run pre-flight and show
/// every row with its fix hint. A used or expired token is never shown again. Time is an argument.
/// </summary>
public sealed class AddServerWizardViewModel : DeployViewModel
{
    private readonly IDeployShell _shell;
    private AddServerStep _step = AddServerStep.Token;
    private TokenHandout? _handout;
    private ServerFacts? _facts;
    private IReadOnlyList<PreflightRow> _rows = [];
    private string _label = string.Empty;

    /// <summary>Creates the wizard.</summary>
    public AddServerWizardViewModel(IDeploymentEngine engine, IDeployShell shell)
        : base(engine)
    {
        _shell = shell ?? throw new ArgumentNullException(nameof(shell));
    }

    /// <summary>A name for the new server, shown in the offer list.</summary>
    public string Label
    {
        get => _label;
        set => SetField(ref _label, value);
    }

    /// <summary>The current step.</summary>
    public AddServerStep Step
    {
        get => _step;
        private set => SetField(ref _step, value);
    }

    /// <summary>The token and commands; null before it is created and after it is used or expires.</summary>
    public TokenHandout? Handout
    {
        get => _handout;
        private set => SetField(ref _handout, value);
    }

    /// <summary>The server's facts, once its agent appeared.</summary>
    public ServerFacts? Facts
    {
        get => _facts;
        private set => SetField(ref _facts, value);
    }

    /// <summary>The pre-flight rows, failures first.</summary>
    public IReadOnlyList<PreflightRow> Rows
    {
        get => _rows;
        private set
        {
            if (SetField(ref _rows, value))
            {
                OnPropertyChanged(nameof(CanFinish));
                OnPropertyChanged(nameof(Summary));
            }
        }
    }

    /// <summary>True when pre-flight ran and no row fails.</summary>
    public bool CanFinish => Step == AddServerStep.Preflight && Rows.Count > 0 && Rows.All(row => row.Severity != Severity.Fail);

    /// <summary>One line over the rows, e.g. "1 failed, 2 to look at, 5 passed".</summary>
    public string Summary
    {
        get
        {
            var failed = Rows.Count(row => row.Severity == Severity.Fail);
            var warned = Rows.Count(row => row.Severity == Severity.Warn);
            var passed = Rows.Count(row => row.Severity == Severity.Pass);
            return $"{failed} failed, {warned} to look at, {passed} passed";
        }
    }

    /// <summary>Creates the enrolment token. A name is required.</summary>
    public async Task<bool> CreateTokenAsync(CancellationToken cancellationToken = default)
    {
        if (string.IsNullOrWhiteSpace(Label))
        {
            Error = "Give the new server a name first.";
            return false;
        }

        EnrolmentOffer? offer = null;
        if (!await TryAsync(async () => offer = await Engine.CreateEnrolmentAsync(Label.Trim(), cancellationToken)))
        {
            return false;
        }

        Handout = new TokenHandout(offer!);
        Status = "Run the command on the new server. This page shows the token until the server connects.";
        Step = AddServerStep.WaitingForAgent;
        return true;
    }

    /// <summary>Copies the command line for the new server. Nothing is copied when no token is shown.</summary>
    public async Task<bool> CopyCommandAsync()
    {
        if (Handout is not { } handout)
        {
            return false;
        }

        await _shell.CopyAsync(handout.CommandLine);
        Status = "Command copied.";
        return true;
    }

    /// <summary>
    /// Asks the engine whether the agent appeared. A used token disappears from the screen at once;
    /// an expired one does too and the wizard goes back to creating a new one.
    /// </summary>
    public async Task<bool> PollAsync(long nowUnixSeconds, CancellationToken cancellationToken = default)
    {
        if (Step != AddServerStep.WaitingForAgent || Handout is not { } handout)
        {
            return false;
        }

        EnrolmentStatus? status = null;
        if (!await TryAsync(async () => status = await Engine.GetEnrolmentAsync(handout.OfferId, nowUnixSeconds, cancellationToken)))
        {
            return false;
        }

        switch (status!.State)
        {
            case EnrolmentState.Used when status.Facts is { } facts:
                Handout = null;
                Facts = facts;
                Status = "The server connected.";
                Step = AddServerStep.Facts;
                return true;
            case EnrolmentState.Used:
                // Used but the agent has not reported yet: the token is spent, so hide it.
                Handout = null;
                Status = "The server connected. Waiting for its report.";
                return false;
            case EnrolmentState.Expired:
                Handout = null;
                Error = "The token expired before the server used it. Create a new one.";
                Step = AddServerStep.Token;
                return false;
            default:
                if (nowUnixSeconds >= handout.ExpiresAtUnixSeconds)
                {
                    Handout = null;
                    Error = "The token expired before the server used it. Create a new one.";
                    Step = AddServerStep.Token;
                }

                return false;
        }
    }

    /// <summary>Runs pre-flight for the new server and shows every row.</summary>
    public async Task<bool> RunPreflightAsync(CancellationToken cancellationToken = default)
    {
        if (Facts is not { } facts || Step is not (AddServerStep.Facts or AddServerStep.Preflight))
        {
            return false;
        }

        IReadOnlyList<PreflightRow>? rows = null;
        if (!await TryAsync(async () => rows = await Engine.RunPreflightAsync([facts.Host], cancellationToken)))
        {
            return false;
        }

        Rows = [.. rows!.OrderByDescending(row => row.Severity)];
        Step = AddServerStep.Preflight;
        Status = CanFinish ? "The server passed pre-flight." : "Fix the failed checks, then run pre-flight again.";
        return true;
    }

    /// <summary>Finishes. Refused while a pre-flight row fails.</summary>
    public bool Finish()
    {
        if (!CanFinish)
        {
            Error = "A pre-flight check failed. Fix it and run pre-flight again.";
            return false;
        }

        Error = null;
        Step = AddServerStep.Done;
        Status = "The server is ready. Give it roles in Create cluster.";
        return true;
    }

    /// <summary>The order the keyboard visits the controls of the current step, for focus handling and tests.</summary>
    public IReadOnlyList<string> FocusOrder => Step switch
    {
        AddServerStep.Token => ["Label", "CreateToken"],
        AddServerStep.WaitingForAgent => ["CopyCommand", "ShowQr", "CheckNow", "Cancel"],
        AddServerStep.Facts => ["RunPreflight"],
        AddServerStep.Preflight => CanFinish ? ["RunPreflightAgain", "Finish"] : ["RunPreflightAgain"],
        _ => ["Close"],
    };
}
