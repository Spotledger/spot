using Avalonia;
using Avalonia.Media;
using Avalonia.Media.Immutable;
using Avalonia.Media.TextFormatting;
using Avalonia.Utilities;

namespace Spotledger.Client.Grid;

/// <summary>Renderer half of <see cref="SpotGrid"/>. Paints everything in one pass:
/// header band (with sort glyphs), frozen columns, body rows (zebra, hover, selection
/// and check marks), the loading shimmer for unfetched rows, the footer and the error
/// row. No control is created per cell — each string draws through a short-lived
/// <see cref="FormattedText"/> which keeps per-line geometry internal and allocation
/// light.</summary>
public partial class SpotGrid
{
    /// <inheritdoc />
    public override void Render(DrawingContext context)
    {
        var headerHeight = HeaderHeight;
        var bodyTop = headerHeight;
        var bodyHeight = BodyHeight();

        context.FillRectangle(TokenBrush("Color.Surface.Canvas"), new Rect(0, 0, Bounds.Width, Bounds.Height));

        DrawBody(context, bodyTop, bodyHeight);
        DrawFooter(context, bodyTop + bodyHeight);
        DrawHeader(context, 0, headerHeight);
    }

    /// <summary>
    /// Draws text so its glyphs are upright in a right-to-left grid. Avalonia mirrors everything a
    /// right-to-left control draws (the mirror is how the layout flips), glyphs included; the text's
    /// box is flipped back about its own centre, so it stays where the mirrored layout put it and
    /// reads the right way round.
    /// </summary>
    private void DrawTextUpright(DrawingContext context, FormattedText formatted, Point origin)
    {
        if (FlowDirection != FlowDirection.RightToLeft)
        {
            context.DrawText(formatted, origin);
            return;
        }

        var centre = origin.X + (formatted.Width / 2);
        var flip = Matrix.CreateTranslation(-centre, 0) * Matrix.CreateScale(-1, 1) * Matrix.CreateTranslation(centre, 0);
        using (context.PushTransform(flip))
        {
            context.DrawText(formatted, origin);
        }
    }

    private static IBrush BrushBorder() => TokenBrush("Color.Border.Default");

    private static Typeface TypefaceBody => new(
        ResolveFontFamily(),
        weight: FontWeight.Normal);

    private static Typeface TypefaceHeader => new(
        ResolveFontFamily(),
        weight: FontWeight.Medium);

    /// <summary>The Latin token family (a <see cref="FontFamily"/> resource, or a name) with the
    /// bundled Urdu face listed after it, so an Urdu header or cell is shaped with the bundled
    /// Noto Naskh Arabic on every machine instead of whichever system font the host falls back
    /// to (the grid painted different pixels on different hosts).</summary>
    private static FontFamily ResolveFontFamily()
    {
        string latin = FontFamily.Default.Name;
        if (Application.Current is { } app
            && app.TryGetResource("Font.Family.Latin", ThemeVariantService.Active, out var value))
        {
            latin = value switch
            {
                FontFamily f => f.ToString(),
                string s => s,
                _ => latin,
            };
        }

        if (_resolvedFamily is { } cached && cached.Latin == latin)
        {
            return cached.Family;
        }

        var family = FontFamily.Parse($"{latin}, {Spotledger.Client.Theme.SpotledgerFonts.Urdu}");
        _resolvedFamily = (latin, family);
        return family;
    }

    private static (string Latin, FontFamily Family)? _resolvedFamily;

    private static double FontSize => TokenDouble("Font.Size.Body");
    private static double FontSizeHeader => TokenDouble("Font.Size.Caption");

    /// <summary>The one shared <see cref="FormattedText"/> construction (culture, LTR by
    /// default, no drawing): every draw/measure call goes through it.</summary>
    private static FormattedText FormatText(string text, Typeface typeface, double size, IBrush brush, bool rtl = false)
        => new(
            text,
            System.Globalization.CultureInfo.CurrentCulture,
            rtl ? FlowDirection.RightToLeft : FlowDirection.LeftToRight,
            typeface,
            size,
            brush);

    /// <summary>A one-pixel pen for a hairline; pens are built per call but are value
    /// types over immutable brushes, so the allocation is the one Pen wrapper.</summary>
    private static Pen LinePen(IBrush brush) => new(brush, 1d, null, PenLineCap.Flat, PenLineJoin.Miter, 0d);

