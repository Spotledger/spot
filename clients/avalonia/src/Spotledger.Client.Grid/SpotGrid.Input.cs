using Avalonia;
using Avalonia.Input;
using Avalonia.Interactivity;
using Avalonia.Media;
using Avalonia.Threading;

namespace Spotledger.Client.Grid;

/// <summary>Input half of <see cref="SpotGrid"/>: hit-testing and the geometry queries
/// the pointer and keyboard both need, hover tracking, measure/arrange, the keyboard
/// table (cursor moves, selection, check toggle, activation, copy), select-all. The
/// drag lifecycle and type-ahead live in their own partials. All keyboard work is
/// synchronous over the applied window — nothing here ever blocks on the source.
///
/// Keyboard table (a key not listed does nothing special; modifiers not listed are ignored):
/// <list type="bullet">
/// <item>Arrows, PageUp/PageDown, Home/End, Ctrl+Home/End: move the cell cursor (Shift extends).</item>
/// <item>Enter: edit the cell, else activate the row. F2: edit. Space: toggle the check.</item>
/// <item>Ctrl+C: copy as TSV. Ctrl+A: select all fetched rows.</item>
/// <item>Alt+Down: show the annotation tooltip of the current cell (plain Down when the cell
/// has no annotation). Any later key, or Escape, dismisses it.</item>
/// </list></summary>
public partial class SpotGrid
{
    // ---- hit testing ------------------------------------------------------------

    /// <summary>The band y ranges: header occupies [0, RowHeight), body the rest.</summary>
    private bool InHeader(double y) => y < HeaderHeight;

    /// <summary>True when x is inside the frozen run (never scrolled horizontally).</summary>
    private bool InFrozenZone(double x) => x < FrozenWidth();

    /// <summary>Maps a point to grid content. Columns are resolved through
    /// <see cref="ColumnAtX"/>, rows through the scroll offset.</summary>
    internal GridHitTest HitTest(double x, double y)
    {
        if (x < 0 || y < 0 || x > Bounds.Width || y > Bounds.Height)
        {
            return new GridHitTest(GridArea.None);
        }

        if (InHeader(y))
        {
            var column = ColumnAtX(x, out var onEdge);
            if (onEdge && column >= 0)
            {
                return new GridHitTest(GridArea.HeaderEdge, column);
            }

            return column >= 0
                ? new GridHitTest(GridArea.Header, column)
                : new GridHitTest(GridArea.None);
        }

        var bodyHeight = BodyHeight();
        if (y > HeaderHeight + bodyHeight)
        {
            return new GridHitTest(GridArea.Below);
        }

        var row = FirstVisibleRow + (int)Math.Floor((y - HeaderHeight) / RowHeight);
        if (row >= TotalRows && IsTotalKnown)
        {
            return new GridHitTest(GridArea.Below);
        }

        var column2 = ColumnAtX(x, out _);
        return column2 >= 0
            ? new GridHitTest(GridArea.Cell, column2, row)
            : new GridHitTest(GridArea.None, -1, row);
    }

    /// <summary>The column whose band contains x (frozen columns first, then the
    /// scrolled region); -1 when x is past every column. <paramref name="onEdge"/>
    /// is true within the resize grip of the column's trailing edge.</summary>
    internal int ColumnAtX(double x, out bool onEdge)
    {
        onEdge = false;
        var offsets = _columnOffsets;
        var widths = _columnWidths;
        if (offsets is null || widths is null || widths.Length == 0)
        {
            return -1;
        }

        // The offset table is content space: the frozen run occupies the table's lead and
        // the scrolled region continues in the table after it, so a scrolled-zone x maps
        // by adding the scroll offset alone (the frozen run is already "behind" x).
        var scrolled = x;
        if (!InFrozenZone(x))
        {
            scrolled = x + ScrollX;
        }

        // The frozen zone maps directly; both zones share the offset table.
        var candidates = Enumerable.Range(0, widths.Length);
        foreach (var i in candidates)
        {
            var start = offsets[i];
            var end = start + widths[i];
            if (i >= FrozenColumnCount && (start < ScrollX == false) && end < ScrollX)
            {
                continue;
            }

            if (scrolled >= start && scrolled < end)
            {
                var fromEnd = end - scrolled;
                onEdge = widths[i] > 0
                    && fromEnd <= GridDefaults.HeaderResizeGripWidth
                    && i < widths.Length - (IsLastColumnVisible(i) ? 1 : 0)
                    && Columns is { } cols
                    && cols[i].IsResizable;
                return i;
            }
        }

        return -1;
    }

