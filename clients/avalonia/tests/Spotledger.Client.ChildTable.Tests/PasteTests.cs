using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.ChildTable.Tests;

public sealed class PasteTests
{
    private static ChildTableModel Make(int rows = 3) => Fixtures.Model(Fixtures.Draft(rows));

    [Fact]
    public void TsvParsesRowsCellsAndQuoting()
    {
        var rows = ChildTableTsv.Parse("a\tb\r\nc\t\"d\te\"\n\"x \"\"q\"\"\ny\"\tz\n");
        Assert.Equal(3, rows.Count);
        Assert.Equal(["a", "b"], rows[0]);
        Assert.Equal(["c", "d\te"], rows[1]);
        Assert.Equal(["x \"q\"\ny", "z"], rows[2]);
    }

    [Fact]
    public void TsvKeepsEmptyCellsAndDropsOnlyTheTrailingNewline()
    {
        Assert.Equal([["a", "", "c"]], ChildTableTsv.Parse("a\t\tc"));
        Assert.Equal([["a", ""]], ChildTableTsv.Parse("a\t\n"));
        Assert.Empty(ChildTableTsv.Parse(string.Empty));
        Assert.Equal([["a"], [""], ["b"]], ChildTableTsv.Parse("a\n\nb"));
    }

    [Fact]
    public void PasteOverwritesFromTheCursorAndParsesPerFieldType()
    {
        var model = Make(3);
        var result = model.Paste("Pasted\t7.50\t12\n\t8\t", 1, Fixtures.Desc);
        Assert.True(result.Succeeded);
        Assert.Equal("Pasted", model.Text(1, Fixtures.Desc));
        Assert.Equal("7.50", model.Text(1, Fixtures.Qty));
        Assert.Equal("12.00", model.Text(1, Fixtures.Rate));
        Assert.Equal(string.Empty, model.Text(2, Fixtures.Desc));
        Assert.Equal("8.00", model.Text(2, Fixtures.Qty));
        Assert.Equal(0, result.Paste!.RowsCreated);
        Assert.Equal((2, Fixtures.Desc), result.Cursor);
        Assert.Equal([1, 2], result.Selection);
    }

    [Fact]
    public void PasteBeyondTheEndAppendsNewRows()
    {
        var model = Make(2);
        var result = model.Paste("A\t1\nB\t2\nC\t3", 1, Fixtures.Desc);
        Assert.True(result.Succeeded);
        Assert.Equal(4, model.RowCount);
        Assert.Equal(2, result.Paste!.RowsCreated);
        Assert.Equal("C", model.Text(3, Fixtures.Desc));
        Assert.True(model.Table.Rows[3].IsNew);
    }

    [Fact]
    public void PasteIsOneUndoStepAndOneNotification()
    {
        var model = Make(2);
        var events = new List<StructureChangedEventArgs>();
        model.Draft.StructureChanged += (_, e) => events.Add(e);
        model.Paste("A\t1\nB\t2\nC\t3", 1, Fixtures.Desc);
        Assert.Equal(StructureChangeKind.BulkOperation, Assert.Single(events).Kind);
        Assert.Equal(1, model.Table.JournalCount);

        Assert.True(model.Undo().Succeeded);
        Assert.Equal(2, model.RowCount);
        Assert.Equal("Line 2", model.Text(1, Fixtures.Desc));
        Assert.False(model.Draft.IsDirty);
    }

