using System;
using System.Collections.Generic;
using System.Linq;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Media;

namespace Spotledger.Client.FlowDesigner;

/// The render half of the canvas: the grid, the connections with their arrowheads, the node cards
/// with their kind, id and problem badge, the ghost connection while one is dragged, and the
/// minimap with its viewport frame. Everything but the minimap sits inside the pan/zoom transform,
/// so hit testing, dragging and drawing all agree on one coordinate space.
public partial class FlowCanvas
{

    private const double NodeRadius = 10;

    private static readonly Color PermittedColor = Color.FromRgb(0x2F, 0x8A, 0x4B);

    private static readonly Color RefusedColor = Color.FromRgb(0xB4, 0x3A, 0x2C);

    public FlowConnectionFeedback? GhostFeedback { get; private set; }

    protected override Size MeasureOverride(Size availableSize)
    {
        double width = double.IsInfinity(availableSize.Width) ? 480 : availableSize.Width;
        double height = double.IsInfinity(availableSize.Height) ? 320 : availableSize.Height;
        return new Size(width, height);
    }

    public override void Render(DrawingContext context)
    {
        ArgumentNullException.ThrowIfNull(context);
        Rect bounds = new(0, 0, Bounds.Width, Bounds.Height);
        context.FillRectangle(FlowTokens.Brush("Color.Surface.Canvas", Brushes.White), bounds);

        if (Document is null)
        {
            DrawCaption(context, FlowStrings.EmptyCanvas, bounds);
            return;
        }

        if (Document.Nodes.Count == 0)
        {
            DrawCaption(context, FlowStrings.EmptyCanvas, bounds);
            return;
        }

        DrawGrid(context, bounds);
        using (context.PushClip(bounds))
        using (context.PushTransform(Matrix.CreateScale(Zoom, Zoom) * Matrix.CreateTranslation(PanX, PanY)))
        {
            foreach (FlowEdge edge in Document.Edges)
            {
                DrawEdge(context, edge);
            }

            if (ConnectFrom is not null && ConnectPointer is { } pointer && Document.Node(ConnectFrom) is { } source)
            {
                DrawGhost(context, source, pointer);
            }

            foreach (FlowNode node in Document.Nodes)
            {
                DrawNode(context, node);
            }
        }

        DrawMinimap(context);
    }

    private void DrawGrid(DrawingContext context, Rect bounds)
    {
        double step = GridSize * Zoom;
        if (step < 6)
        {
            return;
        }

        Pen pen = FlowTokens.Line(FlowTokens.Brush("Color.Border.Subtle", Brushes.Gainsboro), 0.5);
        double startX = PanX % step;
        for (double x = startX; x < bounds.Width; x += step)
        {
            context.DrawLine(pen, new Point(x, 0), new Point(x, bounds.Height));
        }

        double startY = PanY % step;
        for (double y = startY; y < bounds.Height; y += step)
        {
            context.DrawLine(pen, new Point(0, y), new Point(bounds.Width, y));
        }
    }

    private void DrawEdge(DrawingContext context, FlowEdge edge)
    {
        if (Document?.Node(edge.From) is not { } from || Document.Node(edge.To) is not { } to)
        {
            return;
        }

        bool selected = Selection.ContainsEdge(edge.Key);
        IBrush brush = selected
            ? FlowTokens.Brush("Color.Accent.Primary", new SolidColorBrush(PermittedColor))
            : FlowTokens.Brush("Color.Border.Strong", Brushes.Gray);
        Pen pen = FlowTokens.Line(brush, selected ? 3 : 1.5);
        FlowPoint start = OutputPort(from);
        FlowPoint end = InputPort(to);
        double span = Math.Max(48, Math.Abs(end.X - start.X) / 2);
        StreamGeometry route = new();
        using (StreamGeometryContext geometry = route.Open())
        {
            geometry.BeginFigure(new Point(start.X, start.Y), isFilled: false);
            geometry.CubicBezierTo(
                new Point(start.X + span, start.Y),
                new Point(end.X - span, end.Y),
                new Point(end.X, end.Y));
            geometry.EndFigure(isClosed: false);
        }

        context.DrawGeometry(null, pen, route);
        DrawArrow(context, brush, start, end);
    }

    private static void DrawArrow(DrawingContext context, IBrush brush, FlowPoint start, FlowPoint end)
    {
        double angle = Math.Atan2(end.Y - start.Y, end.X - start.X);
        const double Size = 9;
        Point tip = new(end.X, end.Y);
        Point left = new(
            end.X - (Size * Math.Cos(angle - 0.42)),
            end.Y - (Size * Math.Sin(angle - 0.42)));
        Point right = new(
            end.X - (Size * Math.Cos(angle + 0.42)),
            end.Y - (Size * Math.Sin(angle + 0.42)));
        StreamGeometry head = new();
        using (StreamGeometryContext geometry = head.Open())
        {
            geometry.BeginFigure(left, isFilled: true);
            geometry.LineTo(tip);
            geometry.LineTo(right);
            geometry.EndFigure(isClosed: true);
        }

        context.DrawGeometry(brush, null, head);
    }