    private void DrawHeader(DrawingContext context, double y, double height)
    {
        var columns = Columns;
        if (columns is null || _columnOffsets is null || _columnWidths is null)
        {
            // A headerless grid still paints the band edge.
            context.FillRectangle(TokenBrush("Color.Surface.Raised"), new Rect(0, y, Bounds.Width, height));
            return;
        }

        context.FillRectangle(TokenBrush("Color.Surface.Raised"), new Rect(0, y, Bounds.Width, height));
        context.DrawLine(LinePen(BrushBorder()), new Point(0, y + height - 0.5), new Point(Bounds.Width, y + height - 0.5));

        for (var i = 0; i < columns.Count && i < _columnWidths.Length; i++)
        {
            var x = ColumnX(i);
            var width = _columnWidths[i];
            if (x + width < 0 || x > Bounds.Width || width <= 0)
            {
                continue;
            }

            if (i > 0)
            {
                context.DrawLine(LinePen(BrushBorder()), new Point(x, y), new Point(x, y + height));
            }

            var glyphWidth = SortState.Column == i && SortState.Direction != GridSortDirection.None ? 14 : 0;
            DrawHeaderCaption(context, columns[i].Header, x + CellPadding, y, width - (CellPadding * 2) - glyphWidth, height);

            if (SortState.Column == i)
            {
                DrawSortGlyph(context, x + width - CellPadding - 10, (y + height / 2) - 3, SortState.Direction);
            }

            if (i == FrozenColumnCount - 1)
            {
                context.DrawLine(
                    LinePen(TokenBrush("Color.Border.Strong")),
                    new Point(x + width, y),
                    new Point(x + width, y + height));
            }
        }
    }

    /// <summary>Draws the ascending or descending triangle glyph.</summary>
    private static void DrawSortGlyph(DrawingContext context, double x, double centerY, GridSortDirection direction)
    {
        var brush = TokenBrush("Color.Brand.600");
        using var translated = context.PushTransform(Matrix.CreateTranslation(x, centerY));
        var geometry = direction == GridSortDirection.Ascending
            ? StreamGeometry.Parse("M 0 6 L 5 0 L 10 6 Z")
            : StreamGeometry.Parse("M 0 0 L 10 0 L 5 6 Z");
        context.DrawGeometry(brush, null, geometry);
    }

    private void DrawBody(DrawingContext context, double bodyTop, double bodyHeight)
    {
        var clip = new Rect(0, bodyTop, Bounds.Width, bodyHeight);
        using (context.PushClip(clip))
        {
            DrawBodyInner(context, bodyTop, bodyHeight, clip);
        }
    }

    private void DrawBodyInner(DrawingContext context, double bodyTop, double bodyHeight, Rect clip)
    {
        var columns = Columns;
        if (columns is null || _columnWidths is null || _columnOffsets is null)
        {
            return;
        }

        var first = FirstVisibleRow;
        var visible = VisibleRowCount();
        var rows = VisibleRows;
        var frozenWidth = FrozenWidth();
        var rowTop = bodyTop + ((first - ScrollRows) * RowHeight);
        var rowHeight = RowHeight;

        for (var v = 0; v < visible; v++)
        {
            var sourceIndex = first + v;
            if (sourceIndex >= TotalRows && IsTotalKnown)
            {
                break;
            }

            var y = rowTop + (v * rowHeight);
            var gridRow = rows is not null && v < rows.Count ? rows[v] : null;
            if (gridRow is null)
            {
                DrawShimmerRow(context, y, rowHeight, sourceIndex);
                continue;
            }

            DrawDataRow(context, y, rowHeight, sourceIndex, gridRow, frozenWidth);
        }

        if (ErrorState is { } error)
        {
            DrawErrorRow(context, bodyTop, bodyHeight, error.Message);
        }

        if (TotalRows == 0 && !IsLoading && ErrorState is null)
        {
            DrawEmptyState(context, bodyTop);
        }

        DrawColumnSeparators(context, bodyTop, bodyHeight);
    }

    private void DrawDataRow(
        DrawingContext context,
        double y,
        double rowHeight,
        int sourceIndex,
        GridRow row,
        double frozenWidth)
    {
        var columns = Columns!;
        var isHovered = HoveredRow == sourceIndex;
        var isSelected = Selection.HasFlag(sourceIndex, GridRowState.Selected);
        var zebra = sourceIndex % 2 == 1;

        if (zebra)
        {
            context.FillRectangle(TokenBrush("Color.Surface.Sunken"), new Rect(0, y, Bounds.Width, rowHeight));
        }

        if (isSelected)
        {
            context.FillRectangle(TokenBrush("Color.Brand.50"), new Rect(0, y, Bounds.Width, rowHeight));
        }

        if (isHovered && !isSelected)
        {
            context.FillRectangle(TokenBrush("Color.Neutral.100"), new Rect(0, y, Bounds.Width, rowHeight));
        }

        for (var c = 0; c < columns.Count && c < row.Cells.Count && c < _columnWidths!.Length; c++)
        {
            var column = columns[c];
            var cell = row.Cells[c];
            var x = ColumnX(c);
            var width = _columnWidths[c];
            if (width <= 0 || x > Bounds.Width || x + width < 0)
            {
                continue;
            }

            if (column.IsCheckColumn)
            {
                DrawCheckCell(context, x, y, width, rowHeight, sourceIndex);
            }
            else
            {
                DrawCellTextAligned(context, cell, x, y, width, rowHeight, column.DefaultAlignment);
            }

            var rowNote = c == 0 ? row.RowAnnotation : null;
            if (cell.Annotation is not null || rowNote is not null)
            {
                DrawAnnotationMarkers(context, cell.Annotation, rowNote, x, y, width, rowHeight);
            }
        }

        if (CellCursor.HasValue && CellCursor.RowIndex == sourceIndex && CellCursor.ColumnIndex < _columnWidths!.Length)
        {
            var c = CellCursor.ColumnIndex;
            DrawCursorCell(context, ColumnX(c), y, _columnWidths[c], rowHeight);
        }

        if (FrozenColumnCount > 0)
        {
            var edge = FrozenWidth();
            context.DrawLine(
                LinePen(TokenBrush("Color.Border.Strong")),
                new Point(edge, y),
                new Point(edge, y + rowHeight));
        }
    }

