using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Avalonia.VisualTree;
using Spotledger.Client.Controls;
using Spotledger.Client.Grid;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.ChildTable.Tests;

/// <summary>The keyboard table, through the real key pipeline: key events raised on the focused
/// element, routed to the grid or to the open cell editor.</summary>
public sealed class KeyboardTests
{
    private static SpotChildTable Make(int rows = 5, bool canWrite = true, Func<ChildTableLookupRequest, ILookupSource?>? lookups = null)
        => Keys.Show(Fixtures.Draft(rows), canWrite, lookups: lookups);

    private static void Go(SpotChildTable table, int row, int column)
    {
        table.Grid.SetCursor(row, column);
        table.Grid.SetSelection([row]);
        table.Grid.Focus();
        TestHost.Pump();
    }

    private static TextBox ActiveText(SpotChildTable table)
        => Assert.IsAssignableFrom<TextBox>(TopLevel.GetTopLevel(table)!.FocusManager!.GetFocusedElement());

    private static void TypeText(SpotChildTable table, string text)
    {
        foreach (var ch in text)
        {
            Keys.Type(table, ch.ToString());
        }
    }

    [AvaloniaFact]
    public void EnterOpensAnEditorOnAnEditableCellAndEscapeCancelsIt()
    {
        var table = Make();
        Go(table, 1, Fixtures.Qty);
        Keys.Press(table, Key.Enter);
        Assert.True(table.Grid.IsEditing);
        Assert.Equal((1, Fixtures.Qty), table.Grid.EditingCell);
        Assert.IsType<CellEditorHost>(FindHost(table));

        TypeText(table, "9");
        Keys.Press(table, Key.Escape);
        Assert.False(table.Grid.IsEditing);
        Assert.Equal("2.00", table.Model.Text(1, Fixtures.Qty));
        Assert.False(table.Model.Draft.IsDirty);
    }

    [AvaloniaFact]
    public void AnEditorOpenedByEnterIsInTheTablesVisualTreeAndFocused()
    {
        var table = Make();
        Go(table, 1, Fixtures.Qty);
        Keys.Press(table, Key.Enter);

        var host = Assert.IsType<CellEditorHost>(table.CurrentHost);
        Assert.True(table.IsVisualAncestorOf(host), "the editor host must be in the table's visual tree");
        Assert.True(table.Grid.IsVisualAncestorOf(host), "the editor host must be in the grid's overlay layer");
        var focused = TopLevel.GetTopLevel(table)!.FocusManager!.GetFocusedElement();
        Assert.True(
            focused is Avalonia.Visual visual && (ReferenceEquals(visual, host) || host.IsVisualAncestorOf(visual)),
            "keyboard focus must be inside the editor host");

        Keys.Press(table, Key.Escape);
        Assert.False(table.IsVisualAncestorOf(host), "the editor host leaves the tree when the edit ends");
    }

    private static CellEditorHost? FindHost(SpotChildTable table)
        => table.CurrentHost;

    [AvaloniaFact]
    public void EditingADecimalSelectsOnEntryRightAlignsAndCommitsWithEnterThenGoesDown()
    {
        var table = Make();
        Go(table, 1, Fixtures.Qty);
        Keys.Press(table, Key.Enter);
        var box = ActiveText(table);
        Assert.IsAssignableFrom<SpotDecimalInput>(box);
        Assert.Equal(Avalonia.Media.TextAlignment.Right, box.TextAlignment);
        Assert.Equal(box.Text!.Length, Math.Abs(box.SelectionEnd - box.SelectionStart));

        TypeText(table, "12.5");
        Keys.Press(table, Key.Enter);
        Assert.Equal("12.50", table.Model.Text(1, Fixtures.Qty));
        Assert.Contains("items[2].qty", table.Model.Draft.DirtyPaths());
        // Enter commits and goes down, and keeps entering: the next row's cell is open.
        Assert.Equal(new GridCursor(2, Fixtures.Qty, GridCursorOrigin.Keyboard), table.Grid.GridCellCursor);
        Assert.Equal((2, Fixtures.Qty), table.Grid.EditingCell);
    }

