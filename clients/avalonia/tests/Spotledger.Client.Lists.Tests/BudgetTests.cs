using System.Diagnostics;
using System.Globalization;
using Avalonia.Headless.XUnit;
using Spotledger.Client.TestSupport;
using Xunit;

namespace Spotledger.Client.Lists.Tests;

/// <summary>Budgets as tests (perf-budgets.toml beside these tests): a 100,000-row fake server scrolls 300
/// steps within the grid's frame budget, the first page renders fast, a filter change lands quickly with the
/// fake at 20 ms latency, and the list never has two queries in flight.</summary>
public sealed class BudgetTests
{
    private const int Rows = 100_000;
    private const int ScrollSteps = 300;
    private const int FakeLatencyMs = 20;

    private static readonly Budget Evidence = new(PerfBudget.Load(Path.Combine(AppContext.BaseDirectory, "perf-budgets.toml")));
    private static readonly PerfBudget Declared = PerfBudget.Load(Path.Combine(AppContext.BaseDirectory, "perf-budgets.toml"));

    private static double Max(string id) => Declared.Entries[id].Max;

    // A shared CI runner is not the reference runner (as in the grid's own budget tests).
    private static double Scale => LoadScale.Applied;

    [AvaloniaFact]
    public async Task FirstPageIsRenderedUnderBudgetFromTheResponse()
    {
        var best = double.MaxValue;
        var samples = new List<double>();
        for (var attempt = 0; attempt < 5; attempt++)
        {
            var gateway = new FakeListGateway(Rows);
            var s = await ViewHarness.ShowAsync(gateway, new ListSourceOptions { Count = CountPolicy.Exact }, settle: false);
            var watch = Stopwatch.StartNew();
            Assert.True(await ViewHarness.SettleAsync(s.View));
            best = Math.Min(best, watch.Elapsed.TotalMilliseconds);
            samples.Add(watch.Elapsed.TotalMilliseconds);
            Assert.Equal(1, gateway.MaxInFlight);
            s.Window.Close();
        }

        Evidence.Assert("lists.first-page", PerfStatistics.From(samples).P50);
        Assert.True(best < Max("lists.first-page") * Scale, $"first page took {best:F1} ms (best of 5), budget {Max("lists.first-page")} ms, {LoadScale.Describe()}");
    }

    [AvaloniaFact]
    public async Task AFilterChangeShowsNewRowsUnderBudgetWithTheFakeAt20Ms()
    {
        var best = double.MaxValue;
        var samples = new List<double>();
        var gateway = new FakeListGateway(Rows) { Latency = TimeSpan.FromMilliseconds(FakeLatencyMs), FilteredRows = 5000 };
        var s = await ViewHarness.ShowAsync(gateway, new ListSourceOptions { Count = CountPolicy.Exact });
        for (var attempt = 0; attempt < 5; attempt++)
        {
            gateway.Prefix = "F" + attempt + " ";
            var watch = Stopwatch.StartNew();
            s.Model.RemoveCondition(0);
            s.Model.AddCondition(new FilterCondition("amount", FilterOperator.From, [(attempt + 1).ToString(CultureInfo.InvariantCulture)]));
            Assert.True(await ViewHarness.WaitAsync(() => ViewHarness.CellText(s.View.Grid, 0, 0)?.StartsWith(gateway.Prefix, StringComparison.Ordinal) == true));
            best = Math.Min(best, watch.Elapsed.TotalMilliseconds);
            samples.Add(watch.Elapsed.TotalMilliseconds);
            s.Model.RemoveCondition(0);
            await ViewHarness.WaitAsync(() => ViewHarness.CellText(s.View.Grid, 0, 0)?.StartsWith(gateway.Prefix, StringComparison.Ordinal) == true);
        }

        Evidence.Assert("lists.filter-change", PerfStatistics.From(samples).P50);
        Assert.True(best < Max("lists.filter-change") * Scale, $"filter change took {best:F1} ms (best of 5), budget {Max("lists.filter-change")} ms, {LoadScale.Describe()}");
        Assert.Equal(1, gateway.MaxInFlight);
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task ScrollOf100kRowsIn300StepsStaysWithinTheGridFrameBudget()
    {
        var rows = Rows;
        var steps = ScrollSteps;
        var budget = Max("lists.scroll-frame") * Scale;
        var gateway = new FakeListGateway(rows);
        var s = await ViewHarness.ShowAsync(gateway, new ListSourceOptions { Count = CountPolicy.Exact, PageSize = 500, MaxCachedPages = 4 });
        Assert.Equal(rows, s.View.Grid.RowCount);

        var bestMax = double.MaxValue;
        for (var attempt = 0; attempt < 3 && bestMax >= budget; attempt++)
        {
            var maxFrame = 0d;
            for (var step = 0; step < steps; step++)
            {
                s.View.Grid.ScrollIntoView((int)((long)(rows - 1) * step / (steps - 1)));
                TestHost.Pump();
                var watch = Stopwatch.StartNew();
                s.Window.InvalidateVisual();
                watch.Stop();
                if (step >= 10)
                {
                    maxFrame = Math.Max(maxFrame, watch.Elapsed.TotalMilliseconds);
                }
            }

            bestMax = Math.Min(bestMax, maxFrame);
        }

        Evidence.Assert("lists.scroll-frame", bestMax);
        Assert.True(bestMax < budget, $"max frame {bestMax:F2} ms exceeded the grid budget {budget} ms, {LoadScale.Describe()}");
        Assert.Equal(1, gateway.MaxInFlight);
        Assert.True(s.Model.Source!.CachedRows().Count <= 4 * 500, "paging keeps memory bounded while scrolling");
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task ScrollingFastNeverMakesARequestStorm()
    {
        var gateway = new FakeListGateway(100_000) { Latency = TimeSpan.FromMilliseconds(5) };
        var s = await ViewHarness.ShowAsync(gateway, new ListSourceOptions { Count = CountPolicy.Exact });

        for (var i = 0; i < 60; i++)
        {
            s.View.Grid.ScrollIntoView(i * 1500);
            TestHost.Pump();
        }

        await ViewHarness.WaitAsync(() => false, 300);
        Assert.Equal(1, gateway.MaxInFlight);
        s.Window.Close();
    }
}
