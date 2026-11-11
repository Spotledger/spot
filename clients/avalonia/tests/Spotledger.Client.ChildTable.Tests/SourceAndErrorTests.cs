using Spotledger.Client.Api;
using Spotledger.Client.Grid;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.ChildTable.Tests;

public sealed class SourceAndErrorTests
{
    private static ApiError Server(params (string Path, string Message)[] errors) => new()
    {
        Code = "document.validation_failed",
        Message = "Validation failed",
        FieldErrors = [.. errors.Select(e => new ApiFieldError(e.Path, "document.validation_failed", e.Message))],
    };

    private static async Task<IReadOnlyList<GridRow>> Window(ChildTableModel model)
        => await model.Source.GetWindow(0, model.RowCount, CancellationToken.None);

    [Fact]
    public async Task WindowsAreSlicesOfTheRowsWithFormattedCells()
    {
        var model = Fixtures.Model(Fixtures.Draft(5));
        Assert.Equal(GridRowCount.Known(5), model.Source.Count);
        var slice = await model.Source.GetWindow(1, 2, CancellationToken.None);
        Assert.Equal(2, slice.Count);
        Assert.Equal(["ITEM-2", "Line 2", "2.00", "10.00", "20.00", "Nos", "2026-11-03", "true"], slice[0].Cells.Select(c => c.Text));
        Assert.Equal(GridCellAlignment.End, slice[0].Cells[Fixtures.Qty].Alignment);
        Assert.Equal(GridCellAlignment.Center, slice[0].Cells[Fixtures.Taxable].Alignment);
        Assert.Same(model.Table.Rows[1], slice[0].Key);
        Assert.Empty(await model.Source.GetWindow(9, 4, CancellationToken.None));
        Assert.Equal(5, (await model.Source.GetWindow(-3, 99, CancellationToken.None)).Count);
    }

    [Fact]
    public async Task NewRowsHaveAKeyAndKeepItAfterAReorder()
    {
        var model = Fixtures.Model(Fixtures.Draft(2));
        model.AddRow();
        var key = (await Window(model))[2].Key;
        model.MoveRows([2], -1);
        Assert.Same(key, (await Window(model))[1].Key);
    }

    [Fact]
    public async Task ServerCellErrorShowsAsACellAnnotationWithTheMessage()
    {
        var model = Fixtures.Model(Fixtures.Draft(3));
        ErrorMapper.Apply(model.Draft, Server(("items[2].qty", "Quantity must be positive")));
        var rows = await Window(model);
        var annotation = rows[1].Cells[Fixtures.Qty].Annotation;
        Assert.NotNull(annotation);
        Assert.Equal(GridAnnotationSeverity.Error, annotation.Severity);
        Assert.Equal("Quantity must be positive", annotation.Message);
        Assert.Null(rows[1].Cells[Fixtures.Rate].Annotation);
        Assert.Null(rows[0].Cells[Fixtures.Qty].Annotation);
        Assert.Null(rows[1].RowAnnotation);
    }

    [Fact]
    public async Task ServerRowErrorShowsAsARowAnnotation()
    {
        var model = Fixtures.Model(Fixtures.Draft(3));
        ErrorMapper.Apply(model.Draft, Server(("items[3]", "Check this row")));
        var rows = await Window(model);
        Assert.Equal("Check this row", rows[2].RowAnnotation?.Message);
        Assert.Null(rows[1].RowAnnotation);
    }

    [Fact]
    public async Task ErrorFollowsItsRowThroughAReorderAndEditingTheCellClearsIt()
    {
        var model = Fixtures.Model(Fixtures.Draft(3));
        ErrorMapper.Apply(model.Draft, Server(("items[3].desc", "Too long")));
        model.MoveRows([2], -1);
        Assert.Equal("Too long", (await Window(model))[1].Cells[Fixtures.Desc].Annotation?.Message);

        model.SetCell(1, Fixtures.Desc, new TextFieldValue("Short"));
        Assert.Null((await Window(model))[1].Cells[Fixtures.Desc].Annotation);
    }

