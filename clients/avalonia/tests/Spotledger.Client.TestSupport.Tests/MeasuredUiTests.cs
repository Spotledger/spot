using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Threading;
using Spotledger.Client.Grid;
using Spotledger.Client.Theme;
using Xunit;

namespace Spotledger.Client.TestSupport.Tests;

/// <summary>Owned integration bridge until the Grid owner migrates its local assertions.
/// Uses only public source, automation and routed-input APIs, never internal/reflection seams.</summary>
public sealed class MeasuredUiTests
{
    private static PerfBudget GridBudgets() => PerfBudget.Load(Path.Combine(AppContext.BaseDirectory, "grid-perf-budgets.toml"));

    private static List<GridColumnModel> Columns(int count) => Enumerable.Range(0, count).Select(index =>
        new GridColumnModel { Header = $"Column {index}", Width = GridColumnWidth.Pixels(95) }).ToList();

    private static GridRow[] Rows(int count, int columns) => Enumerable.Range(0, count).Select(row =>
        GridRow.Of($"r{row}", Enumerable.Range(0, columns).Select(column => new GridCell($"c{column}:{row}")).ToArray())).ToArray();

    private sealed class MeasuredGrid : SpotGrid
    {
        public void Press(Key key, KeyModifiers modifiers = KeyModifiers.None) =>
            OnKeyDown(new KeyEventArgs { RoutedEvent = InputElement.KeyDownEvent, Key = key, KeyModifiers = modifiers });
    }

    [AvaloniaFact]
    public void RealGridScrollRecordsLegacyFrameAndAllocationBudgets()
    {
        var budgets = GridBudgets();
        Assert.Equal(3, budgets.Entries.Count);
        Assert.Equal(100000, budgets.Workload["rows"]);
        SpotledgerTheme.MergeTokenResources(Avalonia.Application.Current!);
        using var grid = new MeasuredGrid
        {
            Columns = Columns(budgets.Workload["columns"]),
            Source = new InMemoryGridSource(Rows(budgets.Workload["rows"], budgets.Workload["columns"])),
        };
        var window = new Window { Width = 1200, Height = 600, Content = grid };
        try
        {
            window.Show();
            using (var frame = ScreenshotAssert.Capture(window)) Assert.Equal(1200, frame.PixelSize.Width);
            var peer = new SpotGridAutomationPeer(grid);
            Assert.True(peer.GetChildren() is { Count: > 0 });
            grid.Focus();
            var steps = budgets.Workload["scroll_steps"];
            var pages = (int)Math.Ceiling((double)budgets.Workload["rows"] / (steps * peer.GetChildren()!.Count));
            var measurement = PerfMeter.Measure(() =>
            {
                window.InvalidateVisual();
                Dispatcher.UIThread.RunJobs();
                AvaloniaHeadlessPlatform.ForceRenderTimerTick();
                Dispatcher.UIThread.RunJobs();
            }, samples: steps, warmup: 10, prepare: step =>
            {
                if (step is -10 or 0) grid.Press(Key.Home, KeyModifiers.Control);
                if (step == steps - 1) grid.Press(Key.End, KeyModifiers.Control);
                else for (var page = 0; page < pages; page++) grid.Press(Key.PageDown);
            });
            using (var frame = ScreenshotAssert.Capture(window)) Assert.Equal(600, frame.PixelSize.Height);
            // AutomationPeer caches children; a fresh peer observes the newly applied viewport.
            var finalRows = new SpotGridAutomationPeer(grid).GetChildren();
            Assert.True(finalRows is { Count: > 0 });
            Assert.True(finalRows.Any(row => row.GetName()!.StartsWith("row 999", StringComparison.Ordinal)),
                string.Join(", ", finalRows.Select(row => row.GetName())));
            var assertion = new Budget(budgets);
            assertion.Assert("grid.frame.max", measurement);
            assertion.Assert("grid.frame.alloc.mean", measurement);
        }
        finally { window.Close(); }
    }

    [AvaloniaFact]
    public void RealGridFirstPaintRecordsLegacyBudget()
    {
        var budgets = GridBudgets();
        SpotledgerTheme.MergeTokenResources(Avalonia.Application.Current!);
        var columns = Columns(budgets.Workload["columns"]);
        var source = new InMemoryGridSource(Rows(1000, columns.Count));
        // Fresh controls per sample. Source construction is outside; host creation, rendering,
        // pixel capture and row-presence validation are included (conservative first-paint cost).
        var firstPaint = PerfMeter.Measure(() =>
        {
            using var fresh = new SpotGrid { Columns = columns, Source = source };
            var host = new Window { Width = 1200, Height = 2200, Content = fresh };
            try
            {
                host.Show();
                using var frame = ScreenshotAssert.Capture(host);
                var visible = new SpotGridAutomationPeer(fresh).GetChildren();
                Assert.True(visible is { Count: >= 50 }, $"Only {visible?.Count} rows appeared.");
                Assert.Equal(2200, frame.PixelSize.Height);
            }
            finally { host.Close(); }
        }, samples: 10, warmup: 3);
        new Budget(budgets).Assert("grid.first-paint", firstPaint);
    }

    [AvaloniaFact]
    public void SharedMeterMeasuresRealHeadlessRendering()
    {
        var budgets = PerfBudget.Load(Path.Combine(AppContext.BaseDirectory, "perf-budgets.toml"));
        var window = new Window { Width = 64, Height = 48, Content = new Border { Background = Avalonia.Media.Brushes.Green } };
        try
        {
            window.Show();
            var measurement = PerfMeter.Measure(() =>
            {
                window.InvalidateVisual();
                using var frame = ScreenshotAssert.Capture(window);
                Assert.Equal(64, frame.PixelSize.Width);
            });
            var assertion = new Budget(budgets);
            assertion.Assert("support.render", measurement);
            if (OperatingSystem.IsWindows()) assertion.Assert("support.render.windows", measurement);
        }
        finally { window.Close(); }
    }
}
