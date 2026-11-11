using System.Diagnostics;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Threading;
using Spotledger.Client.Graph;
using Spotledger.Client.TestSupport;
using Xunit;

namespace Spotledger.Client.Graph.Tests;

/// <summary>
/// The acceptance budgets: a 10,000-node hierarchy opens cold under a second, scroll frames
/// stay inside a 60 fps frame, keyboard expand/collapse/jump stays under 50 ms, and a
/// two-thousand-node lineage layout finishes well inside its budget. The thresholds live in
/// perf-budgets.toml beside these tests; a CI runner applies its documented multiplier, so a
/// shared box cannot fail the build on noise.
/// </summary>
public sealed class PerfBudgetTests
{
    private static PerfBudget LoadBudgets()
    {
        var candidates = new[]
        {
            Path.Combine(AppContext.BaseDirectory, "perf-budgets.toml"),
            Path.Combine(AppContext.BaseDirectory, "..", "..", "..", "perf-budgets.toml"),
        };
        return PerfBudget.Load(candidates.First(File.Exists));
    }

    private static void Render(SpotHierarchy tree)
    {
        Dispatcher.UIThread.RunJobs();
        AvaloniaHeadlessPlatform.ForceRenderTimerTick();
        tree.InvalidateVisual();
        AvaloniaHeadlessPlatform.ForceRenderTimerTick();
        Dispatcher.UIThread.RunJobs();
    }

    [AvaloniaFact]
    public void ATenThousandNodeOpenStaysInsideTheOpenBudget()
    {
        var budgets = LoadBudgets();
        var sink = new Budget(budgets);
        var measurement = PerfMeter.Measure(
            () =>
            {
                var tree = GraphFixtures.MakeHierarchy(
                    HierarchySamples.AccountsSource(10_000, branching: 250),
                    pageSize: 100,
                    height: 600);
                Assert.NotNull(tree.Model);
                Assert.True(tree.Model!.IsInitialized);
            },
            samples: 5,
            warmup: 1);

        sink.Assert("graph-hierarchy-open", measurement);
    }

    [AvaloniaFact]
    public void ScrollFramesStayInsideAFrameBudget()
    {
        var budgets = LoadBudgets();
        var sink = new Budget(budgets);
        var tree = GraphFixtures.MakeHierarchy(
            HierarchySamples.AccountsSource(10_000, branching: 250),
            pageSize: 100,
            height: 600);
        TestHost.Await(tree.Model!.ExpandAsync(GraphFixtures.RootKey, CancellationToken.None));
        TestHost.Pump();
        Render(tree);

        var worst = 0d;
        for (var step = 0; step < 120; step++)
        {
            var stopwatch = Stopwatch.StartNew();
            tree.ScrollBy(25);
            Render(tree);
            stopwatch.Stop();
            worst = Math.Max(worst, stopwatch.Elapsed.TotalMilliseconds);
        }

        Assert.True(tree.ScrollRows > 100, "the fixture must actually scroll");
        sink.Assert("graph-hierarchy-scroll-frame", worst);
    }

    [AvaloniaFact]
    public void KeyboardExpandAndJumpStayInsideTheKeyboardBudget()
    {
        var budgets = LoadBudgets();
        var sink = new Budget(budgets);
        var tree = GraphFixtures.MakeHierarchy(
            HierarchySamples.AccountsSource(10_000, branching: 250),
            pageSize: 100,
            height: 600);
        GraphFixtures.Press(tree, Avalonia.Input.Key.Right);
        TestHost.Pump();

        var samples = new List<double>(240);
        for (var i = 0; i < 240; i++)
        {
            var key = (i % 4) switch
            {
                0 => Avalonia.Input.Key.Down,
                1 => Avalonia.Input.Key.End,
                2 => Avalonia.Input.Key.Home,
                _ => Avalonia.Input.Key.Up,
            };
            var stopwatch = Stopwatch.StartNew();
            GraphFixtures.Press(tree, key);
            stopwatch.Stop();
            samples.Add(stopwatch.Elapsed.TotalMilliseconds);
        }

        samples.Sort();
        var p95 = samples[(int)Math.Ceiling(samples.Count * 0.95) - 1];
        sink.Assert("graph-hierarchy-keyboard", p95);
    }

    [AvaloniaFact]
    public async Task ATwoThousandNodeLineageLayoutStaysInsideItsBudget()
    {
        var budgets = LoadBudgets();
        var sink = new Budget(budgets);
        const int count = 2_000;
        var nodes = new List<LineageNode>(count);
        var edges = new List<LineageEdge>(count - 1);
        for (var i = 0; i < count; i++)
        {
            var key = new GraphNodeKey("SalesInvoice", $"inv-{i:D5}");
            nodes.Add(new LineageNode(key, $"Invoice {i}", "Posted", "1.00 EUR"));
            if (i > 0)
            {
                edges.Add(new LineageEdge(new GraphNodeKey("SalesInvoice", $"inv-{i - 1:D5}"), key, "follows"));
            }
        }

        var snapshot = new GraphSnapshot(nodes, edges);
        var samples = new List<double>(5);
        var offThread = false;
        for (var attempt = 0; attempt < 5; attempt++)
        {
            var stopwatch = Stopwatch.StartNew();
            var layout = await LineageLayoutEngine.ComputeAsync(snapshot, CancellationToken.None);
            stopwatch.Stop();
            samples.Add(stopwatch.Elapsed.TotalMilliseconds);
            Assert.Equal(count, layout.Nodes.Count);
            offThread |= layout.ComputedOnThreadId != Environment.CurrentManagedThreadId;
        }

        Assert.True(offThread, "a 2,000-node layout must not run on the calling thread");
        samples.Sort();
        var p95 = samples[(int)Math.Ceiling(samples.Count * 0.95) - 1];
        sink.Assert("graph-lineage-layout", p95);
    }
}
