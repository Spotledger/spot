using Avalonia;
using Avalonia.Headless.XUnit;
using Spotledger.Client.Graph;
using Xunit;

namespace Spotledger.Client.Graph.Tests;

/// <summary>The lineage view: the layout completes (off the UI thread for a large graph), pan
/// and zoom stay clamped and keep the anchor stable, a click maps to a card, activation opens
/// the document through the navigation seam, and a locked card never shows its content.</summary>
public sealed class LineageViewTests
{
    private static (SpotLineageView View, RecordingNavigation Navigation) MakeView(GraphSnapshot snapshot, double width = 800, double height = 600)
    {
        var navigation = new RecordingNavigation();
        var view = new SpotLineageView { Navigation = navigation, Clock = new FixedClock() };
        TestHost.Show(view, width, height);
        view.Snapshot = snapshot;
        TestHost.Await(view.PendingLayout);
        TestHost.Pump();
        return (view, navigation);
    }

    [AvaloniaFact]
    public void TheSnapshotLayoutArrivesAndFitsTheViewport()
    {
        var (view, _) = MakeView(GraphFixtures.SupplyChain());

        Assert.NotNull(view.Layout);
        Assert.Equal(5, view.Layout!.Nodes.Count);
        Assert.Equal(4, view.Layout.Edges.Count);
        Assert.InRange(view.Zoom, 0.25, 3);
    }

    [AvaloniaFact]
    public void HitTestMapsAScreenPointToItsCard()
    {
        var (view, _) = MakeView(GraphFixtures.SupplyChain());
        var placement = view.Layout!.Nodes[0];
        var point = new Point(
            (placement.Position.X * view.Zoom) + view.PanX + (LineageLayoutEngine.CardWidth * view.Zoom / 2),
            (placement.Position.Y * view.Zoom) + view.PanY + (LineageLayoutEngine.CardHeight * view.Zoom / 2));

        var hit = view.HitTest(point);

        Assert.NotNull(hit);
        Assert.Equal(placement.Key, hit!.Key);
        Assert.Null(view.HitTest(new Point(-50, -50)));
    }

    [AvaloniaFact]
    public void ZoomKeepsTheAnchorPointAndClampsToItsRange()
    {
        var (view, _) = MakeView(GraphFixtures.SupplyChain());
        var anchor = new Point(200, 150);
        var worldBefore = ((anchor.X - view.PanX) / view.Zoom, (anchor.Y - view.PanY) / view.Zoom);

        view.SetZoom(view.Zoom * 1.5, anchor);

        var worldAfter = ((anchor.X - view.PanX) / view.Zoom, (anchor.Y - view.PanY) / view.Zoom);
        Assert.Equal(worldBefore.Item1, worldAfter.Item1, 3);
        Assert.Equal(worldBefore.Item2, worldAfter.Item2, 3);

        view.SetZoom(50);
        Assert.Equal(3, view.Zoom);
        view.SetZoom(0.01);
        Assert.Equal(0.25, view.Zoom);
    }

    [AvaloniaFact]
    public void PanningMovesTheDrawing()
    {
        var (view, _) = MakeView(GraphFixtures.SupplyChain());
        var before = (view.PanX, view.PanY);

        view.PanBy(40, -25);

        Assert.Equal(before.Item1 + 40, view.PanX);
        Assert.Equal(before.Item2 - 25, view.PanY);
    }

    [AvaloniaFact]
    public void ActivationSelectsTheCardAndOpensTheDocument()
    {
        var (view, navigation) = MakeView(GraphFixtures.SupplyChain());
        var placement = view.Layout!.Nodes[2];
        var activated = new List<GraphNodeKey>();
        view.NodeActivated += (_, e) => activated.Add(e.Key);

        view.Activate(placement);

        Assert.Equal([placement.Key], activated);
        Assert.Equal(placement.Key, view.SelectedKey);
        Assert.Equal(placement.Key, Assert.Single(navigation.Opened));
    }

    [AvaloniaFact]
    public void ArrowKeysWalkTheCardsInLayoutOrder()
    {
        var (view, _) = MakeView(GraphFixtures.SupplyChain());
        view.Select(view.Layout!.Nodes[0].Key);

        view.RaiseEvent(new Avalonia.Input.KeyEventArgs
        {
            Key = Avalonia.Input.Key.Right,
            RoutedEvent = Avalonia.Input.InputElement.KeyDownEvent,
        });

        Assert.Equal(view.Layout.Nodes[1].Key, view.SelectedKey);
    }

    [AvaloniaFact]
    public void ALockedCardShowsOnlyThePlaceholder()
    {
        var (view, _) = MakeView(GraphFixtures.SupplyChain());
        var locked = view.Layout!.Nodes.Single(placement => placement.Node.IsLocked);

        Assert.Equal(LineageNode.LockedTitle, locked.Node.DisplayTitle);
        Assert.True(string.IsNullOrEmpty(locked.Node.Title));
        Assert.True(string.IsNullOrEmpty(locked.Node.Status));
    }

    [AvaloniaFact]
    public void ALargeGraphIsLaidOutOffTheUiThread()
    {
        var nodes = new List<LineageNode>();
        var edges = new List<LineageEdge>();
        for (var i = 0; i < 250; i++)
        {
            var key = new GraphNodeKey("SalesInvoice", $"inv-{i:D4}");
            nodes.Add(new LineageNode(key, $"Invoice {i}", "Posted", "1.00 EUR"));
            if (i > 0)
            {
                edges.Add(new LineageEdge(new GraphNodeKey("SalesInvoice", $"inv-{i - 1:D4}"), key, "follows"));
            }
        }

        var uiThread = Environment.CurrentManagedThreadId;
        var view = new SpotLineageView { Clock = new FixedClock() };
        TestHost.Show(view, 800, 600);
        view.Snapshot = new GraphSnapshot(nodes, edges);
        TestHost.Await(view.PendingLayout);
        TestHost.Pump();

        Assert.Equal(250, view.Layout!.Nodes.Count);
        Assert.NotEqual(uiThread, view.Layout.ComputedOnThreadId);
        Assert.NotEqual(uiThread, view.LastLayoutThreadId);
    }
}
