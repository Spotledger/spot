using System.Globalization;
using Spotledger.Client.Shell;

namespace Spotledger.Client.Assistant;

/// <summary>
/// The approval flow for one proposed fix. T1 (reversible, low risk) takes one click; T2
/// (data repair) lists the affected records and needs a typed confirmation; T3 never
/// applies from here, it routes to the approval task. Nothing runs before the plan has been
/// shown, and the approval is bound to the plan's hash.
/// </summary>
public sealed class FixFlow : ObservableObject
{
    private readonly IAssistantServer _server;
    private readonly Func<long> _nowUnixMs;
    private FixPhase _phase = FixPhase.Proposed;
    private string _typedConfirmation = string.Empty;
    private string _message = string.Empty;
    private long _undoDeadline;
    private bool _planShown;
    private string? _approvalTaskId;

    /// <summary>Creates the flow for <paramref name="plan"/>.</summary>
    public FixFlow(FixPlanView plan, IAssistantServer server, Func<long> nowUnixMs)
    {
        Plan = plan ?? throw new ArgumentNullException(nameof(plan));
        _server = server ?? throw new ArgumentNullException(nameof(server));
        _nowUnixMs = nowUnixMs ?? throw new ArgumentNullException(nameof(nowUnixMs));
    }

    /// <summary>The previewed plan.</summary>
    public FixPlanView Plan { get; }

    /// <summary>The phase.</summary>
    public FixPhase Phase { get => _phase; private set { if (SetField(ref _phase, value)) { NotifyState(); } } }

    /// <summary>The verification or error message after an apply.</summary>
    public string Message { get => _message; private set => SetField(ref _message, value); }

    /// <summary>The id of the approval task a T3 plan was routed to.</summary>
    public string? ApprovalTaskId { get => _approvalTaskId; private set => SetField(ref _approvalTaskId, value); }

    /// <summary>True once the view has displayed the plan.</summary>
    public bool PlanShown => _planShown;

    /// <summary>The phrase a T2 user must type; empty for other tiers.</summary>
    public string RequiredPhrase => Plan.Tier == FixTier.T2
        ? "repair " + Plan.Affected.Count.ToString(CultureInfo.InvariantCulture)
        : string.Empty;

    /// <summary>What the user typed for a T2 confirmation.</summary>
    public string TypedConfirmation
    {
        get => _typedConfirmation;
        set { if (SetField(ref _typedConfirmation, value)) { NotifyState(); } }
    }

    /// <summary>Whether the apply button is enabled.</summary>
    public bool CanApply => _planShown && Phase == FixPhase.Proposed && Plan.Tier switch
    {
        FixTier.T1 => Plan.Risk == FixRisk.Low && Plan.Reversible,
        FixTier.T2 => Plan.Affected.Count > 0 && string.Equals(_typedConfirmation.Trim(), RequiredPhrase, StringComparison.Ordinal),
        _ => false,
    };

    /// <summary>True for a T3 plan: it needs approval from someone else.</summary>
    public bool NeedsApproval => Plan.Tier == FixTier.T3 || (Plan.Tier == FixTier.T1 && (Plan.Risk != FixRisk.Low || !Plan.Reversible));

    /// <summary>Whether Undo is offered now.</summary>
    public bool CanUndo => Phase == FixPhase.Applied && Plan.Reversible && _nowUnixMs() <= _undoDeadline;

    /// <summary>Records that the plan is on screen. Apply is refused before this.</summary>
    public void MarkPlanShown()
    {
        _planShown = true;
        NotifyState();
    }

    /// <summary>Applies the plan if the tier's gate is satisfied.</summary>
    public async Task ApplyAsync(CancellationToken ct)
    {
        if (!CanApply)
        {
            throw new InvalidOperationException("The fix cannot be applied: the plan was not shown or the confirmation is missing.");
        }

        Phase = FixPhase.Applying;
        try
        {
            var result = await _server.ApplyAsync(
                Plan.FixId, Plan.Hash, Plan.Tier == FixTier.T2 ? _typedConfirmation.Trim() : null, ct);
            Message = result.Message;
            _undoDeadline = result.UndoDeadlineUnixMs;
            Phase = result.Verified ? FixPhase.Applied : FixPhase.Failed;
        }
        catch (Exception ex) when (ex is HttpRequestException or OperationCanceledException)
        {
            // Never show a raw exception text: it can carry a URL or a token fragment.
            Message = "The fix could not be applied. Nothing was confirmed as changed; check the record before trying again.";
            Phase = FixPhase.Failed;
        }
    }

    /// <summary>Routes a plan that needs approval to the approval task.</summary>
    public async Task RequestApprovalAsync(CancellationToken ct)
    {
        if (!_planShown || Phase != FixPhase.Proposed || !NeedsApproval)
        {
            throw new InvalidOperationException("This plan does not need approval or has not been shown.");
        }

        Phase = FixPhase.Applying;
        try
        {
            ApprovalTaskId = await _server.RequestApprovalAsync(Plan.FixId, Plan.Hash, ct);
            Phase = FixPhase.NeedsApproval;
        }
        catch (Exception ex) when (ex is HttpRequestException or OperationCanceledException)
        {
            Message = "The approval request could not be sent. Try again.";
            Phase = FixPhase.Proposed;
        }
    }

    /// <summary>Undoes an applied fix while the window is open.</summary>
    public async Task UndoAsync(CancellationToken ct)
    {
        if (Phase != FixPhase.Applied)
        {
            throw new InvalidOperationException("There is nothing to undo.");
        }

        if (!CanUndo)
        {
            Phase = FixPhase.UndoExpired;
            Message = "The undo window has closed.";
            return;
        }

        bool undone;
        try
        {
            undone = await _server.UndoAsync(Plan.FixId, ct);
        }
        catch (Exception ex) when (ex is HttpRequestException or OperationCanceledException)
        {
            undone = false;
        }

        Phase = undone ? FixPhase.Undone : FixPhase.Failed;
        Message = Phase == FixPhase.Undone ? "The change was undone." : "The change could not be undone.";
    }

    /// <summary>Dismisses the proposal without applying it.</summary>
    public void Cancel()
    {
        if (Phase == FixPhase.Proposed)
        {
            Phase = FixPhase.Cancelled;
        }
    }

    private void NotifyState()
    {
        OnPropertyChanged(nameof(CanApply));
        OnPropertyChanged(nameof(CanUndo));
        OnPropertyChanged(nameof(NeedsApproval));
    }
}
