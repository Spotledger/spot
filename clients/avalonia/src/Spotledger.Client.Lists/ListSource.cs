using Spotledger.Client.Grid;

namespace Spotledger.Client.Lists;

/// <summary>How a list states its row count.</summary>
public enum CountPolicy
{
    /// <summary>Never ask for a total: "has more" until the last page (the default; cheap on any table).</summary>
    HasMoreOnly,

    /// <summary>Ask for the exact total with the first page of every query.</summary>
    Exact,
}

/// <summary>Tuning of a <see cref="ListSource"/>.</summary>
public sealed record ListSourceOptions
{
    /// <summary>Rows per query page (1 to 500).</summary>
    public int PageSize { get; init; } = 100;

    /// <summary>Pages kept in memory.</summary>
    public int MaxCachedPages { get; init; } = 12;

    /// <summary>How the count is stated.</summary>
    public CountPolicy Count { get; init; } = CountPolicy.HasMoreOnly;
}

/// <summary>
/// The grid's source over the resource query operation. Paging is by the server's opaque cursor
/// only (the contract has no offset), so a window far down walks the cursor chain page by page and
/// keeps what it walked. There is at most one query in flight per list: windows queue behind it and
/// a window superseded by a query change is cancelled. A response that belongs to an older query is
/// dropped, never shown. Sort and filter are pushed to the server as structured data; the client
/// never sorts or filters rows itself.
/// </summary>
public sealed partial class ListSource : IGridSource, IDisposable
{
    private readonly IListGateway _gateway;
    private readonly string _doctype;
    private readonly ListSourceOptions _options;
    private readonly SynchronizationContext? _context = SynchronizationContext.Current;
    private readonly SemaphoreSlim _gate = new(1, 1);
    private readonly Dictionary<int, PageData> _pages = [];
    private readonly List<int> _recent = [];
    private readonly Dictionary<string, int> _versions = new(StringComparer.Ordinal);
    private readonly Dictionary<(string Target, string Id), string> _labels = [];
    private readonly List<string?> _cursors = [null];
    private CancellationTokenSource _generationCts = new();
    private int _generation;
    private int _lastPage = -1;
    private int _lastPageRows;
    private int? _total;
    private GridRowCount _announced;
    private bool _disposed;

    private sealed record PageData(IReadOnlyList<GridRow> Rows);

    /// <summary>Creates the source.</summary>
    public ListSource(IListGateway gateway, string doctype, ListColumnSet columns, ListFilter? filter = null,
        IReadOnlyList<ListOrder>? order = null, ListSourceOptions? options = null)
    {
        _gateway = gateway ?? throw new ArgumentNullException(nameof(gateway));
        _doctype = ListLimits.IsIdentifier(doctype) ? doctype : throw new ArgumentException("Not a DocType name.", nameof(doctype));
        Columns = columns ?? throw new ArgumentNullException(nameof(columns));
        Filter = filter;
        Order = order ?? [];
        _options = options ?? new ListSourceOptions();
        if (_options.PageSize is < 1 or > ListLimits.MaxPageSize)
        {
            throw new ArgumentOutOfRangeException(nameof(options), "Page size must be 1 to 500.");
        }

        _announced = ComputeCount();
    }

    /// <summary>The columns the rows are built for (index-aligned with the grid's column model).</summary>
    public ListColumnSet Columns { get; private set; }

    /// <summary>The current filter.</summary>
    public ListFilter? Filter { get; private set; }

    /// <summary>The current order.</summary>
    public IReadOnlyList<ListOrder> Order { get; private set; }

    /// <summary>The rows per page.</summary>
    public int PageSize => _options.PageSize;

    /// <summary>Queries sent so far (tests and diagnostics).</summary>
    public int QueriesIssued { get; private set; }

    /// <summary>True while the server has more rows than the pages fetched so far.</summary>
    public bool HasMore => _lastPage < 0;

    /// <summary>The exact total when it is known (asked for, or the last page was reached).</summary>
    public int? ExactTotal => _total ?? (_lastPage >= 0 ? _lastPage * PageSize + _lastPageRows : null);

    /// <summary>Rows fetched at least once so far in the cursor chain.</summary>
    public int FetchedRows => _lastPage >= 0 ? _lastPage * PageSize + _lastPageRows : (_cursors.Count - 1) * PageSize;

    /// <inheritdoc />
    public GridRowCount Count => _announced;

    /// <inheritdoc />
    public event EventHandler<GridSourceChangedEventArgs>? Changed;

    /// <summary>Raised (on the caller's context) when the count statement changed after a page loaded.</summary>
    public event EventHandler? CountChanged;

    /// <summary>Raised when a query failed (not when it was cancelled or superseded).</summary>
    public event EventHandler<Exception>? QueryFailed;

    /// <summary>Raised when a page of the current query loaded.</summary>
    public event EventHandler? PageLoaded;

