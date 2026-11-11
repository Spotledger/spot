using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;

namespace Spotledger.Client.Graph;

/// <summary>
/// The BOM and lineage view: a layered left-to-right node-link drawing of a
/// <see cref="GraphSnapshot"/>. Layout is pure and deterministic
/// (<see cref="LineageLayoutEngine"/>) and runs off the UI thread for a graph over 200 nodes;
/// the control paints cards from the computed positions, pans and zooms the whole drawing,
/// and opens a document through the workspace navigation seam. It owns presentation only:
/// titles, statuses and amounts arrive formatted from the list model.
/// </summary>
public partial class SpotLineageView : Control, IDisposable
{
    /// <summary>Identifies the <see cref="Snapshot"/> property.</summary>
    public static readonly StyledProperty<GraphSnapshot?> SnapshotProperty =
        AvaloniaProperty.Register<SpotLineageView, GraphSnapshot?>(nameof(Snapshot));

    /// <summary>Identifies the <see cref="Navigation"/> property.</summary>
    public static readonly StyledProperty<IWorkspaceNavigation?> NavigationProperty =
        AvaloniaProperty.Register<SpotLineageView, IWorkspaceNavigation?>(nameof(Navigation));

    /// <summary>Identifies the <see cref="SelectedKey"/> property.</summary>
    public static readonly StyledProperty<GraphNodeKey?> SelectedKeyProperty =
        AvaloniaProperty.Register<SpotLineageView, GraphNodeKey?>(nameof(SelectedKey));

    /// <summary>Identifies the <see cref="Zoom"/> property.</summary>
    public static readonly StyledProperty<double> ZoomProperty =
        AvaloniaProperty.Register<SpotLineageView, double>(nameof(Zoom), 1d);

    /// <summary>Identifies the <see cref="PanX"/> property.</summary>
    public static readonly StyledProperty<double> PanXProperty =
        AvaloniaProperty.Register<SpotLineageView, double>(nameof(PanX));

    /// <summary>Identifies the <see cref="PanY"/> property.</summary>
    public static readonly StyledProperty<double> PanYProperty =
        AvaloniaProperty.Register<SpotLineageView, double>(nameof(PanY));

    private CancellationTokenSource _layoutCancellation = new();
    private bool _panning;
    private Point _panStart;
    private double _panOriginX;
    private double _panOriginY;

    /// <summary>Creates the view: focusable, clipped, with a stable automation id.</summary>
    public SpotLineageView()
    {
        Focusable = true;
        ClipToBounds = true;
        AutomationProperties.SetAutomationId(this, "graph-lineage");
        AutomationProperties.SetName(this, "Spotledger lineage view");
    }

    /// <summary>The graph slice to draw.</summary>
    public GraphSnapshot? Snapshot
    {
        get => GetValue(SnapshotProperty);
        set => SetValue(SnapshotProperty, value);
    }

    /// <summary>The workspace navigation seam a click opens documents through.</summary>
    public IWorkspaceNavigation? Navigation
    {
        get => GetValue(NavigationProperty);
        set => SetValue(NavigationProperty, value);
    }

    /// <summary>The selected node's key.</summary>
    public GraphNodeKey? SelectedKey
    {
        get => GetValue(SelectedKeyProperty);
        set => SetValue(SelectedKeyProperty, value);
    }

    /// <summary>The zoom factor (0.25 to 3).</summary>
    public double Zoom
    {
        get => GetValue(ZoomProperty);
        set => SetValue(ZoomProperty, Math.Clamp(value, 0.25, 3));
    }

    /// <summary>The horizontal pan in screen pixels.</summary>
    public double PanX
    {
        get => GetValue(PanXProperty);
        set => SetValue(PanXProperty, value);
    }

    /// <summary>The vertical pan in screen pixels.</summary>
    public double PanY
    {
        get => GetValue(PanYProperty);
        set => SetValue(PanYProperty, value);
    }

    /// <summary>The computed layout, or null while it is being computed.</summary>
    public LineageLayout? Layout { get; private set; }

    internal Task PendingLayout { get; private set; } = Task.CompletedTask;

