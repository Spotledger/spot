using Spotledger.Client.Shell;

namespace Spotledger.AdminCenter.Servers;

/// <summary>
/// Renders a module's <see cref="PlanPreview"/>: lines for what is added, changed and removed, the
/// risk and reversibility in words, and the warnings a person must read before signing.
/// </summary>
public sealed class PlanPreviewDialog
{
    /// <summary>Creates the dialog over a preview.</summary>
    public PlanPreviewDialog(PlanPreview preview)
    {
        Preview = preview ?? throw new ArgumentNullException(nameof(preview));
    }

    /// <summary>The preview shown.</summary>
    public PlanPreview Preview { get; }

    /// <summary>One sentence for the title.</summary>
    public string Summary => Preview.Summary;

    /// <summary>The risk in words.</summary>
    public string RiskText => Preview.Risk switch
    {
        PreviewRisk.Low => "Low risk",
        PreviewRisk.Medium => "Medium risk: people may notice",
        PreviewRisk.High => "High risk: service can be interrupted",
        _ => "Critical risk: data can be lost or access cut off",
    };

    /// <summary>Whether the change can be undone, in words.</summary>
    public string ReversibleText => Preview.Reversible
        ? Preview.RevertWindowSeconds is { } window
            ? $"Reverts itself after {window} seconds unless you confirm it."
            : "Can be undone."
        : "Cannot be undone.";

    /// <summary>The preview as lines: "+" adds, "~" changes, "-" removes.</summary>
    public IReadOnlyList<string> Lines =>
    [
        .. Preview.Adds.Select(add => $"+ {add}"),
        .. Preview.Changes.Select(change => $"~ {change.Field}: {change.Current ?? "(none)"} -> {change.Desired ?? "(removed)"}"),
        .. Preview.Removes.Select(remove => $"- {remove}"),
    ];

    /// <summary>The warnings to read; non-empty means the person must acknowledge them.</summary>
    public IReadOnlyList<string> Warnings => Preview.Warnings;
}

/// <summary>The signing step of one operation: who has signed, who is awaited, and the signed text.</summary>
public sealed class SignaturesPanel : ObservableObject
{
    private string? _error;

    /// <summary>Creates the panel over a signing session.</summary>
    public SignaturesPanel(SigningSession session, string signedText)
    {
        Session = session ?? throw new ArgumentNullException(nameof(session));
        SignedText = signedText;
    }

    /// <summary>The underlying session.</summary>
    public SigningSession Session { get; }

    /// <summary>The exact text being signed (secrets masked).</summary>
    public string SignedText { get; }

    /// <summary>How many signatures the operation needs.</summary>
    public int Required => Session.Required;

    /// <summary>Who has signed.</summary>
    public IReadOnlyList<string> Signers => [.. Session.Approvals.Select(a => a.SignerId)];

    /// <summary>What the screen says: waiting for your signature, for a second approver, or signed.</summary>
    public string StatusText => Session.StatusText;

    /// <summary>True when the operation may run.</summary>
    public bool IsSatisfied => Session.State is SigningState.NotRequired or SigningState.Complete;

    /// <summary>True while someone is awaited (the two-person pending state).</summary>
    public bool IsPending => !IsSatisfied;

    /// <summary>The last refusal.</summary>
    public string? Error
    {
        get => _error;
        private set => SetField(ref _error, value);
    }

    /// <summary>Signs with one approver. The same person cannot sign twice.</summary>
    public async Task<bool> ApproveAsync(ISigner signer, CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(signer);
        Error = null;
        try
        {
            await Session.ApproveAsync(signer, cancellationToken);
        }
        catch (InvalidOperationException refused)
        {
            Error = refused.Message;
            return false;
        }

        OnPropertyChanged(nameof(Signers));
        OnPropertyChanged(nameof(StatusText));
        OnPropertyChanged(nameof(IsSatisfied));
        OnPropertyChanged(nameof(IsPending));
        return true;
    }
}

/// <summary>Where an operation is.</summary>
public enum OperationPhase
{
    /// <summary>Nothing requested.</summary>
    Idle,