    [AvaloniaFact]
    public void TabAndShiftTabMoveBetweenEditableCellsSkippingReadOnlyOnes()
    {
        var table = Make();
        Go(table, 0, Fixtures.Rate);
        Keys.Press(table, Key.Enter);
        Keys.Press(table, Key.Tab);
        // amount (column 4) is read-only: Tab lands on unit (column 5).
        Assert.Equal((0, Fixtures.Unit), table.Grid.EditingCell);
        Keys.Press(table, Key.Tab, KeyModifiers.Shift);
        Assert.Equal((0, Fixtures.Rate), table.Grid.EditingCell);
    }

    [AvaloniaFact]
    public void TabCommitsTheEditedValue()
    {
        var table = Make();
        Go(table, 0, Fixtures.Desc);
        Keys.Press(table, Key.Enter);
        TypeText(table, "!");
        Keys.Press(table, Key.Tab);
        Assert.Equal("Line 1!", table.Model.Text(0, Fixtures.Desc));
        Assert.Equal((0, Fixtures.Qty), table.Grid.EditingCell);
    }

    [AvaloniaFact]
    public void CtrlEnterCommitsAndStaysWithoutAnEditor()
    {
        var table = Make();
        Go(table, 2, Fixtures.Qty);
        Keys.Press(table, Key.Enter);
        TypeText(table, "4");
        Keys.Press(table, Key.Enter, KeyModifiers.Control);
        Assert.False(table.Grid.IsEditing);
        Assert.Equal("4.00", table.Model.Text(2, Fixtures.Qty));
        Assert.Equal(2, table.Grid.GridCellCursor.RowIndex);
    }

    [AvaloniaFact]
    public void UpAndDownInAnEditorCommitAndMove()
    {
        var table = Make();
        Go(table, 2, Fixtures.Qty);
        Keys.Press(table, Key.Enter);
        TypeText(table, "6");
        Keys.Press(table, Key.Up);
        Assert.Equal("6.00", table.Model.Text(2, Fixtures.Qty));
        Assert.Equal((1, Fixtures.Qty), table.Grid.EditingCell);
        Keys.Press(table, Key.Down);
        Keys.Press(table, Key.Down);
        Assert.Equal((3, Fixtures.Qty), table.Grid.EditingCell);
    }

    [AvaloniaFact]
    public void EnterOnTheLastRowsLastEditableCellAddsARowAndEditsItsFirstCell()
    {
        var table = Make(3);
        Go(table, 2, Fixtures.Taxable);
        Keys.Press(table, Key.Enter);
        Assert.Equal((2, Fixtures.Taxable), table.Grid.EditingCell);
        Keys.Press(table, Key.Enter);
        Assert.Equal(4, table.Model.RowCount);
        Assert.True(table.Model.Table.Rows[3].IsNew);
        Assert.Equal((3, Fixtures.Item), table.Grid.EditingCell);
    }

    [AvaloniaFact]
    public void EnterOnTheLastRowsMiddleCellDoesNotAddARow()
    {
        var table = Make(3);
        Go(table, 2, Fixtures.Qty);
        Keys.Press(table, Key.Enter);
        Keys.Press(table, Key.Enter);
        Assert.Equal(3, table.Model.RowCount);
        Assert.False(table.Grid.IsEditing);
    }

    [AvaloniaFact]
    public void EnterOnAReadOnlyCellGoesDownAndOnTheLastCellAddsARow()
    {
        var table = Make(2);
        Go(table, 0, Fixtures.Amount);
        Keys.Press(table, Key.Enter);
        Assert.False(table.Grid.IsEditing);
        Assert.Equal(1, table.Grid.GridCellCursor.RowIndex);
        Go(table, 1, Fixtures.Amount);
        Keys.Press(table, Key.Enter);
        Assert.Equal(2, table.Model.RowCount); // the amount cell is not the last column
        Assert.Equal(1, table.Grid.GridCellCursor.RowIndex);
    }

