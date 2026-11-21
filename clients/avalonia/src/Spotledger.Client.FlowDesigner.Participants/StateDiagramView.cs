using System;
using System.Globalization;
using System.Linq;
using Avalonia;
using Avalonia.Automation.Peers;
using Avalonia.Controls;
using Avalonia.Media;

namespace Spotledger.Client.FlowDesigner.Participants;

/// Draws a generated <see cref="StateDiagram"/>: one box per state in its layer, one arrow per order
/// rule, the acting role on the arrow, an unreachable state in the warning colour. The diagram is
/// read-only; the order table is where a person edits, and the view is regenerated from it.
public sealed class StateDiagramView : Control
{

    public const double BoxWidth = 124;

    public const double BoxHeight = 30;

    public const double ColumnStep = 168;

    public const double RowStep = 48;

    private const double Inset = 16;

    private StateDiagram _diagram = StateDiagram.From(ProcessContract.Empty("empty"));

    public StateDiagram Diagram
    {
        get => _diagram;
        set
        {
            ArgumentNullException.ThrowIfNull(value);
            _diagram = value;
            InvalidateMeasure();
            InvalidateVisual();
        }
    }

    /// The box of one state, in this control's coordinates.
    public static Rect BoxOf(StateNodeInfo node)
    {
        ArgumentNullException.ThrowIfNull(node);
        return new Rect(Inset + (node.Layer * ColumnStep), Inset + (node.Row * RowStep), BoxWidth, BoxHeight);
    }

    protected override Size MeasureOverride(Size availableSize)
    {
        int rows = _diagram.Nodes.Count == 0 ? 1 : _diagram.Nodes.Max(node => node.Row) + 1;
        return new Size(
            (2 * Inset) + (Math.Max(1, _diagram.LayerCount) * ColumnStep),
            (2 * Inset) + (rows * RowStep));
    }

    protected override AutomationPeer OnCreateAutomationPeer() => new StateDiagramPeer(this);

    public override void Render(DrawingContext context)
    {
        ArgumentNullException.ThrowIfNull(context);
        context.FillRectangle(Brushes.White, new Rect(Bounds.Size));
        foreach (StateEdgeInfo edge in _diagram.Edges)
        {
            if (_diagram.Node(edge.From) is not { } from || _diagram.Node(edge.To) is not { } to)
            {
                continue;
            }

            Rect a = BoxOf(from);
            Rect b = BoxOf(to);
            Point start = new(a.Right, a.Center.Y);
            Point end = new(b.Left, b.Center.Y);
            context.DrawLine(new Pen(Brushes.Gray, 1.5), start, end);
            context.DrawGeometry(Brushes.Gray, null, Arrow(end));
            string label = edge.By + (edge.Bound is { } bound ? " x" + bound.ToString(CultureInfo.InvariantCulture) : string.Empty);
            context.DrawText(Text(label, 9, Brushes.DimGray), new Point(((start.X + end.X) / 2) - 12, ((start.Y + end.Y) / 2) - 14));
        }

        foreach (StateNodeInfo node in _diagram.Nodes)
        {
            Color color = !node.Reachable
                ? ParticipantCanvas.CompensableColor
                : node.Kind switch
                {
                    StateKind.Start => ParticipantCanvas.OtherLaneColor,
                    StateKind.Outcome => ParticipantCanvas.ReversibleColor,
                    _ => ParticipantCanvas.OwnLaneColor,
                };
            Rect box = BoxOf(node);
            context.DrawRectangle(new SolidColorBrush(color, 0.15), new Pen(new SolidColorBrush(color), 2), box, 6, 6);
            context.DrawText(Text(node.Label, 11, Brushes.Black), new Point(box.X + 6, box.Y + 7));
        }
    }

    private static StreamGeometry Arrow(Point tip)
    {
        StreamGeometry geometry = new();
        using StreamGeometryContext context = geometry.Open();
        context.BeginFigure(tip, isFilled: true);
        context.LineTo(new Point(tip.X - 8, tip.Y - 4));
        context.LineTo(new Point(tip.X - 8, tip.Y + 4));
        context.EndFigure(isClosed: true);
        return geometry;
    }

    private static FormattedText Text(string value, double size, IBrush brush) =>
        new(value, CultureInfo.CurrentCulture, FlowDirection.LeftToRight, new Typeface(FontFamily.Default.Name), size, brush);

    private sealed class StateDiagramPeer(StateDiagramView owner) : ControlAutomationPeer(owner)
    {

        protected override AutomationControlType GetAutomationControlTypeCore() => AutomationControlType.Pane;

        protected override string GetClassNameCore() => nameof(StateDiagramView);

        protected override string GetNameCore() => "Conversation state diagram";

        // The diagram as text, so a screen reader reads the same arrows a sighted person follows.
        protected override string? GetHelpTextCore() => ((StateDiagramView)Owner).Diagram.ToText();

        protected override bool IsControlElementCore() => true;
    }
}
