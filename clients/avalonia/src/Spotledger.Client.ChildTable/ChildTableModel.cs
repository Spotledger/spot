using Spotledger.Client.Grid;
using Spotledger.Client.Metadata;
using RowDraft = Spotledger.Client.Metadata.DocumentDraft.TableDraft.RowDraft;
using TableDraft = Spotledger.Client.Metadata.DocumentDraft.TableDraft;

namespace Spotledger.Client.ChildTable;

/// <summary>
/// The renderer-free half of the editable child table: the draft's rows seen as columns, the
/// gate that decides whether anything may change, and every row operation. The control above it
/// only turns keys and pointer input into calls on this class.
///
/// <b>The gate.</b> The draft layer enforces only the mask on a value write; it does not know the
/// caller's write right and it does not refuse an edit to a posted document. So this class does,
/// for every path that can change a row: a cell edit, undo, redo, add, insert, delete, move,
/// duplicate, paste and fill-down all call <see cref="TableRefusal"/> first and change nothing
/// when it is not <see cref="ChildTableRefusal.None"/>. The write right is the host's to grant
/// (<see cref="CanWrite"/>, off by default); the document state comes from the draft.
/// </summary>
public sealed partial class ChildTableModel : IDisposable
{
    private readonly string _tableName;
    private readonly List<ChildTableColumnInfo> _columns = [];
    private bool _canWrite;
    private bool _disposed;

    /// <summary>Creates the model over one child table of a draft.</summary>
    /// <param name="draft">The parent document's draft.</param>
    /// <param name="tableName">The table field's name.</param>
    /// <param name="view">The view definition; null shows every visible scalar field.</param>
    public ChildTableModel(DocumentDraft draft, string tableName, ChildTableView? view = null)
    {
        ArgumentNullException.ThrowIfNull(draft);
        ArgumentException.ThrowIfNullOrEmpty(tableName);
        Draft = draft;
        _tableName = tableName;
        Table = draft.Table(tableName);
        var rowModel = Table.RowModel;
        if (rowModel is not null)
        {
            foreach (var column in view?.Columns ?? DefaultColumns(rowModel))
            {
                var field = rowModel.Field(column.Field)
                    ?? throw new ArgumentException($"The row model {rowModel.Name} has no field {column.Field}.", nameof(view));
                _columns.Add(new ChildTableColumnInfo(field, column));
            }
        }

        Source = new ChildTableSource(Table, _columns);
        Totals = new ChildTableTotals(_columns);
        Totals.Recompute(Table.Rows);
        Draft.StructureChanged += OnStructureChanged;
        Draft.ValidationChanged += OnValidationChanged;
        Draft.Rules.RulesChanged += OnRulesChanged;
    }

    /// <summary>The parent document's draft.</summary>
    public DocumentDraft Draft { get; }

    /// <summary>The child table's draft: the one mutation authority.</summary>
    public TableDraft Table { get; }

    /// <summary>The table field's name on the document.</summary>
    public string TableName => _tableName;

    /// <summary>The columns in display order.</summary>
    public IReadOnlyList<ChildTableColumnInfo> Columns => _columns;

    /// <summary>The grid's data source.</summary>
    public ChildTableSource Source { get; }

    /// <summary>The totals row.</summary>
    public ChildTableTotals Totals { get; }

    /// <summary>The number of rows.</summary>
    public int RowCount => Table.Rows.Count;

    /// <summary>Raised when the table may now be editable or no longer is (the write right, the
    /// document state or a permission changed).</summary>
    public event EventHandler? EditabilityChanged;

    /// <summary>Raised when a total changed.</summary>
    public event EventHandler? TotalsChanged;

    /// <summary>The host's write right on the document. Off by default: a table whose host has
    /// not granted the right is read-only.</summary>
    public bool CanWrite
    {
        get => _canWrite;
        set
        {
            if (_canWrite == value)
            {
                return;
            }

            _canWrite = value;
            EditabilityChanged?.Invoke(this, EventArgs.Empty);
        }
    }

    /// <summary>Why the table as a whole cannot change now, or <see cref="ChildTableRefusal.None"/>.
    /// Every mutating call checks this first.</summary>
    public ChildTableRefusal TableRefusal
    {
        get
        {
            if (_disposed || !_canWrite)
            {
                return ChildTableRefusal.NoWriteRight;
            }

            switch (Draft.Status)
            {
                case DocumentStatus.Posted:
                    return ChildTableRefusal.DocumentPosted;
                case DocumentStatus.Cancelled:
                    return ChildTableRefusal.DocumentCancelled;
                case DocumentStatus.Unknown when Draft.RawStatus is not null:
                    // The server named a state this client does not know: fail closed.
                    return ChildTableRefusal.DocumentStateUnknown;
                default:
                    break;
            }

            if (Draft.Fields.TryGetValue(_tableName, out var state))
            {
                if (state.IsMasked)
                {
                    return ChildTableRefusal.TableMasked;
                }

                if (!state.IsEditable || state.IsReadOnly)
                {
                    return ChildTableRefusal.TableReadOnly;
                }
            }

            return Table.Field.ReadOnly ? ChildTableRefusal.TableReadOnly : ChildTableRefusal.None;
        }
    }