    private void DrawCellTextAligned(
        DrawingContext context,
        GridCell cell,
        double x,
        double y,
        double width,
        double rowHeight,
        GridCellAlignment columnDefault)
    {
        var text = cell.Text;
        if (string.IsNullOrEmpty(text) || width <= CellPadding * 2)
        {
            return;
        }

        var alignment = cell.Alignment == GridCellAlignment.Start ? columnDefault : cell.Alignment;
        var rtl = FlowDirection == FlowDirection.RightToLeft;
        var formatted = FormatText(text, TypefaceBody, FontSize, CellBrushFor(cell.StyleClass), rtl);
        var textWidth = formatted.Width;
        var textX = alignment switch
        {
            GridCellAlignment.Center => x + ((width - textWidth) / 2),
            GridCellAlignment.End => x + width - CellPadding - textWidth,
            _ => x + CellPadding,
        };

        var textY = y + ((rowHeight - formatted.Height) / 2);
        using (context.PushClip(new Rect(x, y, width, rowHeight)))
        {
            DrawTextUpright(context, formatted, new Point(textX, textY));
        }
    }

    private static IBrush CellBrushFor(string? styleClass) => styleClass switch
    {
        "danger" => TokenBrush("Color.Semantic.Danger"),
        "success" => TokenBrush("Color.Semantic.Success"),
        "warning" => TokenBrush("Color.Semantic.Warning"),
        "muted" => TokenBrush("Color.Text.Disabled"),
        _ => TokenBrush("Color.Text.Primary"),
    };

    private void DrawCheckCell(DrawingContext context, double x, double y, double width, double rowHeight, int sourceIndex)
    {
        var isChecked = Selection.HasFlag(sourceIndex, GridRowState.Checked);
        var size = Math.Min(14, Math.Max(8, rowHeight - 12));
        var box = new Rect(x + ((width - size) / 2), y + ((rowHeight - size) / 2), size, size);
        var borderPen = new Pen(TokenBrush("Color.Border.Strong"), 1d, null, PenLineCap.Flat, PenLineJoin.Miter, 0d);
        context.DrawRectangle(isChecked ? TokenBrush("Color.Brand.600") : null, borderPen, box, 2d, 2d);
        if (isChecked)
        {
            var pen = new Pen(TokenBrush("Color.Text.Secondary"), 1.4d, null, PenLineCap.Round, PenLineJoin.Round, 0d);
            var x1 = box.X + (size * 0.22);
            var y1 = box.Y + (size * 0.55);
            var x2 = box.X + (size * 0.45);
            var y2 = box.Y + (size * 0.78);
            var x3 = box.X + (size * 0.8);
            var y3 = box.Y + (size * 0.3);
            context.DrawLine(pen, new Point(x1, y1), new Point(x2, y2));
            context.DrawLine(pen, new Point(x2, y2), new Point(x3, y3));
        }
    }

    /// <summary>Draws the focused-cell rectangle (2px brand rule).</summary>
    private static void DrawCursorCell(DrawingContext context, double x, double y, double width, double rowHeight)
    {
        var pen = new Pen(TokenBrush("Color.Brand.600"), 2d, null, PenLineCap.Flat, PenLineJoin.Miter, 0d);
        context.DrawRectangle(null, pen, new Rect(x + 1, y + 1, Math.Max(1, width - 2), Math.Max(1, rowHeight - 2)), 2d, 2d);
    }

