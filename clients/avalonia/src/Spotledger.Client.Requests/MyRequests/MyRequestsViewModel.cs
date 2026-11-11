using System.Collections.ObjectModel;
using System.Net.Http;
using Spotledger.Client.Api;
using Spotledger.Client.Shell;

namespace Spotledger.Client.Requests;

/// <summary>How urgent a row is.</summary>
public enum Urgency
{
    /// <summary>No due time, or plenty left.</summary>
    None,

    /// <summary>Due within the soon window.</summary>
    DueSoon,

    /// <summary>Past its due time.</summary>
    Overdue,
}

/// <summary>One row of My requests, ready to show.</summary>
/// <param name="Summary">The server's row.</param>
/// <param name="Reference">"Request 12" with the run label, never a literal.</param>
/// <param name="Waiting">The waiting line, or null.</param>
/// <param name="Urgency">The SLA badge.</param>
/// <param name="Badge">The badge text, or null when there is no badge.</param>
public sealed record RequestRow(RequestSummary Summary, string Reference, string? Waiting, Urgency Urgency, string? Badge);

/// <summary>
/// My requests: the signed-in person's task inbox and the runs they started, in three views
/// (waiting for me, waiting for others, finished), each row labelled with the run label and badged
/// when it is overdue or due soon. Time is an argument to <see cref="RefreshAsync"/>; the model
/// never reads a clock.
/// </summary>
public sealed class MyRequestsViewModel : ObservableObject
{
    /// <summary>How long before the due time a row counts as "due soon".</summary>
    public static readonly TimeSpan DueSoonWindow = TimeSpan.FromHours(4);

    private readonly IRequestsGateway _gateway;
    private RequestFilter _filter = RequestFilter.WaitingForMe;
    private string? _error;
    private bool _loading;
    private RequestRow? _selected;
    private int _generation;

    /// <summary>Creates the view model.</summary>
    public MyRequestsViewModel(IRequestsGateway gateway, RequestText text)
    {
        _gateway = gateway ?? throw new ArgumentNullException(nameof(gateway));
        Text = text ?? throw new ArgumentNullException(nameof(text));
    }

    /// <summary>The texts.</summary>
    public RequestText Text { get; }

    /// <summary>The page title, with the plural run label.</summary>
    public string Title => Text.Get("mine.title");

    /// <summary>The empty-list line.</summary>
    public string EmptyText => Text.Get("mine.empty");

    /// <summary>The rows of the current view.</summary>
    public ObservableCollection<RequestRow> Rows { get; } = [];

    /// <summary>The current view.</summary>
    public RequestFilter Filter => _filter;

    /// <summary>The row the person selected.</summary>
    public RequestRow? Selected
    {
        get => _selected;
        set => SetField(ref _selected, value);
    }

    /// <summary>A plain sentence when loading failed, else null.</summary>
    public string? Error
    {
        get => _error;
        private set => SetField(ref _error, value);
    }

    /// <summary>True while loading.</summary>
    public bool IsLoading
    {
        get => _loading;
        private set => SetField(ref _loading, value);
    }

    /// <summary>The label of one view's button.</summary>
    public string FilterLabel(RequestFilter filter) => Text.Get(filter switch
    {
        RequestFilter.WaitingForMe => "mine.filter.waiting_for_me",
        RequestFilter.WaitingForOthers => "mine.filter.waiting_for_others",
        _ => "mine.filter.finished",
    });

    /// <summary>Switches the view and reloads it.</summary>
    public async Task SelectFilterAsync(RequestFilter filter, DateTimeOffset now, CancellationToken cancellationToken = default)
    {
        _filter = filter;
        OnPropertyChanged(nameof(Filter));
        await RefreshAsync(now, cancellationToken).ConfigureAwait(true);
    }

    /// <summary>Reloads the current view; <paramref name="now"/> decides the overdue and due-soon badges.</summary>
    public async Task RefreshAsync(DateTimeOffset now, CancellationToken cancellationToken = default)
    {
        IsLoading = true;
        var generation = ++_generation;
        var filter = _filter;
        try
        {
            var runs = await _gateway.ListMineAsync(filter, cancellationToken).ConfigureAwait(true);
            if (generation != _generation)
            {
                // A newer refresh (another view was chosen) owns the list; never show rows of the wrong view.
                return;
            }

            Rows.Clear();
            foreach (var run in runs.OrderBy(r => r.DueAt ?? DateTimeOffset.MaxValue).ThenBy(r => r.StartedAt))
            {
                Rows.Add(ToRow(run, now));
            }

            Error = null;
        }
        catch (Exception e) when (e is ApiRequestException or HttpRequestException or OperationCanceledException)
        {
            if (generation == _generation)
            {
                Rows.Clear();
                Error = Text.Get("error.generic");
            }
        }
        finally
        {
            if (generation == _generation)
            {
                IsLoading = false;
            }
        }

        OnPropertyChanged(nameof(IsEmpty));
    }

    /// <summary>True when the view has no rows and nothing went wrong.</summary>
    public bool IsEmpty => Rows.Count == 0 && Error is null && !IsLoading;

    internal RequestRow ToRow(RequestSummary run, DateTimeOffset now)
    {
        var urgency = Urgency.None;
        string? badge = null;
        if (run.State == RequestState.Running && run.DueAt is { } due)
        {
            if (due < now)
            {
                urgency = Urgency.Overdue;
                badge = Text.Get("mine.overdue");
            }
            else if (due - now <= DueSoonWindow)
            {
                urgency = Urgency.DueSoon;
                badge = Text.Get("mine.due_soon");
            }
        }

        var waiting = !run.WaitingForMe && run.WaitingFor is { Length: > 0 } person
            ? Text.Get("mine.waiting_for", ("person", person))
            : null;
        return new RequestRow(run, Text.Get("mine.reference", ("number", run.Number)), waiting, urgency, badge);
    }
}