    [AvaloniaFact]
    public void InvalidTextKeepsTheEditorOpenWithAMessageInTheCell()
    {
        var table = Make();
        Go(table, 1, Fixtures.Desc);
        Keys.Press(table, Key.Enter);
        Keys.Press(table, Key.Escape);
        Go(table, 1, Fixtures.Qty);
        Keys.Press(table, Key.Enter);
        var box = ActiveText(table);
        box.Text = "1.234";
        Keys.Press(table, Key.Enter);
        Assert.True(table.Grid.IsEditing);
        Assert.NotNull(table.EditorErrorText);
        Assert.Equal("2.00", table.Model.Text(1, Fixtures.Qty));
        Assert.False(table.Model.Draft.IsDirty);
        Keys.Press(table, Key.Escape);
        Assert.False(table.Grid.IsEditing);
    }

    [AvaloniaFact]
    public void CtrlIInsertsAboveAndCtrlShiftIBelow()
    {
        var table = Make(3);
        Go(table, 1, Fixtures.Desc);
        Keys.Press(table, Key.I, KeyModifiers.Control);
        Assert.Equal(["r1", null, "r2", "r3"], table.Model.Ids());
        Assert.Equal(1, table.Grid.GridCellCursor.RowIndex);
        Keys.Press(table, Key.I, KeyModifiers.Control | KeyModifiers.Shift);
        Assert.Equal(["r1", null, null, "r2", "r3"], table.Model.Ids());
        Assert.Equal(2, table.Grid.GridCellCursor.RowIndex);
    }

    [AvaloniaFact]
    public void CtrlDeleteDeletesTheSelectedRowsAndCtrlZBringsThemBack()
    {
        var table = Make(5);
        Go(table, 1, Fixtures.Desc);
        table.Grid.SetSelection([1, 2]);
        Keys.Press(table, Key.Delete, KeyModifiers.Control);
        Assert.Equal(["r1", "r4", "r5"], table.Model.Ids());
        Keys.Press(table, Key.Z, KeyModifiers.Control);
        Assert.Equal(["r1", "r2", "r3", "r4", "r5"], table.Model.Ids());
        Keys.Press(table, Key.Y, KeyModifiers.Control);
        Assert.Equal(["r1", "r4", "r5"], table.Model.Ids());
        Keys.Press(table, Key.Z, KeyModifiers.Control);
        Keys.Press(table, Key.Z, KeyModifiers.Control | KeyModifiers.Shift);
        Assert.Equal(["r1", "r4", "r5"], table.Model.Ids());
    }

    [AvaloniaFact]
    public void CtrlDeleteWithAStaleSelectionDeletesTheCursorRowOnly()
    {
        var table = Make(5);
        table.Grid.SetSelection([0]);
        table.Grid.SetCursor(3, 1);
        table.Grid.Focus();
        Keys.Press(table, Key.Delete, KeyModifiers.Control);
        Assert.Equal(["r1", "r2", "r3", "r5"], table.Model.Ids());
    }

    [AvaloniaFact]
    public void AltUpAndAltDownMoveTheRowAndTheCursorFollows()
    {
        var table = Make(4);
        Go(table, 1, Fixtures.Desc);
        Keys.Press(table, Key.Down, KeyModifiers.Alt);
        Assert.Equal(["r1", "r3", "r2", "r4"], table.Model.Ids());
        Assert.Equal(2, table.Grid.GridCellCursor.RowIndex);
        Assert.Equal([2], table.Grid.SelectedRows);
        Keys.Press(table, Key.Up, KeyModifiers.Alt);
        Keys.Press(table, Key.Up, KeyModifiers.Alt);
        Assert.Equal(["r2", "r1", "r3", "r4"], table.Model.Ids());
        Assert.Equal(0, table.Grid.GridCellCursor.RowIndex);
        Keys.Press(table, Key.Up, KeyModifiers.Alt);
        Assert.Equal(["r2", "r1", "r3", "r4"], table.Model.Ids());
    }

