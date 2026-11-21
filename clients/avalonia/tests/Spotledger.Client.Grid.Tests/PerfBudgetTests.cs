using System.Diagnostics;
using Avalonia.Headless.XUnit;
using System.Globalization;
using Avalonia;
using Avalonia.Controls;
using Spotledger.Client.Grid;
using Xunit;

namespace Spotledger.Client.Grid.Tests;

/// <summary>
/// Performance and budgets (brief §5): an in-memory source of 100,000 rows × 12
/// columns, a scripted scroll from top to bottom in 300 steps; max single-frame draw
/// under 8 ms, allocations per frame near zero, first paint of 50 rows under 50 ms.
/// Budgets live in perf-budgets.toml beside these tests.
/// </summary>
public sealed class PerfBudgetTests
{
    private static (double MaxFrameMs, double MaxAllocPerFrame, double FirstPaintMs, int Rows, int Columns, int Steps, double CatastropheFactor) ReadBudgets()
    {
        var path = FindBudgetFile();
        var lines = File.ReadAllLines(path);
        double maxFrame = 0, maxAlloc = 0, firstPaint = 0;
        var rows = 0; var columns = 0; var steps = 0;
        // catastrophe_factor is not a key of perf-budgets.toml: TestSupport's PerfBudget loader (outside this
        // task's paths) rejects any file whose keys are not exactly the six it knows, and the TestSupport
        // MeasuredUiTests read the same file. The default lives here until that loader accepts it.
        var catastrophe = 10d;
        foreach (var line in lines)
        {
            var trimmed = line.Trim();
            if (trimmed.Length == 0 || trimmed.StartsWith('#'))
            {
                continue;
            }

            var parts = trimmed.Split('=', 2);
            if (parts.Length != 2)
            {
                continue;
            }

            var key = parts[0].Trim();
            var value = parts[1].Trim();
            switch (key)
            {
                case "max_frame_ms": maxFrame = double.Parse(value, CultureInfo.InvariantCulture); break;
                case "max_alloc_bytes_per_frame": maxAlloc = double.Parse(value, CultureInfo.InvariantCulture); break;
                case "first_paint_50_rows_ms": firstPaint = double.Parse(value, CultureInfo.InvariantCulture); break;
                case "rows": rows = int.Parse(value, CultureInfo.InvariantCulture); break;
                case "columns": columns = int.Parse(value, CultureInfo.InvariantCulture); break;
                case "scroll_steps": steps = int.Parse(value, CultureInfo.InvariantCulture); break;
                case "catastrophe_factor": catastrophe = double.Parse(value, CultureInfo.InvariantCulture); break;
            }
        }

        return (maxFrame, maxAlloc, firstPaint, rows, columns, steps, catastrophe);
    }

    /// <summary>
    /// The strict budget is a property of the reference runner, declared by SPOT_PERF_REFERENCE=1
    /// (set from a repository variable of the same name, never inferred from CI). Everywhere else the
    /// test still runs and prints its numbers and fails only on the catastrophe bound
    /// (catastrophe_factor times the budget: a hang or an accidental O(n^2)).
    /// </summary>
    private static bool IsReferenceRunner => Environment.GetEnvironmentVariable("SPOT_PERF_REFERENCE") == "1";

    private static string FindBudgetFile()
    {
        var candidates = new[]
        {
            Path.Combine(AppContext.BaseDirectory, "perf-budgets.toml"),
            Path.Combine(AppContext.BaseDirectory, "..", "..", "..", "perf-budgets.toml"),
        };
        return candidates.First(File.Exists);
    }

    private static List<GridColumnModel> Columns(int count)
    {
        var columns = new List<GridColumnModel>();
        for (var i = 0; i < count; i++)
        {
            columns.Add(new GridColumnModel
            {
                Header = $"Column {i}",
                Width = i == 0 ? GridColumnWidth.Pixels(140) : GridColumnWidth.Star(1),
                DefaultAlignment = i == 0 ? GridCellAlignment.Start : GridCellAlignment.End,
            });
        }

        return columns;
    }

    private static SpotGrid MakePerfGrid(out InMemoryGridSource source, int rows, int columns, out Window window)
    {
        source = new InMemoryGridSource(GridFixture.MakeRows(rows, columns));
        var grid = new SpotGrid
        {
            Columns = Columns(columns),
            Source = source,
        };
        window = new Window { Width = 1200, Height = 600, Content = grid };
        window.Show();
        return grid;
    }