    private void DrawNode(DrawingContext context, FlowNode node)
    {
        Rect rect = WorldBounds(node);
        bool selected = Selection.Contains(node.Id);
        bool focused = string.Equals(FocusedNodeId, node.Id, StringComparison.Ordinal);
        bool readOnly = IsReadOnly;

        IBrush fill = FlowTokens.Brush("Color.Surface.Raised", Brushes.WhiteSmoke);
        IBrush border = selected
            ? FlowTokens.Brush("Color.Accent.Primary", new SolidColorBrush(PermittedColor))
            : FlowTokens.Brush("Color.Border.Default", Brushes.DimGray);
        Pen pen = FlowTokens.Line(border, selected ? 2.5 : 1);
        context.DrawRectangle(fill, pen, rect, NodeRadius, NodeRadius);

        string kindName = Catalogue.DisplayName(node.Kind);
        IBrush text = FlowTokens.Brush("Color.Text.Primary", Brushes.Black);
        IBrush muted = FlowTokens.Brush("Color.Text.Secondary", Brushes.DimGray);
        context.DrawText(FlowTokens.Text(kindName, FlowTokens.Typeface(FontWeight.SemiBold), 13, text), new Point(rect.X + 12, rect.Y + 12));
        context.DrawText(FlowTokens.Text(node.Id, FlowTokens.Typeface(FontWeight.Normal), 11, muted), new Point(rect.X + 12, rect.Y + 32));
        if (readOnly)
        {
            context.DrawText(FlowTokens.Text(FlowStrings.ReadOnly, FlowTokens.Typeface(FontWeight.Normal), 9, muted), new Point(rect.X + 12, rect.Y + 52));
        }
        else if (node.Terminal)
        {
            context.DrawText(FlowTokens.Text("End", FlowTokens.Typeface(FontWeight.Normal), 9, muted), new Point(rect.X + 12, rect.Y + 52));
        }

        if (focused && !selected)
        {
            context.DrawRectangle(null, FlowTokens.Line(border, 1, new DashStyle([3, 3], 0)), rect.Inflate(4), NodeRadius, NodeRadius);
        }

        DrawPorts(context, node, selected);
        DrawProblemBadge(context, node, rect);
    }

    private static void DrawPorts(DrawingContext context, FlowNode node, bool selected)
    {
        IBrush port = selected
            ? FlowTokens.Brush("Color.Accent.Primary", new SolidColorBrush(PermittedColor))
            : FlowTokens.Brush("Color.Border.Strong", Brushes.Gray);
        FlowPoint output = OutputPort(node);
        FlowPoint input = InputPort(node);
        context.DrawEllipse(port, null, new Point(output.X, output.Y), 4, 4);
        context.DrawEllipse(port, null, new Point(input.X, input.Y), 4, 4);
    }

    private void DrawProblemBadge(DrawingContext context, FlowNode node, Rect rect)
    {
        if (Validation is null)
        {
            return;
        }

        IReadOnlyList<FlowProblem> problems = Validation.ForNode(node.Id);
        if (problems.Count == 0)
        {
            return;
        }

        bool error = problems.Any(problem => problem.Severity == FlowProblemSeverity.Error);
        IBrush badge = error
            ? new SolidColorBrush(RefusedColor)
            : FlowTokens.Brush("Color.Status.Warning", Brushes.DarkOrange);
        Point centre = new(rect.Right - 14, rect.Y + 14);
        context.DrawEllipse(badge, null, centre, 8, 8);
        context.DrawText(
            FlowTokens.Text(problems.Count.ToString(System.Globalization.CultureInfo.InvariantCulture), FlowTokens.Typeface(FontWeight.Bold), 10, Brushes.White),
            new Point(centre.X - 3, centre.Y - 7));
    }

    private void DrawGhost(DrawingContext context, FlowNode source, FlowPoint pointer)
    {
        GhostFeedback = CheckConnection(source.Id, ConnectCandidate);
        Color colour = GhostFeedback.Valid ? PermittedColor : RefusedColor;
        Pen pen = FlowTokens.Line(new SolidColorBrush(colour), 2, new DashStyle([4, 3], 0));
        FlowPoint start = OutputPort(source);
        context.DrawLine(pen, new Point(start.X, start.Y), new Point(pointer.X, pointer.Y));
        context.DrawEllipse(new SolidColorBrush(colour), null, new Point(pointer.X, pointer.Y), 4, 4);
    }

    private void DrawMinimap(DrawingContext context)
    {
        if (Document is null || Minimap is not { } projection)
        {
            return;
        }

        Rect frame = MinimapRect;
        context.FillRectangle(FlowTokens.Brush("Color.Surface.Raised", Brushes.WhiteSmoke), frame, 6);
        context.DrawRectangle(null, FlowTokens.Line(FlowTokens.Brush("Color.Border.Default", Brushes.DimGray), 1), frame, 6);

        IBrush nodeBrush = FlowTokens.Brush("Color.Border.Strong", Brushes.Gray);
        foreach ((string id, FlowPoint topLeft, FlowPoint size) in projection.Nodes(Document))
        {
            bool selected = Selection.Contains(id);
            IBrush brush = selected ? new SolidColorBrush(PermittedColor) : nodeBrush;
            context.FillRectangle(
                brush,
                new Rect(frame.X + topLeft.X, frame.Y + topLeft.Y, Math.Max(size.X, 1), Math.Max(size.Y, 1)),
                2);
        }

        (FlowPoint viewTopLeft, FlowPoint viewSize) = projection.ViewportFrame(PanX, PanY, Zoom, Bounds.Width, Bounds.Height);
        Rect viewport = new(
            frame.X + viewTopLeft.X,
            frame.Y + viewTopLeft.Y,
            Math.Max(viewSize.X, 2),
            Math.Max(viewSize.Y, 2));
        context.DrawRectangle(null, FlowTokens.Line(FlowTokens.Brush("Color.Accent.Primary", Brushes.SteelBlue), 1.5), viewport.Intersect(frame), 2);
    }

    private static void DrawCaption(DrawingContext context, string text, Rect bounds)
    {
        FormattedText formatted = FlowTokens.Text(text, FlowTokens.Typeface(FontWeight.Normal), 13, FlowTokens.Brush("Color.Text.Secondary", Brushes.DimGray));
        context.DrawText(formatted, new Point(16, Math.Max(16, (bounds.Height - formatted.Height) / 2)));
    }
}
