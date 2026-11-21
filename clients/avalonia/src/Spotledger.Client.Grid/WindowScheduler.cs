namespace Spotledger.Client.Grid;

/// <summary>
/// The single-flight window broker between a <see cref="SpotGrid"/> and its
/// <see cref="IGridSource"/>. For a view of <c>V</c> rows it issues fetches of at most
/// <c>V + PrefetchMargin</c> rows, extending about half the margin behind the view and
/// the rest ahead; when a newer view supersedes an in-flight fetch it cancels that
/// fetch's token and drops any late completion of it. The scheduler holds one applied
/// window (the latest), which is exactly what the renderer draws from — no accumulation
/// cache, so memory stays bounded by view + margin.
/// </summary>
public sealed class WindowScheduler : IDisposable
{
    private readonly IGridSource _source;
    private readonly int _prefetchMargin;
    private readonly TaskScheduler _uiScheduler;
    private int _sequence;
    private int? _knownCount;
    private bool _countKnown;
    private int? _learnedCount;
    private (int Start, int Length, int Sequence)? _inFlight;
    private CancellationTokenSource? _inFlightCts;
    private GridWindowCompletedEventArgs? _latest;
    private (int ViewStart, int ViewLength)? _lastView;

    /// <summary>Creates the scheduler. <paramref name="prefetchMargin"/> is the total
    /// extra rows allowed beyond the visible view on every fetch.</summary>
    public WindowScheduler(IGridSource source, int prefetchMargin = 25)
    {
        _source = source ?? throw new ArgumentNullException(nameof(source));
        ArgumentOutOfRangeException.ThrowIfNegative(prefetchMargin);

        _prefetchMargin = prefetchMargin;
        _uiScheduler = SynchronizationContext.Current is SynchronizationContext context
            ? TaskScheduler.FromCurrentSynchronizationContext()
            : TaskScheduler.Default;
        _source.Changed += OnSourceChanged;
    }

    /// <summary>The total extra rows fetched beyond the visible view.</summary>
    public int PrefetchMargin => _prefetchMargin;

    /// <summary>True while a fetch is outstanding.</summary>
    public bool HasInFlightRequest => _inFlight is not null;

    /// <summary>Every fetch so far, for diagnostics; tests assert over deltas of this
    /// list, never absolute counts (earlier phases may have primed the log).</summary>
    public IReadOnlyList<GridWindowRequest> IssuedRequests => _issued;

    /// <summary>Every fetch this scheduler cancelled as superseded, for diagnostics.</summary>
    public IReadOnlyList<GridWindowRequest> CancelledRequests => _cancelled;

    private readonly List<GridWindowRequest> _issued = [];
    private readonly List<GridWindowRequest> _cancelled = [];

    /// <summary>The latest applied window, or null before the first completion.</summary>
    public GridWindowCompletedEventArgs? LatestCompleted => _latest;

    /// <summary>The row count the grid should lay out against: the source's stated count
    /// when it has one, otherwise what exhausted windows proved, otherwise 0.</summary>
    public int EffectiveTotalRows => _knownCount ?? _learnedCount ?? 0;

    /// <summary>True when the grid can state a total (stated or learned) for scroll bounds.</summary>
    public bool IsTotalKnown => _knownCount is not null || _learnedCount is not null;

    /// <summary>Raised whenever a fetch is issued (tests track ranges over this).</summary>
    public event EventHandler<GridWindowRequest>? RequestIssued;

    /// <summary>Raised whenever a fetch is cancelled as superseded.</summary>
    public event EventHandler<GridWindowRequest>? RequestCancelled;

    /// <summary>Raised when a fetch completes and is applied.</summary>
    public event EventHandler<GridWindowCompletedEventArgs>? WindowCompleted;

    /// <summary>Raised when a fetch fails with an error; triggers the grid's error row.</summary>
    public event EventHandler<GridWindowFailedEventArgs>? WindowFailed;

    /// <summary>Feeds the source's count statement to the scheduler (call when the grid
    /// attaches a source or the source's count may have changed).</summary>
    public void SetCount(GridRowCount count)
    {
        _countKnown = count.Kind != GridRowCountKind.Unknown;
        _knownCount = count.HasValue ? count.Value : null;
        if (!_countKnown)
        {
            // A count the source once knew but no longer states keeps its last learned value.
            return;
        }

        _learnedCount = null;
    }

