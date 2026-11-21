using Avalonia.Input;
using Avalonia.Interactivity;
using Avalonia.Media;

namespace Spotledger.Client.Grid;

/// <summary>Pointer-drag half of <see cref="SpotGrid"/>: the press/move/release drag
/// lifecycle (scroll-select, header-edge resize, header-cell reorder), selection click
/// application, header-click dispatch (double-click auto-fit, sort cycling), and the
/// keyboard alternatives for reorder, sort and auto-fit.</summary>
public partial class SpotGrid
{
    private enum DragKind
    {
        None,
        /// <summary>Dragging the header edge to resize the column on its left.</summary>
        Resize,
        /// <summary>Dragging a header cell to reorder columns.</summary>
        Reorder,
        /// <summary>Pressing on a row and dragging selects a range.</summary>
        SelectRows,
    }

    private DragKind _drag = DragKind.None;
    private int _resizeColumn = -1;
    private double _resizeStartX;
    private double _resizeStartWidth;
    private int _reorderFrom = -1;
    private double _reorderOffsetX;
    private int _pressRow = -1;

    private DateTimeOffset _lastHeaderClick = DateTimeOffset.MinValue;
    private int _lastHeaderClickColumn = -1;
    private int _sortedColumn = -1;

    private DateTimeOffset _lastRowClick = DateTimeOffset.MinValue;
    private int _lastRowClickRow = -1;

    /// <inheritdoc />
    protected override void OnPointerPressed(PointerPressedEventArgs e)
    {
        if (IsFromEditor(e))
        {
            return;
        }

        var point = e.GetCurrentPoint(this);
        if (!point.Properties.IsLeftButtonPressed)
        {
            return;
        }

        Focus();
        var hit = HitTest(point.Position.X, point.Position.Y);
        var shift = e.KeyModifiers.HasFlag(KeyModifiers.Shift);
        var control = e.KeyModifiers.HasFlag(KeyModifiers.Control)
            || e.KeyModifiers.HasFlag(KeyModifiers.Meta);

        switch (hit.Area)
        {
            case GridArea.Header:
                HandleHeaderClick(hit.ColumnIndex, point.Position.X);
                e.Handled = true;
                break;

            case GridArea.HeaderEdge:
                BeginResize(hit.ColumnIndex, point.Position.X);
                _drag = DragKind.Resize;
                e.Handled = true;
                break;

            case GridArea.Cell:
                var row = hit.RowIndex;
                Cursor = new Cursor(StandardCursorType.Arrow);
                if (hit.ColumnIndex >= 0 && Columns is { } columns && columns[hit.ColumnIndex].IsCheckColumn)
                {
                    Selection.ToggleChecked(row);
                    RowCheckedChanged?.Invoke(this, new GridRowCheckedChangedEventArgs(
                        row, KeyForRow(row), Selection.HasFlag(row, GridRowState.Checked)));
                }
                else
                {
                    _pressRow = row;
                    ApplySelectionClick(row, shift, control);
                    if (IsDoubleClickOnRow(row))
                    {
                        ActivateRow(row);
                    }
                }

                _drag = DragKind.SelectRows;
                e.Handled = true;
                break;
        }

        InvalidateVisual();
    }

    private bool IsDoubleClickOnRow(int row)
    {
        var now = DateTimeOffset.UtcNow;
        var wasDoubleClick = _lastRowClickRow == row
            && (now - _lastRowClick).TotalMilliseconds < 400;
        _lastRowClick = now;
        _lastRowClickRow = row;
        return wasDoubleClick;
    }

    private void ApplySelectionClick(int row, bool shift, bool control)
    {
        var mode = SelectionMode;
        Selection.Kind = mode;
        var extend = mode == GridSelectionMode.Extended
            ? control ? GridSelectionModel.ExtendMode.Toggle
                : shift ? GridSelectionModel.ExtendMode.Range
                : GridSelectionModel.ExtendMode.Replace
            : mode == GridSelectionMode.Range
                ? shift ? GridSelectionModel.ExtendMode.Range : GridSelectionModel.ExtendMode.Replace
                : GridSelectionModel.ExtendMode.Replace;
        Selection.Apply(row, extend);
        CellCursor = new GridCursor(row, ColumnAtXOrFirst(GetSelectionColumn()), GridCursorOrigin.Mouse);
    }

    private int GetSelectionColumn() => CellCursor.ColumnIndex >= 0 ? CellCursor.ColumnIndex : 0;

    private int ColumnAtXOrFirst(int preferred)
        => preferred >= 0 && preferred < ColumnCount ? preferred : 0;