    [AvaloniaFact]
    public void CtrlDDuplicatesAndCtrlShiftDFillsDown()
    {
        var table = Make(3);
        Go(table, 0, Fixtures.Desc);
        Keys.Press(table, Key.D, KeyModifiers.Control);
        Assert.Equal(["r1", null, "r2", "r3"], table.Model.Ids());
        Assert.Equal("Line 1", table.Model.Text(1, Fixtures.Desc));

        Go(table, 2, Fixtures.Rate);
        table.Model.SetCell(1, Fixtures.Rate, new DecimalFieldValue(55m));
        Go(table, 2, Fixtures.Rate);
        Keys.Press(table, Key.D, KeyModifiers.Control | KeyModifiers.Shift);
        Assert.Equal("55.00", table.Model.Text(2, Fixtures.Rate));
    }

    [AvaloniaFact]
    public async Task CtrlVPastesTheClipboardAtTheCursorAsOneUndoStep()
    {
        var table = Make(3);
        table.Clipboard = new FakeClipboard("A\t5\nB\t6\nC\t7");
        Go(table, 2, Fixtures.Desc);
        Keys.Press(table, Key.V, KeyModifiers.Control);
        for (var i = 0; i < 20 && table.Model.RowCount < 5; i++)
        {
            await Task.Delay(5, TestContext.Current.CancellationToken);
            TestHost.Pump();
        }

        Assert.Equal(5, table.Model.RowCount);
        Assert.Equal("C", table.Model.Text(4, Fixtures.Desc));
        Assert.Equal(1, table.Model.Table.JournalCount);
        Assert.Equal([2, 3, 4], table.Grid.SelectedRows);
        Keys.Press(table, Key.Z, KeyModifiers.Control);
        Assert.Equal(3, table.Model.RowCount);
    }

    [AvaloniaFact]
    public async Task PasteWithErrorsChangesNothingAndReportsThem()
    {
        var table = Make(3);
        table.Clipboard = new FakeClipboard("A\tnope");
        Go(table, 0, Fixtures.Desc);
        ChildTableOperationEventArgs? seen = null;
        table.OperationCompleted += (_, e) => seen = e;
        var result = await table.PasteAsync();
        Assert.False(result.Succeeded);
        Assert.False(table.Model.Draft.IsDirty);
        Assert.Equal("paste", seen?.Operation);
        Assert.Equal("qty", Assert.Single(result.Paste!.Errors).Column);
    }

    [AvaloniaFact]
    public void ARowOperationFinishesTheOpenCellFirstAndStopsIfItWillNotCommit()
    {
        var table = Make(3);
        Go(table, 1, Fixtures.Qty);
        Keys.Press(table, Key.Enter);
        ActiveText(table).Text = "abc";
        var result = table.AddRow();
        Assert.False(result.Succeeded);
        Assert.Equal(3, table.Model.RowCount);
        Assert.True(table.Grid.IsEditing);
        Keys.Press(table, Key.Escape);
        Assert.True(table.AddRow().Succeeded);
    }

    [AvaloniaFact]
    public void ARowOperationCommitsTheOpenCellBeforeItRuns()
    {
        var table = Make(3);
        Go(table, 1, Fixtures.Qty);
        Keys.Press(table, Key.Enter);
        TypeText(table, "8");
        Assert.True(table.InsertAbove().Succeeded);
        Assert.False(table.Grid.IsEditing);
        Assert.Equal("8.00", table.Model.Text(2, Fixtures.Qty)); // the edited row moved down
        Assert.Equal(1, table.Grid.GridCellCursor.RowIndex);
    }

    [AvaloniaFact]
    public void KeysInsideAnEditorNeverReachTheGrid()
    {
        var table = Make(5);
        Go(table, 2, Fixtures.Desc);
        Keys.Press(table, Key.Enter);
        // Space would toggle the grid's check, Home would move its cursor, Ctrl+I would insert.
        Keys.Press(table, Key.Space);
        Keys.Press(table, Key.PageDown);
        Keys.Press(table, Key.I, KeyModifiers.Control);
        Keys.Press(table, Key.Delete, KeyModifiers.Control);
        Assert.True(table.Grid.IsEditing);
        Assert.Equal(2, table.Grid.GridCellCursor.RowIndex);
        Assert.Equal(5, table.Model.RowCount);
    }

