using System.Diagnostics;
using System.Globalization;
using System.Text;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Avalonia.Threading;
using Spotledger.Client.Controls;
using Spotledger.Client.Metadata;
using Spotledger.Client.TestSupport;
using Xunit;

namespace Spotledger.Client.ChildTable.Tests;

/// <summary>
/// WP-UI-08's acceptance, second half: a 200-row child table is edited by keyboard within budget.
/// The budgets are in perf-budgets.toml beside these tests; a CI runner applies the TestSupport
/// multiplier, the reference runner applies none.
/// </summary>
public sealed class PerfBudgetTests
{
    private const int Rows = 200;
    private const int Steps = 600;

    private static Budget Budgets()
    {
        var path = new[]
        {
            Path.Combine(AppContext.BaseDirectory, "perf-budgets.toml"),
            Path.Combine(AppContext.BaseDirectory, "..", "..", "..", "perf-budgets.toml"),
        }.First(File.Exists);
        return new Budget(PerfBudget.Load(path));
    }

    private static string PasteText(int rows)
    {
        var text = new StringBuilder();
        for (var i = 1; i <= rows; i++)
        {
            // item, description, qty, rate, amount (read-only, left empty), unit, due, taxable: 8 cells.
            text.Append(CultureInfo.InvariantCulture, $"ITEM-{i}\tLine {i}\t{i}.00\t12.50\t\tKg\t2027-01-{(i % 28) + 1:00}\ttrue\n");
        }

        return text.ToString();
    }

    [AvaloniaFact]
    public void FirstRenderOf200RowsIsWithinBudget()
    {
        var draft = Fixtures.Draft(Rows);
        var samples = new List<double>();
        for (var i = 0; i < 12; i++)
        {
            var started = Stopwatch.GetTimestamp();
            var table = new SpotChildTable(draft, "items") { CanWrite = true };
            var window = new Window { Width = 1200, Height = 600, Content = table };
            window.Show();
            Dispatcher.UIThread.RunJobs();
            AvaloniaHeadlessPlatform.ForceRenderTimerTick();
            Dispatcher.UIThread.RunJobs();
            var elapsed = Stopwatch.GetElapsedTime(started).TotalMilliseconds;
            Assert.True(table.Grid.AppliedRowCount() >= 10, "the first window must have been applied");
            window.Close();
            table.Dispose();
            if (i >= 2)
            {
                samples.Add(elapsed); // the first renders pay JIT and style loading
            }
        }

        Budgets().Assert("childtable-first-render-200-rows", PerfStatistics.From(samples).P95);
    }

    [AvaloniaFact]
    public void PasteOf200RowsIsWithinBudget()
    {
        var text = PasteText(Rows);
        SpotChildTable? table = null;
        var measurement = PerfMeter.Measure(
            () =>
            {
                var result = table!.PasteText(text);
                TestHost.Pump();
                Assert.True(result.Succeeded, result.Message);
            },
            samples: 12,
            warmup: 2,
            prepare: _ =>
            {
                table?.Dispose();
                table = Keys.Show(Fixtures.Draft(0));
            });
        Assert.Equal(Rows, table!.Model.RowCount);
        Assert.Equal("Line 200", table.Model.Text(Rows - 1, Fixtures.Desc));
        Assert.Equal(1, table.Model.Table.JournalCount);
        Budgets().Assert("childtable-paste-200-rows", measurement.WallTime.P95);
    }

    [AvaloniaFact]
    public async Task SixHundredKeystrokesOnA200RowTableStayWithinBudgetAndNoKeyIsLost()
    {
        var lookup = new FakeLookup("ITEM-A", "ITEM-B");
        var table = Keys.Show(Fixtures.Draft(Rows), lookups: _ => lookup, height: 600);
        var window = (Window)TopLevel.GetTopLevel(table)!;
        var script = new List<(string Name, Action Step)>();
        void Press(Key key, KeyModifiers modifiers = KeyModifiers.None) => script.Add(($"{key}", () => Keys.Raise(table, key, modifiers)));
        void Type(string text) => script.Add(($"type {text}", () => Keys.Type(table, text)));

        // Per row (12 steps): open the link cell, search, accept, commit; tab through the
        // description to the quantity, type 12; tab to the rate, type 9; commit and stay; go down.
        const int StepsPerRow = 12;
        var editedRows = (Steps - 24) / StepsPerRow;
        for (var row = 0; row < editedRows; row++)
        {
            Press(Key.Enter);
            Type("A");
            Press(Key.Enter);
            Press(Key.Tab);
            Press(Key.Tab);
            Type("1");
            Type("2");
            Press(Key.Tab);
            Type("9");
            Press(Key.Enter, KeyModifiers.Control);
            Press(Key.Down);
            Press(Key.Home);
        }

        // Then the tail, at the end of the table: Ctrl+End to the last cell; F2 to edit it; Enter, which
        // commits and (on the last row's last cell) adds a row and opens its first cell; Escape to leave
        // that cell; End to the last column again. Four steps per added row, until the steps are spent.
        Press(Key.End, KeyModifiers.Control);
        while (script.Count < Steps)
        {
            Press(Key.F2);
            Press(Key.Enter);
            Press(Key.Escape);
            Press(Key.End);
        }

        script.RemoveRange(Steps, script.Count - Steps);

        // The cursor starts on the first cell.
        table.Grid.SetCursor(0, 0);
        table.Grid.SetSelection([0]);
        table.Grid.Focus();
        TestHost.Pump();

        var times = new List<double>(script.Count);
        for (var i = 0; i < script.Count; i++)
        {
            // A step is the key handled and the layout pass its change asks for. Rasterizing the frame is
            // the frame budget's (WP-UI-08.T2 measures it), so the pump that paints runs outside the timer.
            var started = Stopwatch.GetTimestamp();
            script[i].Step();
            if (script[i].Name == "type A")
            {
                await table.CurrentHost!.Editor.AsLookup().PendingSearch; // completes inline with the instant delay
            }

            window.UpdateLayout();
            times.Add(Stopwatch.GetElapsedTime(started).TotalMilliseconds);
            TestHost.Pump();
        }

        Assert.Equal(Steps, script.Count);
        // No key lost: every row the script edited carries every value it typed.
        for (var row = 0; row < editedRows; row++)
        {
            Assert.Equal("ITEM-A name", table.Model.Text(row, Fixtures.Item));
            Assert.Equal("12.00", table.Model.Text(row, Fixtures.Qty));
            Assert.Equal("9.00", table.Model.Text(row, Fixtures.Rate));
        }

        Assert.True(table.Model.RowCount >= Rows + 5, $"Enter on the last cell added rows: {table.Model.RowCount} cursor {table.Grid.GridCellCursor} editing {table.Grid.EditingCell}");
        Budgets().Assert("childtable-key-step", PerfStatistics.From(times).P95);
    }
}

internal static class PerfHelpers
{
    public static SpotLookupBox AsLookup(this Control editor) => (SpotLookupBox)editor;

    public static int AppliedRowCount(this Spotledger.Client.Grid.SpotGrid grid) => grid.RowCount;
}
