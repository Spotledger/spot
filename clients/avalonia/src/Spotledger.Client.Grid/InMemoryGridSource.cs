namespace Spotledger.Client.Grid;

/// <summary>
/// An <see cref="IGridSource"/> over an in-memory list — the demo/gallery backend and the
/// perf-test fixture (100,000 × 12). Windows are synchronous (the task completes inline);
/// sorts rewrite the underlying ordering and raise <see cref="IGridSource.Changed"/>
/// with a reset. Numerics are held as strings for display; the type column proves the
/// grid never parses cell values (brief must-not: no floats for numeric cell values).
/// </summary>
public sealed class InMemoryGridSource : IGridSource
{
    private readonly List<GridRow> _rows;
    private readonly Func<GridRow, string?> _sortKeySelector;

    /// <summary>The natural text order used for sorts: digit runs compare as numbers,
    /// so label-shaped keys ("row 12", "c0·7") keep their arithmetic order instead of
    /// the ordinal "row 1, row 10, row 2" ordering users would call a bug.</summary>
    private sealed class NaturalOrder : IComparer<string?>
    {
        public static readonly NaturalOrder Instance = new();

        public int Compare(string? x, string? y)
        {
            if (ReferenceEquals(x, y))
            {
                return 0;
            }

            if (x is null)
            {
                return -1;
            }

            if (y is null)
            {
                return 1;
            }

            var ix = 0;
            var iy = 0;
            while (ix < x.Length && iy < y.Length)
            {
                if (char.IsDigit(x[ix]) && char.IsDigit(y[iy]))
                {
                    var nx = ParseDigits(x, ref ix);
                    var ny = ParseDigits(y, ref iy);
                    if (nx != ny)
                    {
                        return nx.CompareTo(ny);
                    }
                }
                else if (x[ix] != y[iy])
                {
                    return x[ix].CompareTo(y[iy]);
                }
                else
                {
                    ix++;
                    iy++;
                }
            }

            return (x.Length - ix).CompareTo(y.Length - iy);
        }

        /// <summary>Consumes one digit run starting at <paramref name="index"/> and
        /// returns its numeric value.</summary>
        private static long ParseDigits(string text, ref int index)
        {
            var start = index;
            while (index < text.Length && char.IsDigit(text[index]))
            {
                index++;
            }

            return long.Parse(text.AsSpan(start, index - start), System.Globalization.CultureInfo.InvariantCulture);
        }
    }

    /// <summary>Creates a source over <paramref name="rows"/>. When a sort key selector
    /// is given, <see cref="Sort"/> orders rows by the selected cell text; an explicit
    /// <see cref="GridRowCount"/> overrides the default Known(count) statement.</summary>
    public InMemoryGridSource(IEnumerable<GridRow> rows, Func<GridRow, string?>? sortKeySelector = null)
        : this(rows, GridRowCount.Known(rows?.Count() ?? 0), sortKeySelector)
    {
    }

    /// <summary>Creates a source with an explicit count statement (Unknown/Estimated
    /// learn their end from exhausted windows in the scheduler).</summary>
    public InMemoryGridSource(IEnumerable<GridRow> rows, GridRowCount count, Func<GridRow, string?>? sortKeySelector = null)
    {
        _rows = [.. rows ?? throw new ArgumentNullException(nameof(rows))];
        Count = count;
        _sortKeySelector = sortKeySelector ?? (row => null);
    }

    /// <inheritdoc />
    public GridRowCount Count { get; private set; }

    /// <inheritdoc />
    public event EventHandler<GridSourceChangedEventArgs>? Changed;

    /// <summary>The current sort, if any (the grid mirrors this for indicators).</summary>
    public (int Column, GridSortDirection Direction)? CurrentSort { get; private set; }

    /// <summary>Sort calls the grid pushed down (column, direction), recorded for tests.</summary>
    public List<(int Column, GridSortDirection Direction)> SortCalls { get; } = [];

