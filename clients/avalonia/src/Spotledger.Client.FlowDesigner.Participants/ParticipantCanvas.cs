using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using Avalonia;
using Avalonia.Media;

namespace Spotledger.Client.FlowDesigner.Participants;

/// The designer's canvas with the participant view painted over it: the other organisations as
/// labelled lanes whose insides are hidden, the process's own lane highlighted, the contract's
/// messages as the only lines that reach another lane, per-step effect badges, commit-point regions
/// shaded, backward edges typed Undo or Rewind, and the server's problems on the edge they are
/// about. The canvas keeps the designer's editing behaviour untouched — it is the same canvas, so
/// the keyboard, the palette and the undo stack are the base designer's — and everything here is
/// painted over what the base draws.
public sealed class ParticipantCanvas : FlowCanvas
{

    public static readonly Color OwnLaneColor = Color.FromRgb(0x25, 0x63, 0xEB);

    public static readonly Color OtherLaneColor = Color.FromRgb(0x6B, 0x72, 0x80);

    public static readonly Color MessageColor = Color.FromRgb(0x7C, 0x3A, 0xED);

    public static readonly Color RegionColor = Color.FromRgb(0xF5, 0x9E, 0x0B);

    public static readonly Color UndoColor = Color.FromRgb(0xDB, 0x27, 0x77);

    public static readonly Color RewindColor = Color.FromRgb(0x0E, 0x74, 0x90);

    public static readonly Color ReversibleColor = Color.FromRgb(0x2F, 0x8A, 0x4B);

    public static readonly Color CompensableColor = Color.FromRgb(0xB7, 0x79, 0x1F);

    public static readonly Color IrreversibleColor = Color.FromRgb(0x9F, 0x12, 0x39);

    private ParticipantSession? _session;

    /// The lanes to draw. A host that keeps its own state binds this instead of a session.
    public ParticipantLanes? Lanes { get; set; }

    /// The effect badges to draw.
    public EffectBadges Effects { get; set; } = EffectBadges.Empty;

    /// The problems drawn beyond the base canvas's own: conformance and the designer's hints.
    public Func<IReadOnlyList<FlowProblem>>? ExtraProblemSource { get; set; }

    /// Whether the held regions are shaded.
    public bool ShowCommitRegions { get; set; } = true;

    /// The session whose state drives the participant view. Setting it also repaints on every change.
    public ParticipantSession? Session
    {
        get => _session;
        set
        {
            if (ReferenceEquals(_session, value))
            {
                return;
            }

            if (_session is not null)
            {
                _session.Changed -= OnSessionChanged;
            }

            _session = value;
            if (_session is not null)
            {
                _session.Changed += OnSessionChanged;
                Effects = _session.Effects;
                Lanes = _session.Lanes;
                ExtraProblemSource = () => _session.ExtraProblems;
            }

            InvalidateVisual();
        }
    }

    /// The bands drawn around the steps: the own lane, then the other lanes alternately above and
    /// below it. Empty when there are no lanes or no steps.
    public IReadOnlyList<LaneBand> Bands
    {
        get
        {
            if (Lanes is null || Document is not { Nodes.Count: > 0 } document)
            {
                return [];
            }

            return Lanes.Bands(StepBounds(document));
        }
    }

    /// The message lines to draw, in step order.
    public IReadOnlyList<LaneMessageLink> MessageLinks =>
        Lanes is null || Document is null ? [] : Lanes.Links(Document);

    /// Every problem the canvas shows: the server's, then the extra ones.
    public IReadOnlyList<FlowProblem> AllProblems =>
        [.. Problems, .. ExtraProblemSource?.Invoke() ?? []];

    /// The problems about one edge, so an edge can say what is wrong with it.
    public IReadOnlyList<FlowProblem> ProblemsOnEdge(FlowEdgeKey key) =>
        [.. AllProblems.Where(problem => problem.Edge is { } edge && edge == key)];

    /// The problems about one step.
    public IReadOnlyList<FlowProblem> ProblemsOnNode(string nodeId)
    {
        ArgumentNullException.ThrowIfNull(nodeId);
        return [.. AllProblems.Where(problem => string.Equals(problem.NodeId, nodeId, StringComparison.Ordinal))];
    }

    /// The steps inside some commit point's region.
    public IReadOnlySet<string> HeldNodes => Document is null ? new HashSet<string>() : CommitRegions.Held(Document);

