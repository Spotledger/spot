using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.ChildTable.Tests;

/// <summary>Each row operation runs through the draft, so it dirties the draft, is one undo step,
/// and keeps row ids on a reorder.</summary>
public sealed class RowOperationTests
{
    private static ChildTableModel Make(int rows = 5) => Fixtures.Model(Fixtures.Draft(rows));

    [Fact]
    public void AddRowAppendsAndUndoRemovesIt()
    {
        var model = Make(3);
        var result = model.AddRow();
        Assert.Equal(4, model.RowCount);
        Assert.Equal((3, Fixtures.Item), result.Cursor);
        Assert.Equal([3], result.Selection);
        Assert.True(model.Draft.IsDirty);
        Assert.Null(model.Table.Rows[3].Id);

        Assert.True(model.Undo().Succeeded);
        Assert.Equal(3, model.RowCount);
        Assert.False(model.Draft.IsDirty);
        Assert.True(model.Redo().Succeeded);
        Assert.Equal(4, model.RowCount);
    }

    [Fact]
    public void InsertPutsTheRowAtThePositionAndUndoRestoresTheOrder()
    {
        var model = Make(3);
        var result = model.InsertRow(1);
        Assert.Equal(["r1", null, "r2", "r3"], model.Ids());
        Assert.Equal((1, Fixtures.Item), result.Cursor);

        model.Undo();
        Assert.Equal(["r1", "r2", "r3"], model.Ids());
        Assert.False(model.Draft.IsDirty);
    }

    [Fact]
    public void InsertBelowTheLastRowAppends()
    {
        var model = Make(2);
        model.InsertRow(99);
        Assert.Equal(["r1", "r2", null], model.Ids());
    }

    [Fact]
    public void DeleteRemovesRowsAsOneUndoStepAndUndoBringsBackIdsValuesAndPositions()
    {
        var model = Make(5);
        var result = model.DeleteRows([3, 1]);
        Assert.Equal(["r1", "r3", "r5"], model.Ids());
        Assert.Equal((1, -1), result.Cursor);
        Assert.Equal(1, model.Table.JournalCount);

        Assert.True(model.Undo().Succeeded);
        Assert.Equal(["r1", "r2", "r3", "r4", "r5"], model.Ids());
        Assert.Equal("2.00", model.Text(1, Fixtures.Qty));
        Assert.Equal("Line 4", model.Text(3, Fixtures.Desc));
        Assert.False(model.Draft.IsDirty);
    }

    [Fact]
    public void DeletingTheLastRowLeavesTheCursorOnTheNewLastRow()
    {
        var model = Make(3);
        var result = model.DeleteRows([2]);
        Assert.Equal((1, -1), result.Cursor);
        model.DeleteRows([0]);
        model.DeleteRows([0]);
        var empty = model.DeleteRows([0]);
        Assert.False(empty.Succeeded);
        Assert.Equal(0, model.RowCount);
    }

    [Fact]
    public void DeleteWithAnOutOfRangeRowChangesNothing()
    {
        var model = Make(3);
        Assert.Equal(ChildTableRefusal.NothingToDo, model.DeleteRows([1, 9]).Refusal);
        Assert.Equal(3, model.RowCount);
    }

    [Fact]
    public void MoveUpAndDownKeepRowIdsAndUndoRestores()
    {
        var model = Make(4);
        var up = model.MoveRows([2], -1, cursorRow: 2);
        Assert.Equal(["r1", "r3", "r2", "r4"], model.Ids());
        Assert.Equal((1, -1), up.Cursor);
        Assert.Equal([1], up.Selection);

        var down = model.MoveRows([1], 1, cursorRow: 1);
        Assert.Equal(["r1", "r2", "r3", "r4"], model.Ids());
        Assert.Equal((2, -1), down.Cursor);

        Assert.True(model.Undo().Succeeded);
        Assert.Equal(["r1", "r3", "r2", "r4"], model.Ids());
        Assert.True(model.Undo().Succeeded);
        Assert.Equal(["r1", "r2", "r3", "r4"], model.Ids());
        Assert.False(model.Draft.IsDirty);
    }

    [Fact]
    public async Task ReorderKeepsEveryRowsValuesAndIdentity()
    {
        var model = Make(4);
        var rows = model.Table.Rows.ToList();
        model.MoveRows([0, 1], 1);
        Assert.Equal(["r3", "r1", "r2", "r4"], model.Ids());
        Assert.Same(rows[0], model.Table.Rows[1]);
        Assert.Same(rows[1], model.Table.Rows[2]);
        Assert.Equal("1.00", model.Text(1, Fixtures.Qty));
        // The grid's row key is the draft row: a reorder keeps identity through the source.
        var window = await model.Source.GetWindow(0, 4, CancellationToken.None);
        Assert.Same(rows[0], window[1].Key);
    }

    [Fact]
    public void MovingASelectionAgainstTheEdgeBlocksOnlyThoseRows()
    {
        var model = Make(5);
        var result = model.MoveRows([0, 1, 3], -1);
        // Rows 0 and 1 cannot move; row 3 can.
        Assert.Equal(["r1", "r2", "r4", "r3", "r5"], model.Ids());
        Assert.Equal([0, 1, 2], result.Selection);

        var none = model.MoveRows([0], -1);
        Assert.Equal(ChildTableRefusal.NothingToDo, none.Refusal);
        Assert.Equal(["r1", "r2", "r4", "r3", "r5"], model.Ids());
    }