    [AvaloniaFact]
    public void FocusFirstErrorPutsTheCursorOnTheFirstInvalidCell()
    {
        var table = Make(6);
        Go(table, 0, 0);
        table.Model.ShowErrors(
        [
            new FieldError("items[5].rate", "x", "Bad rate"),
            new FieldError("items[3].desc", "x", "Bad desc"),
            new FieldError("items[3].qty", "x", "Bad qty"),
        ]);
        Assert.True(table.FocusFirstError());
        Assert.Equal((2, Fixtures.Desc), (table.Grid.GridCellCursor.RowIndex, table.Grid.GridCellCursor.ColumnIndex));
        Assert.Equal([2], table.Grid.SelectedRows);

        table.Model.ClearErrors();
        Assert.False(table.FocusFirstError());
    }

    [AvaloniaFact]
    public void FocusFirstErrorFindsARowErrorToo()
    {
        var table = Make(4);
        ErrorMapper.Apply(table.Model.Draft, new Spotledger.Client.Api.ApiError
        {
            Code = "document.validation_failed",
            Message = "Validation failed",
            FieldErrors = [new Spotledger.Client.Api.ApiFieldError("items[4]", "document.validation_failed", "Check row")],
        });
        Assert.True(table.FocusFirstError());
        Assert.Equal(3, table.Grid.GridCellCursor.RowIndex);
    }

    [AvaloniaFact]
    public async Task LinkCellUsesTheLookupBoxWithTheRowContextAndPicksFromIt()
    {
        var lookup = new FakeLookup("ITEM-A", "ITEM-B", "OTHER");
        ChildTableLookupRequest? request = null;
        var table = Make(3, lookups: r =>
        {
            request = r;
            return lookup;
        });
        Go(table, 1, Fixtures.Item);
        Keys.Press(table, Key.Enter);
        var host = FindHost(table)!;
        var box = Assert.IsType<SpotLookupBox>(host.Editor);
        Assert.Equal("item", request!.Field.LinkTo);
        Assert.Equal(1, request.RowIndex);
        Assert.Equal("Line 2", ((TextFieldValue)request.Row["desc"]).Value);
        Assert.Same(table.Model.Draft, request.Draft);
        Assert.Same(lookup, box.Source);

        Keys.Press(table, Key.A, KeyModifiers.Control);
        Keys.Type(table, "B");
        await box.PendingSearch;
        TestHost.Pump();
        Assert.Equal(["ITEM-B"], box.Items.Select(item => item.Id));
        Keys.Press(table, Key.Enter); // accepts the highlighted candidate
        Assert.Equal("ITEM-B", box.SelectedItem?.Id);
        Assert.True(table.Grid.IsEditing);
        Keys.Press(table, Key.Enter); // commits the cell
        var link = Assert.IsType<LinkFieldValue>(table.Model.Table.Rows[1].GetValue("item"));
        Assert.Equal("ITEM-B", link.Id);
        Assert.Equal("ITEM-B name", link.DisplayText);
        Assert.Contains("items[2].item", table.Model.Draft.DirtyPaths());
    }

    [AvaloniaFact]
    public void SelectOpensOnF4AndAltDownLikeTheComboBox()
    {
        var table = Make(2);
        Go(table, 0, Fixtures.Unit);
        Keys.Press(table, Key.Enter);
        var host = FindHost(table)!;
        var select = Assert.IsAssignableFrom<ComboBox>(host.Editor);
        Assert.True(table.Grid.IsEditing);
        Keys.Press(table, Key.F4);
        Assert.True(select.IsDropDownOpen);
        Keys.Press(table, Key.Escape);
        Assert.False(select.IsDropDownOpen);
        Keys.Press(table, Key.Down, KeyModifiers.Alt);
        Assert.True(select.IsDropDownOpen);
    }
}