    /// <inheritdoc />
    protected override void OnPointerMoved(PointerEventArgs e)
    {
        if (IsFromEditor(e))
        {
            return;
        }

        var point = e.GetCurrentPoint(this);
        if (_drag == DragKind.Resize && _resizeColumn >= 0)
        {
            ApplyResizeMove(point.Position.X);
            e.Handled = true;
            return;
        }

        if (_drag == DragKind.Reorder && _reorderFrom >= 0)
        {
            _reorderOffsetX = point.Position.X;
            e.Handled = true;
            return;
        }

        if (_drag == DragKind.SelectRows && _pressRow >= 0 && SelectionMode != GridSelectionMode.OneRow)
        {
            var hit = HitTest(point.Position.X, point.Position.Y);
            if (hit.Area == GridArea.Cell && hit.RowIndex != _pressRow && hit.RowIndex >= 0)
            {
                Selection.Kind = SelectionMode;
                Selection.Apply(hit.RowIndex, GridSelectionModel.ExtendMode.Range);
                _pressRow = hit.RowIndex;
                InvalidateVisual();
            }

            e.Handled = true;
            return;
        }

        base.OnPointerMoved(e);
        UpdateHover(e);
    }

    /// <inheritdoc />
    protected override void OnPointerReleased(PointerReleasedEventArgs e)
    {
        if (IsFromEditor(e))
        {
            return;
        }

        if (_drag == DragKind.Reorder && _reorderFrom >= 0)
        {
            CompleteReorderDrag();
        }

        _drag = DragKind.None;
        _resizeColumn = -1;
        _reorderFrom = -1;
        _pressRow = -1;
    }

    private void BeginResize(int columnIndex, double x)
    {
        _resizeColumn = columnIndex;
        _resizeStartX = x;
        if (Columns is { } columns && _columnWidths is { } widths)
        {
            _resizeStartWidth = widths[columnIndex];
        }
    }

    /// <summary>Applies one resize-drag move: the column's width follows the pointer,
    /// clamped to its Min/MaxWidth bounds (MaxWidth.Infinity means unbounded).</summary>
    private void ApplyResizeMove(double pointerX)
    {
        var column = Columns![_resizeColumn];
        var next = ClampColumnWidth(column, _resizeStartWidth + (pointerX - _resizeStartX));
        column.Width = GridColumnWidth.Pixels(next);
        OnLayoutChanged();
        ColumnResized?.Invoke(this, new GridColumnResizedEventArgs(_resizeColumn, next));
    }

    /// <summary>Ends the reorder drag: moves the dragged column to the header column
    /// under the pointer when it differs from the origin.</summary>
    private void CompleteReorderDrag()
    {
        var target = ColumnIndexAtReorderX(_reorderOffsetX);
        if (target >= 0 && target != _reorderFrom && Columns is IList<GridColumnModel> list)
        {
            MoveColumn(_reorderFrom, target);
        }
    }

    private int ColumnIndexAtReorderX(double x)
    {
        var hit = HitTest(x, HeaderHeight / 2);
        return hit.Area == GridArea.Header ? hit.ColumnIndex : -1;
    }

    private void HandleHeaderClick(int columnIndex, double x)
    {
        // Double click on the trailing edge auto-fits the column.
        var now = DateTimeOffset.UtcNow;
        var isDoubleClick = _lastHeaderClickColumn == columnIndex
            && (now - _lastHeaderClick).TotalMilliseconds < 400;
        var nearEdge = _columnWidths is { } widths
            && columnIndex < widths.Length
            && widths[columnIndex] - (x - ColumnX(columnIndex)) <= GridDefaults.HeaderResizeGripWidth * 3;

        _lastHeaderClick = now;
        _lastHeaderClickColumn = columnIndex;

        if (isDoubleClick && nearEdge && Columns is { } columns && columns[columnIndex].IsResizable)
        {
            AutoFitColumn(columnIndex);
            return;
        }

        if (Columns is { } cols && cols[columnIndex].IsSortable)
        {
            CycleSort(columnIndex);
        }
    }

