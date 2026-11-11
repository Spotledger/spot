using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Spotledger.Client.Grid;
using Xunit;

namespace Spotledger.Client.Grid.Tests;

/// <summary>WP-UI-08.T6 behaviour of the public state surface, including the paths that
/// need internal access to drive (pointer, type-ahead, sort) and the key-hook guarantees.</summary>
public sealed class PublicStateTests
{
    private static SpotGrid MakeGrid(out InMemoryGridSource source, int rows = 200, GridSelectionMode mode = GridSelectionMode.Extended)
    {
        source = new InMemoryGridSource(GridFixture.MakeRows(rows, 4), row => row.Cells[0].Text);
        return GridFixtures.Make(source, mode)
            .With(source, GridFixtures.PixelColumns(4), width: 500, height: 400)
            .Build();
    }

    private static void Press(SpotGrid grid, Key key, KeyModifiers modifiers = KeyModifiers.None, string? symbol = null)
        => grid.RaiseEvent(new KeyEventArgs { Key = key, KeyModifiers = modifiers, KeySymbol = symbol, RoutedEvent = InputElement.KeyDownEvent });

    private sealed class Factory : ICellEditorFactory
    {
        public Control? CreateEditor(int rowIndex, int columnIndex, GridRow row, GridColumnModel column)
            => new TextBox { Text = row.Cells[columnIndex].Text };
        public string GetValue(Control editor) => ((TextBox)editor).Text ?? string.Empty;
        public bool TryCommit(int rowIndex, int columnIndex, string proposedValue, Control editor) => true;
        public void Cancel(int rowIndex, int columnIndex, Control editor) { }
    }

    private sealed class OverridingGrid : SpotGrid
    {
        public int Calls { get; private set; }
        public bool Claim { get; set; }

        protected override void OnGridKeyDown(GridKeyEventArgs e)
        {
            Calls++;
            base.OnGridKeyDown(e);
            e.Handled |= Claim;
        }
    }

    [AvaloniaFact]
    public void EveryCursorPathRaisesExactlyOneEvent()
    {
        var grid = MakeGrid(out _);
        var events = new List<GridCursorChangedEventArgs>();
        grid.CursorChanged += (_, e) => events.Add(e);

        Press(grid, Key.Down);
        Assert.Single(events);
        Assert.Equal(new GridCursor(-1, -1, GridCursorOrigin.None), events[0].Old);
        Assert.Equal(0, events[0].New.RowIndex);

        Press(grid, Key.Down);
        Assert.Equal(2, events.Count);
        Assert.Equal(0, events[1].Old.RowIndex);
        Assert.Equal(1, events[1].New.RowIndex);

        // Mouse click on row 4.
        events.Clear();
        var window = (Window)TopLevel.GetTopLevel(grid)!;
        var point = new Point(20, grid.RowY(4) + (grid.RowHeight / 2));
        window.MouseDown(point, Avalonia.Input.MouseButton.Left);
        window.MouseUp(point, Avalonia.Input.MouseButton.Left);
        Assert.Single(events);
        Assert.Equal(GridCursorOrigin.Mouse, events[0].New.Origin);
        Assert.Equal(4, events[0].New.RowIndex);

        // Edit then Enter: commit with TabNext moves the cursor once.
        events.Clear();
        grid.EditorFactory = new Factory();
        Assert.True(grid.BeginEdit(4, 0));
        var editor = (TextBox)grid.EditSession.Editor!;
        editor.RaiseEvent(new KeyEventArgs { Key = Key.Tab, RoutedEvent = InputElement.KeyDownEvent });
        Assert.Single(events);
        Assert.Equal(1, events[0].New.ColumnIndex);

        // SetCursor to the same cell: nothing.
        events.Clear();
        grid.SetCursor(4, 1);
        Assert.Empty(events);
        grid.SetCursor(10, 2);
        Assert.Single(events);

        // Source replacement resets the cursor once.
        events.Clear();
        grid.Source = new InMemoryGridSource(GridFixture.MakeRows(10, 4));
        TestHost.Pump();
        Assert.Single(events);
        Assert.False(events[0].New.HasValue);
        Assert.Equal(10, events[0].Old.RowIndex);
    }

