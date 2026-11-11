using Avalonia.Input;
using Avalonia.Headless.XUnit;
using Spotledger.Client.Grid;
using Xunit;

namespace Spotledger.Client.Grid.Tests;

/// <summary>The keyboard table: cursor moves, selection semantics, check toggles,
/// activation, copy — driven through the real OnKeyDown path.</summary>
public sealed class KeyboardTests
{
    private static SpotGrid MakeGrid(out InMemoryGridSource source, int rows = 200)
    {
        source = new InMemoryGridSource(GridFixture.MakeRows(rows, 4));
        return GridFixtures.Make(source)
            .With(source, GridFixtures.PixelColumns(4), width: 500, height: 400)
            .Build();
    }

    [AvaloniaFact]
    public void ArrowsMoveTheCellCursor()
    {
        var grid = MakeGrid(out _);
        Press(grid, Key.Down);

        Assert.True(grid.CellCursor.HasValue);
        Assert.Equal(0, grid.CellCursor.RowIndex);
        Assert.Equal(0, grid.CellCursor.ColumnIndex);

        Press(grid, Key.Right);
        Press(grid, Key.Down);
        Press(grid, Key.Down);
        Assert.Equal(2, grid.CellCursor.RowIndex);
        Assert.Equal(1, grid.CellCursor.ColumnIndex);

        Press(grid, Key.Left);
        Assert.Equal(0, grid.CellCursor.ColumnIndex); // from col 1, Left lands on col 0
        Press(grid, Key.Up);
        Assert.Equal(1, grid.CellCursor.RowIndex); // from row 2, Up lands on row 1
    }

    [AvaloniaFact]
    public void HomeEndJumpRowStartAndRowEnd()
    {
        var grid = MakeGrid(out _);
        Press(grid, Key.Down);
        Press(grid, Key.Right);
        Press(grid, Key.Right);
        Assert.Equal(2, grid.CellCursor.ColumnIndex);

        Press(grid, Key.Home);
        Assert.Equal(0, grid.CellCursor.ColumnIndex);
        Assert.Equal(0, grid.CellCursor.RowIndex); // Home keeps the row, column 0

        Press(grid, Key.End);
        Assert.Equal(3, grid.CellCursor.ColumnIndex);
    }

    [AvaloniaFact]
    public void CtrlHomeAndCtrlEndSpanTheWholeSet()
    {
        var grid = MakeGrid(out _);
        Press(grid, Key.End, KeyModifiers.Control);
        Assert.Equal(199, grid.CellCursor.RowIndex);
        Assert.Equal(3, grid.CellCursor.ColumnIndex);

        // The view followed the cursor.
        Assert.True(grid.FirstVisibleRow >= 199 - 9, "Ctrl+End must scroll to the last page");

        Press(grid, Key.Home, KeyModifiers.Control);
        Assert.Equal(0, grid.CellCursor.RowIndex);
        Assert.Equal(0, grid.ScrollRows);
    }

    [AvaloniaFact]
    public void PageUpDownMoveByViewport()
    {
        var grid = MakeGrid(out _);
        var visible = grid.VisibleRowCount();
        Assert.True(visible > 1);

        Press(grid, Key.Down);
        Press(grid, Key.PageDown);
        Assert.Equal(visible, grid.CellCursor.RowIndex);

        Press(grid, Key.PageUp);
        Assert.Equal(0, grid.CellCursor.RowIndex);
    }

    [AvaloniaFact]
    public void ShiftDownExtendsTheSelectionRange()
    {
        var grid = MakeGrid(out _);
        Press(grid, Key.Down);
        Press(grid, Key.Down, KeyModifiers.Shift);
        Press(grid, Key.Down, KeyModifiers.Shift);

        Assert.Equal(3, grid.Selection.SelectedCount);
        Assert.Contains(0, grid.Selection.SelectedRows);
        Assert.Contains(2, grid.Selection.SelectedRows);
    }

    [AvaloniaFact]
    public void EnterRaisesRowActivated()
    {
        var grid = MakeGrid(out _);
        var activated = new List<(int Row, object Key)>();
        grid.RowActivated += (_, e) => activated.Add((e.RowIndex, e.RowKey));

        Press(grid, Key.Down);
        Press(grid, Key.Enter);

        var single = Assert.Single(activated);
        Assert.Equal(0, single.Row);
        Assert.Equal("r0", single.Key);
    }

    [AvaloniaFact]
    public void SpaceTogglesTheCheckColumn()
    {
        var grid = MakeGrid(out _);
        (grid.Columns![0] as GridColumnModel)!.IsCheckColumn = true;
        var toggles = new List<(int Row, bool Checked)>();
        grid.RowCheckedChanged += (_, e) => toggles.Add((e.RowIndex, e.IsChecked));

        Press(grid, Key.Down);
        Press(grid, Key.Space);
        Assert.Equal(1, grid.Selection.CheckedCount);
        Assert.True(grid.Selection.HasFlag(0, GridRowState.Checked));

        Press(grid, Key.Space);
        Assert.Equal(0, grid.Selection.CheckedCount);
        Assert.Equal(2, toggles.Count);
    }

    [AvaloniaFact]
    public void CtrlCCopiesSelectionAsTsv()
    {
        var grid = MakeGrid(out _);
        var sink = new RecordingClipboardSink();
        grid.ClipboardSink = sink;

        Press(grid, Key.Down);
        Press(grid, Key.Down, KeyModifiers.Shift);
        Press(grid, Key.C, KeyModifiers.Control);

        Assert.Equal(1, sink.Copies);
        var lines = sink.LastText!.Split('\n');
        Assert.Equal(2, lines.Length);
        // The check column is not present; first data column is the row label.
        Assert.StartsWith("row 0\t", lines[0]);
        Assert.StartsWith("row 1\t", lines[1]);
        // Each selected row copies its data cells tab-separated: 4 cells ⇒ 3 separators.
        Assert.Equal(3, lines[0].Count(ch => ch == '\t'));
    }

    [AvaloniaFact]
    public void TypeAheadJumpsWithinTheSortedColumn()
    {
        var source = new InMemoryGridSource(
            GridFixture.MakeRows(500, 4).Select(r => r with { }).ToList(),
            row => row.Cells[0].Text);
        var grid = GridFixtures.Make(source)
            .With(source, GridFixtures.PixelColumns(4))
            .Build();

        grid.CycleSort(0); // ascending on column 0
        TestHost.Pump();
        grid.CellCursor = new GridCursor(0, 0, GridCursorOrigin.Keyboard);

        // Type "row 199" through the real key path; type-ahead should jump there.
        foreach (var character in "row 199")
        {
            grid.RaiseEvent(new KeyEventArgs
            {
                Key = Key.None,
                KeySymbol = character.ToString(),
                RoutedEvent = InputElement.KeyDownEvent,
            });
        }

        Assert.Equal(199, grid.CellCursor.RowIndex);
    }

    /// <summary>Raises a synthesized key event through the real input pipeline.</summary>
    private static void Press(SpotGrid grid, Key key, KeyModifiers modifiers = KeyModifiers.None)
        => grid.RaiseEvent(new KeyEventArgs
        {
            Key = key,
            KeyModifiers = modifiers,
            RoutedEvent = InputElement.KeyDownEvent,
        });
}