    private bool IsLastColumnVisible(int index)
    {
        // A column is the rightmost visible one when nothing wider sits after it in
        // the visible band (frozen columns complicate order; approximate by index).
        return index == ColumnCount - 1;
    }

    /// <summary>The x of a column's leading edge in control coordinates.</summary>
    internal double ColumnX(int index)
    {
        if (_columnOffsets is not { } offsets || index < 0 || index >= offsets.Length)
        {
            return 0;
        }

        var x = index < FrozenColumnCount
            ? offsets[index]
            : offsets[index] - ScrollX + FrozenWidth();
        return x;
    }

    /// <summary>The body viewport height (grid height minus header and footer bands).</summary>
    internal double BodyHeight() => Math.Max(0, Bounds.Height - HeaderHeight - (Footer is not null ? RowHeight : 0));

    /// <summary>The y of a row's top edge in control coordinates (within the body).</summary>
    internal double RowY(int rowIndex) => HeaderHeight + ((rowIndex - ScrollRows) * RowHeight);

    // ---- measure / arrange --------------------------------------------------------

    /// <inheritdoc />
    protected override Size MeasureOverride(Size availableSize)
    {
        var width = double.IsInfinity(availableSize.Width) ? 400 : availableSize.Width;
        var height = double.IsInfinity(availableSize.Height) ? 300 : availableSize.Height;
        _overlayDecorator?.Measure(new Size(width, height));
        return new Size(width, height);
    }

    /// <inheritdoc />
    protected override Size ArrangeOverride(Size finalSize)
    {
        SetViewport(finalSize.Width, BodyHeightFor(finalSize.Height));
        RequestWindow();
        _overlayDecorator?.Arrange(new Rect(finalSize));
        RepositionEditorIfOpen();
        return finalSize;
    }

    private double BodyHeightFor(double totalHeight)
        => Math.Max(0, totalHeight - HeaderHeight - (Footer is not null ? RowHeight : 0));

    // ---- hover ---------------------------------------------------------------------

    /// <summary>Updates the hover row and header cursor from a pointer event (a no-op
    /// while the left button is down — drags paint their own cursor).</summary>
    private void UpdateHover(PointerEventArgs e)
    {
        var point = e.GetCurrentPoint(this);
        if (!point.Properties.IsLeftButtonPressed)
        {
            var hit = HitTest(point.Position.X, point.Position.Y);
            UpdateAnnotationTooltip(point.Position);
            var hovered = hit.Area == GridArea.Cell ? hit.RowIndex : -1;
            if (hovered != HoveredRow)
            {
                HoveredRow = hovered;
                InvalidateVisual();
            }

            Cursor = hit.Area switch
            {
                GridArea.HeaderEdge => new Cursor(StandardCursorType.SizeWestEast),
                GridArea.Header => new Cursor(StandardCursorType.Hand),
                _ => Cursor.Default,
            };
        }
    }

    /// <inheritdoc />
    protected override void OnPointerEntered(PointerEventArgs e)
    {
        if (!IsFromEditor(e))
        {
            UpdateHover(e);
        }
    }

    /// <inheritdoc />
    protected override void OnPointerExited(PointerEventArgs e)
    {
        HoveredRow = -1;
        ClearAnnotationTooltip();
        InvalidateVisual();
    }

    // ---- keyboard: the table -------------------------------------------------------