    /// <summary>The table's gate as a result: null when the table as a whole may change now, else the
    /// refusal. This is <see cref="TableRefusal"/> in the shape every operation reports, and the check a
    /// host hands to the form's undo stack, so an undo or a redo takes exactly the path an edit does.
    /// It adds no rule: the write right, the document state (posted, cancelled, unknown fails closed)
    /// and the masked or read-only state come from <see cref="TableRefusal"/>.</summary>
    public ChildTableResult? CheckEditable()
    {
        var refusal = TableRefusal;
        return refusal == ChildTableRefusal.None ? null : ChildTableResult.Refused(refusal);
    }

    /// <summary>True when the table as a whole may change now.</summary>
    public bool IsEditable => TableRefusal == ChildTableRefusal.None;

    /// <summary>Why a cell cannot be edited now: the table gate first, then the cell's own field,
    /// permission and rule state.</summary>
    /// <param name="rowIndex">The 0-based row.</param>
    /// <param name="column">The column index.</param>
    public ChildTableRefusal CellRefusal(int rowIndex, int column)
    {
        var table = TableRefusal;
        if (table != ChildTableRefusal.None)
        {
            return table;
        }

        if (rowIndex < 0 || rowIndex >= Table.Rows.Count || column < 0 || column >= _columns.Count)
        {
            return ChildTableRefusal.OutOfRange;
        }

        return FieldRefusal(Table.Rows[rowIndex], _columns[column].Field);
    }

    private static ChildTableRefusal FieldRefusal(RowDraft row, FieldModel field)
    {
        if (!IsInlineEditable(field))
        {
            return ChildTableRefusal.CellKindNotEditable;
        }

        if (field.ReadOnly)
        {
            return ChildTableRefusal.CellReadOnly;
        }

        if (!row.Values.TryGetValue(field.Name, out var state))
        {
            return ChildTableRefusal.OutOfRange;
        }

        if (state.IsMasked)
        {
            return ChildTableRefusal.CellMasked;
        }

        if (!state.IsEditable || state.IsReadOnly)
        {
            return ChildTableRefusal.CellReadOnly;
        }

        return state.IsVisible ? ChildTableRefusal.None : ChildTableRefusal.CellHidden;
    }

    /// <summary>The kinds edited inline. Long text, JSON, bytes, files, secrets, multi-pick
    /// selects and nested tables need more than a cell, so they are shown but not edited here.</summary>
    internal static bool IsInlineEditable(FieldModel field) => !field.Multiple && field.Kind is FieldKind.Text
        or FieldKind.IntegerNumber or FieldKind.DecimalNumber or FieldKind.Bool or FieldKind.Select
        or FieldKind.Date or FieldKind.Time or FieldKind.Datetime or FieldKind.Link or FieldKind.DynamicLink;

    /// <summary>The first column of a row that can be edited, or -1.</summary>
    public int FirstEditableColumn(int rowIndex)
    {
        for (var c = 0; c < _columns.Count; c++)
        {
            if (CellRefusal(rowIndex, c) == ChildTableRefusal.None)
            {
                return c;
            }
        }

        return -1;
    }

    /// <summary>The next (or previous) editable cell after a cell in reading order, or null.</summary>
    public (int Row, int Column)? NextEditable(int rowIndex, int column, bool forward)
    {
        var step = forward ? 1 : -1;
        var r = rowIndex;
        var c = column + step;
        while (true)
        {
            if (c >= _columns.Count)
            {
                r++;
                c = 0;
            }
            else if (c < 0)
            {
                r--;
                c = _columns.Count - 1;
            }

            if (r < 0 || r >= Table.Rows.Count || _columns.Count == 0)
            {
                return null;
            }

            if (CellRefusal(r, c) == ChildTableRefusal.None)
            {
                return (r, c);
            }

            c += step;
        }
    }

    /// <summary>A snapshot of a row's current values by field name (what a lookup sends as
    /// <c>doc</c>); masked cells are left out.</summary>
    public IReadOnlyDictionary<string, FieldValue> Snapshot(int rowIndex)
    {
        var snapshot = new Dictionary<string, FieldValue>(StringComparer.Ordinal);
        if (rowIndex >= 0 && rowIndex < Table.Rows.Count)
        {
            foreach (var pair in Table.Rows[rowIndex].Values)
            {
                if (!pair.Value.IsMasked)
                {
                    snapshot[pair.Key] = pair.Value.Value;
                }
            }
        }

        return snapshot;
    }