    /// Fits the steps and the lanes in the window, which the base canvas's fit does not know about.
    public void FitToLanes()
    {
        if (Document is not { Nodes.Count: > 0 } document || Bounds.Width <= 0 || Bounds.Height <= 0)
        {
            return;
        }

        Rect all = StepBounds(document);
        foreach (LaneBand band in Bands)
        {
            all = all.Union(band.Bounds);
        }

        double zoom = Math.Clamp(Math.Min((Bounds.Width - 32) / Math.Max(all.Width, 1), (Bounds.Height - 32) / Math.Max(all.Height, 1)), 0.25, 3);
        SetZoom(zoom, new Point(0, 0));
        double wantX = ((Bounds.Width - (all.Width * Zoom)) / 2) - (all.X * Zoom);
        double wantY = ((Bounds.Height - (all.Height * Zoom)) / 2) - (all.Y * Zoom);
        PanBy(wantX - PanX, wantY - PanY);
    }

    public override void Render(DrawingContext context)
    {
        ArgumentNullException.ThrowIfNull(context);
        base.Render(context);
        if (Document is not { Nodes.Count: > 0 } document)
        {
            return;
        }

        Rect bounds = new(0, 0, Bounds.Width, Bounds.Height);
        using (context.PushClip(bounds))
        using (context.PushTransform(Matrix.CreateScale(Zoom, Zoom) * Matrix.CreateTranslation(PanX, PanY)))
        {
            IReadOnlyList<LaneBand> bands = Bands;
            foreach (LaneBand band in bands)
            {
                DrawBand(context, band);
            }

            if (ShowCommitRegions)
            {
                DrawRegions(context, document);
            }

            DrawMessageLinks(context, document, bands);
            DrawEdgeAnnotations(context, document);
            foreach (FlowNode node in document.Nodes)
            {
                DrawNodeAnnotations(context, node);
            }
        }
    }

    private static Rect StepBounds(FlowDocument document)
    {
        Rect all = WorldBounds(document.Nodes[0]);
        foreach (FlowNode node in document.Nodes.Skip(1))
        {
            all = all.Union(WorldBounds(node));
        }

        return all;
    }

    private static void DrawRegions(DrawingContext context, FlowDocument document)
    {
        foreach (KeyValuePair<string, IReadOnlySet<string>> region in CommitRegions.Of(document))
        {
            if (region.Value.Count == 0)
            {
                continue;
            }

            Rect union = default;
            bool first = true;
            foreach (string id in region.Value)
            {
                if (document.Node(id) is not { } node)
                {
                    continue;
                }

                Rect rect = WorldBounds(node);
                union = first ? rect : union.Union(rect);
                first = false;
            }

            if (first)
            {
                continue;
            }

            Rect shade = union.Inflate(10);
            context.DrawRectangle(
                new SolidColorBrush(RegionColor, 0.20),
                new Pen(new SolidColorBrush(RegionColor), 2.5, new DashStyle([5, 2], 0)),
                shade,
                8,
                8);
            DrawLabel(context, ParticipantStrings.CommitRegionLabel(region.Key), new Point(shade.X + 6, shade.Y + 2), 10, Brushes.DarkGoldenrod);
        }
    }

    private static void DrawBand(DrawingContext context, LaneBand band)
    {
        Color color = band.IsOwn ? OwnLaneColor : OtherLaneColor;
        Pen pen = band.IsOwn
            ? new Pen(new SolidColorBrush(color), 2)
            : new Pen(new SolidColorBrush(color), 1.5, new DashStyle([6, 4], 0));
        context.DrawRectangle(new SolidColorBrush(color, band.IsOwn ? 0.10 : 0.14), pen, band.Bounds, 6, 6);
        LaneInfo lane = new(band.Role, band.Kind, band.IsOwn);
        DrawLabel(
            context,
            ParticipantStrings.LaneLabel(lane),
            new Point(band.Bounds.X + 8, band.Bounds.Y + 4),
            band.IsOwn ? 12 : 11,
            new SolidColorBrush(band.IsOwn ? OwnLaneColor : Color.FromRgb(0x37, 0x41, 0x51)),
            band.IsOwn ? FontWeight.Bold : FontWeight.Normal);
    }