    [AvaloniaFact]
    public void FirstPaintOfFiftyRowsUnderBudget()
    {
        var (_, _, firstPaintBudget, _, _, _, catastropheFactor) = ReadBudgets();
        // Best of five: the first grid pays JIT and style loading, and a shared CI runner adds noise.
        // The verdict is on the thread CPU time of the two pumps: the headless platform renders
        // on the calling (UI) thread, so that thread's CPU time is the paint cost, and a busy
        // host cannot inflate it the way it inflates wall time. Wall time is printed always and
        // asserted only on the reference runner (SPOT_PERF_REFERENCE=1).
        var bestCpu = double.MaxValue;
        var bestWall = double.MaxValue;
        for (var attempt = 0; attempt < 5; attempt++)
        {
            var grid = MakePerfGrid(out _, rows: 1000, columns: 12, out var window);
            var cpuStart = ThreadCpuTime.NowMs();
            var stopwatch = Stopwatch.StartNew();
            TestHost.Pump();
            TestHost.Pump();
            var wall = stopwatch.Elapsed.TotalMilliseconds;
            var cpu = ThreadCpuTime.NowMs() - cpuStart;
            bestCpu = Math.Min(bestCpu, cpu);
            bestWall = Math.Min(bestWall, wall);
            Assert.True(grid.AppliedRows is { Count: > 0 }, "the first window must have completed");
            window.Close();
        }
        var reference = IsReferenceRunner;
        var budget = reference ? firstPaintBudget : firstPaintBudget * catastropheFactor;
        var culture = CultureInfo.InvariantCulture;
        var mode = reference ? "strict (reference runner)" : $"catastrophe bound only ({catastropheFactor.ToString("F0", culture)} x the {firstPaintBudget.ToString("F1", culture)} ms budget)";
        var numbers = $"{ThreadCpuTime.Source} {bestCpu.ToString("F1", culture)} ms, wall time {bestWall.ToString("F1", culture)} ms (best of 5), budget {budget.ToString("F1", culture)} ms, mode {mode}";
        // One line a trend can be read from in CI logs.
        Console.WriteLine($"PERF first-paint source={ThreadCpuTime.Source} cpu_ms={bestCpu.ToString("F1", culture)} wall_ms={bestWall.ToString("F1", culture)} budget_ms={budget.ToString("F1", culture)} mode={(reference ? "strict" : "catastrophe")}");
        Assert.True(bestCpu < budget, $"first paint took {numbers}");
        if (reference)
        {
            Assert.True(bestWall < budget, $"first paint wall time exceeded the budget on the reference runner: {numbers}");
        }
    }

    [Xunit.Fact]
    public void ThreadCpuTimeGrowsWhileSpinningAndNotWhileSleeping()
    {
        var start = ThreadCpuTime.NowMs();
        var spin = Stopwatch.StartNew();
        var sink = 0L;
        // Spin until 20 ms of CPU time accrue (a loaded host gives the thread only part of the wall time), within 10 s.
        while (ThreadCpuTime.NowMs() - start < 20 && spin.ElapsedMilliseconds < 10_000)
        {
            sink += spin.ElapsedTicks;
        }

        var afterSpin = ThreadCpuTime.NowMs();
        Assert.True(sink != 0);
        Assert.True(afterSpin - start > 20, $"spinning advanced {ThreadCpuTime.Source} by {afterSpin - start} ms");

        Thread.Sleep(200);
        var afterSleep = ThreadCpuTime.NowMs();
        Assert.True(afterSleep - afterSpin < 100, $"sleeping 200 ms advanced {ThreadCpuTime.Source} by {afterSleep - afterSpin} ms");
    }

    [AvaloniaFact]
    public void ScrollOf100kRowsIn300StepsStaysWithinBudgets()
    {
        var (maxFrameBudget, allocBudget, _, rows, columns, steps, catastropheFactor) = ReadBudgets();

        var grid = MakePerfGrid(out _, rows, columns, out var window);
        TestHost.Pump();
        TestHost.Pump();
        Assert.NotNull(grid.AppliedRows);
        Assert.NotNull(window);

        var totalRows = rows;
        var maxFrame = 0d;
        var watched = new List<double>();

        // The scripted scroll: 300 even steps top to bottom, each frame measured with
        // a Stopwatch around the render; allocations per frame measured with
        // GC.GetAllocatedBytesForCurrentThread().
        var culture = CultureInfo.InvariantCulture;
        for (var step = 0; step < steps; step++)
        {
            var target = (double)totalRows * step / (steps - 1);
            grid.ScrollToRow(target);

            var before = GC.GetAllocatedBytesForCurrentThread();
            var stopwatch = Stopwatch.StartNew();
            window!.InvalidateVisual();
            // The headless platform paints synchronously inside InvalidateVisual.
            stopwatch.Stop();
            var after = GC.GetAllocatedBytesForCurrentThread();

            var frameMs = stopwatch.Elapsed.TotalMilliseconds;
            var allocBytes = after - before;
            watched.Add(frameMs);
            maxFrame = Math.Max(maxFrame, frameMs);
            if (step >= 10)
            {
                // Measure allocations only after the geometry caches warm up; the
                // budget is a per-frame mean over the watched steps.
                _allocSum += allocBytes;
                _allocFrames++;
            }
        }

        var meanAlloc = _allocFrames > 0 ? _allocSum / _allocFrames : 0;
        _allocSum = 0;
        _allocFrames = 0;

        Assert.True(grid.MaxScrollRows > 4000, "the fixture must be large enough to matter");
        // Max frame time is wall time and follows the reference marker; allocations per frame are
        // host-independent and stay strict everywhere.
        var frameBound = IsReferenceRunner ? maxFrameBudget : maxFrameBudget * catastropheFactor;
        var worst = string.Join(", ", watched.OrderByDescending(x => x).Take(3).Select(x => x.ToString("F3", culture)));
        Console.WriteLine($"PERF scroll max_frame_ms={maxFrame.ToString("F3", culture)} bound_ms={frameBound.ToString("F3", culture)} mode={(IsReferenceRunner ? "strict" : "catastrophe")} mean_alloc_bytes={meanAlloc.ToString("F0", culture)}");
        Assert.True(maxFrame < frameBound,
            $"max single-frame draw {maxFrame.ToString("F3", culture)} ms exceeded bound {frameBound.ToString("F3", culture)} ms ({(IsReferenceRunner ? "strict budget" : "catastrophe bound")}; worst steps: {worst})");
        Assert.True(meanAlloc < allocBudget,
            $"mean per-frame allocations {meanAlloc.ToString("F0", culture)} bytes exceeded budget {allocBudget} bytes");
    }

    private double _allocSum;
    private int _allocFrames;
}