    /// <inheritdoc />
    protected override void OnKeyDown(KeyEventArgs e)
    {
        // A key bubbling up from the open cell editor belongs to the editor.
        if (IsFromEditor(e))
        {
            return;
        }

        if (RaiseGridKeyDown(e))
        {
            e.Handled = true;
            return;
        }

        var key = e.Key;
        var control = e.KeyModifiers.HasFlag(KeyModifiers.Control)
            || e.KeyModifiers.HasFlag(KeyModifiers.Meta);
        var shift = e.KeyModifiers.HasFlag(KeyModifiers.Shift);

        if (key == Key.Down && e.KeyModifiers.HasFlag(KeyModifiers.Alt) && ShowAnnotationTooltipAtCursor())
        {
            e.Handled = true;
            return;
        }

        if (_keyTipOpen)
        {
            // Any other key dismisses the annotation tooltip; Escape does only that.
            CloseKeyboardTooltip();
            if (key == Key.Escape)
            {
                e.Handled = true;
                return;
            }
        }

        switch (key)
        {
            case Key.Left:
                MoveCursorByColumn(e, -1);
                e.Handled = true;
                break;

            case Key.Right:
                MoveCursorByColumn(e, 1);
                e.Handled = true;
                break;

            case Key.Up:
                MoveCursorByRow(e, -1, shift);
                e.Handled = true;
                break;

            case Key.Down:
                MoveCursorByRow(e, 1, shift);
                e.Handled = true;
                break;

            case Key.PageUp:
                MoveCursorByRow(e, -VisibleRowCount(), shift, page: true);
                e.Handled = true;
                break;

            case Key.PageDown:
                MoveCursorByRow(e, VisibleRowCount(), shift, page: true);
                e.Handled = true;
                break;

            case Key.Home when control:
                MoveCursorTo(e, 0, 0, shift);
                e.Handled = true;
                break;

            case Key.End when control:
                MoveCursorTo(e, Math.Max(0, TotalRows - 1), ColumnCount - 1, shift);
                e.Handled = true;
                break;

            case Key.Home:
                MoveCursorTo(e, null, 0, shift);
                e.Handled = true;
                break;

            case Key.End:
                MoveCursorTo(e, null, ColumnCount - 1, shift);
                e.Handled = true;
                break;

            case Key.Enter:
                if (CellCursor.HasValue && !BeginEdit(CellCursor.RowIndex, CellCursor.ColumnIndex))
                {
                    ActivateRow(CellCursor.RowIndex);
                }

                e.Handled = true;
                break;

            case Key.Space when !control:
                ToggleCheckAtCursor();
                e.Handled = true;
                break;

            case Key.C when control:
                CopySelectionToClipboard();
                e.Handled = true;
                break;

            case Key.A when control:
                SelectAllRows();
                e.Handled = true;
                break;

            case Key.F2:
                if (CellCursor.HasValue)
                {
                    BeginEdit(CellCursor.RowIndex, CellCursor.ColumnIndex);
                }

                e.Handled = true;
                break;

            default:
                TypeAhead(e.KeySymbol);
                break;
        }
    }

    private void MoveCursorByColumn(KeyEventArgs e, int delta, bool large = false)
    {
        if (!CellCursor.HasValue)
        {
            CellCursor = new GridCursor(FirstVisibleRow, Math.Max(0, delta > 0 ? 0 : ColumnCount - 1), GridCursorOrigin.Keyboard);
            InvalidateVisual();
            return;
        }

        var column = Math.Clamp(CellCursor.ColumnIndex + delta, 0, Math.Max(0, ColumnCount - 1));
        CellCursor = new GridCursor(CellCursor.RowIndex, column, GridCursorOrigin.Keyboard);
        EnsureCursorVisible();
        InvalidateVisual();
    }

    private void MoveCursorByRow(KeyEventArgs e, int delta, bool extend, bool page = false)
    {
        if (!CellCursor.HasValue)
        {
            CellCursor = new GridCursor(Math.Max(0, FirstVisibleRow), 0, GridCursorOrigin.Keyboard);
            InvalidateVisual();
            return;
        }

        var row = Math.Clamp(CellCursor.RowIndex + delta, 0, Math.Max(0, TotalRows - 1));
        if (extend && !Selection.Anchor.HasValue)
        {
            // A shift-extend with no anchored selection ranges from the cursor's row
            // before the move: the origin becomes the anchor (and joins the selection).
            Selection.Kind = SelectionMode;
            Selection.Apply(CellCursor.RowIndex, GridSelectionModel.ExtendMode.Replace);
        }

        CellCursor = new GridCursor(row, CellCursor.ColumnIndex, GridCursorOrigin.Keyboard);
        if (extend)
        {
            Selection.Kind = SelectionMode;
            Selection.Apply(row, GridSelectionModel.ExtendMode.Range);
        }
        else if (!page)
        {
            Selection.Apply(row, GridSelectionModel.ExtendMode.Replace);
        }

        EnsureCursorVisible();
        InvalidateVisual();
    }

    private void MoveCursorTo(KeyEventArgs e, int? row, int? column, bool extend)
    {
        var currentRow = CellCursor.HasValue ? CellCursor.RowIndex : 0;
        var targetRow = Math.Clamp(row ?? currentRow, 0, Math.Max(0, TotalRows - 1));
        var targetColumn = Math.Clamp(column ?? (CellCursor.HasValue ? CellCursor.ColumnIndex : 0), 0, Math.Max(0, ColumnCount - 1));
        CellCursor = new GridCursor(targetRow, targetColumn, GridCursorOrigin.Keyboard);
        if (extend && SelectionMode != GridSelectionMode.OneRow)
        {
            Selection.Kind = SelectionMode;
            Selection.Apply(targetRow, GridSelectionModel.ExtendMode.Range);
        }
        else
        {
            Selection.Apply(targetRow, GridSelectionModel.ExtendMode.Replace);
        }

        EnsureCursorVisible();
        InvalidateVisual();
    }