    /// <summary>Puts errors keyed by field path (<c>items[3].qty</c>, rows from 1; <c>items[3]</c>
    /// for a row) onto this table's cells and rows and refreshes the grid. Returns the errors
    /// that name no cell of this table. The server errors normally arrive through
    /// <see cref="ErrorMapper"/>; this is the same placement for local validation.</summary>
    public IReadOnlyList<FieldError> ShowErrors(IEnumerable<FieldError> errors)
    {
        ArgumentNullException.ThrowIfNull(errors);
        var perCell = new Dictionary<(RowDraft Row, string? Field), List<FieldError>>();
        var unplaced = new List<FieldError>();
        foreach (var error in errors)
        {
            if (TryParsePath(error.Field, out var row, out var field) && row >= 1 && row <= Table.Rows.Count
                && (field is null || Table.Rows[row - 1].Values.ContainsKey(field)))
            {
                var key = (Table.Rows[row - 1], field);
                if (!perCell.TryGetValue(key, out var list))
                {
                    perCell[key] = list = [];
                }

                list.Add(error);
            }
            else
            {
                unplaced.Add(error);
            }
        }

        foreach (var ((row, field), list) in perCell)
        {
            if (field is null)
            {
                row.SetRowErrors(list);
            }
            else
            {
                row.Values[field].SetErrors(list);
            }

            Source.Invalidate(row);
        }

        Source.RaiseChanged();
        return unplaced;
    }

    /// <summary>Clears every error shown on the table's cells and rows.</summary>
    public void ClearErrors()
    {
        foreach (var row in Table.Rows)
        {
            row.ClearRowErrors();
            foreach (var state in row.Values.Values)
            {
                state.ClearErrors();
            }

            Source.Invalidate(row);
        }

        Source.RaiseChanged();
    }

    private bool TryParsePath(string path, out int row, out string? field)
    {
        row = 0;
        field = null;
        if (!path.StartsWith(_tableName + "[", StringComparison.Ordinal))
        {
            return false;
        }

        var close = path.IndexOf(']', _tableName.Length);
        if (close < 0 || !int.TryParse(path.AsSpan(_tableName.Length + 1, close - _tableName.Length - 1), out row))
        {
            return false;
        }

        var rest = path[(close + 1)..];
        if (rest.Length == 0)
        {
            return true;
        }

        if (!rest.StartsWith('.'))
        {
            return false;
        }

        field = rest[1..];
        return true;
    }

    private static IEnumerable<ChildTableColumn> DefaultColumns(DocTypeModel rowModel)
        => rowModel.Fields
            .Where(field => !field.Hints.Hidden && field.Kind != FieldKind.Table)
            .Select(field => new ChildTableColumn(field.Name));

    // ---- draft notifications --------------------------------------------------------

    private void OnStructureChanged(object? sender, StructureChangedEventArgs e)
    {
        if (e.Table is not null && !string.Equals(e.Table, _tableName, StringComparison.Ordinal))
        {
            return;
        }

        if (e.Kind == StructureChangeKind.CellEdited && e.Table is not null && e.Field is not null
            && e.Index >= 0 && e.Index < Table.Rows.Count)
        {
            var row = Table.Rows[e.Index];
            Source.Invalidate(row);
            Totals.CellChanged(row, e.Field);
            Source.RaiseChanged();
            if (Totals.Affects(e.Field))
            {
                TotalsChanged?.Invoke(this, EventArgs.Empty);
            }

            return;
        }

        Source.InvalidateAll();
        Totals.Recompute(Table.Rows);
        Source.RaiseChanged();
        TotalsChanged?.Invoke(this, EventArgs.Empty);
        if (e.Kind is StructureChangeKind.DocumentLoaded or StructureChangeKind.RowsLoaded
            or StructureChangeKind.Reverted or StructureChangeKind.Saved or StructureChangeKind.PermissionsApplied)
        {
            EditabilityChanged?.Invoke(this, EventArgs.Empty);
        }
    }

    private void OnValidationChanged(object? sender, RowValidationChangedEventArgs e)
    {
        if (!string.Equals(e.Table, _tableName, StringComparison.Ordinal))
        {
            return;
        }

        if (e.Index >= 0 && e.Index < Table.Rows.Count)
        {
            Source.Invalidate(Table.Rows[e.Index]);
        }
        else
        {
            Source.InvalidateAll();
        }

        Source.RaiseChanged();
    }

    private void OnRulesChanged(object? sender, RulesChangedEventArgs e)
    {
        var any = false;
        var totals = false;
        foreach (var path in e.Fields)
        {
            if (TryParsePath(path, out var row, out var field) && row >= 1 && row <= Table.Rows.Count)
            {
                var draftRow = Table.Rows[row - 1];
                Source.Invalidate(draftRow);
                if (field is not null && Totals.Affects(field))
                {
                    Totals.CellChanged(draftRow, field);
                    totals = true;
                }

                any = true;
            }
        }

        if (any)
        {
            Source.RaiseChanged();
        }

        if (totals)
        {
            TotalsChanged?.Invoke(this, EventArgs.Empty);
        }
    }

    /// <summary>Unwires the model from the draft.</summary>
    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        _disposed = true;
        Draft.StructureChanged -= OnStructureChanged;
        Draft.ValidationChanged -= OnValidationChanged;
        Draft.Rules.RulesChanged -= OnRulesChanged;
    }
}