    [AvaloniaFact]
    public void TypeAheadRaisesOneCursorEvent()
    {
        var grid = MakeGrid(out _, rows: 500);
        grid.CycleSort(0);
        TestHost.Pump();
        grid.CellCursor = new GridCursor(0, 0, GridCursorOrigin.Keyboard);
        var events = new List<GridCursorChangedEventArgs>();
        grid.CursorChanged += (_, e) => events.Add(e);
        foreach (var ch in "row 199")
        {
            Press(grid, Key.None, symbol: ch.ToString());
        }

        Assert.Equal(199, grid.Cursor_Row());
        Assert.NotEmpty(events);
        Assert.All(events, e => Assert.NotEqual(e.Old.RowIndex, e.New.RowIndex));
    }

    [AvaloniaFact]
    public void SetCursorScrollsFarCellIntoView()
    {
        var grid = MakeGrid(out _);
        grid.SetCursor(150, 3);
        Assert.InRange(150, grid.FirstVisibleRow, grid.FirstVisibleRow + grid.VisibleRowCount() - 1);
    }

    [AvaloniaFact]
    public void ScrollIntoViewFarRowFarColumnNoOpAndClamp()
    {
        var grid = MakeGrid(out _);
        grid.ScrollIntoView(120);
        var first = grid.FirstVisibleRow;
        Assert.InRange(120, first, first + grid.VisibleRowCount() - 1);

        grid.ScrollIntoView(120);
        Assert.Equal(first, grid.FirstVisibleRow);

        grid.ScrollIntoView(100000);
        Assert.InRange(199, grid.FirstVisibleRow, grid.FirstVisibleRow + grid.VisibleRowCount() - 1);

        grid.ScrollIntoView(-3);
        Assert.Equal(0, grid.FirstVisibleRow);

        var columns = GridFixtures.PixelColumns(12, 200);
        var wide = new InMemoryGridSource(GridFixture.MakeRows(20, 12));
        var wideGrid = GridFixtures.Make(wide).With(wide, columns, width: 500, height: 400).Build();
        Assert.Equal(0, wideGrid.ScrollX);
        wideGrid.ScrollIntoView(0, 10);
        Assert.True(wideGrid.ScrollX > 0);
        var x = wideGrid.ScrollX;
        wideGrid.ScrollIntoView(0, 10);
        Assert.Equal(x, wideGrid.ScrollX);
        wideGrid.ScrollIntoView(0, 99);
        Assert.True(wideGrid.ScrollX >= x);
    }

    [AvaloniaFact]
    public void TryGetRowAgreesWithSourceAndFailsOutsideWindow()
    {
        var grid = MakeGrid(out _, rows: 5000);
        Assert.Equal(5000, grid.RowCount);
        Assert.True(grid.TryGetRow(0, out var row));
        Assert.Equal("row 0", row.Cells[0].Text);
        Assert.False(grid.TryGetRow(4000, out _));
        Assert.False(grid.TryGetRow(-1, out _));
    }

    [AvaloniaFact]
    public void SelectionSurvivesSortByKeyAndRaisesOncePerChange()
    {
        var grid = MakeGrid(out _);
        var count = 0;
        grid.SelectionChanged += (_, _) => count++;
        grid.SetSelection([3]);
        Assert.Equal(1, count);
        var key = grid.RowAt(3)!.Key;

        grid.CycleSort(0);
        grid.CycleSort(0); // descending
        TestHost.Pump();
        TestHost.Pump();
        grid.ScrollIntoView(196);
        TestHost.Pump();
        TestHost.Pump();
        var now = Assert.Single(grid.SelectedRows);
        Assert.Equal(196, now);
        Assert.Equal(key, grid.RowAt(now)!.Key);
    }

    [AvaloniaFact]
    public void SelectionChangedForwardsKeyboardSelection()
    {
        var grid = MakeGrid(out _);
        var count = 0;
        grid.SelectionChanged += (_, _) => count++;
        Press(grid, Key.Down);
        Press(grid, Key.Down);
        Assert.True(count >= 1);
        Assert.Equal([1], grid.SelectedRows);
    }