    /// <summary>Scrolls minimally so the cursor's row and column are visible.</summary>
    private void EnsureCursorVisible()
    {
        if (!CellCursor.HasValue)
        {
            return;
        }

        EnsureCellVisible(CellCursor.RowIndex, CellCursor.ColumnIndex);
    }

    /// <summary>Scrolls minimally so the given cell is visible (no-op when it already is).</summary>
    private void EnsureCellVisible(int row, int columnIndex)
    {
        var first = FirstVisibleRow;
        var visible = VisibleRowCount();
        if (row < first)
        {
            ScrollToRow(row);
        }
        else if (row >= first + visible && visible > 0)
        {
            ScrollToRow(row - visible + 1);
        }

        if (_columnWidths is { } widths && _columnOffsets is { } offsets)
        {
            var index = columnIndex;
            if (index >= 0 && index < widths.Length && index >= FrozenColumnCount)
            {
                var start = offsets[index];
                var end = start + widths[index];
                var viewStart = ScrollX;
                var viewEnd = ScrollX + Math.Max(0, ViewportWidth - FrozenWidth());
                if (start < viewStart)
                {
                    ScrollToX(start);
                }
                else if (end > viewEnd)
                {
                    ScrollToX(end - Math.Max(0, ViewportWidth - FrozenWidth()));
                }
            }
        }
    }

    private void ToggleCheckAtCursor()
    {
        if (!CellCursor.HasValue)
        {
            return;
        }

        var row = CellCursor.RowIndex;
        Selection.ToggleChecked(row);
        RowCheckedChanged?.Invoke(this, new GridRowCheckedChangedEventArgs(
            row, KeyForRow(row), Selection.HasFlag(row, GridRowState.Checked)));
        InvalidateVisual();
    }

    private void ActivateRow(int row)
    {
        RowActivated?.Invoke(this, new GridRowActivatedEventArgs(row, KeyForRow(row)));
    }

    /// <summary>Copy: the selection (or the focused row when nothing is selected, or
    /// the focused cell) as TSV through <see cref="ClipboardSink"/>.</summary>
    internal void CopySelectionToClipboard()
    {
        if (ClipboardSink is not { } sink)
        {
            return;
        }

        var text = BuildTsv();
        if (text.Length == 0)
        {
            return;
        }

        _ = sink.SetTextAsync(text);
    }

    /// <summary>Formats the current selection as TSV. A selected row yields one line of
    /// its data columns joined by tabs; no selection yields just the focused cell.
    /// The check column is excluded.</summary>
    internal string BuildTsv()
    {
        var selected = Selection.SelectedRows;
        if (selected.Count == 0 && CellCursor.HasValue)
        {
            var cell = RowAt(CellCursor.RowIndex)?.Cells.Count > CellCursor.ColumnIndex
                ? RowAt(CellCursor.RowIndex)!.Cells[CellCursor.ColumnIndex].Text
                : string.Empty;
            return cell;
        }

        var builder = new System.Text.StringBuilder();
        foreach (var row in selected)
        {
            builder.AppendLine(BuildRowTsv(row));
        }

        return builder.ToString().TrimEnd('\r', '\n');
    }

    /// <summary>One row's data columns (check column excluded, empty reserve cells
    /// dropped) joined by tabs.</summary>
    private string BuildRowTsv(int rowIndex)
    {
        if (Columns is not { } columns || RowAt(rowIndex) is not { } row)
        {
            return string.Empty;
        }

        var values = new List<string>(columns.Count);
        for (var c = 0; c < columns.Count && c < row.Cells.Count; c++)
        {
            if (columns[c].IsCheckColumn || row.Cells[c].Text.Length == 0)
            {
                // The check column carries no data, and reserve cells the host left
                // empty carry none either — a copied row states only the cells it holds.
                continue;
            }

            values.Add(row.Cells[c].Text);
        }

        return string.Join("\t", values);
    }

    private void SelectAllRows()
    {
        Selection.Kind = SelectionMode;
        if (_scheduler?.LatestCompleted is not { } applied)
        {
            return;
        }

        Selection.SetCheckedRange(applied.Start, applied.Rows.Count, false);
        for (var i = applied.Start; i < applied.Start + applied.Rows.Count; i++)
        {
            Selection.Apply(i, GridSelectionModel.ExtendMode.Toggle);
        }
    }
}