    [Fact]
    public void PasteWithBadCellsAppliesNothingAndListsEveryError()
    {
        var model = Make(3);
        var result = model.Paste("X\tabc\t1.234\nY\t5\t6\nZ\t7\tbroken", 0, Fixtures.Desc);
        Assert.False(result.Succeeded);
        Assert.Equal(ChildTableRefusal.Invalid, result.Refusal);
        var report = result.Paste!;
        Assert.False(report.Applied);
        Assert.Equal(3, report.Errors.Count);
        Assert.Equal((1, "qty", "abc"), (report.Errors[0].Row, report.Errors[0].Column, report.Errors[0].Text));
        Assert.Equal((1, "rate", "1.234"), (report.Errors[1].Row, report.Errors[1].Column, report.Errors[1].Text));
        Assert.Equal((3, "rate", "broken"), (report.Errors[2].Row, report.Errors[2].Column, report.Errors[2].Text));
        Assert.All(report.Errors, error => Assert.False(string.IsNullOrEmpty(error.Message)));
        // Not one cell was written, not even the good ones.
        Assert.False(model.Draft.IsDirty);
        Assert.Equal("Line 1", model.Text(0, Fixtures.Desc));
        Assert.Equal(0, model.Table.JournalCount);
    }

    [Fact]
    public void PasteWithMoreColumnsThanTheTableIsAnError()
    {
        var model = Make(2);
        var result = model.Paste("true\textra", 0, Fixtures.Taxable);
        Assert.False(result.Succeeded);
        var error = Assert.Single(result.Paste!.Errors);
        Assert.Null(error.Column);
        Assert.Equal("extra", error.Text);
        Assert.False(model.Draft.IsDirty);
    }

    [Fact]
    public void PasteParsesDatesBoolsAndSelects()
    {
        var model = Make(2);
        var ok = model.Paste("Nos\t2027-01-31\tfalse", 0, Fixtures.Unit);
        Assert.True(ok.Succeeded);
        Assert.Equal("2027-01-31", model.Text(0, Fixtures.Due));
        Assert.Equal("false", model.Text(0, Fixtures.Taxable));

        var bad = model.Paste("Crate\t31/01/2027\tyes", 1, Fixtures.Unit);
        Assert.False(bad.Succeeded);
        Assert.Equal(["unit", "due", "taxable"], bad.Paste!.Errors.Select(error => error.Column));
    }

    [Fact]
    public void PasteAcceptsThousandsSeparatorsInNumbers()
    {
        var model = Make(1);
        Assert.True(model.Paste("1,234.50", 0, Fixtures.Qty).Succeeded);
        Assert.Equal("1234.50", model.Text(0, Fixtures.Qty));
    }

    [Fact]
    public void PasteOfAnEmptyCellClearsIt()
    {
        var model = Make(1);
        Assert.True(model.Paste("\t", 0, Fixtures.Desc).Succeeded);
        Assert.Equal(string.Empty, model.Text(0, Fixtures.Desc));
        Assert.Equal(string.Empty, model.Text(0, Fixtures.Qty));
    }

    [Fact]
    public void PasteOfTheSameValuesWritesAndJournalsNothing()
    {
        var model = Make(2);
        var result = model.Paste("Line 1\t1.00", 0, Fixtures.Desc);
        Assert.True(result.Succeeded);
        Assert.Equal(0, result.Paste!.CellsWritten);
        Assert.False(model.Draft.IsDirty);
        Assert.Equal(0, model.Table.JournalCount);
    }

    [Fact]
    public void PasteLinkTextIsTheLinkId()
    {
        var model = Make(1);
        Assert.True(model.Paste("ITEM-9", 0, Fixtures.Item).Succeeded);
        var link = Assert.IsType<LinkFieldValue>(model.Table.Rows[0].GetValue("item"));
        Assert.Equal("ITEM-9", link.Id);
    }

    [Fact]
    public void PasteTooManyRowsIsRefused()
    {
        var model = Make(1);
        var text = string.Join('\n', Enumerable.Repeat("x", ChildTableModel.MaxPasteRows + 1));
        var result = model.Paste(text, 0, Fixtures.Desc);
        Assert.False(result.Succeeded);
        Assert.Equal(1, model.RowCount);
    }

    [Fact]
    public void PasteIntoAnEmptyClipboardIsNothingToDo()
    {
        var model = Make(1);
        Assert.Equal(ChildTableRefusal.NothingToDo, model.Paste(string.Empty, 0, 0).Refusal);
    }
}
