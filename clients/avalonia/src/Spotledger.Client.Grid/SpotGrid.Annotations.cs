using Avalonia;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Media;

namespace Spotledger.Client.Grid;

/// <summary>Annotation half of <see cref="SpotGrid"/>: the severity marker a data source
/// asks for on a cell or row, the hit-test and tooltip that explain it, and the
/// next-annotation search a control uses for "go to the first error". The grid knows
/// only cells and annotations; what a note means stays with its source.
///
/// A row annotation shares the row's first column with that cell's own marker: the cell
/// marker sits in the top trailing corner, the row marker in the bottom leading corner.
/// For the tooltip, hit-test and search a cell's own annotation wins in the first column
/// and the row annotation answers when the cell has none.</summary>
public partial class SpotGrid
{
    /// <summary>The longest side of a marker shape, in device-independent pixels.</summary>
    private const double MarkerSize = 8;

    // Unit shapes in an 8 x 8 box. Error is a corner triangle, Warning a diamond and
    // Info a circle: three silhouettes, so severity reads without colour.
    private static readonly Geometry TriangleTopRight = StreamGeometry.Parse("M 0 0 L 8 0 L 8 8 Z");
    private static readonly Geometry TriangleBottomLeft = StreamGeometry.Parse("M 0 0 L 0 8 L 8 8 Z");
    private static readonly Geometry Diamond = StreamGeometry.Parse("M 4 0 L 8 4 L 4 8 L 0 4 Z");
    private static readonly Geometry Circle = StreamGeometry.Parse("M 4 0 A 4 4 0 1 1 4 8 A 4 4 0 1 1 4 0 Z");

    private string? _hoverTipText;
    private bool _keyTipOpen;

    /// <summary>True while the Alt+Down tooltip is open.</summary>
    internal bool KeyboardTooltipOpen => _keyTipOpen;

    /// <summary>The annotation under a point in control coordinates, or null: a cell's
    /// own annotation, else (first column only) the row's.</summary>
    public GridCellAnnotation? GetAnnotationAt(Point point)
    {
        var hit = HitTest(point.X, point.Y);
        return hit.Area == GridArea.Cell ? AnnotationAt(hit.RowIndex, hit.ColumnIndex) : null;
    }

    /// <summary>The next annotated cell after <paramref name="from"/> in reading order
    /// (row by row, left to right), wrapping past the last fetched row to the first.
    /// Only rows already fetched are searched, so a control never waits on the source.
    /// <paramref name="severity"/> filters to one severity; null accepts any. The result
    /// has no value (<see cref="GridCursor.HasValue"/> is false) when nothing matches.
    /// A <paramref name="from"/> without a value starts before the first cell.</summary>
    public GridCursor? FindNextAnnotation(GridCursor? from = null, GridAnnotationSeverity? severity = null)
    {
        if (_scheduler?.LatestCompleted is not { } applied || applied.Rows.Count == 0 || ColumnCount == 0)
        {
            return null;
        }

        var columns = ColumnCount;
        var start = applied.Start;
        var count = applied.Rows.Count;

        // Linear positions inside the fetched slice; the walk visits every position once,
        // ending on the origin itself so a single annotated cell finds itself after a lap.
        var origin = -1;
        if (from is { HasValue: true } at)
        {
            origin = ((at.RowIndex - start) * columns) + Math.Min(at.ColumnIndex, columns - 1);
        }

        var total = count * columns;
        for (var step = 1; step <= total; step++)
        {
            var position = (((origin + step) % total) + total) % total;
            var rowIndex = start + (position / columns);
            var column = position % columns;
            var row = applied.Rows[position / columns];
            if (Matches(AnnotationOf(row, column), severity))
            {
                return new GridCursor(rowIndex, column, GridCursorOrigin.Keyboard);
            }
        }

        return null;
    }

    /// <summary>The annotation message for the current cell (the text assistive
    /// technology and the keyboard tooltip share), or null.</summary>
    internal string? CurrentCellAnnotationText()
        => CellCursor.HasValue ? AnnotationAt(CellCursor.RowIndex, CellCursor.ColumnIndex)?.Message : null;

    /// <summary>The tooltip text the grid currently carries (hover or keyboard).</summary>
    internal string? AnnotationTooltipText => ToolTip.GetTip(this) as string;

    /// <summary>"Error: message" (severity word first), or null for no annotation.</summary>
    internal static string? DescribeAnnotation(GridCellAnnotation? annotation)
        => annotation is null ? null : $"{annotation.Severity}: {annotation.Message}";

    internal GridCellAnnotation? AnnotationAt(int rowIndex, int columnIndex)
        => RowAt(rowIndex) is { } row ? AnnotationOf(row, columnIndex) : null;

    private static GridCellAnnotation? AnnotationOf(GridRow row, int columnIndex)
    {
        if (columnIndex >= 0 && columnIndex < row.Cells.Count && row.Cells[columnIndex].Annotation is { } own)
        {
            return own;
        }

        return columnIndex == 0 ? row.RowAnnotation : null;
    }

