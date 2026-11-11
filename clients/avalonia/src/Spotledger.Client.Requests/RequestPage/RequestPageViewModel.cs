using System.Collections.ObjectModel;
using System.Net.Http;
using Spotledger.Client.Api;
using Spotledger.Client.Shell;

namespace Spotledger.Client.Requests;

/// <summary>A message line of the conversation panel.</summary>
/// <param name="Message">The server's message.</param>
/// <param name="DeliveryText">"Delivered", "Could not be sent" and so on; null for an incoming message.</param>
public sealed record MessageLine(ConversationMessage Message, string? DeliveryText);

/// <summary>A conversation as the panel shows it.</summary>
/// <param name="Peer">The peer.</param>
/// <param name="Last">The last message, or null.</param>
/// <param name="Lines">Every message.</param>
/// <param name="Condition">The stuck or unreachable sentence in plain language, or null.</param>
public sealed record ConversationPanel(string Peer, MessageLine? Last, IReadOnlyList<MessageLine> Lines, string? Condition);

/// <summary>One action button (Undo, Cancel): enabled, or disabled with the reason and the blockers.</summary>
public sealed class RunAction(string label, ActionState state)
{
    /// <summary>The button text.</summary>
    public string Label { get; } = label;

    /// <summary>True when the server allows the action.</summary>
    public bool IsEnabled => state.Allowed;

    /// <summary>Why the action is disabled (a tooltip), or null.</summary>
    public string? DisabledReason => state.Allowed ? null : state.Reason;

    /// <summary>What stands in the way, shown under the button; empty when allowed.</summary>
    public IReadOnlyList<string> Blockers => state.Allowed ? [] : state.Blockers;
}

/// <summary>
/// The request page: the timeline of steps, the records the run created, its tasks and comments, a
/// panel per conversation (last message, delivery state of every message, stuck or unreachable
/// conditions in plain words) and the Undo and Cancel actions. Undo and Cancel are disabled with the
/// server's reason and blocker list when the server says they are blocked; the client never decides.
/// </summary>
public sealed class RequestPageViewModel : ObservableObject
{
    private readonly IRequestsGateway _gateway;
    private readonly string _requestId;
    private RequestDetail? _detail;
    private string? _error;
    private bool _busy;

    /// <summary>Creates the page for <paramref name="requestId"/>.</summary>
    public RequestPageViewModel(IRequestsGateway gateway, string requestId, RequestText text)
    {
        _gateway = gateway ?? throw new ArgumentNullException(nameof(gateway));
        ArgumentException.ThrowIfNullOrWhiteSpace(requestId);
        _requestId = requestId;
        Text = text ?? throw new ArgumentNullException(nameof(text));
    }

    /// <summary>The texts.</summary>
    public RequestText Text { get; }

    /// <summary>The loaded detail, or null.</summary>
    public RequestDetail? Detail => _detail;

    /// <summary>"Request 12" with the run label.</summary>
    public string Title => _detail is null ? Text.Label.Singular : Text.Get("page.title", ("number", _detail.Summary.Number));

    /// <summary>The state in words.</summary>
    public string StateText => _detail is null ? string.Empty : Text.Get(_detail.Summary.State switch
    {
        RequestState.Finished => "state.finished",
        RequestState.Cancelled => "state.cancelled",
        RequestState.Undone => "state.undone",
        _ => "state.running",
    });

    /// <summary>The timeline steps, each with its state in words.</summary>
    public ObservableCollection<(TimelineStep Step, string StateText)> Timeline { get; } = [];

    /// <summary>The conversation panels.</summary>
    public ObservableCollection<ConversationPanel> Conversations { get; } = [];

    /// <summary>The Undo button.</summary>
    public RunAction? Undo { get; private set; }

    /// <summary>The Cancel button.</summary>
    public RunAction? Cancel { get; private set; }

    /// <summary>A plain sentence after a failure, or null.</summary>
    public string? Error
    {
        get => _error;
        private set => SetField(ref _error, value);
    }

    /// <summary>True while a call is in flight.</summary>
    public bool IsBusy
    {
        get => _busy;
        private set => SetField(ref _busy, value);
    }

    /// <summary>Loads (or reloads) the page.</summary>
    public async Task LoadAsync(CancellationToken cancellationToken = default)
    {
        try
        {
            _detail = await _gateway.GetRequestAsync(_requestId, cancellationToken).ConfigureAwait(true);
        }
        catch (Exception e) when (e is ApiRequestException or HttpRequestException or OperationCanceledException)
        {
            Error = Text.Get("error.generic");
            return;
        }

        Error = null;
        Timeline.Clear();
        foreach (var step in _detail.Timeline)
        {
            Timeline.Add((step, Text.Get(step.State switch
            {
                TimelineState.Done => "step.state.done",
                TimelineState.Current => "step.state.current",
                TimelineState.Skipped => "step.state.skipped",
                _ => "step.state.upcoming",
            })));
        }

        Conversations.Clear();
        foreach (var conversation in _detail.Conversations)
        {
            Conversations.Add(ToPanel(conversation));
        }

        Undo = new RunAction(Text.Get("page.undo"), _detail.Undo);
        Cancel = new RunAction(Text.Get("page.cancel"), _detail.Cancel);
        OnPropertyChanged(string.Empty);
    }

    /// <summary>Undoes the run when the server allows it, then reloads. Returns whether it ran.</summary>
    public Task<bool> UndoAsync(CancellationToken cancellationToken = default) =>
        RunAsync(Undo, () => _gateway.UndoAsync(_requestId, cancellationToken), cancellationToken);

    /// <summary>Cancels the run when the server allows it, then reloads. Returns whether it ran.</summary>
    public Task<bool> CancelAsync(string? reason, CancellationToken cancellationToken = default) =>
        RunAsync(Cancel, () => _gateway.CancelAsync(_requestId, reason, cancellationToken), cancellationToken);

    private async Task<bool> RunAsync(RunAction? action, Func<Task> call, CancellationToken cancellationToken)
    {
        if (action is not { IsEnabled: true } || IsBusy)
        {
            return false;
        }

        IsBusy = true;
        try
        {
            await call().ConfigureAwait(true);
        }
        catch (Exception e) when (e is ApiRequestException or HttpRequestException or OperationCanceledException)
        {
            // A plain sentence only: the exception text can carry a URL or an identifier.
            Error = Text.Get("error.generic");
            return false;
        }
        finally
        {
            IsBusy = false;
        }

        await LoadAsync(cancellationToken).ConfigureAwait(true);
        return true;
    }

    private ConversationPanel ToPanel(ConversationInfo conversation)
    {
        var lines = conversation.Messages
            .Select(m => new MessageLine(m, m.Outgoing ? Text.Get(DeliveryKey(m.Delivery)) : null))
            .ToArray();
        string? condition = conversation.Condition switch
        {
            ConversationCondition.Stuck => Text.Get(
                "condition.stuck",
                ("peer", conversation.Peer),
                ("since", conversation.Since?.ToString("u", System.Globalization.CultureInfo.InvariantCulture) ?? string.Empty)),
            ConversationCondition.Unreachable => Text.Get("condition.unreachable", ("peer", conversation.Peer)),
            _ => null,
        };
        return new ConversationPanel(conversation.Peer, lines.Length == 0 ? null : lines[^1], lines, condition);
    }

    private static string DeliveryKey(DeliveryState state) => state switch
    {
        DeliveryState.Sent => "delivery.sent",
        DeliveryState.Delivered => "delivery.delivered",
        DeliveryState.Read => "delivery.read",
        DeliveryState.Failed => "delivery.failed",
        _ => "delivery.queued",
    };
}
