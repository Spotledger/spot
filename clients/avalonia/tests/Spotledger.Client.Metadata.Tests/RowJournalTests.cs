using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Metadata.Tests;

/// <summary>
/// Deliverable 2 (WP-UI-07.T4): the draft's operation-local row journal. Every operation
/// kind is undoable and redoable — a cell edit gets its previous value back, an added row
/// goes away, a deleted row returns <b>with its id and position</b>, a move returns to
/// where it came from — and a bulk operation is one entry. The journal is bounded, is
/// cleared by <c>LoadRows</c>, <c>Revert</c> and <c>MarkSaved</c>, drops its redo tail on
/// a new edit, and never reaches the parent's fields or another table.
/// </summary>
public sealed class RowJournalTests
{
    [Fact]
    public void ChangedPermissionsEndTheJournalSoAnUndoCannotWriteOldValuesBack()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        table.Rows[1].SetValue("qty", new DecimalFieldValue(99m));
        Assert.True(table.CanUndo);

        draft.ApplyPermissions(PermissionSnapshot.Decode("""{"etag":"journal-perm","data":{"doctypes":[]}}"""));

        Assert.False(table.CanUndo);
        Assert.False(table.CanRedo);
        Assert.False(table.UndoRow());
        Assert.Equal("99", Assert.IsType<DecimalFieldValue>(table.Rows[1].Values["qty"].Value).CanonicalText);
    }

    [Fact]
    public void ClearingTheJournalUnderAnOpenOperationReleasesItsNotificationHold()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        var operation = table.Begin("paste");
        table.Rows[0].SetValue("qty", new DecimalFieldValue(7m));

        table.Revert();
        operation.Commit(); // abandoned: a no-op, never throws

        Assert.False(table.IsOperationOpen);
        Assert.False(table.CanUndo);
        var events = 0;
        draft.StructureChanged += (_, _) => events++;
        table.Rows[0].SetValue("qty", new DecimalFieldValue(8m));
        Assert.Equal(1, events); // notifications flow again
    }

    [Fact]
    public void CellEditUndoesToThePreviousValueAndKeepsDirtyTrackingHonest()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        var before = table.Rows[1].Values["qty"].Value;

        table.Rows[1].SetValue("qty", new DecimalFieldValue(99m));
        Assert.True(table.IsDirty);
        Assert.True(table.CanUndo);
        Assert.Equal("edit qty", table.UndoDescription);

        Assert.True(table.UndoRow());

        Assert.Same(before, table.Rows[1].Values["qty"].Value);
        Assert.False(table.IsDirty);
        Assert.False(table.CanUndo);
        Assert.True(table.CanRedo);

        Assert.True(table.RedoRow());
        Assert.Equal("99", Assert.IsType<DecimalFieldValue>(table.Rows[1].Values["qty"].Value).CanonicalText);
        Assert.True(table.IsDirty);
    }

    [Fact]
    public void UndoRestoresACellThatTheEditCreated()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        var row = table.Rows[0];
        Assert.False(row.Values.ContainsKey("discount"));

        row.SetValue("discount", new DecimalFieldValue(5m));
        Assert.True(row.Values.ContainsKey("discount"));

        Assert.True(table.UndoRow());
        Assert.False(row.Values.ContainsKey("discount"));
    }

    [Fact]
    public void AddedRowUndoesAwayAndRedoBringsTheSameRowBack()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        var added = table.AddRow();
        added.SetValue("qty", new DecimalFieldValue(7m));
        Assert.Equal(4, table.Rows.Count);

        Assert.True(table.UndoRow()); // the cell edit
        Assert.True(table.UndoRow()); // the addition
        Assert.Equal(3, table.Rows.Count);
        Assert.False(table.IsDirty);

        Assert.True(table.RedoRow()); // the addition
        Assert.Equal(4, table.Rows.Count);
        Assert.Same(added, table.Rows[3]);
        Assert.True(table.RedoRow()); // the cell edit that followed it
        Assert.Equal("7", Assert.IsType<DecimalFieldValue>(table.Rows[3].Values["qty"].Value).CanonicalText);
    }

    [Fact]
    public void InsertedRowUndoesToItsOriginalPositions()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");

        table.AddRowAt(1);
        Assert.Equal(["row-0001", null, "row-0002", "row-0003"], table.Rows.Select(row => row.Id));

        Assert.True(table.UndoRow());
        Assert.Equal(["row-0001", "row-0002", "row-0003"], table.Rows.Select(row => row.Id));
        Assert.False(table.IsDirty);
    }

    [Fact]
    public void DeletedRowComesBackWithItsIdIndexAndValues()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        var removed = table.Rows[1];

        table.RemoveRowAt(1);
        Assert.Equal(["row-0001", "row-0003"], table.Rows.Select(row => row.Id));

        Assert.True(table.UndoRow());

        var restored = table.Rows[1];
        Assert.Same(removed, restored);
        Assert.Equal("row-0002", restored.Id);
        Assert.Equal("20.00", Assert.IsType<DecimalFieldValue>(restored.Values["qty"].Value).CanonicalText);
        Assert.Equal("ITEM-2", Assert.IsType<LinkFieldValue>(restored.Values["item_code"].Value).Id);
        Assert.Equal(["row-0001", "row-0002", "row-0003"], table.Rows.Select(row => row.Id));
        Assert.False(table.IsDirty);
        Assert.False(table.CanUndo);
        Assert.True(table.CanRedo);

        Assert.True(table.RedoRow());
        Assert.Equal(["row-0001", "row-0003"], table.Rows.Select(row => row.Id));
    }

    [Fact]
    public void DeletedRowKeepsItsUnsavedCellThroughTheUndo()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        table.Rows[2].SetValue("qty", new DecimalFieldValue(31m));

        table.RemoveRowAt(2);
        Assert.True(table.UndoRow()); // the delete

        // The row comes back exactly as it was, its unsaved cell included.
        Assert.Equal("31", Assert.IsType<DecimalFieldValue>(table.Rows[2].Values["qty"].Value).CanonicalText);
        Assert.True(draft.IsDirty);

        Assert.True(table.UndoRow()); // the edit that preceded the delete
        Assert.Equal("30.00", Assert.IsType<DecimalFieldValue>(table.Rows[2].Values["qty"].Value).CanonicalText);
        Assert.False(draft.IsDirty);
    }

    [Fact]
    public void MovedRowUndoesToItsOriginalPosition()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");

        table.MoveRow(0, 2);
        Assert.Equal(["row-0002", "row-0003", "row-0001"], table.Rows.Select(row => row.Id));

        Assert.True(table.UndoRow());
        Assert.Equal(["row-0001", "row-0002", "row-0003"], table.Rows.Select(row => row.Id));
        Assert.False(table.IsDirty);

        Assert.True(table.RedoRow());
        Assert.Equal(["row-0002", "row-0003", "row-0001"], table.Rows.Select(row => row.Id));
    }

    [Fact]
    public void BulkOperationIsOneEntryAndUndoesAsAWhole()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");

        using (var operation = table.Begin("paste 2 rows"))
        {
            table.Rows[0].SetValue("qty", new DecimalFieldValue(11m));
            var first = table.AddRow();
            first.SetValue("qty", new DecimalFieldValue(5m));
            var second = table.AddRow();
            second.SetValue("qty", new DecimalFieldValue(6m));
            operation.Commit();
        }

        Assert.Equal(1, table.JournalCount);
        Assert.Equal("paste 2 rows", table.UndoDescription);

        Assert.True(table.UndoRow());

        Assert.Equal(3, table.Rows.Count);
        Assert.Equal("10.00", Assert.IsType<DecimalFieldValue>(table.Rows[0].Values["qty"].Value).CanonicalText);
        Assert.False(table.IsDirty);

        Assert.True(table.RedoRow());
        Assert.Equal(5, table.Rows.Count);
        Assert.Equal("11", Assert.IsType<DecimalFieldValue>(table.Rows[0].Values["qty"].Value).CanonicalText);
        Assert.Equal("5", Assert.IsType<DecimalFieldValue>(table.Rows[3].Values["qty"].Value).CanonicalText);
    }

    [Fact]
    public void BulkOperationThatChangesNothingIsNotJournaled()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");

        using (var operation = table.Begin("empty paste"))
        {
            operation.Commit();
        }

        Assert.Equal(0, table.JournalCount);
        Assert.False(table.CanUndo);
    }

    [Fact]
    public void NewEditAfterAnUndoDropsTheRedoTail()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        table.Rows[0].SetValue("qty", new DecimalFieldValue(1m));
        table.Rows[1].SetValue("qty", new DecimalFieldValue(2m));
        Assert.True(table.UndoRow());
        Assert.True(table.CanRedo);

        table.Rows[1].SetValue("qty", new DecimalFieldValue(3m));

        Assert.False(table.CanRedo);
        Assert.Equal(2, table.JournalCount);
        Assert.True(table.UndoRow());
        Assert.True(table.UndoRow());
        Assert.False(table.UndoRow());
        Assert.Equal("10.00", Assert.IsType<DecimalFieldValue>(table.Rows[0].Values["qty"].Value).CanonicalText);
    }

    [Fact]
    public void JournalIsClearedByLoadRowsRevertAndMarkSaved()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");

        table.Rows[0].SetValue("qty", new DecimalFieldValue(1m));
        table.LoadRows(new TableFieldValue(
            [new TableRowValue("row-0001", new Dictionary<string, FieldValue>(StringComparer.Ordinal)
            {
                ["qty"] = new DecimalFieldValue(10m),
            })],
            TestModels.TaxesRowModel()));
        Assert.False(table.CanUndo);

        table.Rows[0].SetValue("qty", new DecimalFieldValue(2m));
        draft.Revert();
        Assert.False(table.CanUndo);

        table.Rows[0].SetValue("qty", new DecimalFieldValue(3m));
        draft.MarkSaved();
        Assert.False(table.CanUndo);
        Assert.Equal(0, table.JournalCount);
    }

    [Fact]
    public void JournalDepthDropsTheOldestOperation()
    {
        var draft = new DocumentDraft(TestModels.Invoice(), journalDepth: 2);
        draft.LoadFrom(TestModels.DocumentJson);
        var table = draft.Table("taxes");

        table.Rows[0].SetValue("qty", new DecimalFieldValue(1m));
        table.Rows[0].SetValue("qty", new DecimalFieldValue(2m));
        table.Rows[0].SetValue("qty", new DecimalFieldValue(3m));

        Assert.Equal(2, table.JournalCount);
        Assert.True(table.UndoRow());
        Assert.True(table.UndoRow());
        Assert.False(table.UndoRow());
        // Only the two newest operations were kept: the first edit's previous value is gone.
        Assert.Equal("1", Assert.IsType<DecimalFieldValue>(table.Rows[0].Values["qty"].Value).CanonicalText);
    }

    [Fact]
    public void JournalsArePerTableAndNeverTouchTheParentsFields()
    {
        var draft = TwoTables();
        var items = draft.Table("items");
        var other = draft.Table("other");

        draft.Set("title", new TextFieldValue("edited"));
        items.Rows[0].SetValue("qty", new DecimalFieldValue(2m));

        Assert.Equal(1, items.JournalCount);
        Assert.Equal(0, other.JournalCount);

        Assert.True(items.UndoRow());

        Assert.Equal("1.00", Assert.IsType<DecimalFieldValue>(items.Rows[0].Values["qty"].Value).CanonicalText);
        Assert.Equal("edited", Assert.IsType<TextFieldValue>(draft.Get("title")).Value);
        Assert.False(other.CanUndo);
        // The parent's own edit is untouched by the table's undo.
        Assert.Equal(["title"], draft.DirtyPaths().ToList());
    }

    [Fact]
    public void UndoAndRedoAnswerFalseWhenThereIsNothingToDo()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");

        Assert.False(table.CanUndo);
        Assert.False(table.CanRedo);
        Assert.False(table.UndoRow());
        Assert.False(table.RedoRow());
        Assert.Null(table.UndoDescription);
        Assert.Null(table.RedoDescription);
    }

    [Fact]
    public void UndoingWhileAnOperationIsOpenIsRefused()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        using var operation = table.Begin("paste");

        Assert.Throws<InvalidOperationException>(() => table.UndoRow());
    }

    [Fact]
    public void JournalDepthMustKeepAtLeastOneOperation()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");

        Assert.Throws<ArgumentOutOfRangeException>(() => table.JournalDepth = 0);
        Assert.Throws<ArgumentOutOfRangeException>(() => new DocumentDraft(TestModels.Invoice(), journalDepth: 0));
    }

    /// <summary>A document with two child tables and one parent field (per-table journals).</summary>
    private static DocumentDraft TwoTables()
    {
        var row = TestModels.TaxesRowModel();
        var parent = new DocTypeModel(
            "record",
            "Record",
            1,
            false,
            [
                new FieldModel("title", "Title", FieldKind.Text),
                new FieldModel("items", "Items", FieldKind.Table, childDoctype: "sales_invoice_item"),
                new FieldModel("other", "Other", FieldKind.Table, childDoctype: "sales_invoice_item"),
            ],
            new Dictionary<string, DocTypeModel>(StringComparer.Ordinal)
            {
                ["items"] = row,
                ["other"] = row,
            });
        var draft = new DocumentDraft(parent);
        draft.LoadFrom(
            """{"title":"loaded","items":[{"id":"a1","item_code":"ITEM-1","qty":"1.00"}],"other":[{"id":"b1","item_code":"ITEM-2","qty":"9.00"}]}""");
        return draft;
    }
}