    [Fact]
    public void MovingSeveralRowsIsOneUndoStep()
    {
        var model = Make(5);
        model.MoveRows([1, 2], 1);
        Assert.Equal(["r1", "r4", "r2", "r3", "r5"], model.Ids());
        Assert.Equal(1, model.Table.JournalCount);
        model.Undo();
        Assert.Equal(["r1", "r2", "r3", "r4", "r5"], model.Ids());
    }

    [Fact]
    public void DuplicateCopiesEditableValuesBelowTheLastSelectedRowAsOneUndoStep()
    {
        var model = Make(4);
        var result = model.DuplicateRows([0, 1]);
        Assert.Equal(["r1", "r2", null, null, "r3", "r4"], model.Ids());
        Assert.Equal([2, 3], result.Selection);
        Assert.Equal("Line 1", model.Text(2, Fixtures.Desc));
        Assert.Equal("ITEM-2", model.Text(3, Fixtures.Item));
        Assert.Equal("2.00", model.Text(3, Fixtures.Qty));
        Assert.Equal(1, model.Table.JournalCount);
        Assert.True(model.Table.Rows[2].IsNew);

        Assert.True(model.Undo().Succeeded);
        Assert.Equal(["r1", "r2", "r3", "r4"], model.Ids());
        Assert.False(model.Draft.IsDirty);
    }

    [Fact]
    public void DuplicateDoesNotCopyReadOnlyValues()
    {
        var model = Make(2);
        model.DuplicateRows([1]);
        // amount is read-only (a server-computed value): the copy leaves it to the new row.
        Assert.Equal(string.Empty, model.Text(2, Fixtures.Amount));
    }

    [Fact]
    public void FillDownCopiesTheFirstSelectedRowIntoTheOthersAsOneUndoStep()
    {
        var model = Make(4);
        model.SetCell(0, Fixtures.Rate, new DecimalFieldValue(99.5m));
        Assert.True(model.FillDown([0, 1, 3], Fixtures.Rate).Succeeded);
        Assert.Equal("99.50", model.Text(1, Fixtures.Rate));
        Assert.Equal("99.50", model.Text(3, Fixtures.Rate));
        Assert.Equal("10.00", model.Text(2, Fixtures.Rate));

        Assert.True(model.Undo().Succeeded);
        Assert.Equal("10.00", model.Text(1, Fixtures.Rate));
        Assert.Equal("10.00", model.Text(3, Fixtures.Rate));
    }

    [Fact]
    public void FillDownWithOneRowCopiesFromTheRowAbove()
    {
        var model = Make(3);
        Assert.True(model.FillDown([2], Fixtures.Desc).Succeeded);
        Assert.Equal("Line 2", model.Text(2, Fixtures.Desc));
        Assert.Equal(ChildTableRefusal.NothingToDo, model.FillDown([0], Fixtures.Desc).Refusal);
    }

    [Fact]
    public void FillDownIntoAReadOnlyCellIsRefusedWhole()
    {
        var permissions = Fixtures.Permissions(line: """{"masked":["desc"]}""");
        var model = Fixtures.Model(Fixtures.Draft(4, permissions: permissions));
        var result = model.FillDown([0, 1, 2], Fixtures.Desc);
        Assert.Equal(ChildTableRefusal.CellMasked, result.Refusal);
        Assert.False(model.Draft.IsDirty);
    }

    [Fact]
    public void CellEditUndoRestoresTheValueAndTheCleanState()
    {
        var model = Make(2);
        Assert.True(model.SetCell(1, Fixtures.Qty, new DecimalFieldValue(5.5m)).Succeeded);
        Assert.Contains("items[2].qty", model.Draft.DirtyPaths());
        Assert.True(model.Undo().Succeeded);
        Assert.Equal("2.00", model.Text(1, Fixtures.Qty));
        Assert.False(model.Draft.IsDirty);
        Assert.True(model.Redo().Succeeded);
        Assert.Equal("5.50", model.Text(1, Fixtures.Qty));
    }

    [Fact]
    public void UndoWithNothingToUndoSaysSo()
    {
        var model = Make(2);
        Assert.Equal(ChildTableRefusal.NothingToDo, model.Undo().Refusal);
        Assert.Equal(ChildTableRefusal.NothingToDo, model.Redo().Refusal);
    }

    [Fact]
    public void NextEditableSkipsReadOnlyCellsAndWrapsRows()
    {
        var model = Make(2);
        Assert.Equal((0, Fixtures.Unit), model.NextEditable(0, Fixtures.Rate, forward: true));
        Assert.Equal((1, Fixtures.Item), model.NextEditable(0, Fixtures.Taxable, forward: true));
        Assert.Null(model.NextEditable(1, Fixtures.Taxable, forward: true));
        Assert.Equal((0, Fixtures.Rate), model.NextEditable(0, Fixtures.Unit, forward: false));
        Assert.Null(model.NextEditable(0, Fixtures.Item, forward: false));
    }
}