    internal int LayoutPassCount { get; private set; }

    internal int LastLayoutThreadId => Layout?.ComputedOnThreadId ?? 0;

    /// <summary>Raised when a node is activated (double-click, Enter or automation invoke).</summary>
    public event EventHandler<LineageNodeActivatedEventArgs>? NodeActivated;

    /// <summary>Raised when the selection changed.</summary>
    public event EventHandler? SelectionChanged;

    /// <content>Static constructor: property invalidation.</content>
    static SpotLineageView()
    {
        AffectsRender<SpotLineageView>(SnapshotProperty, SelectedKeyProperty, ZoomProperty, PanXProperty, PanYProperty);
        AffectsMeasure<SpotLineageView>(SnapshotProperty);
    }

    /// <inheritdoc />
    protected override Size MeasureOverride(Size availableSize)
    {
        var width = double.IsInfinity(availableSize.Width) ? 640 : availableSize.Width;
        var height = double.IsInfinity(availableSize.Height) ? 360 : availableSize.Height;
        return new Size(width, height);
    }

    /// <inheritdoc />
    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs change)
    {
        ArgumentNullException.ThrowIfNull(change);
        base.OnPropertyChanged(change);
        if (change.Property == SnapshotProperty)
        {
            StartLayout();
        }
        else if (change.Property == SelectedKeyProperty)
        {
            SelectionChanged?.Invoke(this, EventArgs.Empty);
        }
    }

    /// <summary>Cancels the layout in flight.</summary>
    public void Dispose()
    {
        GC.SuppressFinalize(this);
        _layoutCancellation.Cancel();
        _layoutCancellation.Dispose();
    }

    /// <summary>Zooms around an anchor point (default: the view's centre), keeping the layout
    /// point under the anchor where it was.</summary>
    /// <param name="zoom">The new zoom (clamped).</param>
    /// <param name="anchor">The screen anchor; null means the centre.</param>
    public void SetZoom(double zoom, Point? anchor = null)
    {
        var target = Math.Clamp(zoom, 0.25, 3);
        var point = anchor ?? new Point(Bounds.Width / 2, Bounds.Height / 2);
        var current = Zoom <= 0 ? 1 : Zoom;
        var worldX = (point.X - PanX) / current;
        var worldY = (point.Y - PanY) / current;
        Zoom = target;
        PanX = point.X - (worldX * target);
        PanY = point.Y - (worldY * target);
    }

    /// <summary>Pans by a screen-space delta.</summary>
    /// <param name="dx">Horizontal delta.</param>
    /// <param name="dy">Vertical delta.</param>
    public void PanBy(double dx, double dy)
    {
        PanX += dx;
        PanY += dy;
    }

    /// <summary>Fits the whole layout into the viewport (with a small margin).</summary>
    public void ZoomToFit()
    {
        if (Layout is not { } layout || layout.Width <= 0 || layout.Height <= 0
            || Bounds.Width <= 0 || Bounds.Height <= 0)
        {
            return;
        }

        var scale = Math.Min(Bounds.Width / layout.Width, Bounds.Height / layout.Height);
        var target = Math.Clamp(scale * 0.95, 0.25, 3);
        Zoom = target;
        PanX = (Bounds.Width - (layout.Width * target)) / 2;
        PanY = (Bounds.Height - (layout.Height * target)) / 2;
    }

    /// <summary>The card under a screen point, or null.</summary>
    /// <param name="point">The point in control coordinates.</param>
    public LineageNodePlacement? HitTest(Point point)
    {
        if (Layout is not { } layout || Zoom <= 0)
        {
            return null;
        }

        var worldX = (point.X - PanX) / Zoom;
        var worldY = (point.Y - PanY) / Zoom;
        foreach (var placement in layout.Nodes)
        {
            if (worldX >= placement.Position.X
                && worldX <= placement.Position.X + LineageLayoutEngine.CardWidth
                && worldY >= placement.Position.Y
                && worldY <= placement.Position.Y + LineageLayoutEngine.CardHeight)
            {
                return placement;
            }
        }

        return null;
    }

    /// <summary>Selects a node (null clears the selection).</summary>
    /// <param name="key">The node.</param>
    public void Select(GraphNodeKey? key)
    {
        if (SelectedKey == key)
        {
            return;
        }

        SelectedKey = key;
        InvalidateVisual();
    }

    /// <summary>Opens a node through the navigation seam and raises the activation event.</summary>
    /// <param name="placement">The card.</param>
    public void Activate(LineageNodePlacement placement)
    {
        ArgumentNullException.ThrowIfNull(placement);
        Select(placement.Key);
        NodeActivated?.Invoke(this, new LineageNodeActivatedEventArgs(placement.Node));
        if (Navigation is { } navigation)
        {
            _ = OpenAsync(navigation, placement.Key);
        }
    }

    /// <summary>Moves the selection by one card in layout order (keyboard navigation).</summary>
    /// <param name="delta">Cards to move (negative moves back).</param>
    public void MoveSelection(int delta)
    {
        if (Layout is not { Nodes.Count: > 0 } layout)
        {
            return;
        }

        var index = 0;
        for (var i = 0; i < layout.Nodes.Count; i++)
        {
            if (layout.Nodes[i].Key == SelectedKey)
            {
                index = i;
                break;
            }
        }

        var next = Math.Clamp(index + delta, 0, layout.Nodes.Count - 1);
        Select(layout.Nodes[next].Key);
        EnsureVisible(layout.Nodes[next]);
    }

    /// <summary>Pans so a card is inside the viewport.</summary>
    /// <param name="placement">The card.</param>
    public void EnsureVisible(LineageNodePlacement placement)
    {
        ArgumentNullException.ThrowIfNull(placement);
        var left = (placement.Position.X * Zoom) + PanX;
        var top = (placement.Position.Y * Zoom) + PanY;
        var right = left + (LineageLayoutEngine.CardWidth * Zoom);
        var bottom = top + (LineageLayoutEngine.CardHeight * Zoom);
        if (left < 0)
        {
            PanX -= left - 24;
        }
        else if (right > Bounds.Width)
        {
            PanX -= right - Bounds.Width + 24;
        }

        if (top < 0)
        {
            PanY -= top - 24;
        }
        else if (bottom > Bounds.Height)
        {
            PanY -= bottom - Bounds.Height + 24;
        }

        InvalidateVisual();
    }

    private void StartLayout()
    {
        _layoutCancellation.Cancel();
        _layoutCancellation.Dispose();
        _layoutCancellation = new CancellationTokenSource();
        Layout = null;
        InvalidateVisual();
        InvalidateMeasure();
        PendingLayout = RunLayoutAsync(Snapshot ?? GraphSnapshot.Empty, _layoutCancellation.Token);
    }

    private async Task RunLayoutAsync(GraphSnapshot snapshot, CancellationToken cancellationToken)
    {
        try
        {
            var layout = await LineageLayoutEngine.ComputeAsync(snapshot, cancellationToken).ConfigureAwait(true);
            if (cancellationToken.IsCancellationRequested)
            {
                return;
            }

            Layout = layout;
            LayoutPassCount++;
            ZoomToFit();
            InvalidateMeasure();
            InvalidateVisual();
        }
        catch (OperationCanceledException)
        {
            // A newer snapshot superseded this layout.
        }
    }

    private static async Task OpenAsync(IWorkspaceNavigation navigation, GraphNodeKey key)
    {
        try
        {
            await navigation.OpenDocumentAsync(key, CancellationToken.None).ConfigureAwait(true);
        }
        catch (Exception error) when (error is not OutOfMemoryException)
        {
            // Navigation failures are the shell's to report; the view stays usable.
            _ = error;
        }
    }
}

/// <summary>A card the user activated in the lineage view.</summary>
public sealed class LineageNodeActivatedEventArgs : EventArgs
{
    /// <summary>Creates the event arguments.</summary>
    /// <param name="node">The activated card's node.</param>
    public LineageNodeActivatedEventArgs(LineageNode node) => Node = node;

    /// <summary>The activated node.</summary>
    public LineageNode Node { get; }

    /// <summary>The activated node's key.</summary>
    public GraphNodeKey Key => Node.Key;
}