    /// <summary>The version of a loaded document (a delete needs it), or null.</summary>
    public int? VersionOf(string id) => _versions.TryGetValue(id, out var version) ? version : null;

    /// <summary>Replaces the filter, order and (optionally) the columns, and starts over.</summary>
    public void Apply(ListFilter? filter, IReadOnlyList<ListOrder>? order, ListColumnSet? columns = null)
    {
        if (!ListLimits.Fits(filter))
        {
            throw new ArgumentException("The filter is larger than the server accepts.", nameof(filter));
        }

        Filter = filter;
        Order = order ?? [];
        if (columns is not null)
        {
            Columns = columns;
        }

        Reset();
    }

    /// <summary>Runs the current query again from the first page.</summary>
    public void Refresh() => Reset();

    /// <inheritdoc />
    public Task Sort(int columnIndex, GridSortDirection direction)
    {
        if (direction == GridSortDirection.None || columnIndex < 0 || columnIndex >= Columns.Columns.Count)
        {
            Order = [];
        }
        else if (Columns.Columns[columnIndex].Masked)
        {
            // The order of masked values would reveal them: a masked column is never sorted on.
            Order = [];
        }
        else
        {
            Order = [new ListOrder(Columns.Columns[columnIndex].Field.Name, direction == GridSortDirection.Descending)];
        }

        Reset();
        return Task.CompletedTask;
    }

    private void Reset()
    {
        _generation++;
        var old = _generationCts;
        _generationCts = new CancellationTokenSource();
        old.Cancel();
        old.Dispose();
        _pages.Clear();
        _recent.Clear();
        _versions.Clear();
        _cursors.Clear();
        _cursors.Add(null);
        _lastPage = -1;
        _lastPageRows = 0;
        _total = null;
        _announced = ComputeCount();
        Changed?.Invoke(this, new GridSourceChangedEventArgs(GridChangeKind.Reset));
    }

    /// <inheritdoc />
    public async Task<IReadOnlyList<GridRow>> GetWindow(int start, int length, CancellationToken cancellationToken)
    {
        ArgumentOutOfRangeException.ThrowIfNegative(start);
        ArgumentOutOfRangeException.ThrowIfNegative(length);
        if (length == 0)
        {
            return [];
        }

        var generation = _generation;
        await _gate.WaitAsync(cancellationToken).ConfigureAwait(true);
        try
        {
            Superseded(generation, cancellationToken);
            var rows = new List<GridRow>(length);
            var first = start / PageSize;
            var last = (start + length - 1) / PageSize;
            for (var p = first; p <= last; p++)
            {
                var page = await EnsurePageAsync(p, generation, cancellationToken).ConfigureAwait(true);
                if (page is null)
                {
                    break;
                }

                var from = p == first ? start - p * PageSize : 0;
                var take = Math.Min(length - rows.Count, page.Rows.Count - from);
                for (var i = 0; i < take; i++)
                {
                    rows.Add(page.Rows[from + i]);
                }

                if (page.Rows.Count < PageSize)
                {
                    break;
                }
            }

            return rows;
        }
        finally
        {
            _gate.Release();
        }
    }