    /// <summary>Asking the module for its plan.</summary>
    Previewing,

    /// <summary>The preview is on screen; signatures and acknowledgement are collected here.</summary>
    Review,

    /// <summary>The plan is empty: the desired state already holds.</summary>
    NothingToDo,

    /// <summary>Running, streaming progress.</summary>
    Running,

    /// <summary>Finished and verified.</summary>
    Succeeded,

    /// <summary>Finished badly, or the verify check failed.</summary>
    Failed,

    /// <summary>Applied with a revert timer; waiting for the person to confirm it still works.</summary>
    AwaitingConfirmation,

    /// <summary>The confirmation window passed; the server reverted the change.</summary>
    Reverted,
}

/// <summary>
/// Runs one operation the safe way: it asks the module for its plan, shows it, collects the
/// acknowledgement of any warning and the signatures the operation's risk needs, and only then
/// starts the run, streams progress and shows the result with the verify check. A change that
/// reverts itself is confirmed here, with a countdown driven by a clock the caller passes in.
/// </summary>
public sealed class OperationRunner : ObservableObject
{
    private readonly IServerApi _api;
    private readonly EquivalentCommand _commands;
    private readonly List<string> _progress = [];
    private OperationPhase _phase = OperationPhase.Idle;
    private PlanPreviewDialog? _preview;
    private SignaturesPanel? _signatures;
    private bool _acknowledged;
    private string? _error;
    private OperationResult? _result;
    private RevertTicket? _ticket;
    private long _remainingSeconds;

    /// <summary>Creates a runner for one action.</summary>
    public OperationRunner(IServerApi api, EquivalentCommand commands, AdminAction action)
    {
        _api = api ?? throw new ArgumentNullException(nameof(api));
        _commands = commands ?? throw new ArgumentNullException(nameof(commands));
        Action = action ?? throw new ArgumentNullException(nameof(action));
    }

    /// <summary>The action.</summary>
    public AdminAction Action { get; }

    /// <summary>The current phase.</summary>
    public OperationPhase Phase
    {
        get => _phase;
        private set
        {
            if (SetField(ref _phase, value))
            {
                OnPropertyChanged(nameof(CanRun));
            }
        }
    }

    /// <summary>The plan preview, once shown.</summary>
    public PlanPreviewDialog? Preview
    {
        get => _preview;
        private set => SetField(ref _preview, value);
    }

    /// <summary>The signatures panel, once the preview is shown.</summary>
    public SignaturesPanel? Signatures
    {
        get => _signatures;
        private set => SetField(ref _signatures, value);
    }

    /// <summary>The equivalent command line the person can copy.</summary>
    public string EquivalentCommandText => _commands.Cli(Action);

    /// <summary>The streamed progress lines.</summary>
    public IReadOnlyList<string> Progress => _progress;

    /// <summary>The result, once finished.</summary>
    public OperationResult? Result
    {
        get => _result;
        private set => SetField(ref _result, value);
    }

    /// <summary>The refusal or failure in plain words.</summary>
    public string? Error
    {
        get => _error;
        private set => SetField(ref _error, value);
    }

    /// <summary>Whether the person has read the preview's warnings.</summary>
    public bool WarningsAcknowledged
    {
        get => _acknowledged;
        private set
        {
            if (SetField(ref _acknowledged, value))
            {
                OnPropertyChanged(nameof(CanRun));
            }
        }
    }

    /// <summary>Seconds left to confirm a self-reverting change.</summary>
    public long RemainingSeconds
    {
        get => _remainingSeconds;
        private set => SetField(ref _remainingSeconds, value);
    }

    /// <summary>
    /// True only when the preview is on screen, every warning is acknowledged and the signatures
    /// the operation needs are in.
    /// </summary>
    public bool CanRun =>
        Phase == OperationPhase.Review
        && Preview is { } preview
        && (preview.Warnings.Count == 0 || WarningsAcknowledged)
        && Signatures is { IsSatisfied: true };