    [Fact]
    public async Task ShowErrorsPlacesLocalErrorsAndReturnsTheOnesThatNameNoCell()
    {
        var model = Fixtures.Model(Fixtures.Draft(2));
        var changed = 0;
        model.Source.Changed += (_, _) => changed++;
        var unplaced = model.ShowErrors(
        [
            new FieldError("items[1].qty", "required", "Qty is required"),
            new FieldError("items[2]", "row", "Row is wrong"),
            new FieldError("items[9].qty", "x", "gone"),
            new FieldError("items[1].nope", "x", "no such field"),
            new FieldError("customer", "x", "not ours"),
        ]);
        Assert.Equal(["gone", "no such field", "not ours"], unplaced.Select(e => e.Message));
        var rows = await Window(model);
        Assert.Equal("Qty is required", rows[0].Cells[Fixtures.Qty].Annotation?.Message);
        Assert.Equal("Row is wrong", rows[1].RowAnnotation?.Message);
        Assert.True(changed > 0);

        model.ClearErrors();
        rows = await Window(model);
        Assert.Null(rows[0].Cells[Fixtures.Qty].Annotation);
        Assert.Null(rows[1].RowAnnotation);
    }

    [Fact]
    public async Task ACellEditRebuildsOneRowNotTheTable()
    {
        var model = Fixtures.Model(Fixtures.Draft(50));
        _ = await Window(model);
        var built = model.Source.RowsBuilt;
        model.SetCell(10, Fixtures.Qty, new DecimalFieldValue(3m));
        _ = await Window(model);
        Assert.Equal(built + 1, model.Source.RowsBuilt);
    }

    [Fact]
    public async Task EveryChangeTellsTheGridItsWindowIsStale()
    {
        var model = Fixtures.Model(Fixtures.Draft(3));
        var kinds = new List<GridChangeKind>();
        model.Source.Changed += (_, e) => kinds.Add(e.Kind);
        model.SetCell(0, Fixtures.Qty, new DecimalFieldValue(3m));
        model.AddRow();
        model.Undo();
        Assert.Equal(3, kinds.Count);
        Assert.All(kinds, kind => Assert.Equal(GridChangeKind.Reset, kind));
        Assert.Equal(3, (await Window(model)).Count);
    }

    [Fact]
    public async Task MaskedCellsShowTheServersMaskAndNeverTheStoredValue()
    {
        var permissions = Fixtures.Permissions(line: """{"masked":["rate"]}""");
        var draft = new DocumentDraft(Fixtures.Model());
        draft.ApplyPermissions(permissions);
        draft.LoadFrom("""{"items":[{"id":"r1","desc":"A","qty":"1.00","rate":"****"},{"id":"r2","desc":"B","qty":"2.00","rate":"****"}]}""");
        var model = Fixtures.Model(draft);
        var rows = await Window(model);
        Assert.Equal("****", rows[0].Cells[Fixtures.Rate].Text);
        Assert.Equal("1.00", rows[0].Cells[Fixtures.Qty].Text);
        // The masked value is not a value: the draft holds null, and a copy or total cannot see it.
        Assert.True(draft.Table("items").Rows[0].GetValue("rate")!.IsNull);
    }

    [Fact]
    public async Task ARuleChangedValueRefreshesItsRow()
    {
        var line = new DocTypeModel("line", "Line", 1, false,
        [
            new FieldModel("qty", "Qty", FieldKind.DecimalNumber, scale: 2, precision: 18),
            new FieldModel("rate", "Rate", FieldKind.DecimalNumber, scale: 2, precision: 18),
            new FieldModel("amount", "Amount", FieldKind.DecimalNumber, scale: 2, precision: 18, readOnly: true, defaultExpr: "doc.qty * doc.rate"),
        ], exprVersion: 1);
        var parent = new DocTypeModel("invoice", "Invoice", 1, false,
            [new FieldModel("items", "Items", FieldKind.Table, childDoctype: "line")],
            new Dictionary<string, DocTypeModel> { ["items"] = line }, exprVersion: 1);
        var draft = new DocumentDraft(parent);
        var model = new ChildTableModel(draft, "items") { CanWrite = true };
        // Whatever the rule engine does with the expression, the cell text must equal the draft's value.
        model.AddRow();
        model.SetCell(0, 0, new DecimalFieldValue(2m));
        model.SetCell(0, 1, new DecimalFieldValue(3m));
        var cell = (await Window(model))[0].Cells[2].Text;
        Assert.Equal(CellFormatter.Format(line.Fields[2], draft.Table("items").Rows[0].GetValue("amount")), cell);
    }
}
