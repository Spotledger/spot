using Avalonia;
using Avalonia.Media;

namespace Spotledger.Client.Graph;

/// <summary>Renderer half of <see cref="SpotLineageView"/>: orthogonal edge routes with
/// arrowheads, then the node cards (title, status chip, amount), the selection ring and the
/// locked placeholder. The whole drawing sits inside one pan/zoom transform, and below 45%
/// zoom the cards drop their text so a large graph stays legible as a shape.</summary>
public partial class SpotLineageView
{
    private const double CardRadius = 8;
    private const double CardPadding = 10;

    /// <inheritdoc />
    public override void Render(DrawingContext context)
    {
        ArgumentNullException.ThrowIfNull(context);
        var bounds = new Rect(0, 0, Bounds.Width, Bounds.Height);
        context.FillRectangle(GraphTokens.Brush("Color.Surface.Canvas", Brushes.White), bounds);

        if (Snapshot is null)
        {
            DrawCaption(context, "No lineage data.");
            return;
        }

        if (Layout is not { } layout)
        {
            DrawCaption(context, "Computing layout…");
            return;
        }

        if (layout.Nodes.Count == 0)
        {
            DrawCaption(context, "Nothing is connected to this record.");
            return;
        }

        var transform = Matrix.CreateScale(Zoom, Zoom) * Matrix.CreateTranslation(PanX, PanY);
        using (context.PushClip(bounds))
        using (context.PushTransform(transform))
        {
            DrawEdges(context, layout);
            foreach (var placement in layout.Nodes)
            {
                DrawCard(context, placement);
            }
        }
    }

    private static void DrawEdges(DrawingContext context, LineageLayout layout)
    {
        var pen = GraphTokens.Line(GraphTokens.Brush("Color.Border.Strong", Brushes.Gray), 1.5);
        var arrow = GraphTokens.Brush("Color.Border.Strong", Brushes.Gray);
        foreach (var route in layout.Edges)
        {
            for (var i = 1; i < route.Points.Count; i++)
            {
                var from = route.Points[i - 1];
                var to = route.Points[i];
                context.DrawLine(pen, new Point(from.X, from.Y), new Point(to.X, to.Y));
            }

            if (route.Points.Count >= 2)
            {
                var end = route.Points[^1];
                var previous = route.Points[^2];
                DrawArrow(context, arrow, previous, end);
            }
        }
    }

    private static void DrawArrow(DrawingContext context, IBrush brush, LineagePoint from, LineagePoint to)
    {
        var horizontal = Math.Abs(to.X - from.X) >= Math.Abs(to.Y - from.Y);
        var direction = horizontal ? Math.Sign(to.X - from.X) : Math.Sign(to.Y - from.Y);
        if (direction == 0)
        {
            return;
        }

        const double size = 5;
        var geometry = new StreamGeometry();
        using (var sink = geometry.Open())
        {
            sink.BeginFigure(new Point(to.X, to.Y), isFilled: true);
            if (horizontal)
            {
                sink.LineTo(new Point(to.X - (size * direction), to.Y - size));
                sink.LineTo(new Point(to.X - (size * direction), to.Y + size));
            }
            else
            {
                sink.LineTo(new Point(to.X - size, to.Y - (size * direction)));
                sink.LineTo(new Point(to.X + size, to.Y - (size * direction)));
            }

            sink.EndFigure(isClosed: true);
        }

        context.DrawGeometry(brush, null, geometry);
    }

    private void DrawCard(DrawingContext context, LineageNodePlacement placement)
    {
        var rect = new Rect(
            placement.Position.X,
            placement.Position.Y,
            LineageLayoutEngine.CardWidth,
            LineageLayoutEngine.CardHeight);
        var selected = SelectedKey == placement.Key;
        var isLocked = placement.Node.IsLocked;
        var fill = GraphTokens.Brush(selected ? "Color.Surface.Sunken" : "Color.Surface.Raised", Brushes.White);
        var border = GraphTokens.Brush(
            selected ? "Color.Brand.600" : "Color.Border.Default",
            selected ? Brushes.SteelBlue : Brushes.LightGray);
        var pen = isLocked
            ? new Pen(border, selected ? 2 : 1, new DashStyle([3, 3], 0))
            : GraphTokens.Line(border, selected ? 2 : 1);
        context.DrawRectangle(fill, pen, rect.Deflate(selected ? 1 : 0.5), CardRadius, CardRadius);

        if (Zoom < 0.45)
        {
            return;
        }

        var inner = rect.Deflate(CardPadding);
        var typeface = GraphTokens.Typeface(FontWeight.Medium);
        var titleSize = GraphTokens.Number("Font.Size.Body", 13);
        var captionSize = GraphTokens.Number("Font.Size.Caption", 11);
        var primary = isLocked
            ? GraphTokens.Brush("Color.Text.Disabled", Brushes.Gray)
            : GraphTokens.Brush("Color.Text.Primary", Brushes.Black);
        var secondary = GraphTokens.Brush("Color.Text.Secondary", Brushes.DimGray);

        using (context.PushClip(inner))
        {
            var title = GraphTokens.Text(placement.Node.DisplayTitle, typeface, titleSize, primary, rtl: false);
            var titleY = inner.Y;
            if (isLocked)
            {
                DrawLockMark(context, inner.X, inner.Y + 2);
                titleY = inner.Y;
            }

            context.DrawText(title, new Point(inner.X + (isLocked ? 16 : 0), titleY));

            var status = GraphTokens.Text(
                isLocked ? string.Empty : placement.Node.Status,
                typeface,
                captionSize,
                StatusBrush(placement.Node),
                rtl: false);
            context.DrawText(status, new Point(inner.X, inner.Y + titleSize + 4));

            var amount = GraphTokens.Text(
                isLocked ? string.Empty : placement.Node.Amount,
                GraphTokens.Typeface(FontWeight.Medium),
                captionSize,
                secondary,
                rtl: false);
            context.DrawText(amount, new Point(inner.Right - amount.Width, inner.Y + titleSize + 4));
        }
    }

    private static IBrush StatusBrush(LineageNode node)
    {
        if (node.IsLocked)
        {
            return GraphTokens.Brush("Color.Text.Disabled", Brushes.Gray);
        }

        return node.Status switch
        {
            "Posted" or "Paid" => GraphTokens.Brush("Color.Semantic.Success", Brushes.Green),
            "Cancelled" or "Failed" => GraphTokens.Brush("Color.Semantic.Danger", Brushes.Red),
            "Draft" => GraphTokens.Brush("Color.Text.Secondary", Brushes.DimGray),
            _ => GraphTokens.Brush("Color.Semantic.Info", Brushes.SteelBlue),
        };
    }

    private static void DrawLockMark(DrawingContext context, double x, double y)
    {
        var brush = GraphTokens.Brush("Color.Text.Disabled", Brushes.Gray);
        context.DrawRectangle(brush, null, new Rect(x + 2, y + 6, 8, 6), 1, 1);
        context.DrawEllipse(null, GraphTokens.Line(brush, 1.5), new Point(x + 6, y + 6), 3, 3);
    }

    private static void DrawCaption(DrawingContext context, string text)
    {
        var formatted = GraphTokens.Text(
            text,
            GraphTokens.Typeface(FontWeight.Normal),
            GraphTokens.Number("Font.Size.Body", 13),
            GraphTokens.Brush("Color.Text.Secondary", Brushes.DimGray),
            rtl: false);
        context.DrawText(formatted, new Point(12, 12));
    }
}