    /// <summary>Requests that the scheduler have rows for the view
    /// <c>[viewStart, viewStart + viewLength)</c>. No-ops when the applied window already
    /// covers the view, or when the identical fetch is in flight; otherwise supersedes
    /// any in-flight fetch. This is the only entry point the grid uses while scrolling.</summary>
    public void RequestView(int viewStart, int viewLength)
    {
        if (viewLength <= 0)
        {
            return;
        }

        viewStart = Math.Max(0, viewStart);
        _lastView = (viewStart, viewLength);

        var (start, length) = ComputeBounds(viewStart, viewLength);
        if (length <= 0)
        {
            return;
        }

        if (_latest is { } applied && Covered(applied, viewStart, viewLength))
        {
            // Covered. Any in-flight fetch that does not start at these bounds is now
            // stale prefetch for an abandoned position; cancel it (this is the
            // "cancels superseded requests" path even when nothing new is issued).
            if (_inFlight is { } f && (f.Start != start || f.Length != length))
            {
                CancelInFlight($"superseded by covered view [{viewStart}, {viewStart + viewLength})");
            }

            return;
        }

        if (_inFlight is { } current && current.Start == start && current.Length == length)
        {
            return;
        }

        Issue(start, length);
    }

    /// <summary>Drops the applied window and re-requests the last view. Call after a
    /// source reset, or to retry after a failure. Supersedes the in-flight fetch.</summary>
    public void Invalidate()
    {
        _latest = null;
        CancelInFlight("invalidate");
        if (_lastView is { } view)
        {
            RequestView(view.ViewStart, view.ViewLength);
        }
    }

    /// <summary>Drops the applied window without re-requesting (the grid follows with a
    /// fresh RequestView once layout knows the new geometry).</summary>
    public void DiscardWindow()
    {
        _latest = null;
        CancelInFlight("discard");
        _lastView = null;
    }

    /// <summary>The window rows for a view, sliced out of the applied window, or null
    /// when the applied window does not cover the view (rows will then draw as
    /// shimmer placeholders until the in-flight fetch lands).</summary>
    public IReadOnlyList<GridRow>? TryGetRows(int viewStart, int viewLength)
        => _latest is { } applied && Covered(applied, viewStart, viewLength) ? applied.Rows : null;

    /// <summary>True when the applied window holds every row of the view that exists. A view
    /// taller than the data (a short grid in a tall viewport) is clamped to the known total, or
    /// to the end a short window proved: rows past the end do not exist, so waiting for them
    /// would paint every real row as a loading placeholder forever.</summary>
    private bool Covered(GridWindowCompletedEventArgs applied, int viewStart, int viewLength)
    {
        if (applied.Covers(viewStart, viewLength))
        {
            return true;
        }

        var total = IsTotalKnown ? EffectiveTotalRows : (int?)null;
        if (total is null)
        {
            return false;
        }

        var clamped = Math.Max(0, Math.Min(viewLength, total.Value - viewStart));
        return applied.Covers(viewStart, clamped);
    }

    /// <summary>The fetched row at <paramref name="sourceIndex"/> from the applied
    /// window, or null.</summary>
    public GridRow? TryGetRow(int sourceIndex) => _latest?.RowAt(sourceIndex);

    private (int Start, int Length) ComputeBounds(int viewStart, int viewLength)
    {
        var behind = _prefetchMargin / 2;
        var start = Math.Max(0, viewStart - behind);
        var rawEnd = viewStart + viewLength + _prefetchMargin - behind;
        var total = EffectiveTotalRows;
        var end = total > 0 ? Math.Min(rawEnd, total) : rawEnd;
        var length = end - start;

        // Hard contract: never more than view + margin.
        return length > viewLength + _prefetchMargin
            ? (start, viewLength + _prefetchMargin)
            : (start, length);
    }

    /// <summary>Registers one fetch (superseding any in-flight fetch, cancelling its
    /// token) and records the request. The caller starts the fetch loop AFTER any
    /// probe bookkeeping — a source that completes synchronously must see the probe
    /// sequence already marked, or the probe is mistaken for a regular fetch.</summary>
    private (GridWindowRequest Request, CancellationTokenSource Cts) TrackIssue(int start, int length)
    {
        CancelInFlight("superseded by new request");
        var cts = new CancellationTokenSource();
        _sequence++;
        var request = new GridWindowRequest(_sequence, start, length);
        _inFlight = (start, length, _sequence);
        _inFlightCts = cts;
        _issued.Add(request);
        RequestIssued?.Invoke(this, request);
        return (request, cts);
    }

    private void Issue(int start, int length)
    {
        var (request, cts) = TrackIssue(start, length);
        _ = FetchAsync(request, cts);
    }

