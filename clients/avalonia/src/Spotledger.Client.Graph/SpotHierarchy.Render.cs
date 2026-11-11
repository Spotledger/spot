using Avalonia;
using Avalonia.Media;

namespace Spotledger.Client.Graph;

/// <summary>Renderer half of <see cref="SpotHierarchy"/>: the breadcrumb band, the visible
/// row band (indent guides, chevrons, kind caption, title, detail, lock placeholder, selection
/// and focus), the drop indicator and the status captions. Only rows inside the viewport are
/// painted, which is what keeps a 10,000-node tree at viewport cost.</summary>
public partial class SpotHierarchy
{
    private const double KindCaptionWidth = 96;

    internal IReadOnlyList<string> LastRenderedTitles => _renderedTitles;
    private readonly List<string> _renderedTitles = [];

    /// <inheritdoc />
    public override void Render(DrawingContext context)
    {
        ArgumentNullException.ThrowIfNull(context);
        var bounds = new Rect(0, 0, Bounds.Width, Bounds.Height);
        context.FillRectangle(GraphTokens.Brush("Color.Surface.Canvas", Brushes.White), bounds);

        if (_model is not { } model)
        {
            LastRenderedRowCount = 0;
            DrawCaption(context, "No hierarchy source.");
            return;
        }

        if (model.Error is { } error)
        {
            DrawCaption(context, string.Concat("Could not load: ", error), danger: true);
            return;
        }

        if (!model.IsInitialized)
        {
            LastRenderedRowCount = 0;
            DrawCaption(context, "Loading…");
            return;
        }

        if (model.RowCount == 0)
        {
            LastRenderedRowCount = 0;
            DrawCaption(context, model.Query.HasFilter ? "No nodes match the filter." : "No nodes.");
            return;
        }

        if (ShowBreadcrumb)
        {
            DrawBreadcrumb(context, model);
        }

        DrawRows(context, model);
    }

    private void DrawRows(DrawingContext context, HierarchyModel model)
    {
        var rows = model.Rows;
        var rowHeight = RowHeight;
        var first = FirstVisibleRow;
        var visible = VisibleRowCount();
        _renderedTitles.Clear();
        var drawn = 0;
        using (context.PushClip(new Rect(0, ContentTop, Bounds.Width, Math.Max(0, ViewportHeight))))
        {
            for (var i = first; i < rows.Count && i < first + visible; i++)
            {
                var y = RowY(i);
                if (y + rowHeight < ContentTop || y > ContentTop + ViewportHeight)
                {
                    continue;
                }

                DrawRow(context, model, rows[i], y, rowHeight);
                drawn++;
            }
        }

        LastRenderedRowCount = drawn;
        var loading = model.IsRootLoading || rows.Any(row => row.IsLoadingChildren);
        if (loading)
        {
            DrawLoadingCaption(context);
        }
    }

