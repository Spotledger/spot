using Spotledger.Client.Graph;
using Xunit;

namespace Spotledger.Client.Graph.Tests;

/// <summary>The layout contract: deterministic golden coordinates, stable ordering, cycles
/// contracted into one layer, unknown endpoints ignored, and a graph over 200 nodes computed
/// off the calling thread.</summary>
public sealed class LineageLayoutTests
{
    private static GraphSnapshot Chain(int count)
    {
        var nodes = new List<LineageNode>(count);
        var edges = new List<LineageEdge>(Math.Max(0, count - 1));
        for (var i = 0; i < count; i++)
        {
            var key = new GraphNodeKey("SalesInvoice", $"inv-{i:D4}");
            nodes.Add(new LineageNode(key, $"Invoice {i}", "Posted", "10.00 EUR"));
            if (i > 0)
            {
                edges.Add(new LineageEdge(new GraphNodeKey("SalesInvoice", $"inv-{i - 1:D4}"), key, "follows"));
            }
        }

        return new GraphSnapshot(nodes, edges);
    }

    [Fact]
    public void AStraightChainHasGoldenCoordinates()
    {
        var layout = LineageLayoutEngine.Compute(Chain(4));

        Assert.Equal(4, layout.LayerCount);
        for (var i = 0; i < 4; i++)
        {
            var placement = layout.Nodes[i];
            Assert.Equal(i, placement.Layer);
            Assert.Equal(0, placement.Row);
            Assert.Equal(24 + (i * 280), placement.Position.X);
            Assert.Equal(24, placement.Position.Y);
        }

        Assert.Equal(24 + (4 * 200) + (3 * 80) + 24, layout.Width);
        Assert.Equal(3, layout.Edges.Count);
    }

    [Fact]
    public void TheSameSnapshotGivesTheSameCoordinates()
    {
        var snapshot = GraphFixtures.SupplyChain();
        var first = LineageLayoutEngine.Compute(snapshot);
        var second = LineageLayoutEngine.Compute(snapshot);

        Assert.Equal(first.Nodes.Count, second.Nodes.Count);
        for (var i = 0; i < first.Nodes.Count; i++)
        {
            Assert.Equal(first.Nodes[i].Key, second.Nodes[i].Key);
            Assert.Equal(first.Nodes[i].Position, second.Nodes[i].Position);
            Assert.Equal(first.Nodes[i].Layer, second.Nodes[i].Layer);
        }

        Assert.Equal(first.Width, second.Width);
        Assert.Equal(first.Height, second.Height);
    }

    [Fact]
    public void ACycleIsContractedIntoOneLayerAndTerminates()
    {
        var a = new GraphNodeKey("SalesInvoice", "a");
        var b = new GraphNodeKey("SalesInvoice", "b");
        var c = new GraphNodeKey("SalesInvoice", "c");
        var snapshot = new GraphSnapshot(
            [
                new LineageNode(a, "A", "Posted", "1.00 EUR"),
                new LineageNode(b, "B", "Posted", "1.00 EUR"),
                new LineageNode(c, "C", "Posted", "1.00 EUR"),
            ],
            [
                new LineageEdge(a, b, "credit"),
                new LineageEdge(b, a, "re-issue"),
                new LineageEdge(c, a, "follows"),
            ]);

        var layout = LineageLayoutEngine.Compute(snapshot);

        var placementA = layout.Find(a)!;
        var placementB = layout.Find(b)!;
        var placementC = layout.Find(c)!;
        Assert.Equal(placementA.Layer, placementB.Layer);
        Assert.Equal(1, placementA.Layer); // the cycle sits after C
        Assert.Equal(0, placementC.Layer);
        Assert.Equal(2, layout.LayerCount);
        Assert.NotEqual(placementA.Row, placementB.Row);
        Assert.Equal(3, layout.Edges.Count); // the backward edge still gets a route
    }

    [Fact]
    public void EdgesToUnknownNodesAreIgnored()
    {
        var known = new GraphNodeKey("SalesInvoice", "known");
        var snapshot = new GraphSnapshot(
            [new LineageNode(known, "Known", "Draft", "1.00 EUR")],
            [new LineageEdge(known, new GraphNodeKey("SalesInvoice", "gone"), "missing")]);

        var layout = LineageLayoutEngine.Compute(snapshot);

        Assert.Single(layout.Nodes);
        Assert.Empty(layout.Edges);
        Assert.Equal(1, layout.LayerCount);
    }

    [Fact]
    public void EmptySnapshotLaysOutAsPadding()
    {
        var layout = LineageLayoutEngine.Compute(GraphSnapshot.Empty);

        Assert.Empty(layout.Nodes);
        Assert.Equal(0, layout.LayerCount);
        Assert.Equal(LineageLayoutEngine.Padding * 2, layout.Width);
    }

    [Fact]
    public async Task AGraphOverTwoHundredNodesIsLaidOutOffTheCallingThread()
    {
        var snapshot = Chain(300);
        var callingThread = 0;

        // The caller must be a dedicated, non-pool thread, as the UI thread is in production.
        // On an xUnit pool thread, Task.Run queues to that thread's local queue and the same
        // thread picks the work up once the caller yields, which flakes on 2-CPU runners.
        var layout = await Task.Factory.StartNew(
            async () =>
            {
                callingThread = Environment.CurrentManagedThreadId;
                return await LineageLayoutEngine.ComputeAsync(snapshot, CancellationToken.None);
            },
            CancellationToken.None,
            TaskCreationOptions.LongRunning,
            TaskScheduler.Default).Unwrap();

        Assert.Equal(300, layout.Nodes.Count);
        Assert.NotEqual(0, callingThread);
        Assert.NotEqual(callingThread, layout.ComputedOnThreadId);
    }

    [Fact]
    public async Task ASmallGraphIsLaidOutInline()
    {
        var snapshot = Chain(4);
        var callingThread = Environment.CurrentManagedThreadId;

        var layout = await LineageLayoutEngine.ComputeAsync(snapshot, CancellationToken.None);

        Assert.Equal(callingThread, layout.ComputedOnThreadId);
    }
}