    private static bool Matches(GridCellAnnotation? annotation, GridAnnotationSeverity? severity)
        => annotation is not null && (severity is null || annotation.Severity == severity);

    // ---- drawing ------------------------------------------------------------------

    private static IBrush AnnotationBrush(GridAnnotationSeverity severity) => severity switch
    {
        GridAnnotationSeverity.Error => TokenBrush("Color.Semantic.Danger"),
        GridAnnotationSeverity.Warning => TokenBrush("Color.Semantic.Warning"),
        _ => TokenBrush("Color.Semantic.Info"),
    };

    /// <summary>Draws the markers a cell (and, in column 0, its row) asks for. Callers
    /// invoke it only when an annotation exists, so unannotated cells cost one null check.
    /// Geometry is in the grid's own left-to-right space: with a right-to-left flow
    /// direction Avalonia mirrors the whole control, which puts the markers in the
    /// leading and trailing corners the reader expects without any code here.</summary>
    private static void DrawAnnotationMarkers(
        DrawingContext context,
        GridCellAnnotation? cellNote,
        GridCellAnnotation? rowNote,
        double x,
        double y,
        double width,
        double rowHeight)
    {
        var side = Math.Min(MarkerSize, Math.Max(4, (rowHeight / 2) - 1));
        using var clip = context.PushClip(new Rect(x, y, width, rowHeight));

        if (cellNote is not null)
        {
            DrawMarker(context, cellNote.Severity, x + width - side, y, side, bottom: false);
        }

        if (rowNote is not null)
        {
            DrawMarker(context, rowNote.Severity, x, y + rowHeight - side, side, bottom: true);
        }
    }

    /// <summary>Draws one marker in the box whose top-left is (<paramref name="left"/>,
    /// <paramref name="top"/>). The cell marker hugs the top-right corner, the row marker
    /// the bottom-left one.</summary>
    private static void DrawMarker(
        DrawingContext context,
        GridAnnotationSeverity severity,
        double left,
        double top,
        double side,
        bool bottom)
    {
        var geometry = severity switch
        {
            GridAnnotationSeverity.Error => bottom ? TriangleBottomLeft : TriangleTopRight,
            GridAnnotationSeverity.Warning => Diamond,
            _ => Circle,
        };

        var scale = side / MarkerSize;
        var matrix = Matrix.CreateScale(scale, scale) * Matrix.CreateTranslation(left, top);
        var outline = new Pen(TokenBrush("Color.Surface.Canvas"), 1d, null, PenLineCap.Flat, PenLineJoin.Miter, 0d);
        using var transform = context.PushTransform(matrix);
        context.DrawGeometry(AnnotationBrush(severity), outline, geometry);
    }

    // ---- tooltip ------------------------------------------------------------------

    /// <summary>Shows the pointer tooltip for the annotation under the pointer, or clears
    /// it. Cheap when nothing changed (called from every pointer move).</summary>
    internal void UpdateAnnotationTooltip(Point position)
    {
        if (_keyTipOpen)
        {
            return;
        }

        var text = GetAnnotationAt(position)?.Message;
        if (string.IsNullOrEmpty(text))
        {
            text = null;
        }

        if (string.Equals(text, _hoverTipText, StringComparison.Ordinal))
        {
            return;
        }

        _hoverTipText = text;
        ToolTip.SetPlacement(this, PlacementMode.Pointer);
        ToolTip.SetTip(this, text);
    }

    internal void ClearAnnotationTooltip()
    {
        CloseKeyboardTooltip();
        if (_hoverTipText is not null)
        {
            _hoverTipText = null;
            ToolTip.SetTip(this, null);
        }
    }

    /// <summary>Alt+Down: opens the tooltip for the current cell. False when the cell
    /// carries no annotation (the key then behaves as a plain Down).</summary>
    private bool ShowAnnotationTooltipAtCursor()
    {
        if (!CellCursor.HasValue || CurrentCellAnnotationText() is not { Length: > 0 } text)
        {
            return false;
        }

        var column = CellCursor.ColumnIndex;
        ToolTip.SetPlacement(this, PlacementMode.BottomEdgeAlignedLeft);
        ToolTip.SetHorizontalOffset(this, Math.Max(0, ColumnX(column)));
        ToolTip.SetVerticalOffset(this, -(Bounds.Height - (RowY(CellCursor.RowIndex) + RowHeight)));
        _hoverTipText = text;
        ToolTip.SetTip(this, text);
        ToolTip.SetIsOpen(this, true);
        _keyTipOpen = true;
        return true;
    }

    private void CloseKeyboardTooltip()
    {
        if (!_keyTipOpen)
        {
            return;
        }

        _keyTipOpen = false;
        ToolTip.SetIsOpen(this, false);
    }
}