    /// <summary>Asks the server for the exact total of the current query (one extra query; never automatic
    /// unless <see cref="CountPolicy.Exact"/>).</summary>
    public async Task<int?> RequestExactCountAsync(CancellationToken cancellationToken)
    {
        var generation = _generation;
        await _gate.WaitAsync(cancellationToken).ConfigureAwait(true);
        try
        {
            Superseded(generation, cancellationToken);
            if (ExactTotal is { } known)
            {
                return known;
            }

            using var linked = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken, _generationCts.Token);
            var page = await QueryAsync(1, null, true, generation, linked.Token).ConfigureAwait(true);
            _total = page.Total;
            AnnounceCount();
            return _total;
        }
        finally
        {
            _gate.Release();
        }
    }

    private void Superseded(int generation, CancellationToken cancellationToken)
    {
        cancellationToken.ThrowIfCancellationRequested();
        if (generation != _generation || _disposed)
        {
            throw new OperationCanceledException("The query changed.");
        }
    }

    private async Task<PageData?> EnsurePageAsync(int page, int generation, CancellationToken cancellationToken)
    {
        while (true)
        {
            if (_pages.TryGetValue(page, out var hit))
            {
                Touch(page);
                return hit;
            }

            if (_lastPage >= 0 && page > _lastPage)
            {
                return null;
            }

            // Pages whose cursor is known can be fetched directly; beyond that, walk the chain.
            var target = page < _cursors.Count ? page : _cursors.Count - 1;
            var fetched = await FetchPageAsync(target, generation, cancellationToken).ConfigureAwait(true);
            if (target == page)
            {
                return fetched;
            }
        }
    }

    private void Touch(int page)
    {
        _recent.Remove(page);
        _recent.Add(page);
    }

    private async Task<PageData> FetchPageAsync(int index, int generation, CancellationToken cancellationToken)
    {
        using var linked = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken, _generationCts.Token);
        var withTotal = index == 0 && _options.Count == CountPolicy.Exact && _total is null;
        var response = await QueryAsync(PageSize, _cursors[index], withTotal, generation, linked.Token).ConfigureAwait(true);
        var labels = await ResolveLabelsAsync(response.Records, linked.Token).ConfigureAwait(true);
        Superseded(generation, cancellationToken);

        var columns = Columns;
        var rows = new List<GridRow>(response.Records.Count);
        foreach (var record in response.Records)
        {
            rows.Add(columns.ToRow(record, labels));
            _versions[record.Id] = record.Version;
        }

        var data = new PageData(rows);
        _pages[index] = data;
        Touch(index);
        while (_recent.Count > _options.MaxCachedPages)
        {
            _pages.Remove(_recent[0]);
            _recent.RemoveAt(0);
        }

        if (response.Total is { } total)
        {
            _total = total;
        }

        if (response.NextCursor is null)
        {
            _lastPage = index;
            _lastPageRows = rows.Count;
        }
        else if (_cursors.Count == index + 1)
        {
            _cursors.Add(response.NextCursor);
        }

        AnnounceCount();
        PageLoaded?.Invoke(this, EventArgs.Empty);
        return data;
    }

    private async Task<ListPage> QueryAsync(int limit, string? cursor, bool withTotal, int generation, CancellationToken token)
    {
        try
        {
            var json = new ListQuery(Columns.FieldNames, Filter, Order).ToRequestJson(limit, cursor, withTotal);
            QueriesIssued++;
            var page = await _gateway.QueryAsync(_doctype, json, token).ConfigureAwait(true);
            if (generation != _generation)
            {
                // The answer belongs to an older query: dropped, never applied.
                throw new OperationCanceledException("A response for an older query was dropped.");
            }

            return page;
        }
        catch (OperationCanceledException)
        {
            throw;
        }
        catch (Exception error) when (generation == _generation)
        {
            QueryFailed?.Invoke(this, error);
            throw;
        }
    }

    private async Task<IReadOnlyDictionary<(string, string), string>> ResolveLabelsAsync(IReadOnlyList<ListRecord> records, CancellationToken token)
    {
        // One batch lookup per link column per page, only for ids not already resolved.
        foreach (var column in Columns.Columns.Where(c => c.Field.Kind == Metadata.FieldKind.Link && c.Field.LinkTo is not null && !c.Masked))
        {
            var target = column.Field.LinkTo!;
            var missing = new HashSet<string>(StringComparer.Ordinal);
            foreach (var record in records)
            {
                if (record.Values.TryGetValue(column.Field.Name, out var value) && value.ValueKind == System.Text.Json.JsonValueKind.String
                    && value.GetString() is { } id && !_labels.ContainsKey((target, id)))
                {
                    missing.Add(id);
                }
            }

            if (missing.Count == 0)
            {
                continue;
            }

            var found = await _gateway.ResolveLabelsAsync(target, missing.Take(ListLimits.MaxPageSize).ToArray(), token).ConfigureAwait(true);
            foreach (var id in missing)
            {
                _labels[(target, id)] = found.TryGetValue(id, out var label) ? label : id;
            }
        }

        return _labels;
    }

    private GridRowCount ComputeCount()
    {
        if (_total is { } total)
        {
            return GridRowCount.Known(total);
        }

        if (_lastPage >= 0)
        {
            return GridRowCount.Known(_lastPage * PageSize + _lastPageRows);
        }

        return GridRowCount.Estimated(_cursors.Count * PageSize);
    }

    private void AnnounceCount()
    {
        var count = ComputeCount();
        if (count == _announced)
        {
            return;
        }

        _announced = count;
        void Raise()
        {
            if (_disposed)
            {
                return;
            }

            CountChanged?.Invoke(this, EventArgs.Empty);
            Changed?.Invoke(this, new GridSourceChangedEventArgs(GridChangeKind.Reset));
        }

        if (_context is null)
        {
            Raise();
        }
        else
        {
            _context.Post(static state => ((Action)state!)(), (Action)Raise);
        }
    }

    /// <inheritdoc />
    public void Dispose()
    {
        _disposed = true;
        _generationCts.Cancel();
        _generationCts.Dispose();
        _gate.Dispose();
    }
}

public sealed partial class ListSource
{
    /// <summary>The rows held in the page cache, in list order (what a client-side export can use).</summary>
    public IReadOnlyList<GridRow> CachedRows()
        => _pages.OrderBy(p => p.Key).SelectMany(p => p.Value.Rows).ToArray();

    /// <summary>What the list shows for a loaded document (its first cell), or null when it is not cached.</summary>
    public string? DisplayTextOf(string id)
    {
        foreach (var page in _pages.Values)
        {
            foreach (var row in page.Rows)
            {
                if (row.Key is string key && key == id)
                {
                    return row.Cells.Count > 0 ? row.Cells[0].Text : id;
                }
            }
        }

        return null;
    }
}
