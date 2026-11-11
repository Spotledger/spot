using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.ChildTable.Tests;

public sealed class TotalsTests
{
    private static ChildTableView View() => new(
    [
        new ChildTableColumn("item", Total: ChildTableTotal.Count),
        new ChildTableColumn("desc"),
        new ChildTableColumn("qty", Total: ChildTableTotal.Sum),
        new ChildTableColumn("rate", Total: ChildTableTotal.Average),
        new ChildTableColumn("amount", Total: ChildTableTotal.Sum),
    ]);

    [Fact]
    public void SumCountAndAverageComeFromTheViewDefinition()
    {
        var model = Fixtures.Model(Fixtures.Draft(4), view: View());
        Assert.True(model.Totals.HasTotals);
        Assert.Equal("4", model.Totals.TextFor(0));
        Assert.Equal(string.Empty, model.Totals.TextFor(1));
        Assert.Equal("10.00", model.Totals.TextFor(2));   // 1+2+3+4
        Assert.Equal("10.00", model.Totals.TextFor(3));   // average of four 10.00
        Assert.Equal("100.00", model.Totals.TextFor(4));  // 10+20+30+40
    }

    [Fact]
    public void ACellEditUpdatesTheTotalWithoutRecomputingTheTable()
    {
        var model = Fixtures.Model(Fixtures.Draft(100), view: View());
        var before = model.Totals.ContributionsComputed;
        model.SetCell(5, Fixtures.Qty, new DecimalFieldValue(100.25m));
        Assert.Equal(5050m + 100.25m - 6m, decimal.Parse(model.Totals.TextFor(2), System.Globalization.CultureInfo.InvariantCulture));
        // One cell: the qty total reads one contribution, nothing else is touched.
        Assert.Equal(before + 1, model.Totals.ContributionsComputed);
    }

    [Fact]
    public void TotalsFollowAddDeleteAndUndo()
    {
        var model = Fixtures.Model(Fixtures.Draft(3), view: View());
        Assert.Equal("6.00", model.Totals.TextFor(2));
        model.AddRow();
        model.SetCell(3, Fixtures.Qty, new DecimalFieldValue(4m));
        Assert.Equal("10.00", model.Totals.TextFor(2));
        model.DeleteRows([0]);
        Assert.Equal("9.00", model.Totals.TextFor(2));
        model.Undo();
        Assert.Equal("10.00", model.Totals.TextFor(2));
        model.Undo();
        model.Undo();
        Assert.Equal("6.00", model.Totals.TextFor(2));
    }

    [Fact]
    public void TotalsChangedIsRaisedOnlyWhenADeclaredColumnChanges()
    {
        var model = Fixtures.Model(Fixtures.Draft(3), view: View());
        var raised = 0;
        model.TotalsChanged += (_, _) => raised++;
        model.SetCell(0, Fixtures.Desc, new TextFieldValue("x"));
        Assert.Equal(0, raised);
        model.SetCell(0, Fixtures.Qty, new DecimalFieldValue(9m));
        Assert.Equal(1, raised);
    }

    [Fact]
    public void SumsAreExactWhereBinaryFloatingPointIsNot()
    {
        var draft = Fixtures.Draft(0);
        var model = Fixtures.Model(draft, view: View());
        model.Paste("a\tx\t0.10\t0.10\nb\ty\t0.20\t0.20\nc\tz\t0.30\t0.30", 0, Fixtures.Item);
        Assert.Equal("0.60", model.Totals.TextFor(2));
        Assert.Equal("0.20", model.Totals.TextFor(3));
    }

    [Fact]
    public void AverageRoundsHalfAwayFromZeroAtTheFieldScaleForDisplayOnly()
    {
        var draft = Fixtures.Draft(0);
        var model = Fixtures.Model(draft, view: View());
        model.Paste("a\tx\t1\t0.01\nb\tx\t1\t0.02", 0, Fixtures.Item);
        Assert.Equal("0.02", model.Totals.TextFor(3));   // 0.015 -> 0.02
        model.DeleteRows([0, 1]);
        model.Paste("a\tx\t1\t-0.01\nb\tx\t1\t-0.02", 0, Fixtures.Item);
        Assert.Equal("-0.02", model.Totals.TextFor(3));
        // The stored values are untouched by the display rounding.
        Assert.Equal("-0.01", model.Text(0, Fixtures.Rate));
    }

    [Fact]
    public void AverageOfNothingIsEmptyAndCountIgnoresEmptyCells()
    {
        var model = Fixtures.Model(Fixtures.Draft(0), view: View());
        Assert.Equal(string.Empty, model.Totals.TextFor(3));
        Assert.Equal("0", model.Totals.TextFor(0));
        model.AddRow();
        model.AddRow();
        model.SetCell(0, Fixtures.Item, new LinkFieldValue("A", "A"));
        Assert.Equal("1", model.Totals.TextFor(0));
    }

    [Fact]
    public void WideDecimalsBeyondSystemDecimalAreSummedExactly()
    {
        var line = new DocTypeModel("line", "Line", 1, false,
            [new FieldModel("big", "Big", FieldKind.DecimalNumber, scale: 10, precision: 38)]);
        var parent = new DocTypeModel("invoice", "Invoice", 1, false,
            [new FieldModel("items", "Items", FieldKind.Table, childDoctype: "line")],
            new Dictionary<string, DocTypeModel> { ["items"] = line });
        var draft = new DocumentDraft(parent);
        var model = new ChildTableModel(draft, "items", new ChildTableView([new ChildTableColumn("big", Total: ChildTableTotal.Sum)]))
        {
            CanWrite = true,
        };
        var result = model.Paste("99999999999999999999999999.9999999999\n0.0000000001", 0, 0);
        Assert.True(result.Succeeded, result.Message);
        Assert.Equal("100000000000000000000000000.0000000000", model.Totals.TextFor(0));
    }

    [Fact]
    public void FooterCarriesTheLabelInTheFirstColumnUnlessItHasATotal()
    {
        var model = Fixtures.Model(Fixtures.Draft(2), view: new ChildTableView(
            [new ChildTableColumn("desc"), new ChildTableColumn("qty", Total: ChildTableTotal.Sum)]));
        var footer = model.Totals.BuildFooter("Total");
        Assert.Equal(["Total", "3.00"], footer.Cells.Select(cell => cell.Text));

        var counted = Fixtures.Model(Fixtures.Draft(2), view: View());
        Assert.Equal("2", counted.Totals.BuildFooter("Total").Cells[0].Text);
    }

    [Fact]
    public void ViewNamingAFieldTheRowModelLacksIsRefused()
    {
        var draft = Fixtures.Draft(1);
        Assert.Throws<ArgumentException>(() => new ChildTableModel(draft, "items", new ChildTableView([new ChildTableColumn("nope")])));
    }
}