    private void DrawRow(DrawingContext context, HierarchyModel model, HierarchyRow row, double y, double rowHeight)
    {
        var isCursor = model.Cursor == row.Key;
        var isSelected = model.Selection.Contains(row.Key);
        var band = new Rect(0, y, Bounds.Width, rowHeight);
        if (isSelected)
        {
            context.FillRectangle(GraphTokens.Brush("Color.Surface.Sunken", Brushes.Transparent), band);
        }

        if (isCursor)
        {
            context.DrawRectangle(
                null,
                GraphTokens.Line(GraphTokens.Brush("Color.Border.Strong", Brushes.Gray)),
                band.Deflate(0.5));
        }

        var rtl = IsRtl;
        var textX = ChevronLeft(row.Depth) - RowPadding;
        var titleX = rtl ? textX - ChevronWidth : textX + ChevronWidth;
        var available = Math.Max(0, rtl ? titleX : Bounds.Width - titleX - RowPadding);
        var title = row.Title;
        _renderedTitles.Add(title);

        DrawIndentGuides(context, row.Depth, y, rowHeight, rtl);
        DrawChevron(context, row, y, rowHeight, rtl);
        DrawLock(context, row, titleX, y, rowHeight, rtl);

        var typeface = GraphTokens.Typeface(FontWeight.Normal);
        var bodySize = GraphTokens.Number("Font.Size.Body", 13);
        var captionSize = GraphTokens.Number("Font.Size.Caption", 11);
        var primary = row.Node.IsLocked
            ? GraphTokens.Brush("Color.Text.Disabled", Brushes.Gray)
            : GraphTokens.Brush("Color.Text.Primary", Brushes.Black);
        var secondary = GraphTokens.Brush("Color.Text.Secondary", Brushes.DimGray);

        var offset = row.Node.IsLocked ? 16 : 0;
        var titleText = GraphTokens.Text(title, typeface, bodySize, primary, rtl);
        var titleLeft = rtl ? titleX - available - offset : titleX + offset;
        context.DrawText(titleText, new Point(titleLeft, y + ((rowHeight - titleText.Height) / 2)));

        var detailWidth = 0d;
        if (row.Detail.Length > 0)
        {
            var detail = GraphTokens.Text(row.Detail, typeface, captionSize, secondary, rtl);
            detailWidth = detail.Width + RowPadding;
            var detailLeft = rtl ? RowPadding : Bounds.Width - detail.Width - RowPadding;
            context.DrawText(detail, new Point(detailLeft, y + ((rowHeight - detail.Height) / 2)));
        }

        if (row.IsLoadingChildren)
        {
            var loading = GraphTokens.Text("loading…", typeface, captionSize, secondary, rtl);
            var loadingLeft = rtl ? RowPadding + detailWidth : Bounds.Width - loading.Width - RowPadding - detailWidth;
            context.DrawText(loading, new Point(loadingLeft, y + ((rowHeight - loading.Height) / 2)));
        }

        var kindWidth = Math.Min(KindCaptionWidth, available / 2);
        if (kindWidth > 20)
        {
            var kind = GraphTokens.Text(row.Node.Kind, typeface, captionSize, secondary, rtl);
            if (kind.Width <= kindWidth)
            {
                var kindLeft = rtl ? titleX - available + kindWidth - kind.Width : Bounds.Width - kindWidth - RowPadding;
                context.DrawText(kind, new Point(kindLeft, y + ((rowHeight - kind.Height) / 2)));
            }
        }

        DrawDropIndicator(context, row, y, rowHeight);
    }

    private void DrawIndentGuides(DrawingContext context, int depth, double y, double rowHeight, bool rtl)
    {
        if (depth <= 0)
        {
            return;
        }

        var pen = GraphTokens.Line(GraphTokens.Brush("Color.Border.Default", Brushes.LightGray));
        for (var level = 0; level < depth; level++)
        {
            var x = rtl
                ? Bounds.Width - RowPadding - (level * IndentWidth) - (IndentWidth / 2)
                : RowPadding + (level * IndentWidth) + (IndentWidth / 2);
            context.DrawLine(pen, new Point(x, y), new Point(x, y + rowHeight));
        }
    }

    private void DrawChevron(DrawingContext context, HierarchyRow row, double y, double rowHeight, bool rtl)
    {
        if (!row.Node.HasChildren)
        {
            return;
        }

        var left = ChevronLeft(row.Depth);
        var centreY = y + (rowHeight / 2);
        var size = 4.5;
        var brush = GraphTokens.Brush("Color.Text.Secondary", Brushes.DimGray);
        var geometry = new StreamGeometry();
        using (var sink = geometry.Open())
        {
            if (row.IsExpanded)
            {
                // Down chevron.
                sink.BeginFigure(new Point(left + 3, centreY - size / 2), isFilled: true);
                sink.LineTo(new Point(left + ChevronWidth - 3, centreY - size / 2));
                sink.LineTo(new Point(left + (ChevronWidth / 2), centreY + size));
            }
            else
            {
                var direction = rtl ? -1 : 1;
                var apex = left + (ChevronWidth / 2) + ((ChevronWidth / 2 - 3) * direction);
                sink.BeginFigure(new Point(apex, centreY), isFilled: true);
                sink.LineTo(new Point(apex - (size * direction), centreY - size));
                sink.LineTo(new Point(apex - (size * direction), centreY + size));
            }

            sink.EndFigure(isClosed: true);
        }

        context.DrawGeometry(brush, null, geometry);
    }

