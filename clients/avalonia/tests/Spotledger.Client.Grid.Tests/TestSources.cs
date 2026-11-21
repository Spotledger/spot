using Spotledger.Client.Grid;

namespace Spotledger.Client.Grid.Tests;

/// <summary>
/// A grid source whose windows never complete until released — the cancellation and
/// superseding tests drive it. Windows complete synchronously via <see cref="CompleteAll"/>
/// or selectively via <see cref="CompleteOne"/>, and the source token is honoured: when it
/// cancels, the pending window faults with OperationCanceledException (the token the
/// scheduler handed to GetWindow is linked, so cancelling the test handle cancels the
/// whole chain).
/// </summary>
public sealed class DeferredGridSource : IGridSource
{
    private readonly int _rows;
    private readonly List<DeferredWindow> _inFlight = [];

    /// <summary>Creates the source with <paramref name="rows"/> synthetic rows and an
    /// optional count statement kind.</summary>
    public DeferredGridSource(int rows, GridRowCountKind kind = GridRowCountKind.Known)
    {
        _rows = rows;
        Count = kind == GridRowCountKind.Unknown
            ? GridRowCount.Unknown()
            : new GridRowCount(kind, rows);
    }

    /// <inheritdoc />
    public GridRowCount Count { get; }

    /// <inheritdoc />
    public event EventHandler<GridSourceChangedEventArgs>? Changed;

    /// <summary>Windows whose async completion is still pending.</summary>
    public IReadOnlyList<DeferredWindow> Pending => _inFlight;

    /// <inheritdoc />
    public Task<IReadOnlyList<GridRow>> GetWindow(int start, int length, CancellationToken cancellationToken)
    {
        var deferred = new TaskCompletionSource<IReadOnlyList<GridRow>>(TaskCreationOptions.RunContinuationsAsynchronously);
        var window = new DeferredWindow(start, length, deferred, cancellationToken);
        _inFlight.Add(window);
        return deferred.Task;
    }

    /// <inheritdoc />
    public Task Sort(int columnIndex, GridSortDirection direction)
    {
        Changed?.Invoke(this, new GridSourceChangedEventArgs(GridChangeKind.Reset));
        return Task.CompletedTask;
    }

    /// <summary>Releases one pending window: its rows deliver immediately, and every
    /// other still-pending window delivers too (a release is a snapshot of the source;
    /// the scheduler keeps only windows still in flight — stale ones arrive and are
    /// dropped, the current one is applied).</summary>
    public void CompleteOne(DeferredWindow window)
    {
        if (!_inFlight.Remove(window))
        {
            return;
        }

        window.Complete(_rows);
        foreach (var pending in _inFlight.ToArray())
        {
            _inFlight.Remove(pending);
            pending.Complete(_rows);
        }
    }
}

/// <summary>One outstanding window of <see cref="DeferredGridSource"/>.</summary>
public sealed class DeferredWindow
{
    private readonly TaskCompletionSource<IReadOnlyList<GridRow>> _completion;
    private readonly CancellationTokenRegistration _registration;

    internal DeferredWindow(int start, int length, TaskCompletionSource<IReadOnlyList<GridRow>> completion, CancellationToken token)
    {
        Start = start;
        Length = length;
        CancellationTokenSource = CancellationTokenSource.CreateLinkedTokenSource(token);
        _completion = completion;
        // Honouring the token in both directions: a cancel of the linked handle (the
        // test's, or the scheduler's via the linked parent) faults the pending window
        // with OperationCanceledException so the scheduler sees a cancelled fetch and
        // never applies its late completion.
        _registration = CancellationTokenSource.Token.Register(
            static state => ((DeferredWindow)state!).FaultFromSource(),
            this);
    }

    private void FaultFromSource()
    {
        if (IsCancelled)
        {
            _completion.TrySetCanceled(CancellationTokenSource.Token);
        }
    }

    /// <summary>The window start.</summary>
    public int Start { get; }

    /// <summary>The requested length.</summary>
    public int Length { get; }

    /// <summary>The linked token source (the test can cancel independently).</summary>
    public CancellationTokenSource CancellationTokenSource { get; }

    /// <summary>True when the source token or the test cancelled this window.</summary>
    public bool IsCancelled => CancellationTokenSource.IsCancellationRequested;

    /// <summary>Delivers the synthetic rows for this window (row shape shared with
    /// <see cref="GridFixtures.MakeRowsRange"/>).</summary>
    public void Complete(int totalRows)
    {
        if (IsCancelled)
        {
            _completion.TrySetCanceled(CancellationTokenSource.Token);
            return;
        }

        var take = Math.Min(Length, Math.Max(0, totalRows - Start));
        _completion.TrySetResult(GridFixtures.MakeRowsRange(Start, take));
    }
}

/// <summary>A recording clipboard sink for the TSV copy tests.</summary>
public sealed class RecordingClipboardSink : IClipboardSink
{
    /// <inheritdoc />
    public Task SetTextAsync(string text, CancellationToken cancellationToken = default)
    {
        LastText = text;
        Copies++;
        return Task.CompletedTask;
    }

    /// <inheritdoc />
    public string? LastText { get; private set; }

    /// <summary>How many copy requests landed.</summary>
    public int Copies { get; private set; }
}