    private void DrawMessageLinks(DrawingContext context, FlowDocument document, IReadOnlyList<LaneBand> bands)
    {
        if (Lanes is null)
        {
            return;
        }

        foreach (LaneMessageLink link in Lanes.Links(document))
        {
            if (document.Node(link.NodeId) is not { } node)
            {
                continue;
            }

            LaneBand? band = bands.FirstOrDefault(candidate => !candidate.IsOwn && string.Equals(candidate.Role, link.Role, StringComparison.Ordinal));
            Rect step = WorldBounds(node);
            Color color = link.Known ? MessageColor : UndoColor;
            Pen pen = new(new SolidColorBrush(color), 2, new DashStyle([4, 3], 0));
            if (band is null)
            {
                // The far end is not a lane of the contract: mark the step instead of inventing one.
                context.DrawLine(pen, new Point(step.Center.X, step.Top), new Point(step.Center.X, step.Top - 18));
                DrawLabel(context, ParticipantStrings.MessageLinkLabel(link), new Point(step.Center.X + 4, step.Top - 30), 10, new SolidColorBrush(color));
                continue;
            }

            Point port = ParticipantLanes.Port(band, step);
            Point edge = new(port.X, band.Above ? step.Top : step.Bottom);
            context.DrawLine(pen, edge, port);
            double arrow = link.Direction == MessageDirection.Out ? 1 : -1;
            Point head = link.Direction == MessageDirection.Out ? port : edge;
            double dir = (band.Above ? -1 : 1) * arrow;
            context.DrawGeometry(
                new SolidColorBrush(color),
                null,
                Triangle(head, dir));
            DrawLabel(
                context,
                ParticipantStrings.MessageLinkLabel(link),
                new Point(port.X + 6, (edge.Y + port.Y) / 2 - 6),
                10,
                new SolidColorBrush(color));
        }
    }

    private static StreamGeometry Triangle(Point tip, double direction)
    {
        StreamGeometry geometry = new();
        using StreamGeometryContext context = geometry.Open();
        context.BeginFigure(tip, isFilled: true);
        context.LineTo(new Point(tip.X - 5, tip.Y - (9 * direction)));
        context.LineTo(new Point(tip.X + 5, tip.Y - (9 * direction)));
        context.EndFigure(isClosed: true);
        return geometry;
    }

    private void DrawEdgeAnnotations(DrawingContext context, FlowDocument document)
    {
        foreach (FlowEdge edge in document.Edges)
        {
            if (document.Node(edge.From) is not { } from || document.Node(edge.To) is not { } to)
            {
                continue;
            }

            FlowPoint start = OutputPort(from);
            FlowPoint end = InputPort(to);
            Point mid = new((start.X + end.X) / 2, (start.Y + end.Y) / 2);
            EdgeMode mode = EdgeModes.Of(edge);
            double y = mid.Y - 9;
            if (mode != EdgeMode.Forward)
            {
                DrawPill(context, mode.Word(), new Point(mid.X, y), mode == EdgeMode.Undo ? UndoColor : RewindColor);
                y += 16;
            }

            foreach (FlowProblem problem in ProblemsOnEdge(edge.Key).Take(2))
            {
                DrawPill(context, problem.Code, new Point(mid.X, y), SeverityColor(problem.Severity));
                y += 16;
            }
        }
    }

    private void DrawNodeAnnotations(DrawingContext context, FlowNode node)
    {
        Rect card = WorldBounds(node);
        if (Effects.Badge(node.Id) is { } badge)
        {
            Color color = badge.Worst switch
            {
                EffectReversibility.Reversible => ReversibleColor,
                EffectReversibility.Compensable => CompensableColor,
                _ => IrreversibleColor,
            };
            DrawPill(context, badge.Text, new Point(card.Right - 4, card.Top - 2), color, alignRight: true);
        }

        FlowProblem? worst = ProblemsOnNode(node.Id).OrderBy(problem => problem.Severity).FirstOrDefault();
        if (worst is not null)
        {
            context.DrawEllipse(new SolidColorBrush(SeverityColor(worst.Severity)), null, new Point(card.X + 6, card.Y + 6), 4, 4);
        }
    }

    private static Color SeverityColor(FlowProblemSeverity severity) => severity switch
    {
        FlowProblemSeverity.Error => UndoColor,
        FlowProblemSeverity.Warning => CompensableColor,
        _ => OtherLaneColor,
    };

    private static void DrawPill(DrawingContext context, string text, Point at, Color color, bool alignRight = false)
    {
        FormattedText formatted = Text(text, 9, Brushes.White, FontWeight.SemiBold);
        double width = Math.Max(28, formatted.Width + 12);
        double x = alignRight ? at.X - width : at.X - (width / 2);
        Rect pill = new(x, at.Y, width, 14);
        context.DrawRectangle(new SolidColorBrush(color), null, pill, 7, 7);
        context.DrawText(formatted, new Point(pill.X + 6, pill.Y + 1));
    }

    private static void DrawLabel(DrawingContext context, string text, Point at, double size, IBrush brush, FontWeight weight = FontWeight.Normal) =>
        context.DrawText(Text(text, size, brush, weight), at);

    private static FormattedText Text(string value, double size, IBrush brush, FontWeight weight) =>
        new(
            value,
            CultureInfo.CurrentCulture,
            FlowDirection.LeftToRight,
            new Typeface(FontFamily.Default.Name, weight: weight),
            size,
            brush);

    private void OnSessionChanged(object? sender, EventArgs e)
    {
        if (_session is not null)
        {
            Effects = _session.Effects;
            Lanes = _session.Lanes;
        }

        InvalidateVisual();
    }
}
