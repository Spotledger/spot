using Spotledger.Client.Grid;

namespace Spotledger.Client.Reports;

/// <summary>
/// The read-only grid source over one report result. The whole result is in memory (a run never
/// returns more than the report's <c>max_rows</c>), so sort, quick filter and column order are done
/// here on the loaded rows and nothing else: grouping and filtering beyond the loaded rows stay the
/// server's job. Cells are the exact text received: a decimal is never parsed into a floating-point
/// type, and a masked value is shown as the server sent it. Totals use exact decimal arithmetic.
/// </summary>
public sealed class ReportResultSource : IGridSource
{
    /// <summary>The label of the totals row.</summary>
    public const string TotalLabel = "Total of loaded rows";

    private readonly IReadOnlyList<ReportColumn> _columns;
    private readonly IReadOnlyList<string?[]> _rows;
    private readonly bool _truncated;
    private int[] _layout;
    private int[] _view;
    private string _filter = string.Empty;
    private int _sortColumn = -1;
    private GridSortDirection _sortDirection = GridSortDirection.None;

    /// <summary>Creates the source. <paramref name="truncated"/> hides the totals row: a total of a cut
    /// result would be a number that looks complete and is not.</summary>
    public ReportResultSource(IReadOnlyList<ReportColumn> columns, IReadOnlyList<string?[]> rows, bool truncated = false)
    {
        _columns = columns ?? throw new ArgumentNullException(nameof(columns));
        _rows = rows ?? throw new ArgumentNullException(nameof(rows));
        _truncated = truncated;
        _layout = Enumerable.Range(0, columns.Count).ToArray();
        _view = Enumerable.Range(0, rows.Count).ToArray();
    }

    /// <summary>Resolves a link cell to its label: (column name, id) to a label, or null to show the id.</summary>
    public Func<string, string, string?>? LabelOf { get; set; }

    /// <inheritdoc />
    public GridRowCount Count => GridRowCount.Known(_view.Length);

    /// <inheritdoc />
    public event EventHandler<GridSourceChangedEventArgs>? Changed;

    /// <summary>How many rows were loaded.</summary>
    public int LoadedRows => _rows.Count;

    /// <summary>How many rows the quick filter lets through.</summary>
    public int ShownRows => _view.Length;

    /// <summary>The columns in their current order (the grid's column order).</summary>
    public IReadOnlyList<ReportColumn> Columns => _layout.Select(i => _columns[i]).ToArray();

    /// <summary>The current quick filter text (empty: none).</summary>
    public string Filter => _filter;

    /// <summary>The layout index of the sorted column, or -1.</summary>
    public int SortColumn => _sortColumn < 0 ? -1 : Array.IndexOf(_layout, _sortColumn);

    /// <summary>The sort direction.</summary>
    public GridSortDirection SortDirection => _sortDirection;

    /// <summary>Builds the grid's column models in the current order.</summary>
    public List<GridColumnModel> BuildGridColumns()
    {
        var models = new List<GridColumnModel>(_layout.Length);
        foreach (var index in _layout)
        {
            var column = _columns[index];
            models.Add(new GridColumnModel
            {
                Header = column.Label,
                Width = column.IsNumeric ? GridColumnWidth.Pixels(NumericWidth(index, column)) : GridColumnWidth.Star(1),
                MinWidth = 72,
                DefaultAlignment = column.Alignment,
            });
        }

        return models;
    }

    /// <summary>A number column is wide enough for its longest loaded value (and its total): a right-aligned exact
    /// number that is clipped loses its leading digits, which would be a wrong number rather than a short one.</summary>
    private double NumericWidth(int originalColumn, ReportColumn column)
    {
        var longest = column.Label.Length;
        var sample = Math.Min(_rows.Count, 2000);
        for (var i = 0; i < sample; i++)
        {
            longest = Math.Max(longest, _rows[i][originalColumn]?.Length ?? 0);
        }

        if (column is { IsDecimal: true, Summable: true } && !_truncated)
        {
            longest = Math.Max(longest, TotalOf(originalColumn).Length);
        }

        return Math.Clamp((longest * 8.5) + 28, 132, 440);
    }

    /// <inheritdoc />
    public Task<IReadOnlyList<GridRow>> GetWindow(int start, int length, CancellationToken cancellationToken)
    {
        cancellationToken.ThrowIfCancellationRequested();
        if (start < 0 || length < 0)
        {
            throw new ArgumentOutOfRangeException(nameof(start), "A window cannot start before the first row or have a negative length.");
        }

        var end = (int)Math.Min((long)start + length, _view.Length);
        var rows = new List<GridRow>(Math.Max(0, end - start));
        for (var position = start; position < end; position++)
        {
            var rowIndex = _view[position];
            var cells = new GridCell[_layout.Length];
            for (var c = 0; c < cells.Length; c++)
            {
                var column = _columns[_layout[c]];
                cells[c] = new GridCell(DisplayText(rowIndex, _layout[c]), column.Alignment);
            }

            rows.Add(new GridRow(rowIndex, cells));
        }

        return Task.FromResult<IReadOnlyList<GridRow>>(rows);
    }

    /// <inheritdoc />
    public Task Sort(int columnIndex, GridSortDirection direction)
    {
        if (direction == GridSortDirection.None || columnIndex < 0 || columnIndex >= _layout.Length)
        {
            _sortColumn = -1;
            _sortDirection = GridSortDirection.None;
        }
        else
        {
            _sortColumn = _layout[columnIndex];
            _sortDirection = direction;
        }

        Rebuild();
        return Task.CompletedTask;
    }