    private static void DrawLock(DrawingContext context, HierarchyRow row, double x, double y, double rowHeight, bool rtl)
    {
        if (!row.Node.IsLocked)
        {
            return;
        }

        var pen = GraphTokens.Line(GraphTokens.Brush("Color.Text.Disabled", Brushes.Gray), 1.5);
        var bodyLeft = rtl ? x - 12 : x + 2;
        var centreY = y + (rowHeight / 2);
        context.DrawRectangle(
            GraphTokens.Brush("Color.Text.Disabled", Brushes.Gray),
            null,
            new Rect(bodyLeft, centreY - 1, 8, 6),
            1,
            1);
        context.DrawEllipse(null, pen, new Point(bodyLeft + 4, centreY - 1), 3, 3);
    }

    private void DrawDropIndicator(DrawingContext context, HierarchyRow row, double y, double rowHeight)
    {
        if (_reparent is not { IsDragging: true } reparent || reparent.HoverTarget != row.Key)
        {
            return;
        }

        var allowed = reparent.HoverDecision?.Allowed != false;
        var brush = GraphTokens.Brush(allowed ? "Color.Semantic.Success" : "Color.Semantic.Danger", Brushes.Gray);
        context.DrawLine(GraphTokens.Line(brush, 2), new Point(0, y + rowHeight - 1), new Point(Bounds.Width, y + rowHeight - 1));
        if (!allowed && reparent.LastRefusal is { Length: > 0 } reason)
        {
            var caption = GraphTokens.Text(reason, GraphTokens.Typeface(FontWeight.Medium), GraphTokens.Number("Font.Size.Caption", 11), brush, IsRtl);
            context.DrawText(caption, new Point(RowPadding, y + ((rowHeight - caption.Height) / 2)));
        }
    }

    private void DrawBreadcrumb(DrawingContext context, HierarchyModel model)
    {
        var band = new Rect(0, 0, Bounds.Width, BreadcrumbHeight);
        context.FillRectangle(GraphTokens.Brush("Color.Surface.Raised", Brushes.WhiteSmoke), band);
        context.DrawLine(
            GraphTokens.Line(GraphTokens.Brush("Color.Border.Default", Brushes.LightGray)),
            new Point(0, BreadcrumbHeight - 0.5),
            new Point(Bounds.Width, BreadcrumbHeight - 0.5));

        var trail = model.Breadcrumbs(model.Cursor);
        var text = trail.Count == 0
            ? "Hierarchy"
            : string.Join("  ›  ", trail.Select(step => step.Title));
        var typeface = GraphTokens.Typeface(FontWeight.Medium);
        var formatted = GraphTokens.Text(text, typeface, GraphTokens.Number("Font.Size.Caption", 11),
            GraphTokens.Brush("Color.Text.Secondary", Brushes.DimGray), IsRtl);
        using (context.PushClip(band))
        {
            var x = IsRtl ? Bounds.Width - formatted.Width - RowPadding : RowPadding;
            context.DrawText(formatted, new Point(x, (BreadcrumbHeight - formatted.Height) / 2));
        }
    }

    private void DrawCaption(DrawingContext context, string text, bool danger = false)
    {
        var typeface = GraphTokens.Typeface(FontWeight.Normal);
        var brush = GraphTokens.Brush(danger ? "Color.Semantic.Danger" : "Color.Text.Secondary", Brushes.DimGray);
        var formatted = GraphTokens.Text(text, typeface, GraphTokens.Number("Font.Size.Body", 13), brush, IsRtl);
        context.DrawText(formatted, new Point(RowPadding, ContentTop + RowPadding));
    }

    private void DrawLoadingCaption(DrawingContext context)
    {
        var typeface = GraphTokens.Typeface(FontWeight.Normal);
        var formatted = GraphTokens.Text(
            "Loading…",
            typeface,
            GraphTokens.Number("Font.Size.Caption", 11),
            GraphTokens.Brush("Color.Text.Disabled", Brushes.Gray),
            IsRtl);
        context.DrawText(formatted, new Point(RowPadding, ContentTop + ViewportHeight - formatted.Height - RowPadding));
    }
}
