using Spotledger.Client.Grid;
using Spotledger.Client.Metadata;
using RowDraft = Spotledger.Client.Metadata.DocumentDraft.TableDraft.RowDraft;
using TableDraft = Spotledger.Client.Metadata.DocumentDraft.TableDraft;

namespace Spotledger.Client.ChildTable;

/// <summary>
/// The grid's <see cref="IGridSource"/> over a draft's child rows. The whole table is held in
/// memory (large imports are WP-DOC-09's), so a window is a slice of the draft's rows turned
/// into display strings. The row key is the <see cref="RowDraft"/> itself: it is the row's
/// identity across a reorder (ids are retained, ADR-057), and it exists for a new row that has no
/// server id yet. Cells carry their error as a grid annotation: a cell error as the cell's marker,
/// a row error as the row's. The table is not sortable: the draft's order is the contract's
/// <c>idx</c>, so a header click can never reorder what a save would send.
/// </summary>
public sealed class ChildTableSource : IGridSource
{
    private readonly TableDraft _table;
    private readonly IReadOnlyList<ChildTableColumnInfo> _columns;
    private readonly Dictionary<RowDraft, GridRow> _cache = new(ReferenceEqualityComparer.Instance);

    internal ChildTableSource(TableDraft table, IReadOnlyList<ChildTableColumnInfo> columns)
    {
        _table = table;
        _columns = columns;
    }

    /// <inheritdoc />
    public GridRowCount Count => GridRowCount.Known(_table.Rows.Count);

    /// <inheritdoc />
    public event EventHandler<GridSourceChangedEventArgs>? Changed;

    /// <summary>How many rows have been turned into grid rows since creation (tests assert a cell
    /// edit rebuilds one row, not the table).</summary>
    public int RowsBuilt { get; private set; }

    /// <inheritdoc />
    public Task<IReadOnlyList<GridRow>> GetWindow(int start, int length, CancellationToken cancellationToken)
    {
        var rows = _table.Rows;
        start = Math.Clamp(start, 0, rows.Count);
        length = Math.Clamp(length, 0, rows.Count - start);
        var window = new GridRow[length];
        for (var i = 0; i < length; i++)
        {
            window[i] = RowAt(rows[start + i]);
        }

        return Task.FromResult<IReadOnlyList<GridRow>>(window);
    }

    /// <inheritdoc />
    public Task Sort(int columnIndex, GridSortDirection direction) => Task.CompletedTask;

    /// <summary>The grid row for a draft row, built once until it is invalidated.</summary>
    internal GridRow RowAt(RowDraft row)
    {
        if (_cache.TryGetValue(row, out var cached))
        {
            return cached;
        }

        var cells = new GridCell[_columns.Count];
        for (var c = 0; c < cells.Length; c++)
        {
            var field = _columns[c].Field;
            row.Values.TryGetValue(field.Name, out var state);
            var text = state is null
                ? string.Empty
                : state.IsMasked ? state.MaskedText ?? "••••" : CellFormatter.Format(field, state.Value);
            var cell = new GridCell(text, CellFormatter.Align(field));
            if (state is { Errors.Count: > 0 })
            {
                cell = cell with { Annotation = Annotate(state.Errors.Select(error => error.Message)) };
            }

            cells[c] = cell;
        }

        var built = new GridRow(row, cells)
        {
            RowAnnotation = row.RowErrors.Count > 0 ? Annotate(row.RowErrors.Select(error => error.Message)) : null,
        };
        RowsBuilt++;
        _cache[row] = built;
        return built;
    }

    private static GridCellAnnotation Annotate(IEnumerable<string> messages)
        => new(GridAnnotationSeverity.Error, string.Join("; ", messages));

    /// <summary>Forgets one row's cached cells (its value or errors changed).</summary>
    internal void Invalidate(RowDraft row) => _cache.Remove(row);

    /// <summary>Forgets every cached row (rows came, went or moved).</summary>
    internal void InvalidateAll() => _cache.Clear();

    /// <summary>Tells the grid its window is stale.</summary>
    internal void RaiseChanged() => Changed?.Invoke(this, new GridSourceChangedEventArgs(GridChangeKind.Reset));
}