    /// <summary>Shows only the rows with a cell that contains <paramref name="text"/> (any case); empty shows all.</summary>
    public void SetFilter(string? text)
    {
        var next = (text ?? string.Empty).Trim();
        if (string.Equals(next, _filter, StringComparison.Ordinal))
        {
            return;
        }

        _filter = next;
        Rebuild();
    }

    /// <summary>Follows the grid after the person moved a column: the same permutation, no refetch of anything.</summary>
    public void MoveColumn(int from, int to)
    {
        if (from == to || from < 0 || to < 0 || from >= _layout.Length || to >= _layout.Length)
        {
            return;
        }

        var list = _layout.ToList();
        var item = list[from];
        list.RemoveAt(from);
        list.Insert(to, item);
        _layout = list.ToArray();
        Changed?.Invoke(this, new GridSourceChangedEventArgs(GridChangeKind.Reset));
    }

    /// <summary>The text of the cell at (<paramref name="viewRow"/>, <paramref name="layoutColumn"/>).</summary>
    public string CellText(int viewRow, int layoutColumn) => DisplayText(_view[viewRow], _layout[layoutColumn]);

    /// <summary>The cells of one shown row in the current column order, as received.</summary>
    public string?[] RawRow(int viewRow)
    {
        var source = _rows[_view[viewRow]];
        return _layout.Select(i => source[i]).ToArray();
    }

    /// <summary>The rows the grid shows, in order, with the cells in the current column order and as received
    /// (a link is its id, not its label). This is what export writes.</summary>
    public IEnumerable<string?[]> ViewRows()
    {
        foreach (var rowIndex in _view)
        {
            var source = _rows[rowIndex];
            var cells = new string?[_layout.Length];
            for (var c = 0; c < cells.Length; c++)
            {
                cells[c] = source[_layout[c]];
            }

            yield return cells;
        }
    }

    /// <summary>The totals row for the grid's footer, or null when the result is cut or no column is summable.</summary>
    public GridRow? BuildFooter()
    {
        if (_truncated || _rows.Count == 0)
        {
            return null;
        }

        var summable = new bool[_layout.Length];
        var any = false;
        for (var c = 0; c < _layout.Length; c++)
        {
            summable[c] = _columns[_layout[c]] is { IsDecimal: true, Summable: true };
            any |= summable[c];
        }

        if (!any)
        {
            return null;
        }

        var labelAt = Array.FindIndex(summable, s => !s);
        var cells = new GridCell[_layout.Length];
        for (var c = 0; c < cells.Length; c++)
        {
            if (summable[c])
            {
                var total = TotalOf(_layout[c]);
                cells[c] = new GridCell(labelAt < 0 && c == 0 ? TotalLabel + ": " + total : total, GridCellAlignment.End);
            }
            else
            {
                cells[c] = new GridCell(c == labelAt ? TotalLabel : string.Empty);
            }
        }

        return new GridRow("totals", cells);
    }

    /// <summary>The exact total of one loaded column, as text with the largest scale seen; empty when a cell is not a number.</summary>
    public string TotalOf(int originalColumn)
    {
        var sum = new ExactNumber(0, 0);
        foreach (var row in _rows)
        {
            var text = row[originalColumn];
            if (text is null)
            {
                continue;
            }

            if (!ExactNumber.TryParse(text, out var number))
            {
                return string.Empty;
            }

            sum = ExactNumber.Add(sum, number);
        }

        return sum.ToString();
    }

    private string DisplayText(int rowIndex, int column)
    {
        var text = _rows[rowIndex][column];
        if (text is null)
        {
            return string.Empty;
        }

        var spec = _columns[column];
        if (LabelOf is { } resolve && string.Equals(spec.Type, "link", StringComparison.Ordinal) && resolve(spec.Name, text) is { Length: > 0 } label)
        {
            return label;
        }

        return text;
    }

    private void Rebuild()
    {
        IEnumerable<int> indexes = Enumerable.Range(0, _rows.Count);
        if (_filter.Length > 0)
        {
            indexes = indexes.Where(MatchesFilter);
        }

        var view = indexes.ToArray();
        if (_sortColumn >= 0 && _sortDirection != GridSortDirection.None)
        {
            Array.Sort(view, BuildComparison(_sortColumn, _sortDirection == GridSortDirection.Descending));
        }

        _view = view;
        Changed?.Invoke(this, new GridSourceChangedEventArgs(GridChangeKind.Reset));
    }

    private bool MatchesFilter(int rowIndex)
    {
        for (var column = 0; column < _columns.Count; column++)
        {
            if (DisplayText(rowIndex, column).Contains(_filter, StringComparison.OrdinalIgnoreCase))
            {
                return true;
            }
        }

        return false;
    }

    private Comparison<int> BuildComparison(int column, bool descending)
    {
        var spec = _columns[column];
        var sign = descending ? -1 : 1;
        if (spec.IsNumeric)
        {
            var keys = new ExactNumber?[_rows.Count];
            for (var i = 0; i < keys.Length; i++)
            {
                keys[i] = ExactNumber.TryParse(_rows[i][column], out var n) ? n : null;
            }

            return (a, b) =>
            {
                var left = keys[a];
                var right = keys[b];
                var order = (left, right) switch
                {
                    (null, null) => 0,
                    (null, _) => 1,
                    (_, null) => -1,
                    _ => sign * ExactNumber.Compare(left.Value, right.Value),
                };
                return order != 0 ? order : a.CompareTo(b);
            };
        }

        return (a, b) =>
        {
            var left = _rows[a][column];
            var right = _rows[b][column];
            var order = (left, right) switch
            {
                (null, null) => 0,
                (null, _) => 1,
                (_, null) => -1,
                _ => sign * string.Compare(DisplayText(a, column), DisplayText(b, column), StringComparison.OrdinalIgnoreCase),
            };
            return order != 0 ? order : a.CompareTo(b);
        };
    }
}