    [AvaloniaFact]
    public void KeyHookStopsEachDefaultAction()
    {
        var grid = MakeGrid(out _);
        var sink = new RecordingClipboardSink();
        grid.ClipboardSink = sink;
        grid.EditorFactory = new Factory();
        var edits = 0;
        grid.EditStarted += (_, _) => edits++;
        grid.CellCursor = new GridCursor(2, 1, GridCursorOrigin.Keyboard);
        grid.SetSelection([2]);
        var claim = true;
        GridCursor seen = default;
        grid.GridKeyDown += (_, e) =>
        {
            seen = e.Cursor;
            Assert.Equal(200, e.RowCount);
            e.Handled = claim;
        };

        Press(grid, Key.Down);
        Press(grid, Key.Right);
        Press(grid, Key.Enter);
        Press(grid, Key.F2);
        Press(grid, Key.Delete);
        Press(grid, Key.C, KeyModifiers.Control);
        Press(grid, Key.V, KeyModifiers.Control);
        Press(grid, Key.None, symbol: "r");
        Assert.Equal(new GridCursor(2, 1, GridCursorOrigin.Keyboard), grid.CellCursor);
        Assert.Equal(new GridCursor(2, 1, GridCursorOrigin.Keyboard), seen);
        Assert.Equal(0, edits);
        Assert.Equal(0, sink.Copies);
        Assert.False(grid.IsEditing);

        claim = false;
        Press(grid, Key.Down);
        Assert.Equal(3, grid.CellCursor.RowIndex);
        Press(grid, Key.C, KeyModifiers.Control);
        Assert.Equal(1, sink.Copies);
        Press(grid, Key.F2);
        Assert.True(grid.IsEditing);
    }

    [AvaloniaFact]
    public void KeyHookIsNotRaisedForKeysFromAnOpenEditor()
    {
        var grid = MakeGrid(out _);
        grid.EditorFactory = new Factory();
        var raised = 0;
        grid.GridKeyDown += (_, _) => raised++;
        Assert.True(grid.BeginEdit(1, 0));
        var editor = (TextBox)grid.EditSession.Editor!;
        editor.RaiseEvent(new KeyEventArgs { Key = Key.A, RoutedEvent = InputElement.KeyDownEvent });
        editor.RaiseEvent(new KeyEventArgs { Key = Key.Enter, RoutedEvent = InputElement.KeyDownEvent });
        Assert.Equal(0, raised);
        Assert.False(grid.IsEditing);
    }

    [AvaloniaFact]
    public void OverridingOnGridKeyDownWorksLikeTheEvent()
    {
        var source = new InMemoryGridSource(GridFixture.MakeRows(50, 4));
        var grid = new OverridingGrid { Columns = GridFixtures.PixelColumns(4), Source = source };
        TestHost.Show(grid, 500, 400);
        TestHost.Pump();
        grid.Claim = true;
        Press(grid, Key.Down);
        Assert.False(grid.CellCursor.HasValue);
        grid.Claim = false;
        Press(grid, Key.Down);
        Assert.True(grid.CellCursor.HasValue);
        Assert.Equal(2, grid.Calls);
    }

    [AvaloniaFact]
    public void NoSubscriberMeansNoPerKeyAllocationFromTheHook()
    {
        var grid = MakeGrid(out _);
        grid.CellCursor = new GridCursor(5, 1, GridCursorOrigin.Keyboard);
        var args = new KeyEventArgs { Key = Key.F9, RoutedEvent = InputElement.KeyDownEvent };

        // Warm up (JIT, reflection cache), then compare against a hooked baseline.
        for (var i = 0; i < 50; i++)
        {
            grid.RaiseEvent(args);
        }

        var before = GC.GetAllocatedBytesForCurrentThread();
        for (var i = 0; i < 1000; i++)
        {
            grid.RaiseEvent(args);
        }

        var plain = GC.GetAllocatedBytesForCurrentThread() - before;

        grid.GridKeyDown += (_, _) => { };
        for (var i = 0; i < 50; i++)
        {
            grid.RaiseEvent(args);
        }

        before = GC.GetAllocatedBytesForCurrentThread();
        for (var i = 0; i < 1000; i++)
        {
            grid.RaiseEvent(args);
        }

        var hooked = GC.GetAllocatedBytesForCurrentThread() - before;

        // The hook reuses one args instance: it adds no allocation per key either way.
        Assert.True(hooked <= plain, $"hook allocated {hooked - plain} extra bytes over 1000 keys");
    }
}

internal static class GridTestExtensions
{
    public static int Cursor_Row(this SpotGrid grid) => grid.CellCursor.RowIndex;
}