    private async Task FetchAsync(GridWindowRequest request, CancellationTokenSource cts)
    {
        IReadOnlyList<GridRow> rows;
        try
        {
            var task = _source.GetWindow(request.Start, request.Length, cts.Token);
            if (task.IsCompletedSuccessfully)
            {
                rows = task.Result;
            }
            else
            {
                // A source that signals cancellation without faulting its returned task
                // still drops the fetch: race the window against the token.
                var winner = await Task.WhenAny(task, Task.Delay(Timeout.InfiniteTimeSpan, cts.Token))
                    .ConfigureAwait(true);
                if (winner != task)
                {
                    OnCancelled(request);
                    return;
                }

                rows = await task.ConfigureAwait(true);
            }
        }
        catch (OperationCanceledException)
        {
            // Honouring the token: the scheduler dropped this fetch, nothing to apply.
            OnCancelled(request);
            return;
        }
        catch (Exception error)
        {
            if (request.Sequence == _inFlight?.Sequence)
            {
                _inFlight = null;
                _inFlightCts = null;
            }

            WindowFailed?.Invoke(this, new GridWindowFailedEventArgs(request, error));
            return;
        }

        // A late completion for a superseded sequence is dropped, never applied.
        var isProbe = request.Sequence == _probeSequence;
        if (_inFlight is not { } flight || flight.Sequence != request.Sequence)
        {
            OnCancelled(request);
            return;
        }

        _inFlight = null;
        _inFlightCts = null;
        if (!isProbe)
        {
            _latest = new GridWindowCompletedEventArgs(request.Sequence, request.Start, rows);
            WindowCompleted?.Invoke(this, _latest);
        }

        // Unknown-count learning: an exhausted window proves the end; a probe past the
        // furthest known row confirms nothing more exists (bounded so a drifting source
        // cannot spin).
        if (!_countKnown)
        {
            var end = request.Start + rows.Count;
            if (rows.Count < request.Length && (_learnedCount is null || end > _learnedCount))
            {
                _learnedCount = end;
            }

            _probeSequence = -1;
            var keepProbing = !isProbe || rows.Count > 0;
            if (keepProbing && _probeDepth < MaxEndProbes)
            {
                if (isProbe)
                {
                    _probeDepth++;
                }

                IssueProbe(Math.Max(_learnedCount ?? 0, end));
            }
        }
        else if (rows.Count < request.Length && _knownCount is not null && request.Start + rows.Count < _knownCount)
        {
            // A stated count a window outran (the set shrank underneath the statement):
            // trust the exhausted window.
            _learnedCount = request.Start + rows.Count;
        }
    }

    private int _probeSequence = -1;
    private int _probeDepth;
    private const int MaxEndProbes = 16;
    private const int ProbeLength = 2;

    /// <summary>Issues one end-probe fetch (a small window at the furthest known row):
    /// rows coming back prove more data exists (probe again), an empty probe confirms
    /// the learned end.</summary>
    private void IssueProbe(int start)
    {
        var (request, cts) = TrackIssue(start, ProbeLength);
        _probeSequence = request.Sequence;
        _ = FetchAsync(request, cts);
    }

    /// <summary>Side query used by type-ahead: fetches a single row straight from the
    /// source without disturbing the applied window or the single-flight state.</summary>
    internal async Task<GridRow?> PeekRowAsync(int index, CancellationToken cancellationToken)
    {
        if (index < 0)
        {
            return null;
        }

        if (TryGetRow(index) is { } applied)
        {
            return applied;
        }

        try
        {
            var rows = await _source.GetWindow(index, 1, cancellationToken).ConfigureAwait(true);
            return rows.Count > 0 ? rows[0] : null;
        }
        catch (Exception)
        {
            // A failed peek just means "unknown row" for the type-ahead heuristic.
            return null;
        }
    }

    private void OnSourceChanged(object? sender, GridSourceChangedEventArgs e)
    {
        if (e.Kind == GridChangeKind.RowsInserted || e.Kind == GridChangeKind.RowsRemoved)
        {
            if (e.Count <= 0)
            {
                return;
            }
        }

        // Any change invalidates every cached identity; the applied window is stale.
        _latest = null;
        CancelInFlight("source changed");
        _inFlight = null;
        _inFlightCts = null;
        _probeSequence = -1;
        _probeDepth = 0;
    }

    private void CancelInFlight(string reason)
    {
        if (_inFlight is not { } flight)
        {
            return;
        }

        _inFlightCts?.Cancel();
        _inFlightCts?.Dispose();
        _inFlightCts = null;
        _inFlight = null;
        var cancelled = new GridWindowRequest(flight.Sequence, flight.Start, flight.Length);
        _cancelled.Add(cancelled);
        RequestCancelled?.Invoke(this, cancelled);
    }

    private void OnCancelled(GridWindowRequest request)
    {
        if (!_cancelled.Any(c => c.Sequence == request.Sequence))
        {
            _cancelled.Add(request);
            RequestCancelled?.Invoke(this, request);
        }
    }

    /// <inheritdoc />
    public void Dispose()
    {
        _source.Changed -= OnSourceChanged;
        CancelInFlight("disposed");
    }
}