    /// <inheritdoc />
    public Task<IReadOnlyList<GridRow>> GetWindow(int start, int length, CancellationToken cancellationToken)
    {
        ArgumentOutOfRangeException.ThrowIfNegative(start);
        ArgumentOutOfRangeException.ThrowIfNegative(length);
        cancellationToken.ThrowIfCancellationRequested();
        var take = Math.Min(length, Math.Max(0, _rows.Count - start));
        IReadOnlyList<GridRow> window = _rows.GetRange(start, take);
        return Task.FromResult(window);
    }

    /// <inheritdoc />
    public Task Sort(int columnIndex, GridSortDirection direction)
    {
        SortCalls.Add((columnIndex, direction));
        if (direction != GridSortDirection.None)
        {
            IEnumerable<GridRow> ordered = direction == GridSortDirection.Ascending
                ? _rows.OrderBy(_sortKeySelector, NaturalOrder.Instance)
                : _rows.OrderByDescending(_sortKeySelector, NaturalOrder.Instance);
            var sorted = ordered.ToList();
            _rows.Clear();
            _rows.AddRange(sorted);
            CurrentSort = (columnIndex, direction);
        }
        else
        {
            CurrentSort = null;
        }

        // None closes the cycle (and every sort ends with) a reset; the grid re-requests.
        Changed?.Invoke(this, new GridSourceChangedEventArgs(GridChangeKind.Reset));
        return Task.CompletedTask;
    }

    /// <summary>Mutates the row set directly (gallery fixtures); raises insert/remove/reset events.</summary>
    public void AddRow(GridRow row)
    {
        _rows.Add(row);
        Count = new GridRowCount(Count.Kind, _rows.Count);
        Changed?.Invoke(this, new GridSourceChangedEventArgs(GridChangeKind.RowsInserted, _rows.Count - 1, 1));
    }

    /// <summary>Removes a row by index; raises the removal event.</summary>
    public void RemoveRowAt(int index)
    {
        if (index < 0 || index >= _rows.Count)
        {
            return;
        }

        _rows.RemoveAt(index);
        Count = new GridRowCount(Count.Kind, _rows.Count);
        Changed?.Invoke(this, new GridSourceChangedEventArgs(GridChangeKind.RowsRemoved, index, 1));
    }
}

/// <summary>Builds classic tabular fixture rows: <c>r{index}</c> keys with
/// <c>col{c}</c>-named cells whose values embed the row index (so sorts and window
/// assertions are meaningful). The 13th synthetic column carries a decimal-formatted
/// number string produced with invariant formatting — a string, as the contract requires.</summary>
public static class GridFixture
{
    /// <summary>Makes <paramref name="rowCount"/> rows of <paramref name="columnCount"/> cells.</summary>
    public static List<GridRow> MakeRows(int rowCount, int columnCount)
        => MakeRows(rowCount, columnCount, static (r, c) => c == 0 ? $"row {r}" : $"c{c}·{r}");

    /// <summary>Makes the editing-fixture shape: cells named "c{2c}·r" (slot 0 = "c0·r",
    /// slot 1 = "c2·r", ...) — the exact texts the editing tests pin per model column.</summary>
    public static List<GridRow> MakeEditingRows(int rowCount, int columnCount)
        => MakeRows(rowCount, columnCount, static (r, c) => $"c{(2 * c).ToString(System.Globalization.CultureInfo.InvariantCulture)}·{r}");

    /// <summary>One row shape: <c>r{r}</c> keys, cells from <paramref name="cellText"/>,
    /// column 0 start-aligned, the rest end-aligned.</summary>
    private static List<GridRow> MakeRows(int rowCount, int columnCount, Func<int, int, string> cellText)
    {
        var rows = new List<GridRow>(rowCount);
        for (var r = 0; r < rowCount; r++)
        {
            var cells = new GridCell[columnCount];
            for (var c = 0; c < columnCount; c++)
            {
                cells[c] = new GridCell(
                    cellText(r, c),
                    c == 0 ? GridCellAlignment.Start : GridCellAlignment.End);
            }

            rows.Add(GridRow.Of($"r{r}", cells));
        }

        return rows;
    }
}