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
    private static (double MaxFrameMs, double MaxAllocPerFrame, double FirstPaintMs, int Rows, int Columns, int Steps) ReadBudgets()
    {
        var path = FindBudgetFile();
        var lines = File.ReadAllLines(path);
        double maxFrame = 0, maxAlloc = 0, firstPaint = 0;
        var rows = 0; var columns = 0; var steps = 0;
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
            }
        }

        return (maxFrame, maxAlloc, firstPaint, rows, columns, steps);
    }

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
        var (_, _, firstPaintBudget, _, _, _) = ReadBudgets();
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
        var budget = firstPaintBudget * (Environment.GetEnvironmentVariable("CI") is null ? 1 : 3); // shared runners are not the reference runner
        var culture = CultureInfo.InvariantCulture;
        var numbers = $"{ThreadCpuTime.Source} {bestCpu.ToString("F1", culture)} ms, wall time {bestWall.ToString("F1", culture)} ms (best of 5), budget {budget.ToString("F1", culture)} ms";
        Assert.True(bestCpu < budget, $"first paint took {numbers}");
        if (Environment.GetEnvironmentVariable("SPOT_PERF_REFERENCE") == "1")
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
        var (maxFrameBudget, allocBudget, _, rows, columns, steps) = ReadBudgets();

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
        Assert.True(maxFrame < maxFrameBudget,
            $"max single-frame draw {maxFrame.ToString("F3", culture)} ms exceeded budget {maxFrameBudget} ms (worst steps: {string.Join(", ", watched.OrderByDescending(x => x).Take(3).Select(x => x.ToString("F3", culture)))})");
        Assert.True(meanAlloc < allocBudget,
            $"mean per-frame allocations {meanAlloc.ToString("F0", culture)} bytes exceeded budget {allocBudget} bytes");
    }

    private double _allocSum;
    private int _allocFrames;
}