    /// <summary>Asks the module for its plan and shows it. Nothing runs yet.</summary>
    public async Task<bool> PrepareAsync(CancellationToken cancellationToken = default)
    {
        Phase = OperationPhase.Previewing;
        Error = null;
        try
        {
            var preview = await _api.PreviewAsync(Action, cancellationToken);
            var signing = new SigningSession(Action, _commands);
            Preview = new PlanPreviewDialog(preview);
            Signatures = new SignaturesPanel(signing, _commands.SignedCommandPreview(Action));
            Phase = preview.IsEmpty ? OperationPhase.NothingToDo : OperationPhase.Review;
            return !preview.IsEmpty;
        }
        catch (Exception failure) when (failure is ServerApiException or KeyNotFoundException)
        {
            Error = failure.Message;
            Phase = OperationPhase.Failed;
            return false;
        }
    }

    /// <summary>Records that the person read the preview's warnings (for example the lock-out warning).</summary>
    public void AcknowledgeWarnings()
    {
        if (Phase == OperationPhase.Review)
        {
            WarningsAcknowledged = true;
        }
    }

    /// <summary>Starts the run. Refused until <see cref="CanRun"/>.</summary>
    public async Task<bool> RunAsync(CancellationToken cancellationToken = default)
    {
        if (!CanRun || Signatures is null)
        {
            Error = Phase != OperationPhase.Review
                ? "Show the preview first."
                : Preview is { Warnings.Count: > 0 } && !WarningsAcknowledged
                    ? "Read and acknowledge the warnings first."
                    : "The signatures this change needs are not complete.";
            return false;
        }

        Error = null;
        Phase = OperationPhase.Running;
        try
        {
            await foreach (var update in _api.RunAsync(Action, Signatures.Session.Approvals, cancellationToken))
            {
                _progress.Add(update.Message);
                OnPropertyChanged(nameof(Progress));
                if (update.Final is { } final)
                {
                    Result = final;
                }
            }
        }
        catch (ServerApiException failure)
        {
            Error = failure.Message;
            Phase = OperationPhase.Failed;
            return false;
        }

        var result = Result;
        if (result is null || !result.Succeeded || result.Verify is { Ok: false })
        {
            Error = result?.Verify is { Ok: false } verify ? $"The change ran, but the check failed: {verify.Detail}" : result?.Detail ?? "The operation ended without a result.";
            Phase = OperationPhase.Failed;
            return false;
        }

        if (result.Revert is { } ticket)
        {
            _ticket = ticket;
            Phase = OperationPhase.AwaitingConfirmation;
            return true;
        }

        Phase = OperationPhase.Succeeded;
        return true;
    }

    /// <summary>Updates the countdown. When the deadline passes, the server has reverted the change.</summary>
    public void Tick(long nowUnixSeconds)
    {
        if (Phase != OperationPhase.AwaitingConfirmation || _ticket is not { } ticket)
        {
            return;
        }

        RemainingSeconds = Math.Max(0, ticket.DeadlineUnixSeconds - nowUnixSeconds);
        if (RemainingSeconds == 0)
        {
            Error = "You did not confirm in time, so the change was reverted.";
            Phase = OperationPhase.Reverted;
        }
    }

    /// <summary>Confirms a self-reverting change, if its window is still open.</summary>
    public async Task<bool> ConfirmAsync(long nowUnixSeconds, CancellationToken cancellationToken = default)
    {
        if (Phase != OperationPhase.AwaitingConfirmation || _ticket is not { } ticket)
        {
            return false;
        }

        Tick(nowUnixSeconds);
        if (Phase != OperationPhase.AwaitingConfirmation)
        {
            return false;
        }

        try
        {
            await _api.ConfirmAsync(ticket, cancellationToken);
        }
        catch (ServerApiException failure)
        {
            Error = failure.Message;
            return false;
        }

        Phase = OperationPhase.Succeeded;
        return true;
    }

    /// <summary>Drops the action before it runs.</summary>
    public void Cancel()
    {
        if (Phase is OperationPhase.Review or OperationPhase.NothingToDo or OperationPhase.Previewing)
        {
            Phase = OperationPhase.Idle;
            Preview = null;
            Signatures = null;
            WarningsAcknowledged = false;
        }
    }
}
