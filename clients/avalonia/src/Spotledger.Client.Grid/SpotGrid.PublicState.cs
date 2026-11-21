using Avalonia.Input;

namespace Spotledger.Client.Grid;

/// <summary>
/// The public state surface of <see cref="SpotGrid"/> (WP-UI-08.T6): thin wrappers over the
/// internal cursor, selection, edit and scroll state, for hosts such as the child-table
/// control. Index of members:
/// <list type="bullet">
/// <item><see cref="Cursor"/>, <see cref="SetCursor"/>, <see cref="CursorChanged"/>: the focused cell.</item>
/// <item><see cref="SelectedRows"/>, <see cref="SetSelection"/>, <see cref="SelectionChanged"/>: row selection.</item>
/// <item><see cref="IsEditing"/>, <see cref="EditingCell"/>, <see cref="EndEdit"/>: the open cell editor (<see cref="BeginEdit"/> opens one).</item>
/// <item><see cref="ScrollIntoView"/>, <see cref="RowCount"/>, <see cref="TryGetRow"/>: scrolling and row access.</item>
/// <item><see cref="GridKeyDown"/>, <see cref="OnGridKeyDown"/>: claim a key before default handling.</item>
/// </list>
/// </summary>
public partial class SpotGrid
{
    /// <summary>Raised whenever the focused cell changes (keyboard, mouse, type-ahead,
    /// edit move, <see cref="SetCursor"/> or a source reset).</summary>
    public event EventHandler<GridCursorChangedEventArgs>? CursorChanged;

    /// <summary>Raised when the selection (or check) state changes.</summary>
    public event EventHandler? SelectionChanged;

    /// <summary>Raised for a key that reaches the grid, before default handling. Set
    /// <see cref="GridKeyEventArgs.Handled"/> to claim it.</summary>
    public event EventHandler<GridKeyEventArgs>? GridKeyDown;

    /// <summary>The current cell; <see cref="GridCursor.RowIndex"/> and
    /// <see cref="GridCursor.ColumnIndex"/> are -1 when there is none.</summary>
    public GridCursor GridCellCursor => CellCursor;

    /// <summary>Moves the cursor to a cell (clamped to the table, origin keyboard), scrolls
    /// it into view and raises <see cref="CursorChanged"/> only when the cell changes. Never
    /// opens an editor.</summary>
    public void SetCursor(int row, int column)
    {
        if (TotalRows <= 0 || ColumnCount <= 0)
        {
            return;
        }

        row = Math.Clamp(row, 0, TotalRows - 1);
        column = Math.Clamp(column, 0, ColumnCount - 1);
        CellCursor = new GridCursor(row, column, GridCursorOrigin.Keyboard);
        EnsureCellVisible(row, column);
        InvalidateVisual();
    }

    /// <summary>Selected source row indexes, ascending (a copy).</summary>
    public IReadOnlyList<int> SelectedRows
    {
        get
        {
            var rows = new List<int>(Selection.SelectedRows);
            rows.Sort();
            return rows;
        }
    }

    /// <summary>Replaces the selection. Indexes outside the table are ignored; with
    /// <see cref="GridSelectionMode.OneRow"/> only the first valid index is kept.</summary>
    public void SetSelection(IEnumerable<int> rows)
    {
        ArgumentNullException.ThrowIfNull(rows);
        var before = SelectedRows;
        var total = TotalRows;
        var wanted = new List<int>();
        var seen = new HashSet<int>();
        foreach (var row in rows)
        {
            if (row >= 0 && row < total && seen.Add(row))
            {
                wanted.Add(row);
            }
        }

        if (SelectionMode == GridSelectionMode.OneRow && wanted.Count > 1)
        {
            wanted.RemoveRange(1, wanted.Count - 1);
        }

        _suppressSelectionEvent = true;
        try
        {
            Selection.Kind = SelectionMode;
            Selection.ClearSelection();
            foreach (var row in wanted)
            {
                Selection.Apply(row, wanted[0] == row
                    ? GridSelectionModel.ExtendMode.Replace
                    : GridSelectionModel.ExtendMode.Toggle);
            }
        }
        finally
        {
            _suppressSelectionEvent = false;
        }

        if (!before.SequenceEqual(SelectedRows))
        {
            InvalidateVisual();
            SelectionChanged?.Invoke(this, EventArgs.Empty);
        }
    }

