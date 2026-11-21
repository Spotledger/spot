using System;
using System.Collections.Generic;
using System.Globalization;
using Avalonia;
using Avalonia.Media;
using Avalonia.Media.TextFormatting;
using Spotledger.Client.FlowDesigner;

namespace Spotledger.Client.FlowDesigner.Run;

/// The designer's canvas with a node's run state on it: a pill on the card in the colour of the
/// state, and a dashed outline around a node that took the error branch. The canvas keeps the
/// designer's editing behaviour untouched — a test run never changes the document — so the badges
/// are painted over the card and nothing else is.
public sealed class RunCanvas : FlowCanvas
{

    private static readonly Color PendingColor = Color.FromRgb(0x6B, 0x72, 0x80);

    private static readonly Color RunningColor = Color.FromRgb(0x25, 0x63, 0xEB);

    private static readonly Color SucceededColor = Color.FromRgb(0x2F, 0x8A, 0x4B);

    private static readonly Color FailedColor = Color.FromRgb(0xB4, 0x3A, 0x2C);

    private static readonly Color WaitingColor = Color.FromRgb(0xB7, 0x79, 0x1F);

    private static readonly Color SkippedColor = Color.FromRgb(0x9C, 0xA3, 0xAF);

    private static readonly Color StoppedColor = Color.FromRgb(0x4B, 0x55, 0x63);

    private FlowRunSession? _session;

    /// Where a badge comes from. A host that keeps its own run state binds this instead of a
    /// session; the canvas itself never asks the server anything.
    public Func<string, RunNodeBadge?>? BadgeSource { get; set; }

    /// The session whose snapshot drives the badges. Setting it also repaints on every change.
    public FlowRunSession? Session
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
            }

            BadgeSource = value is null ? null : value.Badge;
            InvalidateVisual();
        }
    }

    /// Every badge the canvas would paint, in node order.
    public IReadOnlyList<RunNodeBadge> Badges
    {
        get
        {
            List<RunNodeBadge> badges = [];
            if (Document is null || BadgeSource is null)
            {
                return badges;
            }

            foreach (FlowNode node in Document.Nodes)
            {
                if (BadgeSource(node.Id) is { } badge)
                {
                    badges.Add(badge);
                }
            }

            return badges;
        }
    }

    /// The word on one node's badge, or null when it has none.
    public string? BadgeText(string nodeId)
    {
        ArgumentNullException.ThrowIfNull(nodeId);
        return BadgeSource?.Invoke(nodeId)?.Text;
    }

    /// Whether a node is on the error branch of the current run.
    public bool IsErrorBranch(string nodeId)
    {
        ArgumentNullException.ThrowIfNull(nodeId);
        return _session?.Snapshot?.Node(nodeId)?.ErrorBranch ?? false;
    }

    public override void Render(DrawingContext context)
    {
        ArgumentNullException.ThrowIfNull(context);
        base.Render(context);
        if (Document is not { Nodes.Count: > 0 } || (BadgeSource is null && _session is null))
        {
            return;
        }

        Rect bounds = new(0, 0, Bounds.Width, Bounds.Height);
        using (context.PushClip(bounds))
        using (context.PushTransform(Matrix.CreateScale(Zoom, Zoom) * Matrix.CreateTranslation(PanX, PanY)))
        {
            foreach (FlowNode node in Document.Nodes)
            {
                Rect rect = WorldBounds(node);
                if (IsErrorBranch(node.Id))
                {
                    Pen pen = new(new SolidColorBrush(FailedColor), 2, new DashStyle([4, 3], 0));
                    context.DrawRectangle(null, pen, rect.Inflate(3), 12, 12);
                }

                if (BadgeSource?.Invoke(node.Id) is { } badge)
                {
                    DrawBadge(context, rect, badge);
                }
            }
        }
    }

    private void OnSessionChanged(object? sender, EventArgs e) => InvalidateVisual();

    private static void DrawBadge(DrawingContext context, Rect card, RunNodeBadge badge)
    {
        FormattedText text = Text(badge.Text);
        double width = Math.Max(28, text.Width + 12);
        Rect pill = new(card.X + 8, card.Bottom - 20, width, 14);
        context.FillRectangle(new SolidColorBrush(ColorFor(badge.Status)), pill, 7);
        context.DrawText(text, new Point(pill.X + 6, pill.Y + 1));
    }

    private static FormattedText Text(string value) =>
        new(
            value,
            CultureInfo.CurrentCulture,
            FlowDirection.LeftToRight,
            new Typeface(FontFamily.Default.Name, weight: FontWeight.SemiBold),
            9,
            Brushes.White);

    private static Color ColorFor(RunNodeStatus status) => status switch
    {
        RunNodeStatus.Running => RunningColor,
        RunNodeStatus.Succeeded => SucceededColor,
        RunNodeStatus.Failed => FailedColor,
        RunNodeStatus.Waiting => WaitingColor,
        RunNodeStatus.Skipped => SkippedColor,
        RunNodeStatus.Stopped => StoppedColor,
        _ => PendingColor,
    };
}
