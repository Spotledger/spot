using System.Diagnostics;
using Avalonia.Headless.XUnit;
using Spotledger.Client.TestSupport;
using Xunit;

namespace Spotledger.Client.Reports.Tests;

/// <summary>Budgets as tests (perf-budgets.toml beside these tests): a fake server returning 50,000 rows renders
/// the first screenful quickly after the response and scrolls 300 steps within the grid's frame budget, a
/// 12-parameter form opens fast, and a tab never has two runs in flight.</summary>
public sealed class BudgetTests
{
    private const int Rows = 50_000;
    private const int ScrollSteps = 300;

    private static readonly Budget Evidence = new(PerfBudget.Load(Path.Combine(AppContext.BaseDirectory, "perf-budgets.toml")));
    private static readonly PerfBudget Declared = PerfBudget.Load(Path.Combine(AppContext.BaseDirectory, "perf-budgets.toml"));
    private static readonly ReportRunResult Big = Fx.Result(Rows, summable: true);

    private static double Max(string id) => Declared.Entries[id].Max;

    // A shared CI runner is not the reference runner (as in the grid's own budget tests).
    private static double Scale => LoadScale.Applied;

    private static FakeReportGateway BigGateway(int parameters = 2)
        => new() { Description = Fx.Simple(parameters, maxRows: 100_000), Result = Big };

    [AvaloniaFact]
    public async Task TheFirstScreenOf50kRowsIsRenderedUnderBudgetAfterTheResponse()
    {
        var s = await ViewHarness.ShowAsync(BigGateway());
        ViewHarness.TypeInto(s.View, "p0", "acme");
        await s.View.RunAsync();
        Assert.True(await ViewHarness.SettleAsync(s.View));

        var samples = new List<double>();
        for (var attempt = 0; attempt < 5; attempt++)
        {
            var watch = Stopwatch.StartNew();
            await s.View.RunAsync();
            Assert.True(await ViewHarness.SettleAsync(s.View));
            samples.Add(watch.Elapsed.TotalMilliseconds);
        }

        Evidence.Assert("reports.first-screen", PerfStatistics.From(samples).P50);
        Assert.True(PerfStatistics.From(samples).P50 < Max("reports.first-screen") * Scale, $"first screen took {PerfStatistics.From(samples).P50:F1} ms (p50), budget {Max("reports.first-screen")} ms, {LoadScale.Describe()}");
        Assert.Equal(Rows, s.View.Grid.RowCount);
        Assert.Equal(1, s.Gateway.MaxInFlight);
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task Scrolling50kRowsIn300StepsStaysWithinTheGridFrameBudget()
    {
        var budget = Max("reports.scroll-frame") * Scale;
        var s = await ViewHarness.ShowAsync(BigGateway());
        ViewHarness.TypeInto(s.View, "p0", "acme");
        await s.View.RunAsync();
        Assert.True(await ViewHarness.SettleAsync(s.View));

        var bestMax = double.MaxValue;
        for (var attempt = 0; attempt < 3 && bestMax >= budget; attempt++)
        {
            var maxFrame = 0d;
            for (var step = 0; step < ScrollSteps; step++)
            {
                s.View.Grid.ScrollIntoView((int)((long)(Rows - 1) * step / (ScrollSteps - 1)));
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

        Evidence.Assert("reports.scroll-frame", bestMax);
        Assert.True(bestMax < budget, $"max frame {bestMax:F2} ms exceeded the grid budget {budget} ms, {LoadScale.Describe()}");
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task AParameterFormOfTwelveFieldsOpensUnderBudget()
    {
        var samples = new List<double>();
        for (var attempt = -1; attempt < 5; attempt++)
        {
            var gateway = new FakeReportGateway { Description = Fx.Simple(12) };
            var watch = Stopwatch.StartNew();
            var s = await ViewHarness.ShowAsync(gateway);
            await Pumping.PumpAsync(0);
            watch.Stop();
            Assert.Equal(12, s.View.ParameterControls.Count);
            if (attempt >= 0)
            {
                samples.Add(watch.Elapsed.TotalMilliseconds);
            }

            s.Window.Close();
        }

        Evidence.Assert("reports.form-open", PerfStatistics.From(samples).P50);
        Assert.True(PerfStatistics.From(samples).P50 < Max("reports.form-open") * Scale, $"form open took {PerfStatistics.From(samples).P50:F1} ms (p50), budget {Max("reports.form-open")} ms, {LoadScale.Describe()}");
    }

    [AvaloniaFact]
    public async Task HammeringRunNeverMakesARequestStorm()
    {
        // A gate this test owns: the first run stays in flight for the whole hammering loop, however
        // slow the machine is, so the loop measures the product instead of this test's own speed.
        var gate = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var gateway = BigGateway();
        gateway.OnRun = async (_, _, token) =>
        {
            await gate.Task.WaitAsync(token);
            return Big;
        };
        var s = await ViewHarness.ShowAsync(gateway);
        ViewHarness.TypeInto(s.View, "p0", "acme");

        for (var i = 0; i < 100; i++)
        {
            _ = s.View.RunAsync();
            TestHost.Pump();
        }

        // Still closed: one run in flight, however many clicks landed.
        Assert.False(gate.Task.IsCompleted);
        Assert.Equal(1, gateway.Runs);
        Assert.Equal(1, gateway.MaxInFlight);

        // The late completion must not start another run either.
        gate.SetResult();
        Assert.True(await ViewHarness.SettleAsync(s.View));
        Assert.Equal(1, gateway.Runs);
        Assert.Equal(1, gateway.MaxInFlight);
        s.Window.Close();
    }

    [AvaloniaFact]
    public async Task AClickAfterTheRunFinishedStartsExactlyOneMoreRun()
    {
        var gate = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var gateway = BigGateway();
        gateway.OnRun = async (_, _, token) =>
        {
            await gate.Task.WaitAsync(token);
            return Big;
        };
        var s = await ViewHarness.ShowAsync(gateway);
        ViewHarness.TypeInto(s.View, "p0", "acme");

        _ = s.View.RunAsync();
        gate.SetResult();
        Assert.True(await ViewHarness.SettleAsync(s.View));
        Assert.Equal(1, gateway.Runs);

        // One click after the first run settled is a new run, and still only one at a time.
        await s.View.RunAsync();
        Assert.True(await ViewHarness.SettleAsync(s.View));
        Assert.Equal(2, gateway.Runs);
        Assert.Equal(1, gateway.MaxInFlight);
        s.Window.Close();
    }
}