    /// <summary>The open editor's cell (row, column), or null when none is open.</summary>
    public (int Row, int Column)? EditingCell
        => IsEditing ? (_session.RowIndex, _session.ColumnIndex) : null;

    /// <summary>Closes the open editor like Enter (commit) or Escape (cancel): the factory's
    /// commit or cancel and <c>EditFinished</c> run as usual. Returns false when nothing was
    /// open or the commit was refused and the editor stayed open.</summary>
    public bool EndEdit(bool commit)
    {
        if (!IsEditing)
        {
            return false;
        }

        var before = _session;
        CloseEditor(commit);
        return !ReferenceEquals(before, _session);
    }

    /// <summary>Scrolls minimally so the row (and, when <paramref name="column"/> is 0 or
    /// more, the column) is fully visible; no-op if it already is. Indexes are clamped.</summary>
    public void ScrollIntoView(int row, int column = -1)
    {
        if (TotalRows <= 0)
        {
            return;
        }

        row = Math.Clamp(row, 0, TotalRows - 1);
        column = column < 0 ? -1 : Math.Min(column, Math.Max(0, ColumnCount - 1));
        EnsureCellVisible(row, column);
    }

    /// <summary>The total row count the grid lays out against (0 with no source).</summary>
    public int RowCount => TotalRows;

    /// <summary>Gets a row from the latest completed window; false when the row is outside it.</summary>
    public bool TryGetRow(int row, out GridRow value)
    {
        value = RowAt(row)!;
        return value is not null;
    }

    /// <summary>Called first for a key that reaches the grid; raises <see cref="GridKeyDown"/>.
    /// Set <see cref="GridKeyEventArgs.Handled"/> to stop all default handling.</summary>
    /// <param name="e">The key and grid state.</param>
    protected virtual void OnGridKeyDown(GridKeyEventArgs e) => GridKeyDown?.Invoke(this, e);

    private GridKeyEventArgs? _gridKeyArgs;
    private bool? _overridesGridKey;
    private bool _suppressSelectionEvent;

    /// <summary>Raises the key hook; true when a handler claimed the key.</summary>
    private bool RaiseGridKeyDown(KeyEventArgs e)
    {
        _overridesGridKey ??= GetType().GetMethod(
            nameof(OnGridKeyDown),
            System.Reflection.BindingFlags.Instance | System.Reflection.BindingFlags.NonPublic)
            ?.DeclaringType != typeof(SpotGrid);
        if (GridKeyDown is null && _overridesGridKey != true)
        {
            return false;
        }

        // A key bubbling up from the open cell editor belongs to the editor.
        if (IsEditing && !ReferenceEquals(e.Source, this))
        {
            return false;
        }

        var args = _gridKeyArgs ??= new GridKeyEventArgs();
        args.Reset(e.Key, e.KeyModifiers, CellCursor, IsEditing, TotalRows);
        OnGridKeyDown(args);
        return args.Handled;
    }

    private GridSelectionModel CreateSelection()
    {
        var model = new GridSelectionModel(FindRowByKey, KeyForRow);
        model.Changed += OnSelectionModelChanged;
        return model;
    }

    private void ResetSelection()
    {
        var old = Selection;
        var hadState = old.Flags.Count > 0;
        old.Changed -= OnSelectionModelChanged;

        Selection = CreateSelection();
        if (hadState)
        {
            SelectionChanged?.Invoke(this, EventArgs.Empty);
        }
    }

    private void OnSelectionModelChanged(object? sender, EventArgs e)
    {
        if (!_suppressSelectionEvent)
        {
            SelectionChanged?.Invoke(this, EventArgs.Empty);
        }
    }
}