    /// <summary>Advances the header sort cycle none → ascending → descending → none for
    /// the column and pushes the result to the source. A non-sortable column ignores the
    /// request entirely (no cycle step, no push, no event).</summary>
    internal void CycleSort(int columnIndex)
    {
        if (Columns is { } cols && (columnIndex < 0 || columnIndex >= cols.Count || !cols[columnIndex].IsSortable))
        {
            return;
        }

        var direction = _sortedColumn == columnIndex
            ? SortState.Direction switch
            {
                GridSortDirection.None => GridSortDirection.Ascending,
                GridSortDirection.Ascending => GridSortDirection.Descending,
                _ => GridSortDirection.None,
            }
            : GridSortDirection.Ascending;

        _sortedColumn = direction == GridSortDirection.None ? -1 : columnIndex;
        SortState = (columnIndex, direction);
        ApplySortToSource(columnIndex, direction);
        Sorted?.Invoke(this, new GridSortedEventArgs(columnIndex, direction));
        InvalidateVisual();
    }

    private void ApplySortToSource(int columnIndex, GridSortDirection direction)
    {
        if (Source is not { } source)
        {
            return;
        }

        _ = source.Sort(columnIndex, direction);
        // The source replies with a Changed (reset) event, which re-requests everything.
    }

    /// <summary>Clamps <paramref name="width"/> into the column's Min/MaxWidth bounds
    /// (MaxWidth.Infinity means unbounded).</summary>
    private static double ClampColumnWidth(GridColumnModel column, double width)
        => Math.Clamp(width, column.MinWidth, double.IsFinite(column.MaxWidth) ? column.MaxWidth : double.MaxValue);

    /// <summary>Sets the column's pixel width to fit the widest currently visible cell
    /// (measured with the text formatter), clamped to Min/MaxWidth. Layout is refreshed
    /// without regenerating the layout tables from a null state, so the caller's width
    /// read stays valid.</summary>
    internal void AutoFitColumn(int columnIndex)
    {
        if (Columns is not { } columns || columnIndex < 0 || columnIndex >= columns.Count)
        {
            return;
        }

        var column = columns[columnIndex];
        var widest = MeasureHeaderTextWidth(column.Header);
        if (_scheduler?.LatestCompleted is { } applied)
        {
            for (var i = 0; i < applied.Rows.Count; i++)
            {
                var cell = columnIndex < applied.Rows[i].Cells.Count ? applied.Rows[i].Cells[columnIndex] : null;
                widest = Math.Max(widest, MeasureCellTextWidth(cell?.Text ?? string.Empty));
            }
        }

        var target = ClampColumnWidth(column, widest + (CellPadding * 2) + 4);
        column.Width = GridColumnWidth.Pixels(target);
        // Recompute the layout tables now (they only read the model + viewport), so
        // ColumnWidths stays non-null and the new width shows immediately.
        ComputeColumnLayout(Math.Max(0, ViewportWidth));
        ColumnResized?.Invoke(this, new GridColumnResizedEventArgs(columnIndex, target));
    }

    /// <summary>Keyboard alternative to the reorder drag: moves the column at the cell
    /// cursor one slot left/right (with Ctrl, to the far end).</summary>
    internal void ReorderColumnKeyboard(int columnIndex, int delta)
    {
        var target = Math.Clamp(columnIndex + delta, 0, Math.Max(0, ColumnCount - 1));
        if (target != columnIndex)
        {
            MoveColumn(columnIndex, target);
        }
    }

    /// <summary>Permutes the column model from <paramref name="from"/> to
    /// <paramref name="to"/>: through the raw writable IList view when the host gave one
    /// (arrays report IsReadOnly=true on the generic view but allow element writes), or
    /// through a fresh list republished via the property otherwise.</summary>
    private void MoveColumn(int from, int to)
    {
        if (from == to || Columns is not { } columns
            || from < 0 || to < 0 || from >= columns.Count || to >= columns.Count)
        {
            return;
        }

        if (Columns is System.Collections.IList raw && !raw.IsReadOnly)
        {
            var column = raw[from]!;
            if (to > from)
            {
                for (var i = from; i < to; i++)
                {
                    raw[i] = raw[i + 1]!;
                }
            }
            else
            {
                for (var i = from; i > to; i--)
                {
                    raw[i] = raw[i - 1]!;
                }
            }

            raw[to] = column;
        }
        else
        {
            // No writable view: reorder through a fresh list and republish.
            var reordered = new List<GridColumnModel>(columns.Count);
            for (var i = 0; i < columns.Count; i++)
            {
                if (i != from)
                {
                    reordered.Add(columns[i]);
                }
            }

            reordered.Insert(Math.Min(to, reordered.Count), columns[from]);
            SetCurrentValue(ColumnsProperty, reordered);
        }

        OnLayoutChanged();
        ColumnReordered?.Invoke(this, new GridColumnReorderedEventArgs(from, to));
    }
}