    private void DrawShimmerRow(DrawingContext context, double y, double rowHeight, int sourceIndex)
    {
        using var faded = context.PushOpacity(0.55);
        var brush = TokenBrush("Color.Neutral.200");
        for (var c = 0; c < ColumnCount && c < _columnWidths!.Length; c++)
        {
            if (Columns is { } columns && columns[c].IsCheckColumn)
            {
                continue;
            }

            var x = ColumnX(c);
            var width = _columnWidths[c];
            if (width <= 0)
            {
                continue;
            }

            var bandWidth = Math.Min(width - (CellPadding * 2), width * 0.6);
            if (bandWidth <= 0)
            {
                continue;
            }

            var bandHeight = Math.Min(10, Math.Max(4, rowHeight - 12));
            context.FillRectangle(
                brush,
                new Rect(x + CellPadding, y + ((rowHeight - bandHeight) / 2), bandWidth, bandHeight),
                (float)(bandHeight / 2));
        }
    }

    private void DrawErrorRow(DrawingContext context, double bodyTop, double bodyHeight, string message)
    {
        var bandHeight = Math.Min(RowHeight, bodyHeight);
        var bandY = bodyTop + bodyHeight - bandHeight;
        context.FillRectangle(TokenBrush("Color.Semantic.Danger"), new Rect(0, bandY, Bounds.Width, bandHeight));
        using var faded = context.PushOpacity(0.85);
        var formatted = FormatText($"Load failed: {message} — click Retry", TypefaceBody, FontSize, TokenBrush("Color.Text.Secondary"));
        DrawTextUpright(context, formatted, new Point(CellPadding, bandY + ((bandHeight - formatted.Height) / 2)));
    }

    private void DrawEmptyState(DrawingContext context, double bodyTop)
    {
        var formatted = FormatText("No rows.", TypefaceBody, FontSize, TokenBrush("Color.Text.Disabled"));
        DrawTextUpright(context, formatted, new Point(CellPadding, bodyTop + CellPadding));
    }

    private void DrawColumnSeparators(DrawingContext context, double bodyTop, double bodyHeight)
    {
        if (_columnWidths is not { } widths)
        {
            return;
        }

        for (var i = 1; i < widths.Length; i++)
        {
            var x = ColumnX(i);
            if (x <= 0 || x > Bounds.Width)
            {
                continue;
            }

            context.DrawLine(LinePen(BrushBorder()), new Point(x, bodyTop), new Point(x, bodyTop + bodyHeight));
        }

        if (FrozenColumnCount > 0)
        {
            var edge = FrozenWidth();
            context.DrawLine(LinePen(TokenBrush("Color.Border.Strong")), new Point(edge, bodyTop), new Point(edge, bodyTop + bodyHeight));
        }
    }

    private void DrawFooter(DrawingContext context, double y)
    {
        if (Footer is not { } footer || _columnWidths is null || Columns is not { } columns)
        {
            return;
        }

        var height = RowHeight;
        context.FillRectangle(TokenBrush("Color.Surface.Raised"), new Rect(0, y, Bounds.Width, height));
        context.DrawLine(LinePen(BrushBorder()), new Point(0, y), new Point(Bounds.Width, y));

        for (var c = 0; c < columns.Count && c < footer.Cells.Count && c < _columnWidths.Length; c++)
        {
            var column = columns[c];
            if (column.IsCheckColumn)
            {
                continue;
            }

            var cell = footer.Cells[c];
            var x = ColumnX(c);
            var width = _columnWidths[c];
            DrawCellTextAligned(context, cell, x, y, width, height, column.DefaultAlignment);
        }
    }

    private void DrawHeaderCaption(
        DrawingContext context,
        string header,
        double x,
        double y,
        double maxWidth,
        double height)
    {
        if (string.IsNullOrEmpty(header) || maxWidth <= 2)
        {
            return;
        }

        var formatted = FormatText(header, TypefaceHeader, FontSizeHeader, TokenBrush("Color.Text.Secondary"));
        using (context.PushClip(new Rect(x - 2, y, maxWidth + 4, height)))
        {
            DrawTextUpright(context, formatted, new Point(x, y + ((height - formatted.Height) / 2)));
        }
    }

    /// <summary>Measures a string with the body typeface (auto-fit); a one-shot
    /// <see cref="FormattedText"/> allocation, no drawing.</summary>
    internal static double MeasureCellTextWidth(string text)
    {
        if (string.IsNullOrEmpty(text))
        {
            return 0;
        }

        var formatted = FormatText(text, TypefaceBody, FontSize, TokenBrush("Color.Text.Primary"));
        return formatted.Width;
    }

    /// <summary>Measures the header caption width (auto-fit).</summary>
    internal static double MeasureHeaderTextWidth(string header)
    {
        if (string.IsNullOrEmpty(header))
        {
            return 0;
        }

        var formatted = FormatText(header, TypefaceHeader, FontSizeHeader, TokenBrush("Color.Text.Secondary"));
        return formatted.Width;
    }
}