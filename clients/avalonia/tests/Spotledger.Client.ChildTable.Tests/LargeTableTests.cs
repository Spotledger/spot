using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Xunit;

namespace Spotledger.Client.ChildTable.Tests;

/// <summary>The grid shows a window of rows; the table is held whole. Operations on rows far from
/// the first window must still act on exactly the rows asked for. (Row i, counted from 0, has the id
/// "r" + (i + 1).)</summary>
public sealed class LargeTableTests
{
    private static SpotChildTable Make(int rows) => Keys.Show(Fixtures.Draft(rows), height: 400);

    private static void Select(SpotChildTable table, int cursorRow, params int[] rows)
    {
        table.Grid.ScrollIntoView(cursorRow, 0);
        table.Grid.SetCursor(cursorRow, Fixtures.Desc);
        table.Grid.SetSelection(rows);
        table.Grid.Focus();
        TestHost.Pump();
    }

    [AvaloniaFact]
    public void TheHeaderCannotReorderTheRows()
    {
        var table = Make(5);
        Assert.All(table.Grid.Columns!, column => Assert.False(column.IsSortable));
    }

    [AvaloniaFact]
    public void ASelectionFarFromTheFirstWindowIsTheRowsSelected()
    {
        var table = Make(600);
        Select(table, 594, 593, 594, 595);
        Assert.Equal([593, 594, 595], table.Grid.SelectedRows);
    }

    [AvaloniaFact]
    public void MovingRowsFarDownMovesExactlyThoseRowsAndKeepsThemSelected()
    {
        var table = Make(600);
        Select(table, 594, 593, 594, 595);
        Keys.Press(table, Key.Down, KeyModifiers.Alt);
        Assert.Equal(["r597", "r594", "r595", "r596"], table.Model.Ids().Skip(593).Take(4));
        Assert.Equal([594, 595, 596], table.Grid.SelectedRows);
        Assert.Equal(595, table.Grid.GridCellCursor.RowIndex);
    }

    [AvaloniaFact]
    public void DeletingRowsFarDownDeletesExactlyThoseRowsAndUndoRestoresThem()
    {
        var table = Make(600);
        Select(table, 550, 550, 551, 553);
        Keys.Press(table, Key.Delete, KeyModifiers.Control);
        Assert.Equal(597, table.Model.RowCount);
        Assert.Equal(["r550", "r553", "r555"], table.Model.Ids().Skip(549).Take(3));
        Assert.Equal([550], table.Grid.SelectedRows);

        Keys.Press(table, Key.Z, KeyModifiers.Control);
        Assert.Equal(600, table.Model.RowCount);
        Assert.Equal(["r550", "r551", "r552", "r553", "r554", "r555"], table.Model.Ids().Skip(549).Take(6));
    }

    [AvaloniaFact]
    public void DuplicatingAndInsertingFarDownLandsBelowTheRightRow()
    {
        var table = Make(600);
        Select(table, 580, 580);
        Keys.Press(table, Key.D, KeyModifiers.Control);
        Assert.Equal(["r581", null, "r582"], table.Model.Ids().Skip(580).Take(3));
        Assert.Equal([581], table.Grid.SelectedRows);
        Keys.Press(table, Key.I, KeyModifiers.Control);
        Assert.Equal(["r581", null, null, "r582"], table.Model.Ids().Skip(580).Take(4));
        Assert.Equal(581, table.Grid.GridCellCursor.RowIndex);
    }

    [AvaloniaFact]
    public void EveryRowOperationIsOneUndoStepAndUndoRestoresTheRowsExactly()
    {
        var table = Make(30);
        var original = table.Model.Ids();
        var operations = new (string Name, Action Run)[]
        {
            ("insert", () => Keys.Press(table, Key.I, KeyModifiers.Control)),
            ("insert below", () => Keys.Press(table, Key.I, KeyModifiers.Control | KeyModifiers.Shift)),
            ("delete", () => Keys.Press(table, Key.Delete, KeyModifiers.Control)),
            ("move up", () => Keys.Press(table, Key.Up, KeyModifiers.Alt)),
            ("move down", () => Keys.Press(table, Key.Down, KeyModifiers.Alt)),
            ("duplicate", () => Keys.Press(table, Key.D, KeyModifiers.Control)),
            ("fill down", () => Keys.Press(table, Key.D, KeyModifiers.Control | KeyModifiers.Shift)),
        };
        foreach (var (name, run) in operations)
        {
            Select(table, 10, 10);
            run();
            Assert.True(table.Model.Table.CanUndo, name);
            Keys.Press(table, Key.Z, KeyModifiers.Control);
            Assert.False(table.Model.Table.CanUndo, $"{name} is one undo step");
            Assert.Equal(original, table.Model.Ids());
            Assert.False(table.Model.Draft.IsDirty, name);
        }
    }
}
